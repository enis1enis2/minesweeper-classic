/*
 * ms_core.c - platform-independent core of the Linux Minesweeper client.
 *
 * POSIX port of the game logic, custom seed system, telemetry metrics,
 * leaderboard persistence and CLI command dispatch from src/minesweeper.c and
 * src/leader.c.  The frontends (terminal, X11, headless) drive this core from
 * a single main thread; the telemetry and CLI threads communicate with it
 * exclusively through the marshalled event queue (ms_event_*).
 *
 * MIT License
 */
#include "ms_core.h"
#include "ms_net.h"
#include "ms_ini.h"
#include "analyze.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>
#include <time.h>
#include <unistd.h>
#include <fcntl.h>

/* ---------- constants ---------- */
#define CUSTOM_SEED_TARGET_DIGITS 19   /* 19 digits always fits in uint64 */
#define CUSTOM_SEED_MAX_STEPS     32   /* hard cap: x2, x4, ... x2^32     */
#define CUSTOM_SEED_MAX_BUF      160

/* ---------- difficulty presets (classic) ---------- */
const Difficulty g_presets[3] = {
    {  8,  8, 10, DIFF_BEGIN },        /* Beginner */
    { 16, 16, 40, DIFF_INTERMEDIATE }, /* Intermediate */
    { 16, 30, 99, DIFF_EXPERT },       /* Expert */
};

const char *const g_diff_names[DIFF_COUNT] = {
    "beginner", "intermediate", "expert", "custom"
};

static const char *const g_diff_salts[DIFF_COUNT] = {
    "beginner", "intermediate", "expert", "custom"
};

/* ---------- game state ---------- */
static Game g_game;
static Difficulty g_custom = { 16, 16, 40, DIFF_CUSTOM };

/* one-shot seed override (legacy): consumed by the next game_reset() */
static int      g_seed_override = 0;
static uint64_t g_seed_override_val = 0;

/* per-difficulty seed slots */
static SeedSlot g_diff_seeds[DIFF_COUNT];

/* last started board's resolved seed */
static int      g_game_seed_active = 0;
static uint64_t g_game_seed_active_val = 0;

/* telemetry metrics (collected on the main thread) */
static unsigned long long g_metric_clicks = 0;
static double g_metric_latency_ema_us = 0.0;
static int    g_metric_latency_n = 0;

/* mouse / chord tracking */
static int g_pressed_cell = -1;
static int g_chord_cell   = -1;
static int g_face_pressed = 0;
static int g_left_down    = 0;
static int g_right_down   = 0;

/* repaint after CLI-triggered mutations? (refresh command; on by default) */
static int g_cli_refresh = 1;

/* ------------------------------------------------------------------ */
/* frontend hooks                                                      */
/* ------------------------------------------------------------------ */
void (*fe_repaint)(void) = NULL;
void (*fe_set_title)(const char *title) = NULL;
void (*fe_hof_start)(void) = NULL;
void (*fe_hof_entry)(int rank, const char *diff, const char *name,
                     int time_ms, long long ts) = NULL;
void (*fe_hof_end)(void) = NULL;
void (*fe_denied)(void) = NULL;

static void request_repaint(void) {
    if (fe_repaint) fe_repaint();
}

static void request_title(void) {
    if (fe_set_title) {
        char title[96];
        if (g_game_seed_active)
            snprintf(title, sizeof(title), "Minesweeper  [Seed: %llu]",
                     (unsigned long long)g_game_seed_active_val);
        else
            snprintf(title, sizeof(title), "Minesweeper");
        fe_set_title(title);
    }
}

/* ------------------------------------------------------------------ */
/* time                                                                */
/* ------------------------------------------------------------------ */
static uint64_t mono_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000u + (uint64_t)(ts.tv_nsec / 1000000);
}

uint64_t ms_now_ms(void) { return mono_ms(); }
uint64_t ms_now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000u + (uint64_t)(ts.tv_nsec / 1000);
}

/* ------------------------------------------------------------------ */
/* helpers                                                             */
/* ------------------------------------------------------------------ */
static inline int IDX(const Game *g, int r, int c) { return r * g->cols + c; }
static inline int INB(const Game *g, int r, int c) {
    return r >= 0 && r < g->rows && c >= 0 && c < g->cols;
}

static uint64_t xorshift(uint64_t *s) {
    uint64_t x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    return x;
}

/* ---------- board management ---------- */
static void free_board(Game *g) {
    free(g->mine);      g->mine = NULL;
    free(g->adj);       g->adj = NULL;
    free(g->revealed);  g->revealed = NULL;
    free(g->mark);      g->mark = NULL;
}

static int alloc_board(Game *g) {
    int n = g->rows * g->cols;
    if (n > MAX_CELLS) return 0;
    free_board(g);
    g->mine = calloc((size_t)n, 1);
    g->adj = calloc((size_t)n, 1);
    g->revealed = calloc((size_t)n, 1);
    g->mark = calloc((size_t)n, 1);
    if (!g->mine || !g->adj || !g->revealed || !g->mark) { free_board(g); return 0; }
    return 1;
}

static void compute_adj(Game *g) {
    int r, c, dr, dc;
    for (r = 0; r < g->rows; r++)
        for (c = 0; c < g->cols; c++) {
            int cnt = 0;
            for (dr = -1; dr <= 1; dr++)
                for (dc = -1; dc <= 1; dc++) {
                    int rr = r + dr, cc = c + dc;
                    if (INB(g, rr, cc) && g->mine[IDX(g, rr, cc)]) cnt++;
                }
            g->adj[IDX(g, r, c)] = (unsigned char)cnt;
        }
}

/* Place mines guaranteeing the 3x3 around the first click is clear. */
static void place_mines(Game *g, int sr, int sc) {
    int placed = 0, n = 0;
    int *pool = malloc(sizeof(int) * MAX_CELLS);
    int r, c;

    for (r = 0; r < g->rows; r++)
        for (c = 0; c < g->cols; c++) {
            int rr = r - sr, cc = c - sc;
            if (rr <= 1 && rr >= -1 && cc <= 1 && cc >= -1) continue;
            pool[n++] = IDX(g, r, c);
        }
    if (n < g->mines) {
        n = 0;
        for (r = 0; r < g->rows; r++)
            for (c = 0; c < g->cols; c++)
                if (!(r == sr && c == sc)) pool[n++] = IDX(g, r, c);
    }
    while (placed < g->mines && n > 0) {
        int k = (int)(xorshift(&g->rng) % (uint64_t)n);
        int idx = pool[k];
        pool[k] = pool[--n];
        if (!g->mine[idx]) { g->mine[idx] = 1; placed++; }
    }
    free(pool);
    compute_adj(g);
}

/* ---------- end game + reveal ---------- */
static void end_game_lose(Game *g);
static void end_game_win(Game *g);

static void metric_emit_start(int diff);
static void metric_emit_over(const char *kind);

static void reveal_cell(Game *g, int r, int c) {
    int i, dr, dc;
    if (!INB(g, r, c)) return;
    i = IDX(g, r, c);
    if (g->revealed[i] || g->mark[i] == 1) return;
    if (g->mine[i]) { end_game_lose(g); return; }
    if (g->over) return;
    g->revealed[i] = 1;
    g->opened++;
    if (g->adj[i] == 0)
        for (dr = -1; dr <= 1; dr++)
            for (dc = -1; dc <= 1; dc++)
                reveal_cell(g, r + dr, c + dc);
    if (g->opened == g->rows * g->cols - g->mines)
        end_game_win(g);
}

static void end_game_lose(Game *g) {
    int i, n = g->rows * g->cols;
    if (g->over) return;
    g->over = -1;
    for (i = 0; i < n; i++)
        if (g->mine[i]) g->revealed[i] = 1;
    g_pressed_cell = g_chord_cell = -1;
    metric_emit_over("loss");
    if (g_cli_refresh) request_repaint();
}

static void end_game_win(Game *g) {
    int i, n = g->rows * g->cols;
    if (g->over) return;
    g->over = 1;
    for (i = 0; i < n; i++)
        if (g->mine[i] && g->mark[i] != 1) {
            g->mark[i] = 1;
            g->flags++;
        }
    g_pressed_cell = g_chord_cell = -1;
    metric_emit_over("win");
    leader_submit_win(g->diff, g->time * 1000);
    if (g_cli_refresh) request_repaint();
}

static int first_click(Game *g, int r, int c) {
    if (g->started) return 0;
    g->started = 1;
    place_mines(g, r, c);
    return 1;
}

/* chord: reveal neighbours of a revealed number when flag count matches */
static void do_chord(Game *g, int cell) {
    int r = cell / g->cols, c = cell % g->cols;
    int cnt = 0, dr, dc;
    for (dr = -1; dr <= 1; dr++)
        for (dc = -1; dc <= 1; dc++) {
            int rr = r + dr, cc = c + dc;
            if (INB(g, rr, cc) && g->mark[IDX(g, rr, cc)] == 1) cnt++;
        }
    if (cnt == g->adj[cell]) {
        for (dr = -1; dr <= 1; dr++)
            for (dc = -1; dc <= 1; dc++) {
                int rr = r + dr, cc = c + dc;
                if (INB(g, rr, cc))
                    reveal_cell(g, rr, cc);
            }
    }
}

static void cycle_mark(Game *g, int cell) {
    if (g->over) return;
    switch (g->mark[cell]) {
    case 0:
        g->mark[cell] = 1;
        g->flags++;
        break;
    case 1:
        g->flags--;
        g->mark[cell] = g->marks_enabled ? 2 : 0;
        break;
    default:
        g->mark[cell] = 0;
        break;
    }
    if (g_cli_refresh) request_repaint();
}

/* ------------------------------------------------------------------ */
/* game reset                                                          */
/* ------------------------------------------------------------------ */
static const char *diff_str(const Game *g) {
    if (g->diff >= DIFF_BEGIN && g->diff < DIFF_COUNT)
        return g_diff_names[g->diff];
    return "custom";
}

Game *game_state(void) { return &g_game; }

void ms_refresh_title(void) { request_title(); }

void game_reset(const Difficulty *d, int marks) {
    Game *g = &g_game;
    g->rows = d->rows;
    g->cols = d->cols;
    g->mines = d->mines;
    g->diff = d->diff;
    g->marks_enabled = marks;
    g->flags = 0;
    g->opened = 0;
    g->started = 0;
    g->over = 0;
    g->time = 0;
    g->paused = 0;
    {
        uint64_t sval;
        int seeded;
        resolve_board_seed(d->diff, &sval, &seeded);
        if (seeded) {
            g_game_seed_active_val = sval;
            g->rng = sval;
            g_game_seed_active = 1;
        } else {
            g->rng = mono_ms() ^
                     ((uint64_t)(uintptr_t)g << 32) ^ (uint64_t)time(NULL);
            g_game_seed_active = 0;
        }
    }
    metrics_reset();
    metric_emit_start(d->diff);
    if (!alloc_board(g)) return;

    g_pressed_cell = g_chord_cell = -1;
    g_face_pressed = g_left_down = g_right_down = 0;

    request_title();
    request_repaint();
}

void game_new_diff(int diff) {
    Game *g = &g_game;
    if (diff >= DIFF_BEGIN && diff <= DIFF_EXPERT)
        game_reset(&g_presets[diff], g->marks_enabled);
}

/* Apply a streamed `seed <diff> <n>` as the persistent Normal seed and start
 * a fresh board (mirrors the Win32 WM_APP_TELEMETRY_SEED handler). */
void game_apply_telemetry_seed(int diff, uint64_t seed) {
    Game *g = &g_game;
    if (diff >= 0 && diff < DIFF_COUNT) {
        SeedSlot *sl = &g_diff_seeds[diff];
        sl->mode = SEED_NORMAL;
        snprintf(sl->value, sizeof(sl->value), "%llu",
                 (unsigned long long)seed);
    }
    game_reset(&g_presets[g->diff], g->marks_enabled);
}

/* ------------------------------------------------------------------ */
/* input: click / mark / chord / pointer (chord-anchor logic)          */
/* ------------------------------------------------------------------ */
static void arm_chord(Game *g, int cell) {
    g_chord_cell = -1;
    g_pressed_cell = -1;
    if (cell < 0) return;
    if (g->revealed[cell] && g->adj[cell] > 0) {
        g_chord_cell = cell;
    } else if (!g->revealed[cell] && g->mark[cell] != 1) {
        g_pressed_cell = cell;
    }
}

void game_click(int r, int c) {
    Game *g = &g_game;
    int cell;
    if (!INB(g, r, c)) return;
    metrics_note_click();
    if (g->over) return;
    cell = IDX(g, r, c);
    if (g->revealed[cell] || g->mark[cell] == 1) return;
    if (!g->started) first_click(g, r, c);
    reveal_cell(g, r, c);
    request_repaint();
}

void game_mark(int r, int c) {
    Game *g = &g_game;
    if (!INB(g, r, c)) return;
    metrics_note_click();
    if (g->revealed[IDX(g, r, c)]) return;
    cycle_mark(g, IDX(g, r, c));
}

void game_chord_at(int r, int c) {
    Game *g = &g_game;
    int cell;
    if (!INB(g, r, c)) return;
    metrics_note_click();
    cell = IDX(g, r, c);
    if (g->revealed[cell] && g->adj[cell] > 0) {
        do_chord(g, cell);
        request_repaint();
    }
}

/* region: PTR_FACE or PTR_GRID; button: 0 = left, 1 = right */
void game_pointer_down(int region, int cell, int button) {
    Game *g = &g_game;
    if (region == PTR_FACE) {
        g_face_pressed = 1;
        g_pressed_cell = g_chord_cell = -1;
        request_repaint();
        return;
    }
    if (cell < 0) { g_pressed_cell = g_chord_cell = -1; request_repaint(); return; }
    metrics_note_click();

    if (button == 0) {
        g_left_down = 1;
        if (g_right_down) {
            arm_chord(g, cell);
            request_repaint();
            return;
        }
        if (!g->revealed[cell] && g->mark[cell] != 1) {
            int r = cell / g->cols, c = cell % g->cols;
            if (!g->over) {
                if (!g->started) first_click(g, r, c);
                reveal_cell(g, r, c);
            }
            g_pressed_cell = -1;
        } else if (g->revealed[cell] && g->adj[cell] > 0) {
            g_pressed_cell = cell;
        } else {
            g_pressed_cell = -1;
        }
    } else {
        g_right_down = 1;
        if (g_left_down) {
            arm_chord(g, cell);
            request_repaint();
            return;
        }
        if (!g->revealed[cell])
            cycle_mark(g, cell);
        else if (g->adj[cell] > 0)
            g_pressed_cell = cell;
    }
    request_repaint();
}

void game_pointer_up(int region, int cell, int button) {
    Game *g = &g_game;
    int was_face = 0;
    (void)cell;

    if (g_face_pressed) {
        was_face = (region == PTR_FACE);
    }

    /* first of the two buttons to release completes a chord */
    if (g_left_down && g_right_down) {
        if (g_chord_cell != -1) {
            int ch = g_chord_cell;
            g_chord_cell = -1;
            g_pressed_cell = -1;
            do_chord(g, ch);
        } else if (g_pressed_cell != -1) {
            int pc = g_pressed_cell;
            int r = pc / g->cols, c = pc % g->cols;
            g_pressed_cell = -1;
            if (!g->over) {
                if (!g->started) first_click(g, r, c);
                reveal_cell(g, r, c);
            }
        }
    }

    if (button == 0) g_left_down = 0; else g_right_down = 0;

    if (was_face) {
        g_face_pressed = 0;
        if (!g->over) game_reset(&g_presets[g->diff], g->marks_enabled);
    }
    g_face_pressed = 0;
    g_pressed_cell = g_chord_cell = -1;
    request_repaint();
}

void game_pointer_cancel(void) {
    g_left_down = g_right_down = 0;
    g_pressed_cell = g_chord_cell = -1;
    g_face_pressed = 0;
    request_repaint();
}

/* 1-second timer step (called by the frontend loop) */
void game_tick(void) {
    Game *g = &g_game;
    if (g->over == 0 && !g->paused) {
        if (g->time < 999) g->time++;
        /* periodic UI-latency telemetry (every 10 s while playing) */
        if (g->time > 0 && (g->time % 10) == 0 && net_telemetry_active())
            net_send_metric("metric latency us=%.0f t=%llu",
                            g_metric_latency_ema_us,
                            (unsigned long long)ms_now_ms());
    }
}

int game_seed_active(void) { return g_game_seed_active; }
uint64_t game_seed_val(void) { return g_game_seed_active_val; }

int game_face_state(void) {
    Game *g = &g_game;
    if (g->over == 1) return 2;
    if (g->over == -1) return 3;
    if (g_face_pressed || g_pressed_cell != -1 || g_chord_cell != -1 ||
        (g_left_down && g_right_down))
        return 1;
    return 0;
}

/* ------------------------------------------------------------------ */
/* custom seed system                                                  */
/* ------------------------------------------------------------------ */
int custom_seed_all_digits(const char *s) {
    if (!s || !*s) return 0;
    for (; *s; s++) if (*s < '0' || *s > '9') return 0;
    return 1;
}

/* FNV-1a 64-bit: lightweight hash for alphanumeric seeds */
static uint64_t custom_seed_fnv1a64(const char *s) {
    uint64_t h = 14695981039346656037ULL;
    for (; *s; s++) {
        h ^= (unsigned char)*s;
        h *= 1099511628211ULL;
    }
    return h;
}

/* multiply a most-significant-first decimal string by m, in place.
   Returns the new length (buffer must hold len + 12 chars). */
static int custom_seed_str_mul(char *buf, int len, uint64_t m) {
    char tmp[160];
    int k = 0, i;
    uint64_t carry = 0;
    for (i = len - 1; i >= 0; i--) {
        uint64_t v = (uint64_t)(buf[i] - '0') * m + carry;
        tmp[k++] = (char)('0' + (v % 10));
        carry = v / 10;
    }
    while (carry) {
        tmp[k++] = (char)('0' + (carry % 10));
        carry /= 10;
    }
    for (i = 0; i < k; i++) buf[i] = tmp[k - 1 - i];
    return k;
}

/* Generate the final 64-bit seed from a raw input string. */
int custom_seed_generate(const char *input, uint64_t *out,
                         int *steps_out, int *truncated_out) {
    char buf[CUSTOM_SEED_MAX_BUF];
    int len, i, steps = 0, truncated = 0;
    int target = CUSTOM_SEED_TARGET_DIGITS;

    if (steps_out) *steps_out = 0;
    if (truncated_out) *truncated_out = 0;
    if (!out || !input || !*input) return 0;

    if (custom_seed_all_digits(input)) {
        len = (int)strlen(input);
        if (len >= (int)sizeof(buf)) len = (int)sizeof(buf) - 1;
        memcpy(buf, input, (size_t)len);
    } else {
        uint64_t hv = custom_seed_fnv1a64(input);
        len = snprintf(buf, sizeof(buf), "%llu", (unsigned long long)hv);
    }

    /* strip leading zeros */
    i = 0;
    while (i < len - 1 && buf[i] == '0') i++;
    if (i) { memmove(buf, buf + i, (size_t)(len - i)); len -= i; }

    if (len == 1 && buf[0] == '0') { *out = 0; return 1; }

    if (len >= target) {
        if (len > target) { len = target; truncated = 1; }
    } else {
        while (len < target && steps < CUSTOM_SEED_MAX_STEPS) {
            uint64_t m = 1ULL << (steps + 1);
            len = custom_seed_str_mul(buf, len, m);
            steps++;
            if (len > target) {
                len = target;
                truncated = 1;
                break;
            }
        }
    }

    {
        uint64_t v = 0;
        for (i = 0; i < len; i++) v = v * 10 + (uint64_t)(buf[i] - '0');
        *out = v;
    }
    if (steps_out) *steps_out = steps;
    if (truncated_out) *truncated_out = truncated;
    return 1;
}

int parse_diff_name(const char *s) {
    int i;
    if (!s || !*s) return -1;
    for (i = 0; i < DIFF_COUNT; i++)
        if (stricmp(s, g_diff_names[i]) == 0) return i;
    return -1;
}

/* Derive the seed for a per-difficulty CUSTOM slot. Pure numbers are used
   directly; anything else is hashed with "difficulty:" folded in. */
int diff_custom_seed_generate(int diff, const char *input, uint64_t *out,
                              int *steps_out, int *truncated_out) {
    if (!input || !*input) return 0;
    if (custom_seed_all_digits(input))
        return custom_seed_generate(input, out, steps_out, truncated_out);
    {
        char salted[CUSTOM_SEED_INPUT_MAX + 48];
        const char *salt = (diff >= 0 && diff < DIFF_COUNT)
                               ? g_diff_salts[diff] : "";
        snprintf(salted, sizeof(salted), "%s:%s", salt, input);
        return custom_seed_generate(salted, out, steps_out, truncated_out);
    }
}

/* Pick the seed for a new board of the given difficulty. */
void resolve_board_seed(int diff, uint64_t *out, int *seeded) {
    const SeedSlot *sl;
    *seeded = 0;
    if (g_seed_override) {
        *out = g_seed_override_val;
        g_seed_override = 0;
        *seeded = 1;
        return;
    }
    if (diff < 0 || diff >= DIFF_COUNT) return;
    sl = &g_diff_seeds[diff];
    if (sl->mode == SEED_NORMAL) {
        *out = strtoull(sl->value, NULL, 10);
        *seeded = 1;
    } else if (sl->mode == SEED_CUSTOM) {
        uint64_t v;
        if (diff_custom_seed_generate(diff, sl->value, &v, NULL, NULL)) {
            *out = v;
            *seeded = 1;
        }
    }
}

const SeedSlot *diff_seed_slot(int diff) {
    if (diff < 0 || diff >= DIFF_COUNT) return NULL;
    return &g_diff_seeds[diff];
}

void diff_seed_set_normal(int diff, uint64_t v) {
    if (diff < 0 || diff >= DIFF_COUNT) return;
    g_diff_seeds[diff].mode = SEED_NORMAL;
    snprintf(g_diff_seeds[diff].value, sizeof(g_diff_seeds[diff].value),
             "%llu", (unsigned long long)v);
}

void diff_seed_set_custom(int diff, const char *value) {
    if (diff < 0 || diff >= DIFF_COUNT || !value) return;
    g_diff_seeds[diff].mode = SEED_CUSTOM;
    strncpy(g_diff_seeds[diff].value, value,
            sizeof(g_diff_seeds[diff].value) - 1);
    g_diff_seeds[diff].value[sizeof(g_diff_seeds[diff].value) - 1] = 0;
}

void diff_seed_clear(int diff) {
    if (diff < 0 || diff >= DIFF_COUNT) return;
    g_diff_seeds[diff].mode = SEED_OFF;
}

void seed_override_set(uint64_t v) {
    g_seed_override = 1;
    g_seed_override_val = v;
}

void seed_override_clear(void) {
    g_seed_override = 0;
}

/* Apply one --seed / --seed-custom argument. */
static void apply_seed_arg(const char *arg, int custom) {
    char dname[32];
    const char *colon = strchr(arg, ':');
    const char *value = arg;
    int diff = -1;

    if (colon && colon != arg) {
        size_t n = (size_t)(colon - arg);
        if (n < sizeof(dname)) {
            memcpy(dname, arg, n);
            dname[n] = 0;
            diff = parse_diff_name(dname);
            if (diff >= 0) value = colon + 1;
        }
    }

    if (diff >= 0) {
        if (custom)
            diff_seed_set_custom(diff, value);
        else
            diff_seed_set_normal(diff, strtoull(value, NULL, 10));
    } else if (custom) {
        uint64_t v;
        if (custom_seed_generate(arg, &v, NULL, NULL))
            seed_override_set(v);
    } else {
        seed_override_set(strtoull(arg, NULL, 10));
    }
}

/* Scan argv for --seed / --seed-custom arguments (both "flag value" and
 * "flag=value" forms) and apply them. */
void ms_parse_seed_args(int argc, char **argv) {
    int i;
    for (i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (stricmp(a, "--seed-custom") == 0 && i + 1 < argc) {
            apply_seed_arg(argv[++i], 1);
        } else if (strnicmp(a, "--seed-custom=", 14) == 0) {
            apply_seed_arg(a + 14, 1);
        } else if (stricmp(a, "--seed") == 0 && i + 1 < argc) {
            apply_seed_arg(argv[++i], 0);
        } else if (strnicmp(a, "--seed=", 7) == 0) {
            apply_seed_arg(a + 7, 0);
        }
    }
}

/* ------------------------------------------------------------------ */
/* metrics                                                             */
/* ------------------------------------------------------------------ */
void metrics_reset(void) {
    g_metric_clicks = 0;
    g_metric_latency_ema_us = 0.0;
    g_metric_latency_n = 0;
}

void metrics_note_click(void) {
    g_metric_clicks++;
}

void metrics_note_ui_latency(long us) {
    if (us < 0) us = 0;
    if (g_metric_latency_n == 0)
        g_metric_latency_ema_us = (double)us;
    else
        g_metric_latency_ema_us = 0.8 * g_metric_latency_ema_us + 0.2 * (double)us;
    g_metric_latency_n++;
}

static void metric_emit_start(int diff) {
    if (!net_telemetry_active()) return;
    net_send_metric("metric start diff=%s seed=%llu seeded=%d t=%llu",
                    g_diff_names[diff],
                    (unsigned long long)g_game_seed_active_val,
                    g_game_seed_active ? 1 : 0,
                    (unsigned long long)ms_now_ms());
}

static void metric_emit_over(const char *kind) {
    Game *g = &g_game;
    if (!net_telemetry_active()) return;
    net_send_metric("metric %s diff=%s seed=%llu seeded=%d time=%d "
                    "clicks=%llu latency=%.0f t=%llu",
                    kind, g_diff_names[g->diff],
                    (unsigned long long)g_game_seed_active_val,
                    g_game_seed_active ? 1 : 0,
                    g->time, g_metric_clicks, g_metric_latency_ema_us,
                    (unsigned long long)ms_now_ms());
}

/* ------------------------------------------------------------------ */
/* leaderboard persistence + solver credentials                        */
/* ------------------------------------------------------------------ */
#define LB_INI_SEC "leaderboard"

int leader_name_valid(const char *name) {
    size_t i, n;
    if (!name) return 0;
    n = strlen(name);
    if (n < 1 || n > LEADER_NAME_MAX) return 0;
    for (i = 0; i < n; i++) {
        char c = name[i];
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
              (c >= '0' && c <= '9') || c == '-' || c == '_'))
            return 0;
    }
    return 1;
}

static char g_name[LEADER_NAME_MAX + 1];

const char *leader_player_name(void) {
    if (!g_name[0]) {
        ms_ini_get_str(LB_INI_SEC, "name", "Player", g_name, sizeof(g_name));
        if (!leader_name_valid(g_name)) strcpy(g_name, "Player");
    }
    return g_name;
}

int leader_set_player_name(const char *name) {
    if (!leader_name_valid(name)) return 0;
    strncpy(g_name, name, sizeof(g_name) - 1);
    g_name[sizeof(g_name) - 1] = 0;
    ms_ini_set_str(LB_INI_SEC, "name", g_name);
    return 1;
}

int leader_auto_submit(void) {
    return ms_ini_get_int(LB_INI_SEC, "auto_submit", 1) != 0;
}

void leader_set_auto_submit(int on) {
    ms_ini_set_int(LB_INI_SEC, "auto_submit", on ? 1 : 0);
}

void leader_submit_win(int diff, int time_ms) {
    const char *name;
    if (!leader_auto_submit()) return;
    name = leader_player_name();
    if (!name || !name[0]) return;
    if (diff < 0 || diff >= 3) return;
    if (time_ms <= 0 || time_ms > 3600000) return;
    net_send_score(name, diff, time_ms);
}

/* Extract the value of "key":"..." from a tiny hand-written JSON file. */
static void json_str(const char *buf, const char *key, char *out, size_t outsz,
                     int *found) {
    const char *k = buf;
    size_t klen = strlen(key);
    if (outsz == 0) return;
    out[0] = 0;
    while ((k = strchr(k, '"')) != NULL) {
        k++;
        if (strnicmp(k, key, klen) == 0 && k[klen] == '"') {
            k += klen + 1;
            while (*k == ' ' || *k == '\t') k++;
            if (*k != ':') continue;
            k++;
            while (*k == ' ' || *k == '\t') k++;
            if (*k != '"') continue;
            k++;
            {
                size_t o = 0;
                while (*k && *k != '"' && o + 1 < outsz) out[o++] = *k++;
                out[o] = 0;
            }
            *found = 1;
            return;
        }
    }
}

static void solver_config_read(const char *path, char *user, char *pass,
                               int *have_user, int *have_pass) {
    FILE *f;
    char *buf = NULL;
    long len;
    if (!path || !*path) return;
    f = fopen(path, "rb");
    if (!f) return;
    if (fseek(f, 0, SEEK_END) == 0 && (len = ftell(f)) >= 0 &&
        len <= 65536) {
        if (fseek(f, 0, SEEK_SET) == 0) {
            buf = (char *)malloc((size_t)len + 1);
            if (buf) {
                size_t rd = fread(buf, 1, (size_t)len, f);
                buf[rd] = 0;
                json_str(buf, "user", user, 64, have_user);
                json_str(buf, "pass", pass, 128, have_pass);
                free(buf);
            }
        }
    }
    fclose(f);
}

void leader_setup_solver(int argc, char **argv) {
    char user[64] = "";
    char pass[128] = "";
    int have_user = 0, have_pass = 0;
    int i;

    for (i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (stricmp(a, "--solver-user") == 0 && i + 1 < argc) {
            strncpy(user, argv[++i], sizeof(user) - 1);
            have_user = 1;
        } else if (stricmp(a, "--solver-pass") == 0 && i + 1 < argc) {
            strncpy(pass, argv[++i], sizeof(pass) - 1);
            have_pass = 1;
        } else if (stricmp(a, "--solver-config") == 0 && i + 1 < argc) {
            solver_config_read(argv[++i], user, pass, &have_user, &have_pass);
        } else if (strnicmp(a, "--solver-user=", 14) == 0) {
            strncpy(user, a + 14, sizeof(user) - 1);
            have_user = 1;
        } else if (strnicmp(a, "--solver-pass=", 14) == 0) {
            strncpy(pass, a + 14, sizeof(pass) - 1);
            have_pass = 1;
        } else if (strnicmp(a, "--solver-config=", 16) == 0) {
            solver_config_read(a + 16, user, pass, &have_user, &have_pass);
        }
    }

    /* fall back to the environment */
    if (!have_user || !have_pass) {
        const char *eu = getenv("MS_SOLVER_USER");
        const char *ep = getenv("MS_SOLVER_PASS");
        if (!have_user && eu && *eu) {
            strncpy(user, eu, sizeof(user) - 1);
            have_user = 1;
        }
        if (!have_pass && ep && *ep) {
            strncpy(pass, ep, sizeof(pass) - 1);
            have_pass = 1;
        }
    }

    net_set_solver_creds(have_user && have_pass ? user : NULL,
                         have_user && have_pass ? pass : NULL);
}

/* ------------------------------------------------------------------ */
/* event queue (telemetry thread + CLI thread -> main thread)          */
/* ------------------------------------------------------------------ */
#define EV_CAP 256
static MsEvent g_ev[EV_CAP];
static int g_ev_head = 0;
static int g_ev_count = 0;
static pthread_mutex_t g_ev_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t  g_ev_cond = PTHREAD_COND_INITIALIZER;

void ms_events_init(void) {
    /* mutex/cond are statically initialised; nothing else to do */
}

int ms_event_poll(MsEvent *e) {
    if (!e) return 0;
    pthread_mutex_lock(&g_ev_lock);
    if (g_ev_count == 0) {
        pthread_mutex_unlock(&g_ev_lock);
        return 0;
    }
    *e = g_ev[g_ev_head];
    g_ev_head = (g_ev_head + 1) % EV_CAP;
    g_ev_count--;
    pthread_mutex_unlock(&g_ev_lock);
    return 1;
}

void ms_event_push(const MsEvent *e) {
    if (!e) return;
    pthread_mutex_lock(&g_ev_lock);
    if (g_ev_count >= EV_CAP) {
        g_ev_head = (g_ev_head + 1) % EV_CAP;
        g_ev_count--;
    }
    g_ev[(g_ev_head + g_ev_count) % EV_CAP] = *e;
    g_ev_count++;
    pthread_cond_signal(&g_ev_cond);
    pthread_mutex_unlock(&g_ev_lock);
}

/* Synchronous CLI dispatch: block until the main loop has executed the
 * command and set cmd->ready.  Equivalent to SendMessage on Win32. */
void ms_event_push_cli(CliCmd *cmd) {
    if (!cmd) return;
    {
        MsEvent e;
        memset(&e, 0, sizeof(e));
        e.kind = EV_CLI;
        e.cli = cmd;
        ms_event_push(&e);
    }
    pthread_mutex_lock(&cmd->lock);
    while (!cmd->ready)
        pthread_cond_wait(&cmd->cv, &cmd->lock);
    pthread_mutex_unlock(&cmd->lock);
}

/* ------------------------------------------------------------------ */
/* CLI dispatch (runs on the main thread)                              */
/* ------------------------------------------------------------------ */
static void cli_append(char **buf, size_t *len, size_t *cap,
                       const char *fmt, ...) {
    va_list ap;
    int need;
    if (*buf == NULL) {
        *cap = 256;
        *buf = malloc(*cap);
        if (!*buf) { *cap = 0; return; }
        **buf = 0;
    }
    for (;;) {
        va_start(ap, fmt);
        need = vsnprintf(*buf + *len, *cap - *len, fmt, ap);
        va_end(ap);
        if (need >= 0 && (size_t)need < *cap - *len) { *len += (size_t)need; return; }
        *cap = *cap ? *cap * 2 : 256;
        *buf = realloc(*buf, *cap);
        if (!*buf) { *cap = 0; return; }
    }
}

static void cli_dispatch(CliCmd *cc) {
    Game *g = &g_game;
    char *buf = NULL;
    size_t cap = 0, len = 0;

    switch (cc->op) {
    case CLI_PING:
        cli_append(&buf, &len, &cap, "OK\n");
        break;

    case CLI_HELP:
        cli_append(&buf, &len, &cap,
            "commands: ping | help | new <beginner|intermediate|expert|custom [r c m]> | "
            "click <r> <c> | flag <r> <c> | chord <r> <c> | state | board | "
            "marks [0|1] | pause | resume | seed [<diff>] <n> | seedcustom [<diff>] <value> | "
            "seeds | telemetry [on|off] | reqseed <diff> <n> [count] | "
            "reqbatch <diff> <count> | scenarios | refresh [0|1] | quit\n");
        break;

    case CLI_NEW: {
        Difficulty d;
        if (cc->a == DIFF_CUSTOM) {
            if (cc->rows > 0) {
                d.rows = cc->rows < 8 ? 8 : (cc->rows > MAX_ROWS ? MAX_ROWS : cc->rows);
                d.cols = cc->cols < 8 ? 8 : (cc->cols > MAX_COLS ? MAX_COLS : cc->cols);
                d.mines = cc->mines;
                if (d.mines < 1) d.mines = 1;
                if (d.mines > d.rows * d.cols - 9) d.mines = d.rows * d.cols - 9;
                d.diff = DIFF_CUSTOM;
                g_custom = d;
            } else {
                d = g_custom;
            }
            game_reset(&d, g->marks_enabled);
        } else if (cc->a >= DIFF_BEGIN && cc->a <= DIFF_EXPERT) {
            game_reset(&g_presets[cc->a], g->marks_enabled);
        } else {
            cli_append(&buf, &len, &cap, "ERR unknown difficulty\n");
            break;
        }
        cli_append(&buf, &len, &cap, "OK\n");
        break;
    }

    case CLI_CLICK:
    case CLI_FLAG:
    case CLI_CHORD: {
        int r = cc->a, c = cc->b, i;
        if (!INB(g, r, c)) {
            cli_append(&buf, &len, &cap, "ERR out of bounds\n");
            break;
        }
        i = IDX(g, r, c);
        if (cc->op == CLI_CLICK) {
            if (!g->started) first_click(g, r, c);
            reveal_cell(g, r, c);
            if (g_cli_refresh) request_repaint();
        } else if (cc->op == CLI_FLAG) {
            cycle_mark(g, i);
        } else {
            do_chord(g, i);
            if (g_cli_refresh) request_repaint();
        }
        cli_append(&buf, &len, &cap, "OK\n");
        break;
    }

    case CLI_STATE:
        cli_append(&buf, &len, &cap,
            "difficulty=%s\nrows=%d\ncols=%d\nmines=%d\nflags=%d\nopened=%d\n"
            "time=%d\nstarted=%d\nover=%d\npaused=%d\nmarks=%d\n"
            "seeded=%d\nseed=%llu\n",
            diff_str(g), g->rows, g->cols, g->mines, g->flags, g->opened,
            g->time, g->started, g->over, g->paused, g->marks_enabled,
            g_game_seed_active ? 1 : 0,
            (unsigned long long)g_game_seed_active_val);
        break;

    case CLI_BOARD: {
        int r, c;
        for (r = 0; r < g->rows; r++) {
            for (c = 0; c < g->cols; c++) {
                int i = IDX(g, r, c);
                char ch;
                if (!g->revealed[i]) {
                    ch = (g->mark[i] == 1) ? 'F' : (g->mark[i] == 2) ? '?' : '.';
                } else if (g->mine[i]) {
                    ch = '*';
                } else {
                    ch = (char)('0' + g->adj[i]);
                }
                cli_append(&buf, &len, &cap, "%c", ch);
            }
            cli_append(&buf, &len, &cap, "\n");
        }
        break;
    }

    case CLI_MARKS:
        if (cc->a >= 0) g->marks_enabled = cc->a ? 1 : 0;
        cli_append(&buf, &len, &cap, "marks=%d\n", g->marks_enabled);
        break;

    case CLI_PAUSE:
        g->paused = 1;
        cli_append(&buf, &len, &cap, "OK\n");
        break;

    case CLI_RESUME:
        g->paused = 0;
        cli_append(&buf, &len, &cap, "OK\n");
        break;

    case CLI_SEED:
        if (cc->a == -3) {
            cli_append(&buf, &len, &cap, "ERR unknown difficulty\n");
        } else if (cc->a == -2) {
            seed_override_clear();
            cli_append(&buf, &len, &cap, "OK seed off\n");
        } else if (cc->a < 0) {
            seed_override_set(cc->u);
            cli_append(&buf, &len, &cap, "OK seed=%llu\n",
                       (unsigned long long)cc->u);
        } else if (cc->b == 0) {
            diff_seed_clear(cc->a);
            cli_append(&buf, &len, &cap, "OK seed off\n");
        } else {
            diff_seed_set_normal(cc->a, cc->u);
            cli_append(&buf, &len, &cap, "OK seed=%llu\n",
                       (unsigned long long)cc->u);
        }
        break;

    case CLI_SEEDCUSTOM: {
        uint64_t v;
        int steps = 0, truncated = 0;
        if (cc->a == -3) {
            cli_append(&buf, &len, &cap, "ERR unknown difficulty\n");
        } else if (cc->a == -2) {
            seed_override_clear();
            cli_append(&buf, &len, &cap, "OK seed off\n");
        } else if (cc->a < 0) {
            if (!custom_seed_generate(cc->s, &v, &steps, &truncated)) {
                cli_append(&buf, &len, &cap, "ERR bad seed input\n");
                break;
            }
            seed_override_set(v);
            cli_append(&buf, &len, &cap, "OK seed=%llu steps=%d truncated=%d\n",
                       (unsigned long long)v, steps, truncated);
        } else if (cc->b == 0) {
            diff_seed_clear(cc->a);
            cli_append(&buf, &len, &cap, "OK seed off\n");
        } else {
            if (!diff_custom_seed_generate(cc->a, cc->s, &v, &steps,
                                           &truncated)) {
                cli_append(&buf, &len, &cap, "ERR bad seed input\n");
                break;
            }
            diff_seed_set_custom(cc->a, cc->s);
            cli_append(&buf, &len, &cap, "OK seed=%llu steps=%d truncated=%d\n",
                       (unsigned long long)v, steps, truncated);
        }
        break;
    }

    case CLI_SEEDS: {
        int i;
        for (i = 0; i < DIFF_COUNT; i++) {
            const SeedSlot *sl = &g_diff_seeds[i];
            if (sl->mode == SEED_OFF)
                cli_append(&buf, &len, &cap, "%s=off\n", g_diff_names[i]);
            else if (sl->mode == SEED_NORMAL)
                cli_append(&buf, &len, &cap, "%s=normal:%s\n",
                           g_diff_names[i], sl->value);
            else {
                uint64_t v;
                if (diff_custom_seed_generate(i, sl->value, &v, NULL, NULL))
                    cli_append(&buf, &len, &cap, "%s=custom:%llu\n",
                               g_diff_names[i], (unsigned long long)v);
                else
                    cli_append(&buf, &len, &cap, "%s=custom:invalid\n",
                               g_diff_names[i]);
            }
        }
        if (g_seed_override)
            cli_append(&buf, &len, &cap, "pending=%llu\n",
                       (unsigned long long)g_seed_override_val);
        else
            cli_append(&buf, &len, &cap, "pending=off\n");
        break;
    }

    case CLI_REFRESH:
        if (cc->a >= 0) g_cli_refresh = cc->a ? 1 : 0;
        cli_append(&buf, &len, &cap, "refresh=%d\n", g_cli_refresh);
        if (g_cli_refresh) request_repaint();
        break;

    case CLI_TELEMETRY:
        if (cc->a == 0)
            net_telemetry_stop();
        else if (cc->a == 1)
            net_telemetry_start("135.125.79.15", 28571);
        if (cc->a == -2)
            cli_append(&buf, &len, &cap, "ERR arg\n");
        else {
            NetStats st;
            net_get_stats(&st);
            cli_append(&buf, &len, &cap,
                       "telemetry=%d connected=%d "
                       "attempts=%llu seeds=%llu outcomes=%llu wins=%llu "
                       "sent=%llu dropped=%llu\n",
                       net_telemetry_active(),
                       st.connected ? 1 : 0,
                       (unsigned long long)st.attempts,
                       (unsigned long long)st.seeds_recv,
                       (unsigned long long)st.outcomes_recv,
                       (unsigned long long)st.wins_recv,
                       (unsigned long long)st.metrics_sent,
                       (unsigned long long)st.metrics_dropped);
        }
        break;

    case CLI_REQSEED:
    case CLI_REQBATCH:
        if (cc->a < 0 || cc->a >= DIFF_COUNT) {
            cli_append(&buf, &len, &cap, "ERR unknown difficulty\n");
            break;
        }
        if (cc->op == CLI_REQSEED && cc->u == 0 && !cc->have_u) {
            cli_append(&buf, &len, &cap,
                       "ERR reqseed needs a seed: reqseed <diff> <n> [count]\n");
            break;
        }
        if (!net_telemetry_active()) {
            cli_append(&buf, &len, &cap, "ERR telemetry off\n");
            break;
        }
        if (cc->op == CLI_REQSEED) {
            int count = cc->c > 0 ? cc->c : 1;
            int ok = (count > 1)
                ? net_send_request("reqseed %s %llu %d", g_diff_names[cc->a],
                                   (unsigned long long)cc->u, count)
                : net_send_request("reqseed %s %llu", g_diff_names[cc->a],
                                   (unsigned long long)cc->u);
            cli_append(&buf, &len, &cap,
                       ok ? "OK reqseed %s %llu count=%d\n"
                          : "ERR reqseed not queued (solver auth pending or "
                            "credentials not configured)\n",
                       g_diff_names[cc->a], (unsigned long long)cc->u, count);
        } else {
            int count = cc->c > 0 ? cc->c : 1;
            int ok = net_send_request("reqbatch %s %d", g_diff_names[cc->a],
                                      count);
            cli_append(&buf, &len, &cap,
                       ok ? "OK reqbatch %s count=%d\n"
                          : "ERR reqbatch not queued (solver auth pending or "
                            "credentials not configured)\n",
                       g_diff_names[cc->a], count);
        }
        break;

    case CLI_SCENARIOS: {
        ScenarioReport rep;
        int i;
        if (!scenario_analyze(g->rows, g->cols, g->mines, g->revealed,
                              g->mine, g->mark, g->adj, &rep)) {
            cli_append(&buf, &len, &cap, "ERR %s\n", rep.reason);
            break;
        }
        cli_append(&buf, &len, &cap,
                   "hidden=%d free=%d nonfrontier_p=%.12g solved=%d\n",
                   rep.n_hidden, rep.n_free, rep.nonfrontier_p, rep.solved);
        for (i = 0; i < rep.n_scenarios; i++) {
            const Scenario *s = &rep.scenarios[i];
            cli_append(&buf, &len, &cap,
                       "cell %d r %d c %d p_mine %.12g p_safe %.12g "
                       "frontier %d reveals %d\n",
                       s->cell, s->r, s->c, s->p_mine, s->p_safe,
                       s->frontier ? 1 : 0, s->reveals);
        }
        scenario_report_free(&rep);
        break;
    }

    default:
        cli_append(&buf, &len, &cap, "ERR unknown command\n");
        break;
    }

    /* all responses end with the END marker */
    cli_append(&buf, &len, &cap, "END\n");
    cc->reply = buf;
}

/* ------------------------------------------------------------------ */
/* event pump (main thread)                                            */
/* ------------------------------------------------------------------ */
void ms_loop_pump(void) {
    MsEvent e;
    while (ms_event_poll(&e)) {
        switch (e.kind) {
        case EV_TELEMETRY_SEED:
            game_apply_telemetry_seed(e.diff, e.seed);
            break;
        case EV_LB_START:
            if (fe_hof_start) fe_hof_start();
            break;
        case EV_LB_ENTRY:
            if (fe_hof_entry)
                fe_hof_entry(e.rank, e.diffname, e.name, e.time_ms, e.ts);
            break;
        case EV_LB_END:
            if (fe_hof_end) fe_hof_end();
            break;
        case EV_SOLVER_DENIED:
            if (fe_denied) fe_denied();
            break;
        case EV_CLI:
            if (e.cli) {
                cli_dispatch(e.cli);
                pthread_mutex_lock(&e.cli->lock);
                e.cli->ready = 1;
                pthread_cond_signal(&e.cli->cv);
                pthread_mutex_unlock(&e.cli->lock);
            }
            break;
        default:
            break;
        }
    }
}
