/*
 * Minesweeper (Classic) - a faithful original port of the 1992 Windows 3.1
 * game that no longer runs on modern Windows (the original is a 16-bit
 * executable and 64-bit Windows 7/8.1/10/11 provide no NTVDM support for
 * it; the classic games were also removed from Windows 8 and later).
 *
 * This is an ORIGINAL, from-scratch implementation of the classic gameplay
 * and look.  It shares no code with any Microsoft product.  It is written
 * against the Win32 API only (no external dependencies) so the same source
 * builds as a 32-bit and a 64-bit binary that runs on Windows 7, 8.1, 10
 * and 11.
 *
 * MIT License
 */
#define WIN32_LEAN_AND_MEAN
#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0600        /* Vista+ APIs: EM_SETCUEBANNER */
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <windowsx.h>
#include <commctrl.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <stdarg.h>
#include <time.h>
#include <math.h>

#include "resource.h"
#include "network.h"
#include "analyze.h"
#include "diag.h"
#include "leader.h"

#define ID_TIMER 1
#define ID_TIMER_DIAG 2
#define WM_APP_CLI (WM_APP + 1)
#define WM_APP_TELEMETRY_SEED (WM_APP + 2)
#define WM_APP_DIAG_BANNER (WM_APP + 3)

/* CLI scripting server (defined below wnd_proc) */
typedef struct CliCmd CliCmd;
static void cli_dispatch(CliCmd *cc);

#define MAX_ROWS 30
#define MAX_COLS 30
#define MAX_CELLS (MAX_ROWS * MAX_COLS)

/* max characters in the GUI custom-seed input box */
#define CUSTOM_SEED_INPUT_MAX 64

/* when the user picks "Multi Sim" for a remote simulation, the server is
 * asked to replay the chosen seed this many times (same board, varied
 * solver tie-breaks) to expose the seed's range of outcomes */
#define REMOTE_SIM_MULTI_N 25

/* when the user picks "Sim Until Loss", replay the seed until the simulated
 * player loses (capped at the server's per-request budget) */
#define REMOTE_SIM_UNTIL_N 10000

/* ---------- base geometry (at 96 DPI) ---------- */
#define CELL_BASE    16
#define LED_DIGIT_W  13
#define LED_DIGIT_H  23
#define LED_DIGITS    3
#define FACE_BASE    26
#define MARGIN_BASE  12
#define FRAME_BASE    2

/* ---------- difficulty presets (classic) ---------- */
enum { DIFF_BEGIN, DIFF_INTERMEDIATE, DIFF_EXPERT, DIFF_CUSTOM, DIFF_COUNT };

typedef struct {
    int rows, cols, mines;
    int diff;
} Difficulty;

static const Difficulty g_presets[3] = {
    {  8,  8, 10, DIFF_BEGIN },        /* Beginner */
    { 16, 16, 40, DIFF_INTERMEDIATE }, /* Intermediate */
    { 16, 30, 99, DIFF_EXPERT },       /* Expert */
};

/* ---------- game state ---------- */
typedef struct {
    int rows, cols, mines;
    unsigned char *mine;     /* 1 = mine */
    unsigned char *adj;      /* adjacent mine count 0..8 */
    unsigned char *revealed;
    unsigned char *mark;     /* 0 = none, 1 = flag, 2 = question */
    int flags;               /* flags currently placed */
    int opened;              /* revealed non-mine cells */
    int started;             /* first click done (mines placed, timer armed) */
    int over;                /* -1 lost, 0 playing, 1 won */
    int time;                /* elapsed seconds */
    int marks_enabled;       /* allow question marks */
    int paused;              /* timer paused by CLI */
    int diff;
    uint64_t rng;
} Game;

static Game g_game;
static HWND g_hwnd;
static int g_dpi = 96;

static const char *diff_str(const Game *g);   /* defined with the CLI section */

/* repaint on CLI-triggered mutations? (refresh command; on by default) */
static volatile LONG g_cli_refresh = 1;

/* ---------- telemetry metrics (collected on the UI thread) ---------- */
static unsigned long long g_metric_clicks = 0;   /* grid clicks this game */
static double g_metric_latency_ema_us = 0.0;     /* EWMA of input latency  */
static int    g_metric_latency_n = 0;

static void metrics_reset(void);
static void metrics_note_click(void);
static void metrics_note_ui_latency(long us);
static void metric_emit_start(int diff);
static void metric_emit_over(const char *kind);

/* telemetry endpoint.  Forced on by default (connects to the deployed
 * simulation server); --telemetry <host>:<port> overrides the endpoint and
 * --no-telemetry disables it for this session.  The default endpoint is
 * obfuscated (base64) in network.c, not readable as a plain string here.
 * --telemetry-http / --telemetry-https switch the session from the raw TCP
 * stream to the /ms-sim/ HTTP(S) endpoints (WinHTTP); --telemetry-https
 * with --telemetry-https-insecure skips certificate validation (debug). */
static char     g_telemetry_host[128];
static unsigned g_telemetry_port;
static int      g_telemetry_http;          /* 0=raw TCP, 1=HTTP, 2=HTTPS */
static int      g_telemetry_https_insecure;

/* Marshals a streamed `seed <diff> <n>` line from the network thread to the
 * UI thread (the heap pointer travels as the WM_APP_TELEMETRY_SEED LPARAM). */
typedef struct {
    int diff;
    unsigned long long seed;
} TelemetrySeedMsg;

/* one-shot seed override (legacy): set by the CLI or --seed / --seed-custom,
   consumed by the next reset_game() regardless of difficulty. */
static int      g_seed_override = 0;
static uint64_t g_seed_override_val = 0;

/* per-difficulty seed slots (persist for the session; used every time a game
 * of that difficulty starts, until cleared). */
enum { SEED_OFF = 0, SEED_NORMAL = 1, SEED_CUSTOM = 2 };

typedef struct {
    int  mode;                          /* SEED_OFF / SEED_NORMAL / SEED_CUSTOM */
    char value[CUSTOM_SEED_INPUT_MAX + 1];
} SeedSlot;

static SeedSlot g_diff_seeds[DIFF_COUNT];

/* last started board's resolved seed (any difficulty), shown in the title */
static int      g_game_seed_active = 0;
static uint64_t g_game_seed_active_val = 0;

static void update_window_title(HWND hwnd);
static int custom_seed_all_digits(const char *s);
static int custom_seed_generate(const char *input, uint64_t *out,
                                int *steps_out, int *truncated_out);
static int diff_custom_seed_generate(int diff, const char *input, uint64_t *out,
                                     int *steps_out, int *truncated_out);
static void resolve_board_seed(int diff, uint64_t *out, int *seeded);

static int client_w(const Game *g);
static int client_h(const Game *g);

/* mouse / chord tracking */
static int g_pressed_cell = -1; /* cell currently "pressed" (chord anchor) */
static int g_chord_cell   = -1; /* revealed number being chorded */
static int g_face_pressed = 0;
static int g_left_down    = 0;
static int g_right_down   = 0;

static HFONT g_font_num = NULL;
static HFONT g_font_q   = NULL;

/* ---------- helpers ---------- */
static inline int S(int v) { return MulDiv(v, g_dpi, 96); }

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

/* A resolved seed of zero would leave xorshift64 stuck at zero forever (every
   output step is x ^= x<<13 ^ x>>7 ^ x<<17 with x == 0), so seed 0 is mapped
   onto a fixed nonzero constant.  This must stay identical in
   server/sim_engine.py and ms/core/sim-engine.js (ZERO_SEED_FALLBACK). */
#define RNG_ZERO_SEED_FALLBACK 0x9E3779B97F4A7C15ULL

static uint64_t rng_seed_or_fallback(uint64_t seed) {
    return seed != 0 ? seed : RNG_ZERO_SEED_FALLBACK;
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
    g->mine = calloc(n, 1);
    g->adj = calloc(n, 1);
    g->revealed = calloc(n, 1);
    g->mark = calloc(n, 1);
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
    int i, placed = 0, n = 0;
    int *pool = malloc(sizeof(int) * MAX_CELLS);
    int r, c;

    for (r = 0; r < g->rows; r++)
        for (c = 0; c < g->cols; c++) {
            if (abs(r - sr) <= 1 && abs(c - sc) <= 1) continue;
            pool[n++] = IDX(g, r, c);
        }
    if (n < g->mines) { /* tiny board: only keep the clicked cell safe */
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

/* Window caption: show the active board seed so the player can see it */
static void update_window_title(HWND hwnd) {
    char title[96];
    if (g_game_seed_active) {
        _snprintf(title, sizeof(title), "Minesweeper  [Seed: %llu]",
                  (unsigned long long)g_game_seed_active_val);
    } else {
        strcpy(title, "Minesweeper");
    }
    SetWindowTextA(hwnd, title);
}

static void reset_game(HWND hwnd, const Difficulty *d, int marks) {
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
            g->rng = (uint64_t)GetTickCount64() ^
                     ((uint64_t)(size_t)hwnd << 32) ^ (uint64_t)time(NULL);
            g_game_seed_active = 0;
        }
    }
    metrics_reset();
    metric_emit_start(d->diff);
    KillTimer(hwnd, ID_TIMER);
    if (!alloc_board(g)) return;

    g_pressed_cell = g_chord_cell = -1;
    g_face_pressed = g_left_down = g_right_down = 0;

    /* resize to fit the new board (skip until the window exists) */
    if (hwnd) {
        RECT rc = { 0, 0, client_w(g), client_h(g) };
        AdjustWindowRectEx(&rc, GetWindowLongPtrW(hwnd, GWL_STYLE),
                           GetMenu(hwnd) != NULL, 0);
        SetWindowPos(hwnd, NULL, 0, 0,
                     rc.right - rc.left, rc.bottom - rc.top,
                     SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        update_window_title(hwnd);
        InvalidateRect(hwnd, NULL, TRUE);
    }
}

/* ---------- flood-fill reveal ---------- */
static void end_game_lose(Game *g);
static void end_game_win(Game *g);

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
    KillTimer(g_hwnd, ID_TIMER);
    g_pressed_cell = g_chord_cell = -1;
    metric_emit_over("loss");
    if (g_cli_refresh) InvalidateRect(g_hwnd, NULL, TRUE);
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
    KillTimer(g_hwnd, ID_TIMER);
    g_pressed_cell = g_chord_cell = -1;
    metric_emit_over("win");
    leader_submit_win(g->diff, g->time * 1000);
    if (g_cli_refresh) InvalidateRect(g_hwnd, NULL, TRUE);
}

static int first_click(Game *g, int r, int c) {
    if (g->started) return 0;
    g->started = 1;
    place_mines(g, r, c);
    SetTimer(g_hwnd, ID_TIMER, 1000, NULL);
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
    if (g_cli_refresh) InvalidateRect(g_hwnd, NULL, TRUE);
}

/* ---------- layout ---------- */
static int client_w(const Game *g) {
    return S(MARGIN_BASE) * 2 + g->cols * S(CELL_BASE) + S(FRAME_BASE) * 2;
}
static int client_h(const Game *g) {
    return S(MARGIN_BASE) + S(LED_DIGIT_H) + S(MARGIN_BASE)
         + g->rows * S(CELL_BASE) + S(FRAME_BASE) * 2 + S(MARGIN_BASE);
}
static int grid_x(void) { return S(MARGIN_BASE) + S(FRAME_BASE); }
static int grid_y(void) { return S(MARGIN_BASE) + S(LED_DIGIT_H) + S(MARGIN_BASE) + S(FRAME_BASE); }

static void led_rects(const Game *g, RECT *counter, RECT *timer, RECT *face) {
    int margin = S(MARGIN_BASE) + S(FRAME_BASE);
    int led_w = S(LED_DIGIT_W) * LED_DIGITS;
    int led_h = S(LED_DIGIT_H);
    int top = S(MARGIN_BASE);
    int right = client_w(g) - margin;

    counter->left = margin;
    counter->top = top;
    counter->right = counter->left + led_w;
    counter->bottom = top + led_h;

    timer->right = right;
    timer->left = right - led_w;
    timer->top = top;
    timer->bottom = top + led_h;

    face->top = top + (led_h - S(FACE_BASE)) / 2;
    face->bottom = face->top + S(FACE_BASE);
    face->left = counter->right + (timer->left - counter->right - S(FACE_BASE)) / 2;
    face->right = face->left + S(FACE_BASE);
}

static int cell_at(const Game *g, int x, int y) {
    int c = (x - grid_x()) / S(CELL_BASE);
    int r = (y - grid_y()) / S(CELL_BASE);
    if (INB(g, r, c)) return IDX(g, r, c);
    return -1;
}

/* ---------- drawing ---------- */
static const unsigned char SEG[10] = {
    0b1110111, /* 0: a b c d e f        */
    0b0010001, /* 1: b c                */
    0b1011110, /* 2: a b g e d          */
    0b1011011, /* 3: a b g c d          */
    0b0111001, /* 4: f g b c            */
    0b1101011, /* 5: a f g c d          */
    0b1101111, /* 6: a f g e c d        */
    0b1010001, /* 7: a b c              */
    0b1111111, /* 8: a b c d e f g      */
    0b1111011, /* 9: a b c d f g        */
};

static void led_seg(HDC dc, int x, int y, int w, int h, int on, COLORREF col) {
    RECT rc;
    HBRUSH br = CreateSolidBrush(on ? col : RGB(48, 8, 8));
    rc.left = x; rc.top = y; rc.right = x + w; rc.bottom = y + h;
    FillRect(dc, &rc, br);
    DeleteObject(br);
}

static void draw_led_digit(HDC dc, int x, int y, int digit, COLORREF on) {
    int s = S(1);
    int seg = (digit >= 0 && digit <= 9) ? SEG[digit] : 0;
    int W = S(LED_DIGIT_W), H = S(LED_DIGIT_H);

    /* horizontal segments */
    led_seg(dc, x + 3 * s, y + 1 * s, 7 * s, 3 * s, seg & (1 << 6), on); /* a */
    led_seg(dc, x + 3 * s, y + 10 * s, 7 * s, 3 * s, seg & (1 << 3), on); /* g */
    led_seg(dc, x + 3 * s, y + 19 * s, 7 * s, 3 * s, seg & (1 << 1), on); /* d */
    /* vertical segments */
    led_seg(dc, x + 1 * s, y + 3 * s, 3 * s, 7 * s, seg & (1 << 5), on); /* f */
    led_seg(dc, x + 9 * s, y + 3 * s, 3 * s, 7 * s, seg & (1 << 4), on); /* b */
    led_seg(dc, x + 1 * s, y + 13 * s, 3 * s, 7 * s, seg & (1 << 2), on); /* e */
    led_seg(dc, x + 9 * s, y + 13 * s, 3 * s, 7 * s, seg & (1 << 0), on); /* c */
    (void)W; (void)H;
}

static void draw_led(HDC dc, int x, int y, int value) {
    int w = S(LED_DIGIT_W), h = S(LED_DIGIT_H);
    RECT rc;
    HBRUSH br;
    COLORREF on = RGB(255, 16, 16);
    int i, v = value;

    if (v < 0) v = 0;
    if (v > 999) v = 999;

    rc.left = x - S(2); rc.top = y - S(2);
    rc.right = x + w * LED_DIGITS + S(2); rc.bottom = y + h + S(2);
    br = CreateSolidBrush(RGB(0, 0, 0));
    FillRect(dc, &rc, br);
    DeleteObject(br);

    for (i = LED_DIGITS - 1; i >= 0; i--) {
        int d = v % 10;
        v /= 10;
        draw_led_digit(dc, x + i * w, y, d, on);
    }
}

static void draw_bevel(HDC dc, int x, int y, int w, int h, int sunken) {
    RECT rc = { x, y, x + w, y + h };
    DrawEdge(dc, &rc, sunken ? EDGE_SUNKEN : EDGE_RAISED, BF_RECT);
}

static void draw_mine(HDC dc, int x, int y, int cs) {
    int cx = x + cs / 2, cy = y + cs / 2;
    int R = cs * 5 / 32;
    int spike = cs * 6 / 32;
    HBRUSH br, old;
    HPEN pen, oldpen;
    int a;
    static const int ang[8] = { 0, 45, 90, 135, 180, 225, 270, 315 };

    /* 8 spikes */
    pen = CreatePen(PS_SOLID, 1, RGB(0, 0, 0));
    oldpen = SelectObject(dc, pen);
    for (a = 0; a < 8; a++) {
        double rad = ang[a] * 3.14159265358979 / 180.0;
        int x1 = cx + (int)(R * 0.5 * (cos(rad) > 0 ? 1.2 : 1.0) * cos(rad));
        int y1 = cy + (int)(R * 0.5 * 1.0 * sin(rad));
        int x2 = cx + (int)(spike * cos(rad));
        int y2 = cy + (int)(spike * sin(rad));
        (void)x1; (void)y1;
        MoveToEx(dc, cx, cy, NULL);
        LineTo(dc, x2, y2);
    }
    SelectObject(dc, oldpen);
    DeleteObject(pen);

    /* body */
    br = CreateSolidBrush(RGB(0, 0, 0));
    old = SelectObject(dc, br);
    Ellipse(dc, cx - R, cy - R, cx + R, cy + R);
    /* highlight */
    br = CreateSolidBrush(RGB(255, 255, 255));
    SelectObject(dc, br);
    Ellipse(dc, cx - R + 1, cy - R + 1, cx - R / 2, cy - R / 2);
    SelectObject(dc, old);
    DeleteObject(br);
}

static void draw_flag(HDC dc, int x, int y, int cs) {
    int pole_x = x + cs * 7 / 16;
    int base_y = y + cs * 3 / 4;
    HPEN pen = CreatePen(PS_SOLID, max(1, S(1)), RGB(0, 0, 0));
    HPEN oldpen = SelectObject(dc, pen);
    HBRUSH br, old;

    /* pole */
    MoveToEx(dc, pole_x, y + cs / 4, NULL);
    LineTo(dc, pole_x, base_y);
    /* base */
    br = CreateSolidBrush(RGB(0, 0, 0));
    old = SelectObject(dc, br);
    Rectangle(dc, pole_x - S(2), base_y, pole_x + S(3), base_y + S(2));

    /* flag triangle */
    br = CreateSolidBrush(RGB(255, 0, 0));
    SelectObject(dc, br);
    {
        POINT pt[3];
        pt[0].x = pole_x;                 pt[0].y = y + cs / 4;
        pt[1].x = pole_x;                 pt[1].y = y + cs / 2;
        pt[2].x = x + cs * 3 / 4;         pt[2].y = y + cs * 3 / 8;
        Polygon(dc, pt, 3);
    }
    SelectObject(dc, old);
    DeleteObject(br);
    SelectObject(dc, oldpen);
    DeleteObject(pen);
}

static void draw_number(HDC dc, int x, int y, int cs, int n) {
    static const COLORREF col[9] = {
        RGB(0, 0, 0),
        RGB(0, 0, 255),      /* 1 blue */
        RGB(0, 128, 0),      /* 2 green */
        RGB(255, 0, 0),      /* 3 red */
        RGB(0, 0, 128),      /* 4 navy */
        RGB(128, 0, 0),      /* 5 maroon */
        RGB(0, 128, 128),    /* 6 teal */
        RGB(0, 0, 0),        /* 7 black */
        RGB(128, 128, 128),  /* 8 gray */
    };
    RECT rc = { x, y, x + cs, y + cs };
    char buf[4];
    if (n < 1 || n > 8) return;
    _itoa_s(n, buf, sizeof(buf), 10);
    SetTextColor(dc, col[n]);
    SetBkMode(dc, TRANSPARENT);
    SelectObject(dc, g_font_num);
    DrawTextA(dc, buf, -1, &rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP);
}

static void draw_question(HDC dc, int x, int y, int cs) {
    RECT rc = { x, y, x + cs, y + cs };
    SetTextColor(dc, RGB(0, 0, 255));
    SetBkMode(dc, TRANSPARENT);
    SelectObject(dc, g_font_q);
    DrawTextA(dc, "?", -1, &rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP);
}

static void draw_face(HDC dc, int x, int y, int sz, int state, int pressed) {
    int cx = x + sz / 2, cy = y + sz / 2;
    int r = sz / 2 - S(1);
    int ex1 = cx - sz / 4, ex2 = cx + sz / 4, ey = cy - sz / 6;
    HBRUSH br, old;
    HPEN pen, oldpen;

    draw_bevel(dc, x, y, sz, sz, pressed);

    /* face disc */
    br = CreateSolidBrush(RGB(255, 255, 0));
    old = SelectObject(dc, br);
    pen = CreatePen(PS_SOLID, 1, RGB(0, 0, 0));
    oldpen = SelectObject(dc, pen);
    Ellipse(dc, x + S(1), y + S(1), x + sz - S(1), y + sz - S(1));
    SelectObject(dc, old);
    DeleteObject(br);

    if (state == 3) { /* dead: X eyes, open mouth */
        MoveToEx(dc, ex1 - S(2), ey - S(2), NULL);
        LineTo(dc, ex1 + S(2), ey + S(2));
        MoveToEx(dc, ex1 + S(2), ey - S(2), NULL);
        LineTo(dc, ex1 - S(2), ey + S(2));
        MoveToEx(dc, ex2 - S(2), ey - S(2), NULL);
        LineTo(dc, ex2 + S(2), ey + S(2));
        MoveToEx(dc, ex2 + S(2), ey - S(2), NULL);
        LineTo(dc, ex2 - S(2), ey + S(2));
        br = CreateSolidBrush(RGB(0, 0, 0));
        SelectObject(dc, br);
        Ellipse(dc, cx - sz / 8, cy + sz / 8, cx + sz / 8, cy + sz / 3);
        SelectObject(dc, old);
        DeleteObject(br);
    } else if (state == 2) { /* won: sunglasses + smile */
        br = CreateSolidBrush(RGB(0, 0, 0));
        SelectObject(dc, br);
        Rectangle(dc, ex1 - sz / 6, ey - S(2), ex2 + sz / 6, ey + S(2));
        Rectangle(dc, ex1 - sz / 8, ey - S(2), ex1 + sz / 8, ey - S(2) - S(2));
        SelectObject(dc, old);
        DeleteObject(br);
        Arc(dc, cx - sz / 4, cy, cx + sz / 4, cy + sz / 2, cx + sz / 4, cy + sz / 8, cx - sz / 4, cy + sz / 8);
    } else if (state == 1) { /* surprised */
        br = CreateSolidBrush(RGB(0, 0, 0));
        SelectObject(dc, br);
        Ellipse(dc, ex1 - S(1), ey - S(1), ex1 + S(1), ey + S(1));
        Ellipse(dc, ex2 - S(1), ey - S(1), ex2 + S(1), ey + S(1));
        Ellipse(dc, cx - S(2), cy + sz / 8 - S(1), cx + S(2), cy + sz / 8 + S(3));
        SelectObject(dc, old);
        DeleteObject(br);
    } else { /* normal: dots + smile */
        br = CreateSolidBrush(RGB(0, 0, 0));
        SelectObject(dc, br);
        Ellipse(dc, ex1 - S(1), ey - S(1), ex1 + S(1), ey + S(1));
        Ellipse(dc, ex2 - S(1), ey - S(1), ex2 + S(1), ey + S(1));
        SelectObject(dc, old);
        DeleteObject(br);
        Arc(dc, cx - sz / 4, cy, cx + sz / 4, cy + sz / 2, cx + sz / 4, cy + sz / 8, cx - sz / 4, cy + sz / 8);
    }
    SelectObject(dc, oldpen);
    DeleteObject(pen);
}

static int face_state(void) {
    Game *g = &g_game;
    if (g->over == 1) return 2;
    if (g->over == -1) return 3;
    if (g_face_pressed || g_pressed_cell != -1 || g_chord_cell != -1 ||
        (g_left_down && g_right_down))
        return 1;
    return 0;
}

static void paint_cell(HDC dc, int r, int c, int px, int py) {
    Game *g = &g_game;
    int i = IDX(g, r, c);
    int cs = S(CELL_BASE);
    int sunken = 0;

    if (g->revealed[i]) {
        sunken = 1;
    } else {
        /* pressed preview when chorded */
        if (g_chord_cell != -1 && !g->mark[i]) {
            int cr = g_chord_cell / g->cols, cc = g_chord_cell % g->cols;
            if (abs(cr - r) <= 1 && abs(cc - c) <= 1) sunken = 1;
        }
        if (g_pressed_cell == i) sunken = 1;
    }

    draw_bevel(dc, px, py, cs, cs, sunken);

    if (g->revealed[i]) {
        if (g->mine[i]) {
            draw_mine(dc, px, py, cs);
            if (g->mark[i] == 1) { /* wrong flag: red X over the mine */
                HPEN pen = CreatePen(PS_SOLID, max(1, S(2)), RGB(255, 0, 0));
                HPEN oldpen = SelectObject(dc, pen);
                MoveToEx(dc, px + S(2), py + S(2), NULL);
                LineTo(dc, px + cs - S(2), py + cs - S(2));
                MoveToEx(dc, px + cs - S(2), py + S(2), NULL);
                LineTo(dc, px + S(2), py + cs - S(2));
                SelectObject(dc, oldpen);
                DeleteObject(pen);
            }
        } else if (g->adj[i] > 0) {
            draw_number(dc, px, py, cs, g->adj[i]);
        }
    } else {
        if (g->mark[i] == 1) draw_flag(dc, px, py, cs);
        else if (g->mark[i] == 2) draw_question(dc, px, py, cs);
    }
}

static void paint(HDC dc) {
    Game *g = &g_game;
    RECT rc;
    RECT counter, timer, face;
    int r, c;

    GetClientRect(g_hwnd, &rc);
    FillRect(dc, &rc, (HBRUSH)(COLOR_BTNFACE + 1));

    /* grid frame */
    {
        int gx = grid_x(), gy = grid_y();
        RECT fr = { gx - S(FRAME_BASE), gy - S(FRAME_BASE),
                    gx + g->cols * S(CELL_BASE) + S(FRAME_BASE),
                    gy + g->rows * S(CELL_BASE) + S(FRAME_BASE) };
        DrawEdge(dc, &fr, EDGE_RAISED, BF_RECT);
    }

    /* LEDs and face */
    led_rects(g, &counter, &timer, &face);
    draw_led(dc, counter.left, counter.top, g->mines - g->flags);
    draw_led(dc, timer.left, timer.top, g->time);
    draw_face(dc, face.left, face.top, face.right - face.left,
              face_state(), g_face_pressed);

    /* cells */
    for (r = 0; r < g->rows; r++)
        for (c = 0; c < g->cols; c++)
            paint_cell(dc, r, c,
                       grid_x() + c * S(CELL_BASE),
                       grid_y() + r * S(CELL_BASE));
}

/* ---------- input ---------- */
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

static void on_lbutton_down(Game *g, int x, int y) {
    RECT counter, timer, face;
    int cell;

    g_left_down = 1;
    led_rects(g, &counter, &timer, &face);

    if (x >= face.left && x < face.right && y >= face.top && y < face.bottom) {
        g_face_pressed = 1;
        g_pressed_cell = g_chord_cell = -1;
        InvalidateRect(g_hwnd, NULL, TRUE);
        return;
    }
    cell = cell_at(g, x, y);
    if (cell < 0) { g_pressed_cell = g_chord_cell = -1; InvalidateRect(g_hwnd, NULL, TRUE); return; }
    metrics_note_click();

    if (g_right_down) {
        arm_chord(g, cell);
        InvalidateRect(g_hwnd, NULL, TRUE);
        return;
    }
    if (!g->revealed[cell] && g->mark[cell] != 1) {
        int r = cell / g->cols, c = cell % g->cols;
        if (!g->over) {
            first_click(g, r, c);
            reveal_cell(g, r, c);
        }
        g_pressed_cell = -1;
    } else if (g->revealed[cell] && g->adj[cell] > 0) {
        g_pressed_cell = cell;
    } else {
        g_pressed_cell = -1;
    }
    InvalidateRect(g_hwnd, NULL, TRUE);
}

static void on_rbutton_down(Game *g, int x, int y) {
    RECT counter, timer, face;
    int cell;

    g_right_down = 1;
    led_rects(g, &counter, &timer, &face);

    if (x >= face.left && x < face.right && y >= face.top && y < face.bottom)
        return;
    cell = cell_at(g, x, y);
    if (cell < 0) return;
    metrics_note_click();

    if (g_left_down) {
        arm_chord(g, cell);
        InvalidateRect(g_hwnd, NULL, TRUE);
        return;
    }
    if (!g->revealed[cell])
        cycle_mark(g, cell);
    else if (g->adj[cell] > 0)
        g_pressed_cell = cell;
}

static void on_button_up(Game *g, int x, int y, int left) {
    RECT counter, timer, face;
    int was_face = 0;

    led_rects(g, &counter, &timer, &face);
    if (g_face_pressed) {
        was_face = (x >= face.left && x < face.right &&
                    y >= face.top && y < face.bottom);
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
                first_click(g, r, c);
                reveal_cell(g, r, c);
            }
        }
    }

    if (left) g_left_down = 0; else g_right_down = 0;

    if (was_face) {
        g_face_pressed = 0;
        if (!g->over) reset_game(g_hwnd, &g_presets[g->diff], g->marks_enabled);
    }
    g_face_pressed = 0;
    g_pressed_cell = g_chord_cell = -1;
    InvalidateRect(g_hwnd, NULL, TRUE);
}

/* ---------- custom dialog ---------- */
static Difficulty g_custom = { 16, 16, 40, DIFF_CUSTOM };

/* Keep only visible alphanumeric characters: strips invisible chars,
 * whitespace (incl. leading/trailing spaces) and unsupported symbols.
 * Returns the cleaned length. */
static size_t sanitize_seed_input(const char *in, char *out, size_t outsz) {
    size_t n = 0;
    if (!outsz) return 0;
    for (; in && *in; in++) {
        unsigned char ch = (unsigned char)*in;
        if ((ch >= '0' && ch <= '9') || (ch >= 'A' && ch <= 'Z') ||
            (ch >= 'a' && ch <= 'z')) {
            if (n + 1 >= outsz) break;
            out[n++] = (char)ch;
        }
    }
    out[n] = 0;
    return n;
}

static INT_PTR CALLBACK custom_proc(HWND hDlg, UINT msg, WPARAM wp, LPARAM lp) {
    (void)lp;
    switch (msg) {
    case WM_INITDIALOG: {
        char buf[8];
        _itoa_s(g_custom.rows, buf, sizeof(buf), 10);
        SetDlgItemTextA(hDlg, IDC_ROWS, buf);
        _itoa_s(g_custom.cols, buf, sizeof(buf), 10);
        SetDlgItemTextA(hDlg, IDC_COLS, buf);
        _itoa_s(g_custom.mines, buf, sizeof(buf), 10);
        SetDlgItemTextA(hDlg, IDC_MINES, buf);
        return TRUE;
    }
    case WM_COMMAND:
        switch (LOWORD(wp)) {
        case IDOK: {
            int rows = GetDlgItemInt(hDlg, IDC_ROWS, NULL, FALSE);
            int cols = GetDlgItemInt(hDlg, IDC_COLS, NULL, FALSE);
            int mines = GetDlgItemInt(hDlg, IDC_MINES, NULL, FALSE);
            if (rows < 8) rows = 8;
            if (rows > MAX_ROWS) rows = MAX_ROWS;
            if (cols < 8) cols = 8;
            if (cols > MAX_COLS) cols = MAX_COLS;
            if (mines < 1) mines = 1;
            if (mines > rows * cols - 9) mines = rows * cols - 9;
            g_custom.rows = rows;
            g_custom.cols = cols;
            g_custom.mines = mines;
            reset_game(g_hwnd, &g_custom, g_game.marks_enabled);
            EndDialog(hDlg, IDOK);
            return TRUE;
        }
        case IDCANCEL:
            EndDialog(hDlg, IDCANCEL);
            return TRUE;
        }
        break;
    }
    return FALSE;
}

/* ---------- Seeds dialog ----------
 * Per-difficulty seed slots. The dialog edits a copy of g_diff_seeds and
 * commits it on OK. Each row: Off / Normal / Custom radios, a value edit
 * and a live label showing the resolved board seed. */
static SeedSlot g_seeds_edit[DIFF_COUNT];
static int g_seeds_editing = 0;

static int seeds_row_base(int row) {
    return IDC_SEED_OFF_BEGIN + row * IDC_SEED_ROW_STRIDE;
}

static void seeds_apply_row(HWND hDlg, int row) {
    int off = seeds_row_base(row);
    int mode = g_seeds_edit[row].mode;
    CheckRadioButton(hDlg, off, off + 2, off + mode);
    EnableWindow(GetDlgItem(hDlg, off + 3), mode != SEED_OFF);
}

static void seeds_update_result(HWND hDlg, int row) {
    int off = seeds_row_base(row);
    const SeedSlot *sl = &g_seeds_edit[row];
    char out[96];
    if (sl->mode == SEED_OFF || !sl->value[0]) {
        SetDlgItemTextA(hDlg, off + 4, "");
        return;
    }
    if (sl->mode == SEED_NORMAL) {
        /* Normal uses the number as-is (same as resolve_board_seed). */
        if (!custom_seed_all_digits(sl->value)) {
            SetDlgItemTextA(hDlg, off + 4, "Seed: (enter a number)");
            return;
        }
        _snprintf(out, sizeof(out), "Seed: %llu",
                  (unsigned long long)strtoull(sl->value, NULL, 10));
        SetDlgItemTextA(hDlg, off + 4, out);
        return;
    }
    {
        uint64_t v;
        int truncated;
        if (diff_custom_seed_generate(row, sl->value, &v, NULL, &truncated)) {
            if (truncated)
                _snprintf(out, sizeof(out), "Seed: %llu (truncated)",
                          (unsigned long long)v);
            else
                _snprintf(out, sizeof(out), "Seed: %llu", (unsigned long long)v);
            SetDlgItemTextA(hDlg, off + 4, out);
        } else {
            SetDlgItemTextA(hDlg, off + 4, "");
        }
    }
}

static void seeds_update_all(HWND hDlg) {
    int i;
    for (i = 0; i < DIFF_COUNT; i++) {
        seeds_apply_row(hDlg, i);
        seeds_update_result(hDlg, i);
    }
}

/* ---------- remote simulation prompt ---------- */

static const char *const g_diff_names[DIFF_COUNT];   /* defined below */

static INT_PTR CALLBACK remsim_proc(HWND hDlg, UINT msg, WPARAM wp, LPARAM lp) {
    switch (msg) {
    case WM_INITDIALOG:
        SetDlgItemTextA(hDlg, IDC_REMSIM_MSG, (LPCSTR)lp);
        return TRUE;
    case WM_COMMAND:
        switch (LOWORD(wp)) {
        case IDYES:
        case IDC_REMSIM_MULTI:
        case IDC_REMSIM_UNTIL:
        case IDCANCEL:
            EndDialog(hDlg, LOWORD(wp));
            return TRUE;
        }
        break;
    }
    return FALSE;
}

/* Ask whether a committed custom seed should also be simulated on the remote
 * server (Yes = once, Multi Sim = replay REMOTE_SIM_MULTI_N times, No =
 * skip).  Runs on the UI thread; the actual request is queued by the network
 * thread, so nothing here blocks. */
static void remsim_prompt_for_seed(HWND owner, int diff, uint64_t seed) {
    char msg[256];
    int rc;
    if (!net_telemetry_active()) return;
    _snprintf(msg, sizeof(msg),
              "Seed %s: %llu\n"
              "Send this seed to the simulation server for a win/loss check?",
              g_diff_names[diff], (unsigned long long)seed);
    rc = (int)DialogBoxParamA(GetModuleHandle(NULL),
                              MAKEINTRESOURCEA(IDD_REMSIM),
                              owner ? owner : g_hwnd,
                              remsim_proc, (LPARAM)msg);
    if (rc == IDYES) {
        net_send_request("reqseed %s %llu", g_diff_names[diff],
                         (unsigned long long)seed);
    } else if (rc == IDC_REMSIM_MULTI) {
        net_send_request("reqseed %s %llu %d", g_diff_names[diff],
                         (unsigned long long)seed, REMOTE_SIM_MULTI_N);
    } else if (rc == IDC_REMSIM_UNTIL) {
        net_send_request("requntil %s %llu %d", g_diff_names[diff],
                         (unsigned long long)seed, REMOTE_SIM_UNTIL_N);
    }
}

static INT_PTR CALLBACK seeds_proc(HWND hDlg, UINT msg, WPARAM wp, LPARAM lp) {
    (void)lp;
    switch (msg) {
    case WM_INITDIALOG: {
        int i;
        memcpy(g_seeds_edit, g_diff_seeds, sizeof(g_seeds_edit));
        for (i = 0; i < DIFF_COUNT; i++) {
            int off = seeds_row_base(i);
            SendDlgItemMessageA(hDlg, off + 3, EM_SETLIMITTEXT,
                                CUSTOM_SEED_INPUT_MAX, 0);
            if (g_seeds_edit[i].value[0])
                SetDlgItemTextA(hDlg, off + 3, g_seeds_edit[i].value);
        }
        seeds_update_all(hDlg);
        return TRUE;
    }
    case WM_COMMAND: {
        int id = (int)LOWORD(wp);
        if (id >= IDC_SEED_OFF_BEGIN &&
            id < IDC_SEED_OFF_BEGIN + DIFF_COUNT * IDC_SEED_ROW_STRIDE) {
            int row = (id - IDC_SEED_OFF_BEGIN) / IDC_SEED_ROW_STRIDE;
            int sub = (id - IDC_SEED_OFF_BEGIN) % IDC_SEED_ROW_STRIDE;
            if (sub <= 2 && HIWORD(wp) == BN_CLICKED) {
                g_seeds_edit[row].mode = sub;
                seeds_apply_row(hDlg, row);
                seeds_update_result(hDlg, row);
            } else if (sub == 3 && HIWORD(wp) == EN_CHANGE) {
                char raw[CUSTOM_SEED_INPUT_MAX + 1];
                if (g_seeds_editing) return TRUE;   /* ignore re-entrant notify */
                g_seeds_editing = 1;
                GetDlgItemTextA(hDlg, id, raw, sizeof(raw));
                sanitize_seed_input(raw, g_seeds_edit[row].value,
                                    sizeof(g_seeds_edit[row].value));
                SetDlgItemTextA(hDlg, id, g_seeds_edit[row].value);
                seeds_update_result(hDlg, row);
                g_seeds_editing = 0;
            }
            return TRUE;
        }
        switch (id) {
        case IDOK: {
            int i;
            int changed_custom[DIFF_COUNT];
            for (i = 0; i < DIFF_COUNT; i++) changed_custom[i] = 0;
            for (i = 0; i < DIFF_COUNT; i++) {
                if (g_seeds_edit[i].mode == SEED_NORMAL &&
                    !custom_seed_all_digits(g_seeds_edit[i].value)) {
                    MessageBoxA(hDlg,
                        "Normal seed values must be a number.\n"
                        "Pick Custom to hash text, or Off for a random board.",
                        "Seeds", MB_OK | MB_ICONWARNING);
                    SetFocus(GetDlgItem(hDlg, seeds_row_base(i) + 3));
                    return TRUE;
                }
                if (g_seeds_edit[i].mode == SEED_CUSTOM &&
                    !g_seeds_edit[i].value[0]) {
                    MessageBoxA(hDlg,
                        "Enter a value for the Custom seed, or set it to Off.",
                        "Seeds", MB_OK | MB_ICONWARNING);
                    SetFocus(GetDlgItem(hDlg, seeds_row_base(i) + 3));
                    return TRUE;
                }
                if (g_seeds_edit[i].mode == SEED_CUSTOM &&
                    g_seeds_edit[i].value[0] &&
                    (g_diff_seeds[i].mode != SEED_CUSTOM ||
                     strcmp(g_diff_seeds[i].value,
                            g_seeds_edit[i].value) != 0)) {
                    changed_custom[i] = 1;
                }
            }
            memcpy(g_diff_seeds, g_seeds_edit, sizeof(g_diff_seeds));
            /* offer a remote simulation for each custom seed just committed */
            for (i = 0; i < DIFF_COUNT; i++) {
                uint64_t v;
                if (changed_custom[i] &&
                    diff_custom_seed_generate(i, g_diff_seeds[i].value,
                                              &v, NULL, NULL)) {
                    remsim_prompt_for_seed(hDlg, i, v);
                }
            }
            EndDialog(hDlg, IDOK);
            return TRUE;
        }
        case IDCANCEL:
            EndDialog(hDlg, IDCANCEL);
            return TRUE;
        }
        break;
    }
    }
    return FALSE;
}

/* ---------- in-game scenario probabilities ---------- */

static INT_PTR CALLBACK scenario_proc(HWND hDlg, UINT msg, WPARAM wp,
                                      LPARAM lp) {
    (void)lp;
    switch (msg) {
    case WM_INITDIALOG: {
        Game *g = &g_game;
        ScenarioReport rep;
        int i;
        char info[128];
        if (!scenario_analyze(g->rows, g->cols, g->mines, g->revealed,
                              g->mine, g->mark, g->adj, &rep)) {
            SetDlgItemTextA(hDlg, IDC_SCEN_INFO, rep.reason);
            return TRUE;
        }
        _snprintf(info, sizeof(info),
                  "%s %dx%d, %d mines - %d hidden cells (%d free)",
                  diff_str(g), g->rows, g->cols, g->mines,
                  rep.n_hidden, rep.n_free);
        SetDlgItemTextA(hDlg, IDC_SCEN_INFO, info);
        for (i = 0; i < rep.n_scenarios; i++) {
            const Scenario *s = &rep.scenarios[i];
            char line[96];
            _snprintf(line, sizeof(line),
                      "[%2d,%2d]  mine %5.1f%%  safe %5.1f%%  opens %3d  %s",
                      s->r, s->c, s->p_mine * 100.0, s->p_safe * 100.0,
                      s->reveals, s->frontier ? "frontier" : "free");
            SendDlgItemMessageA(hDlg, IDC_SCEN_LIST, LB_ADDSTRING, 0,
                                (LPARAM)line);
        }
        scenario_report_free(&rep);
        SendDlgItemMessageA(hDlg, IDC_SCEN_LIST, LB_SETCURSEL, 0, 0);
        return TRUE;
    }
    case WM_COMMAND:
        if (LOWORD(wp) == IDOK || LOWORD(wp) == IDCANCEL) {
            EndDialog(hDlg, LOWORD(wp));
            return TRUE;
        }
        break;
    }
    return FALSE;
}

/* ---------- window proc ---------- */
/* first-connect disclosure banner has been posted (once per session) */
static int g_banner_posted = 0;

/* Dismissible device-diagnostics disclosure banner. */
static INT_PTR CALLBACK diag_banner_proc(HWND hDlg, UINT msg, WPARAM wp,
                                         LPARAM lp) {
    (void)lp;
    if (msg == WM_COMMAND &&
        (LOWORD(wp) == IDOK || LOWORD(wp) == IDCANCEL)) {
        EndDialog(hDlg, LOWORD(wp));
        return TRUE;
    }
    return FALSE;
}

/* Settings > Privacy: opt-out checkbox for device diagnostics. */
static INT_PTR CALLBACK privacy_proc(HWND hDlg, UINT msg, WPARAM wp,
                                     LPARAM lp) {
    (void)lp;
    switch (msg) {
    case WM_INITDIALOG:
        CheckDlgButton(hDlg, IDC_PRIV_DIAG,
                       diag_opt_out() ? BST_UNCHECKED : BST_CHECKED);
        return TRUE;
    case WM_COMMAND:
        if (LOWORD(wp) == IDOK) {
            diag_set_opt_out(IsDlgButtonChecked(hDlg, IDC_PRIV_DIAG) ? 0 : 1);
            EndDialog(hDlg, IDOK);
            return TRUE;
        }
        if (LOWORD(wp) == IDCANCEL) {
            EndDialog(hDlg, IDCANCEL);
            return TRUE;
        }
        break;
    }
    return FALSE;
}

static void update_menu_checks(HMENU menu) {
    Game *g = &g_game;
    CheckMenuRadioItem(menu, IDM_BEGIN, IDM_EXPERT, IDM_BEGIN + g->diff, MF_BYCOMMAND);
    CheckMenuItem(menu, IDM_MARKS, MF_BYCOMMAND | (g->marks_enabled ? MF_CHECKED : MF_UNCHECKED));
    leader_sync_menu(menu);
}

static LRESULT CALLBACK wnd_proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp) {
    Game *g = &g_game;

    switch (msg) {
    case WM_CREATE:
        g_hwnd = hwnd;
        SetTimer(hwnd, ID_TIMER_DIAG, 2000, NULL);
        break;

    case WM_COMMAND:
        switch (LOWORD(wp)) {
        case IDM_NEW:
            reset_game(hwnd, &g_presets[g->diff], g->marks_enabled);
            break;
        case IDM_BEGIN:
        case IDM_INTERMEDIATE:
        case IDM_EXPERT:
            reset_game(hwnd, &g_presets[LOWORD(wp) - IDM_BEGIN], g->marks_enabled);
            break;
        case IDM_CUSTOM:
            DialogBoxParamA(GetModuleHandle(NULL), MAKEINTRESOURCEA(IDD_CUSTOM),
                            hwnd, custom_proc, 0);
            break;
        case IDM_SEEDS:
            DialogBoxParamA(GetModuleHandle(NULL), MAKEINTRESOURCEA(IDD_SEEDS),
                            hwnd, seeds_proc, 0);
            break;
        case IDM_SCENARIOS:
            DialogBoxParamA(GetModuleHandle(NULL),
                            MAKEINTRESOURCEA(IDD_SCENARIO),
                            hwnd, scenario_proc, 0);
            break;
        case IDM_PRIVACY:
            DialogBoxParamA(GetModuleHandle(NULL), MAKEINTRESOURCEA(IDD_PRIVACY),
                            hwnd, privacy_proc, 0);
            break;
        case IDM_HOF_PLAYER:
            leader_show_player(hwnd);
            break;
        case IDM_HOF_VIEW:
            leader_show_hof(hwnd);
            break;
        case IDM_HOF_AUTO:
            leader_set_auto_submit(!leader_auto_submit());
            break;
        case IDM_MARKS:
            g->marks_enabled = !g->marks_enabled;
            break;
        case IDM_EXIT:
            PostMessage(hwnd, WM_CLOSE, 0, 0);
            break;
        case IDM_ABOUT:
            MessageBoxA(hwnd,
                "Minesweeper (Classic)\n\n"
                "A faithful original port of the classic 1992 Windows 3.1\n"
                "game, rebuilt for Windows 7, 8.1, 10 and 11 (32-bit and\n"
                "64-bit). The original 16-bit game no longer runs on\n"
                "modern Windows.\n\n"
                "Left-click: uncover   Right-click: flag / ? / clear\n"
                "Press both buttons (or middle-click) on a number to chord.\n"
                "F2: new game",
                "About Minesweeper", MB_OK | MB_ICONINFORMATION);
            break;
        }
        if (GetMenu(hwnd)) update_menu_checks(GetMenu(hwnd));
        return 0;

    case WM_LBUTTONDOWN:
        {
            LARGE_INTEGER qa, qb, freq;
            QueryPerformanceFrequency(&freq);
            QueryPerformanceCounter(&qa);
            SetCapture(hwnd);
            on_lbutton_down(g, GET_X_LPARAM(lp), GET_Y_LPARAM(lp));
            QueryPerformanceCounter(&qb);
            if (freq.QuadPart > 0)
                metrics_note_ui_latency((long)((qb.QuadPart - qa.QuadPart)
                                               * 1000000 / freq.QuadPart));
        }
        return 0;
    case WM_RBUTTONDOWN:
        SetCapture(hwnd);
        on_rbutton_down(g, GET_X_LPARAM(lp), GET_Y_LPARAM(lp));
        return 0;
    case WM_MBUTTONDOWN:
        /* middle-click chord convenience */
        {
            int cell = cell_at(g, GET_X_LPARAM(lp), GET_Y_LPARAM(lp));
            if (cell >= 0 && g->revealed[cell] && g->adj[cell] > 0) {
                metrics_note_click();
                do_chord(g, cell);
            }
        }
        return 0;
    case WM_LBUTTONUP:
        on_button_up(g, GET_X_LPARAM(lp), GET_Y_LPARAM(lp), 1);
        if (!g_left_down) ReleaseCapture();
        return 0;
    case WM_RBUTTONUP:
        on_button_up(g, GET_X_LPARAM(lp), GET_Y_LPARAM(lp), 0);
        if (!g_left_down) ReleaseCapture();
        return 0;
    case WM_MBUTTONUP:
        return 0;
    case WM_CAPTURECHANGED:
        g_left_down = g_right_down = 0;
        g_pressed_cell = g_chord_cell = -1;
        g_face_pressed = 0;
        InvalidateRect(hwnd, NULL, TRUE);
        return 0;

    case WM_TIMER:
        if (wp == ID_TIMER_DIAG) {
            /* diagnostics hook: act on telemetry connect edges, independent
             * of gameplay state.  On the rising edge (re)connect, log the
             * disclosure notice, queue an HTTPS delivery, and show the
             * one-time banner. */
            static int prev_connected = 0;
            if (net_telemetry_active()) {
                NetStats st;
                net_get_stats(&st);
                if (st.connected && !prev_connected) {
                    diag_on_connected();
                    if (diag_banner_needed() && !g_banner_posted) {
                        g_banner_posted = 1;
                        PostMessage(hwnd, WM_APP_DIAG_BANNER, 0, 0);
                    }
                }
                prev_connected = st.connected;
            } else {
                prev_connected = 0;
            }
            return 0;
        }
        if (wp == ID_TIMER && g->over == 0 && !g->paused) {
            if (g->time < 999) g->time++;
            /* periodic UI-latency telemetry (every 10 s while playing) */
            if (g->time > 0 && (g->time % 10) == 0 && net_telemetry_active())
                net_send_metric("metric latency us=%.0f t=%llu",
                                g_metric_latency_ema_us,
                                (unsigned long long)GetTickCount64());
            InvalidateRect(hwnd, NULL, TRUE);
        }
        return 0;

    case WM_KEYDOWN:
        if (wp == VK_F2) reset_game(hwnd, &g_presets[g->diff], g->marks_enabled);
        return 0;

    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(hwnd, &ps);
        paint(dc);
        EndPaint(hwnd, &ps);
        return 0;
    }
    case WM_ERASEBKGND:
        return 1;

    case WM_DISPLAYCHANGE:
        g_dpi = GetDeviceCaps(GetDC(NULL), LOGPIXELSX);
        InvalidateRect(hwnd, NULL, TRUE);
        return 0;

    case WM_APP_CLI:
        cli_dispatch((CliCmd *)lp);
        return 0;

    case WM_APP_TELEMETRY_SEED: {
        /* seed streamed from the telemetry server (heap ptr in lp) */
        TelemetrySeedMsg *m = (TelemetrySeedMsg *)lp;
        if (m) {
            if (m->diff >= 0 && m->diff < DIFF_COUNT) {
                SeedSlot *sl = &g_diff_seeds[m->diff];
                sl->mode = SEED_NORMAL;
                _snprintf(sl->value, sizeof(sl->value), "%llu",
                          (unsigned long long)m->seed);
            }
            free(m);
            reset_game(hwnd, &g_presets[g->diff], g->marks_enabled);
        }
        return 0;
    }

    case WM_APP_DIAG_BANNER:
        DialogBoxParamA(GetModuleHandle(NULL),
                        MAKEINTRESOURCEA(IDD_DIAG_BANNER),
                        hwnd, diag_banner_proc, 0);
        diag_mark_banner_seen();
        return 0;

    case WM_APP_SOLVER_DENIED: {
        /* the server refused an authenticated solver request (e.g. the
         * credentials were revoked); warn once per session */
        static int warned = 0;
        if (!warned) {
            warned = 1;
            MessageBoxA(hwnd,
                "The simulation server declined a remote-simulation request. "
                "Solver access is not granted for these credentials.",
                "Minesweeper", MB_OK | MB_ICONWARNING);
        }
        return 0;
    }

    case WM_DESTROY:
        KillTimer(hwnd, ID_TIMER);
        KillTimer(hwnd, ID_TIMER_DIAG);
        if (g_font_num) DeleteObject(g_font_num);
        if (g_font_q) DeleteObject(g_font_q);
        free_board(g);
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

/* ================= custom seed generation =================
 *
 * Opt-in deterministic seed generator. Activated ONLY by the
 * "--seed-custom <value>" command-line argument or the CLI
 * "seedcustom <value>" command. Normal play is unaffected.
 *
 * Logic:
 *   1. If the input contains any letter it is first converted to a
 *      numeric value via a lightweight FNV-1a 64-bit hash (O(n)).
 *   2. The numeric seed's digit count is compared to a target digit
 *      count (CUSTOM_SEED_TARGET_DIGITS).
 *   3. While the seed has fewer digits than the target, a geometric
 *      multiplier sequence is applied cumulatively: 1st step x2,
 *      2nd x4, 3rd x8, 4th x16, ... (i.e. x2^k at step k).
 *   4. After every step the digit count is re-checked; if it exceeds
 *      the target, the excess trailing digits are sliced off so the
 *      seed is exactly target digits long, and the loop terminates.
 *
 * Arithmetic is done on a decimal string (O(n) per step), so large
 * inputs never overflow. The loop is bounded by CUSTOM_SEED_MAX_STEPS
 * and the buffer is fixed-size, so it cannot spin, leak or lag.
 */

#define CUSTOM_SEED_TARGET_DIGITS 19   /* 19 digits always fits in uint64 */
#define CUSTOM_SEED_MAX_STEPS     32   /* hard cap: x2, x4, ... x2^32     */
#define CUSTOM_SEED_MAX_BUF      160

static int custom_seed_all_digits(const char *s) {
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
        tmp[k++] = (char)('0' + (v % 10));   /* least significant first */
        carry = v / 10;
    }
    while (carry) {
        tmp[k++] = (char)('0' + (carry % 10));
        carry /= 10;
    }
    for (i = 0; i < k; i++) buf[i] = tmp[k - 1 - i];
    return k;
}

/* Generate the final 64-bit seed from a raw input string.
   Returns 1 on success, 0 on invalid input. */
static int custom_seed_generate(const char *input, uint64_t *out,
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
        /* alphanumeric: hash it to a numeric value first */
        uint64_t hv = custom_seed_fnv1a64(input);
        len = _snprintf(buf, sizeof(buf), "%llu", (unsigned long long)hv);
    }

    /* strip leading zeros */
    i = 0;
    while (i < len - 1 && buf[i] == '0') i++;
    if (i) { memmove(buf, buf + i, (size_t)(len - i)); len -= i; }

    /* degenerate "0" input -> seed 0 */
    if (len == 1 && buf[0] == '0') { *out = 0; return 1; }

    /* already at/over target? just trim excess trailing digits */
    if (len >= target) {
        if (len > target) { len = target; truncated = 1; }
    } else {
        /* multiplier loop: x2, x4, x8, x16, ... until we reach target */
        while (len < target && steps < CUSTOM_SEED_MAX_STEPS) {
            uint64_t m = 1ULL << (steps + 1);          /* 2^(steps+1) */
            len = custom_seed_str_mul(buf, len, m);
            steps++;
            if (len > target) {                         /* slice excess */
                len = target;
                truncated = 1;
                break;
            }
        }
    }

    /* final value has <= 19 digits, always fits in uint64 */
    {
        uint64_t v = 0;
        for (i = 0; i < len; i++) v = v * 10 + (uint64_t)(buf[i] - '0');
        *out = v;
    }
    if (steps_out) *steps_out = steps;
    if (truncated_out) *truncated_out = truncated;
    return 1;
}

/* ---------- per-difficulty seed derivation ----------
 * A per-difficulty CUSTOM seed folds the difficulty name into the hash, so
 * the same phrase produces a different board per difficulty. A pure number
 * is always used as-is (difficulty-independent). The legacy one-shot
 * "seedcustom <value>" / "--seed-custom <value>" forms keep the original
 * unsalted math. */

static const char *const g_diff_names[DIFF_COUNT] = {
    "beginner", "intermediate", "expert", "custom"
};

static const char *const g_diff_salts[DIFF_COUNT] = {
    "beginner", "intermediate", "expert", "custom"
};

static int parse_diff_name(const char *s) {
    int i;
    if (!s || !*s) return -1;
    for (i = 0; i < DIFF_COUNT; i++)
        if (stricmp(s, g_diff_names[i]) == 0) return i;
    return -1;
}

/* ---------- telemetry metric helpers (UI thread) ---------- */
static unsigned long long metric_now_ms(void) {
    return (unsigned long long)GetTickCount64();
}

static void metrics_reset(void) {
    g_metric_clicks = 0;
    g_metric_latency_ema_us = 0.0;
    g_metric_latency_n = 0;
}

static void metrics_note_click(void) {
    g_metric_clicks++;
}

static void metrics_note_ui_latency(long us) {
    if (us < 0) us = 0;
    if (g_metric_latency_n == 0)
        g_metric_latency_ema_us = (double)us;
    else
        g_metric_latency_ema_us = 0.8 * g_metric_latency_ema_us + 0.2 * (double)us;
    g_metric_latency_n++;
}

/* board (re)started: report the difficulty + resolved seed */
static void metric_emit_start(int diff) {
    if (!net_telemetry_active()) return;
    net_send_metric("metric start diff=%s seed=%llu seeded=%d t=%llu",
                    g_diff_names[diff],
                    (unsigned long long)g_game_seed_active_val,
                    g_game_seed_active ? 1 : 0,
                    metric_now_ms());
}

/* game ended: report win/loss with time, clicks and UI latency */
static void metric_emit_over(const char *kind) {
    Game *g = &g_game;
    if (!net_telemetry_active()) return;
    net_send_metric("metric %s diff=%s seed=%llu seeded=%d time=%d "
                    "clicks=%llu latency=%.0f t=%llu",
                    kind, g_diff_names[g->diff],
                    (unsigned long long)g_game_seed_active_val,
                    g_game_seed_active ? 1 : 0,
                    g->time, g_metric_clicks, g_metric_latency_ema_us,
                    metric_now_ms());
}

/* Received `seed <diff> <n>` from the telemetry server: apply it as the
 * persistent Normal seed for that difficulty.  Runs on the network thread,
 * so marshal to the UI thread via PostMessage. */
static void telemetry_seed_sink(int diff, unsigned long long seed) {
    TelemetrySeedMsg *m = (TelemetrySeedMsg *)malloc(sizeof(*m));
    if (!m) return;
    m->diff = diff;
    m->seed = seed;
    if (g_hwnd) {
        if (!PostMessageA(g_hwnd, WM_APP_TELEMETRY_SEED, 0, (LPARAM)m))
            free(m);
    } else {
        free(m);
    }
}

/* Derive the seed for a per-difficulty CUSTOM slot. Pure numbers are used
   directly; anything else is hashed with "difficulty:" folded in. */
static int diff_custom_seed_generate(int diff, const char *input, uint64_t *out,
                                     int *steps_out, int *truncated_out) {
    if (!input || !*input) return 0;
    if (custom_seed_all_digits(input))
        return custom_seed_generate(input, out, steps_out, truncated_out);
    {
        char salted[CUSTOM_SEED_INPUT_MAX + 48];
        const char *salt = (diff >= 0 && diff < DIFF_COUNT)
                               ? g_diff_salts[diff] : "";
        _snprintf(salted, sizeof(salted), "%s:%s", salt, input);
        return custom_seed_generate(salted, out, steps_out, truncated_out);
    }
}

/* Pick the seed for a new board of the given difficulty: a pending one-shot
   override wins (and is consumed), otherwise the difficulty's slot, otherwise
   random (seeded = 0). */
static void resolve_board_seed(int diff, uint64_t *out, int *seeded) {
    const SeedSlot *sl;
    *seeded = 0;
    if (g_seed_override) {
        *out = rng_seed_or_fallback(g_seed_override_val);
        g_seed_override = 0;
        *seeded = 1;
        return;
    }
    if (diff < 0 || diff >= DIFF_COUNT) return;
    sl = &g_diff_seeds[diff];
    if (sl->mode == SEED_NORMAL) {
        *out = rng_seed_or_fallback(strtoull(sl->value, NULL, 10));
        *seeded = 1;
    } else if (sl->mode == SEED_CUSTOM) {
        uint64_t v;
        if (diff_custom_seed_generate(diff, sl->value, &v, NULL, NULL)) {
            *out = rng_seed_or_fallback(v);
            *seeded = 1;
        }
    }
}

/* ================= CLI / scripting interface =================
 *
 * Optional localhost TCP server, enabled only when the game is started as
 *     minesweeper.exe --listen <port>
 *
 * Protocol: newline-delimited text commands, one per line.  Every response
 * is a set of lines terminated by a line containing just "END".
 *
 *   ping                       -> OK
 *   help                       -> command list
 *   new beginner|intermediate|expert|custom [rows cols mines]
 *                              -> start a new game
 *   click <row> <col>          -> reveal a cell (first click is safe)
 *   flag  <row> <col>          -> cycle mark: flag / ? / clear
 *   chord <row> <col>          -> chord a revealed number
 *   state                      -> dump game state (key=value lines)
 *   board                      -> dump the board as rows of cell chars
 *   marks [0|1]                -> query or set question marks
 *   pause | resume             -> pause / resume the timer
 *   seed <n>                   -> one-shot numeric seed for the next new
 *   seed <diff> <n>            -> persistent NORMAL seed for a difficulty
 *   seed <diff> off            -> clear a difficulty's seed
 *   seed off                   -> clear the one-shot pending seed
 *   seedcustom <value>         -> one-shot custom seed for the next new
 *   seedcustom <diff> <value>  -> persistent CUSTOM seed for a difficulty
 *   seedcustom <diff> off      -> clear a difficulty's seed
 *   seeds                      -> list per-difficulty seeds + pending
 *   refresh [0|1]              -> query or toggle repaints after CLI actions
 *   quit                       -> close this connection
 *
 * <diff> is beginner | intermediate | expert | custom. A custom value is
 * alphanumeric input hashed to a number, then multiplied by 2,4,8,16,...
 * until it reaches 19 digits (truncated if it overruns); the difficulty name
 * is folded into the hash for per-difficulty seeds. A pure number is always
 * used as-is. Every game of a difficulty uses its stored seed until cleared.
 *
 * Board dump cell characters: . = hidden, F = flag, ? = question,
 * * = revealed mine, 0..8 = revealed number.
 */

enum {
    CLI_PING, CLI_HELP, CLI_NEW, CLI_CLICK, CLI_FLAG, CLI_CHORD,
    CLI_STATE, CLI_BOARD, CLI_MARKS, CLI_PAUSE, CLI_RESUME,
    CLI_SEED, CLI_REFRESH, CLI_SEEDCUSTOM, CLI_SEEDS,
    CLI_TELEMETRY, CLI_REQSEED, CLI_REQBATCH, CLI_SCENARIOS, CLI_QUIT
};

struct CliCmd {
    int op;               /* CLI_* */
    int a, b, c;          /* generic args (row/col, marks, seed...) */
    int have_u;           /* CLI_REQSEED: seed argument was provided */
    int rows, cols, mines;/* CLI_NEW custom dimensions */
    unsigned long long u; /* 64-bit seed value */
    char *s;              /* string arg (custom seed) */
    char *reply;          /* malloc'd reply, built by the UI thread */
};

static SOCKET  g_cli_sock = INVALID_SOCKET;
static HANDLE  g_cli_thread = NULL;
static volatile LONG g_cli_running = 0;
static volatile LONG g_cli_started = 0;

static void cli_append(char **buf, size_t *len, size_t *cap, const char *fmt, ...) {
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

static const char *diff_str(const Game *g) {
    if (g->diff >= DIFF_BEGIN && g->diff < DIFF_COUNT)
        return g_diff_names[g->diff];
    return "custom";
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
            if (cc->rows > 0) { /* custom dims given */
                d.rows = cc->rows < 8 ? 8 : (cc->rows > MAX_ROWS ? MAX_ROWS : cc->rows);
                d.cols = cc->cols < 8 ? 8 : (cc->cols > MAX_COLS ? MAX_COLS : cc->cols);
                d.mines = cc->mines;
                if (d.mines < 1) d.mines = 1;
                if (d.mines > d.rows * d.cols - 9) d.mines = d.rows * d.cols - 9;
                d.diff = DIFF_CUSTOM;
                g_custom = d;
            } else { /* reuse last custom board */
                d = g_custom;
            }
            reset_game(g_hwnd, &d, g->marks_enabled);
        } else if (cc->a >= DIFF_BEGIN && cc->a <= DIFF_EXPERT) {
            reset_game(g_hwnd, &g_presets[cc->a], g->marks_enabled);
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
            if (g_cli_refresh) InvalidateRect(g_hwnd, NULL, TRUE);
        } else if (cc->op == CLI_FLAG) {
            cycle_mark(g, i);
        } else {
            do_chord(g, i);
            if (g_cli_refresh) InvalidateRect(g_hwnd, NULL, TRUE);
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
        } else if (cc->a == -2) {          /* "seed off": clear pending */
            g_seed_override = 0;
            cli_append(&buf, &len, &cap, "OK seed off\n");
        } else if (cc->a < 0) {            /* one-shot: next new uses it */
            g_seed_override = 1;
            g_seed_override_val = cc->u;
            cli_append(&buf, &len, &cap, "OK seed=%llu\n",
                       (unsigned long long)g_seed_override_val);
        } else if (cc->b == 0) {           /* per-difficulty clear */
            g_diff_seeds[cc->a].mode = SEED_OFF;
            cli_append(&buf, &len, &cap, "OK seed off\n");
        } else {                           /* per-difficulty normal seed */
            SeedSlot *sl = &g_diff_seeds[cc->a];
            sl->mode = SEED_NORMAL;
            _snprintf(sl->value, sizeof(sl->value), "%llu",
                      (unsigned long long)cc->u);
            cli_append(&buf, &len, &cap, "OK seed=%llu\n",
                       (unsigned long long)cc->u);
        }
        break;

    case CLI_SEEDCUSTOM: {
        uint64_t v;
        int steps = 0, truncated = 0;
        if (cc->a == -3) {
            cli_append(&buf, &len, &cap, "ERR unknown difficulty\n");
        } else if (cc->a == -2) {          /* "seedcustom off": clear pending */
            g_seed_override = 0;
            cli_append(&buf, &len, &cap, "OK seed off\n");
        } else if (cc->a < 0) {            /* one-shot: legacy unsalted math */
            if (!custom_seed_generate(cc->s, &v, &steps, &truncated)) {
                cli_append(&buf, &len, &cap, "ERR bad seed input\n");
                break;
            }
            g_seed_override = 1;
            g_seed_override_val = v;
            cli_append(&buf, &len, &cap, "OK seed=%llu steps=%d truncated=%d\n",
                       (unsigned long long)v, steps, truncated);
        } else if (cc->b == 0) {           /* per-difficulty clear */
            g_diff_seeds[cc->a].mode = SEED_OFF;
            cli_append(&buf, &len, &cap, "OK seed off\n");
        } else {                           /* per-difficulty custom seed */
            SeedSlot *sl = &g_diff_seeds[cc->a];
            if (!diff_custom_seed_generate(cc->a, cc->s, &v, &steps,
                                           &truncated)) {
                cli_append(&buf, &len, &cap, "ERR bad seed input\n");
                break;
            }
            sl->mode = SEED_CUSTOM;
            strncpy(sl->value, cc->s, sizeof(sl->value) - 1);
            sl->value[sizeof(sl->value) - 1] = 0;
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
        if (g_cli_refresh) InvalidateRect(g_hwnd, NULL, TRUE);
        break;

    case CLI_TELEMETRY:
        if (cc->a == 0)
            net_telemetry_stop();
        else if (cc->a == 1) {
            net_set_http_mode(g_telemetry_http);
            net_set_https_insecure(g_telemetry_https_insecure);
            net_telemetry_start(g_telemetry_host, g_telemetry_port);
        }
        if (cc->a == -2)
            cli_append(&buf, &len, &cap, "ERR arg\n");
        else {
            NetStats st;
            net_get_stats(&st);
            cli_append(&buf, &len, &cap,
                       "telemetry=%d host=%s port=%u connected=%d "
                       "attempts=%llu seeds=%llu outcomes=%llu wins=%llu "
                       "sent=%llu dropped=%llu\n",
                       net_telemetry_active(),
                       g_telemetry_host, g_telemetry_port,
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

static int cli_send_all(SOCKET s, const char *buf, int len) {
    int off = 0;
    while (off < len) {
        int n = send(s, buf + off, len - off, 0);
        if (n <= 0) return 0;
        off += n;
    }
    return 1;
}

static void cli_handle_client(SOCKET s) {
    char line[512];
    int used = 0;
    int done = 0;

    while (!done) {
        char c;
        int n = recv(s, &c, 1, 0);
        if (n <= 0) break;
        if (c == '\n') {
            CliCmd cmd;
            char *tok, *save = NULL;
            int argc = 0;
            char *argv[8];
            line[used] = 0;
            used = 0;

            tok = strtok_r(line, " \t\r", &save);
            while (tok && argc < 8) { argv[argc++] = tok; tok = strtok_r(NULL, " \t\r", &save); }
            if (argc == 0) continue;

            memset(&cmd, 0, sizeof(cmd));
            cmd.a = cmd.b = cmd.c = -1;
            cmd.rows = cmd.cols = cmd.mines = -1;
            cmd.reply = NULL;

            if (stricmp(argv[0], "ping") == 0) cmd.op = CLI_PING;
            else if (stricmp(argv[0], "help") == 0) cmd.op = CLI_HELP;
            else if (stricmp(argv[0], "state") == 0) cmd.op = CLI_STATE;
            else if (stricmp(argv[0], "board") == 0) cmd.op = CLI_BOARD;
            else if (stricmp(argv[0], "pause") == 0) cmd.op = CLI_PAUSE;
            else if (stricmp(argv[0], "resume") == 0) cmd.op = CLI_RESUME;
            else if (stricmp(argv[0], "quit") == 0) cmd.op = CLI_QUIT;
            else if (stricmp(argv[0], "new") == 0) {
                cmd.op = CLI_NEW;
                if (argc >= 2) {
                    if (stricmp(argv[1], "beginner") == 0) cmd.a = DIFF_BEGIN;
                    else if (stricmp(argv[1], "intermediate") == 0) cmd.a = DIFF_INTERMEDIATE;
                    else if (stricmp(argv[1], "expert") == 0) cmd.a = DIFF_EXPERT;
                    else if (stricmp(argv[1], "custom") == 0) cmd.a = DIFF_CUSTOM;
                    else { cmd.a = -1; }
                    if (cmd.a == DIFF_CUSTOM && argc >= 5) {
                        cmd.rows = atoi(argv[2]);
                        cmd.cols = atoi(argv[3]);
                        cmd.mines = atoi(argv[4]);
                    }
                }
            }
            else if (stricmp(argv[0], "click") == 0) { cmd.op = CLI_CLICK; if (argc >= 3) { cmd.a = atoi(argv[1]); cmd.b = atoi(argv[2]); } }
            else if (stricmp(argv[0], "flag") == 0) { cmd.op = CLI_FLAG; if (argc >= 3) { cmd.a = atoi(argv[1]); cmd.b = atoi(argv[2]); } }
            else if (stricmp(argv[0], "chord") == 0) { cmd.op = CLI_CHORD; if (argc >= 3) { cmd.a = atoi(argv[1]); cmd.b = atoi(argv[2]); } }
            else if (stricmp(argv[0], "marks") == 0) { cmd.op = CLI_MARKS; if (argc >= 2) cmd.a = atoi(argv[1]); else cmd.a = -1; }
            else if (stricmp(argv[0], "seed") == 0) {
                cmd.op = CLI_SEED;
                if (argc == 2) {
                    if (stricmp(argv[1], "off") == 0) { cmd.a = -2; }
                    else { cmd.a = -1; cmd.u = strtoull(argv[1], NULL, 10); }
                } else if (argc == 3) {
                    cmd.a = parse_diff_name(argv[1]);
                    if (cmd.a < 0) cmd.a = -3;   /* unknown difficulty */
                    if (stricmp(argv[2], "off") == 0) { cmd.b = 0; }
                    else { cmd.b = 1; cmd.u = strtoull(argv[2], NULL, 10); }
                }
            }
            else if (stricmp(argv[0], "seedcustom") == 0) {
                cmd.op = CLI_SEEDCUSTOM;
                if (argc == 2) {
                    if (stricmp(argv[1], "off") == 0) { cmd.a = -2; }
                    else { cmd.a = -1; cmd.s = argv[1]; }
                } else if (argc == 3) {
                    cmd.a = parse_diff_name(argv[1]);
                    if (cmd.a < 0) cmd.a = -3;   /* unknown difficulty */
                    if (stricmp(argv[2], "off") == 0) { cmd.b = 0; }
                    else { cmd.b = 1; cmd.s = argv[2]; }
                }
            }
            else if (stricmp(argv[0], "seeds") == 0) cmd.op = CLI_SEEDS;
            else if (stricmp(argv[0], "scenarios") == 0) cmd.op = CLI_SCENARIOS;
            else if (stricmp(argv[0], "telemetry") == 0) {
                cmd.op = CLI_TELEMETRY;
                if (argc >= 2) {
                    if (stricmp(argv[1], "on") == 0) cmd.a = 1;
                    else if (stricmp(argv[1], "off") == 0) cmd.a = 0;
                    else cmd.a = -2;   /* unknown argument */
                } else cmd.a = -1;     /* query */
            }
            else if (stricmp(argv[0], "reqseed") == 0) {
                /* reqseed <diff> <n> [count]: ask the telemetry server to
                 * simulate this exact seed (optionally `count` times) */
                cmd.op = CLI_REQSEED;
                cmd.a = -1;
                if (argc >= 2) cmd.a = parse_diff_name(argv[1]);
                if (argc >= 3) {
                    cmd.u = strtoull(argv[2], NULL, 10);
                    cmd.have_u = 1;
                }
                if (argc >= 4) cmd.c = atoi(argv[3]);
            }
            else if (stricmp(argv[0], "reqbatch") == 0) {
                /* reqbatch <diff> <count>: ask the telemetry server to
                 * simulate `count` random boards at this difficulty */
                cmd.op = CLI_REQBATCH;
                cmd.a = -1;
                if (argc >= 2) cmd.a = parse_diff_name(argv[1]);
                if (argc >= 3) cmd.c = atoi(argv[2]);
            }
            else if (stricmp(argv[0], "refresh") == 0) { cmd.op = CLI_REFRESH; if (argc >= 2) cmd.a = atoi(argv[1]); else cmd.a = -1; }
            else { cmd.op = -1; }

            if (cmd.op == CLI_QUIT) {
                cli_send_all(s, "OK\nEND\n", 7);
                done = 1;
                break;
            }
            if (cmd.op == -1) {
                cli_send_all(s, "ERR unknown command\nEND\n", 24);
                continue;
            }

            /* execute on the UI thread where the game state is safe */
            if (g_hwnd) {
                SendMessageA(g_hwnd, WM_APP_CLI, 0, (LPARAM)&cmd);
            } else {
                cmd.reply = _strdup("ERR no window\nEND\n");
            }
            if (cmd.reply) {
                cli_send_all(s, cmd.reply, (int)strlen(cmd.reply));
                free(cmd.reply);
            }
        } else if (used < (int)sizeof(line) - 1) {
            line[used++] = c;
        }
    }
}

static DWORD WINAPI cli_server_thread(LPVOID arg) {
    (void)arg;
    while (g_cli_running) {
        SOCKET client = accept(g_cli_sock, NULL, NULL);
        if (client == INVALID_SOCKET) break;
        /* disable Nagle: keep single click/flag packets from being delayed */
        {
            char one = 1;
            setsockopt(client, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
        }
        cli_handle_client(client);
        closesocket(client);
    }
    return 0;
}

static int cli_start(int port) {
    WSADATA wsa;
    struct sockaddr_in addr;
    SOCKET s;

    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return 0;

    s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (s == INVALID_SOCKET) { WSACleanup(); return 0; }

    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);   /* localhost only */
    addr.sin_port = htons((u_short)port);

    if (bind(s, (struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR ||
        listen(s, 4) == SOCKET_ERROR) {
        closesocket(s);
        WSACleanup();
        return 0;
    }

    g_cli_sock = s;
    InterlockedExchange(&g_cli_started, 1);
    InterlockedExchange(&g_cli_running, 1);
    g_cli_thread = CreateThread(NULL, 0, cli_server_thread, NULL, 0, NULL);
    if (!g_cli_thread) {
        InterlockedExchange(&g_cli_running, 0);
        closesocket(s);
        WSACleanup();
        InterlockedExchange(&g_cli_started, 0);
        g_cli_sock = INVALID_SOCKET;
        return 0;
    }
    return 1;
}

static void cli_stop(void) {
    InterlockedExchange(&g_cli_running, 0);
    if (g_cli_sock != INVALID_SOCKET) {
        closesocket(g_cli_sock);
        g_cli_sock = INVALID_SOCKET;
    }
    if (g_cli_thread) {
        WaitForSingleObject(g_cli_thread, 1000);
        CloseHandle(g_cli_thread);
        g_cli_thread = NULL;
    }
    if (g_cli_started) {
        WSACleanup();
        InterlockedExchange(&g_cli_started, 0);
    }
}

static int parse_listen_port(LPSTR lpCmd) {
    /* scan for --listen <port> or --listen=port */
    char buf[256];
    char *tok, *save = NULL;
    if (!lpCmd || !*lpCmd) return -1;
    strncpy(buf, lpCmd, sizeof(buf) - 1);
    buf[sizeof(buf) - 1] = 0;
    tok = strtok_r(buf, " \t\r", &save);
    while (tok) {
        if (stricmp(tok, "--listen") == 0) {
            tok = strtok_r(NULL, " \t\r", &save);
            if (tok) return atoi(tok);
            return -1;
        }
        if (_strnicmp(tok, "--listen=", 9) == 0) return atoi(tok + 9);
        tok = strtok_r(NULL, " \t\r", &save);
    }
    return -1;
}

/* Apply one --seed / --seed-custom argument.
 *   "value"            -> one-shot override for the first board (legacy)
 *   "difficulty:value" -> persistent per-difficulty seed slot
 * For --seed the value is numeric; for --seed-custom it is hashed text. */
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
        SeedSlot *sl = &g_diff_seeds[diff];
        if (custom) {
            uint64_t v;
            if (diff_custom_seed_generate(diff, value, &v, NULL, NULL)) {
                sl->mode = SEED_CUSTOM;
                strncpy(sl->value, value, sizeof(sl->value) - 1);
                sl->value[sizeof(sl->value) - 1] = 0;
            }
        } else {
            sl->mode = SEED_NORMAL;
            _snprintf(sl->value, sizeof(sl->value), "%llu",
                      (unsigned long long)strtoull(value, NULL, 10));
        }
    } else if (custom) {
        uint64_t v;
        if (custom_seed_generate(arg, &v, NULL, NULL)) {
            g_seed_override = 1;
            g_seed_override_val = v;
        }
    } else {
        g_seed_override = 1;
        g_seed_override_val = strtoull(arg, NULL, 10);
    }
}

/* scan the command line for --seed / --seed-custom arguments (both "flag value"
 * and "flag=value" forms) and apply them. */
static void parse_seed_args(LPSTR lpCmd) {
    char buf[512];
    char *tok, *save = NULL;
    if (!lpCmd || !*lpCmd) return;
    strncpy(buf, lpCmd, sizeof(buf) - 1);
    buf[sizeof(buf) - 1] = 0;
    tok = strtok_r(buf, " \t\r", &save);
    while (tok) {
        if (stricmp(tok, "--seed-custom") == 0) {
            tok = strtok_r(NULL, " \t\r", &save);
            if (tok) apply_seed_arg(tok, 1);
        } else if (_strnicmp(tok, "--seed-custom=", 14) == 0) {
            apply_seed_arg(tok + 14, 1);
        } else if (stricmp(tok, "--seed") == 0) {
            tok = strtok_r(NULL, " \t\r", &save);
            if (tok) apply_seed_arg(tok, 0);
        } else if (_strnicmp(tok, "--seed=", 7) == 0) {
            apply_seed_arg(tok + 7, 0);
        }
        tok = strtok_r(NULL, " \t\r", &save);
    }
}

/* parse --telemetry <host>:<port>. Returns 1 and fills the globals on success. */
static int parse_telemetry_arg(const char *arg) {
    char host[128];
    const char *colon;
    size_t hlen;
    unsigned port;
    if (!arg || !*arg) return 0;
    colon = strchr(arg, ':');
    if (!colon) return 0;
    hlen = (size_t)(colon - arg);
    if (hlen == 0 || hlen >= sizeof(host)) return 0;
    memcpy(host, arg, hlen);
    host[hlen] = 0;
    port = (unsigned)atoi(colon + 1);
    if (port == 0 || port > 65535) return 0;
    strncpy(g_telemetry_host, host, sizeof(g_telemetry_host) - 1);
    g_telemetry_host[sizeof(g_telemetry_host) - 1] = 0;
    g_telemetry_port = port;
    return 1;
}

/* scan the command line for --telemetry <host:port> / --no-telemetry and
 * set the endpoint accordingly.  Telemetry is forced on by default. */
static void parse_telemetry_args(LPSTR lpCmd) {
    char buf[512];
    char *tok, *save = NULL;
    if (!lpCmd || !*lpCmd) return;
    strncpy(buf, lpCmd, sizeof(buf) - 1);
    buf[sizeof(buf) - 1] = 0;
    tok = strtok_r(buf, " \t\r", &save);
    while (tok) {
        if (stricmp(tok, "--no-telemetry") == 0) {
            g_telemetry_port = 0;
        } else if (stricmp(tok, "--telemetry") == 0) {
            tok = strtok_r(NULL, " \t\r", &save);
            if (tok) parse_telemetry_arg(tok);
        } else if (_strnicmp(tok, "--telemetry=", 12) == 0) {
            parse_telemetry_arg(tok + 12);
        } else if (stricmp(tok, "--telemetry-http") == 0) {
            g_telemetry_http = 1;
        } else if (stricmp(tok, "--telemetry-https") == 0) {
            g_telemetry_http = 2;
        } else if (stricmp(tok, "--telemetry-https-insecure") == 0) {
            g_telemetry_https_insecure = 1;
        }
        tok = strtok_r(NULL, " \t\r", &save);
    }
}


int WINAPI WinMain(HINSTANCE hInst, HINSTANCE hPrev, LPSTR lpCmd, int nShow) {
    (void)hPrev; (void)lpCmd;
    WNDCLASSA wc;
    HWND hwnd;
    MSG msg;
    RECT rc;
    Game *g = &g_game;
    int cli_port;
    HDC sdc = GetDC(NULL);
    g_dpi = GetDeviceCaps(sdc, LOGPIXELSX);
    ReleaseDC(NULL, sdc);

    memset(&wc, 0, sizeof(wc));
    wc.style = CS_HREDRAW | CS_VREDRAW;
    wc.lpfnWndProc = wnd_proc;
    wc.hInstance = hInst;
    wc.hIcon = LoadIconA(hInst, MAKEINTRESOURCEA(IDI_MAIN));
    wc.hCursor = LoadCursor(NULL, IDC_ARROW);
    wc.hbrBackground = (HBRUSH)(COLOR_BTNFACE + 1);
    wc.lpszMenuName = MAKEINTRESOURCEA(IDR_MENU);
    wc.lpszClassName = "MinesweeperClassicPort";
    if (!RegisterClassA(&wc)) return 1;

    g_font_num = CreateFontA(-S(14), 0, 0, 0, FW_BOLD, 0, 0, 0,
                             DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                             CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                             DEFAULT_PITCH | FF_SWISS, "Arial");
    g_font_q = CreateFontA(-S(14), 0, 0, 0, FW_BOLD, 0, 0, 0,
                           DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                           CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                           DEFAULT_PITCH | FF_SWISS, "Arial");

    /* optional seeds: --seed-custom <value>, --seed <n>, or per-difficulty
     * forms like --seed-custom beginner:hello / --seed expert:42 */
    parse_seed_args(lpCmd);
    /* optional telemetry: --telemetry <host>:<port> */
    {
        unsigned short def_port = 0;
        if (net_endpoint_default(g_telemetry_host, sizeof(g_telemetry_host),
                                 &def_port))
            g_telemetry_port = def_port;
    }
    parse_telemetry_args(lpCmd);
    net_set_http_mode(g_telemetry_http);
    net_set_https_insecure(g_telemetry_https_insecure);
    /* optional solver credentials: --solver-user/--solver-pass/
     * --solver-config <file>, else MS_SOLVER_USER/MS_SOLVER_PASS */
    leader_setup_solver(lpCmd);

    /* device diagnostics: config dir, persisted flags, machine id, crash
     * filter (installed before any code that could fault) */
    diag_init();

    reset_game(NULL, &g_presets[DIFF_BEGIN], 1);
    g_hwnd = NULL;

    hwnd = CreateWindowA(wc.lpszClassName, "Minesweeper",
                         WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
                         CW_USEDEFAULT, CW_USEDEFAULT, 0, 0,
                         NULL, NULL, hInst, NULL);
    if (!hwnd) return 1;
    g_hwnd = hwnd;
    if (g_game_seed_active) update_window_title(hwnd);

    /* size to the beginner board */
    rc.left = rc.top = 0;
    rc.right = client_w(g);
    rc.bottom = client_h(g);
    AdjustWindowRectEx(&rc, GetWindowLongPtrA(hwnd, GWL_STYLE), TRUE, 0);
    SetWindowPos(hwnd, NULL, 0, 0, rc.right - rc.left, rc.bottom - rc.top,
                 SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);

    ShowWindow(hwnd, nShow);
    UpdateWindow(hwnd);

    cli_port = parse_listen_port(lpCmd);
    if (cli_port > 0) {
        if (!cli_start(cli_port))
            MessageBoxA(hwnd, "Failed to start the scripting server. "
                              "Check the port and try again.",
                        "Minesweeper", MB_OK | MB_ICONWARNING);
    }

    if (g_telemetry_port != 0) {
        net_set_seed_sink(telemetry_seed_sink);
        net_set_notify_hwnd(hwnd);
        net_telemetry_start(g_telemetry_host, g_telemetry_port);
    }

    while (GetMessageA(&msg, NULL, 0, 0) > 0) {
        if (!IsDialogMessageA(hwnd, &msg)) {
            TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }
    }
    cli_stop();
    net_telemetry_stop();
    return (int)msg.wParam;
}
