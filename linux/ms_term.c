/*
 * ms_term.c - terminal frontend for the Linux Minesweeper client.
 *
 * Uses only termios + ANSI escape sequences (no ncurses dependency): raw
 * mode input, keyboard cursor control, and a full-screen redraw of the LEDs,
 * face and board.  Runs on a single main thread that also pumps the core
 * event queue (telemetry seeds, leaderboard, solver-denied notices).
 *
 * MIT License
 */
#include "ms_core.h"
#include "ms_net.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>
#include <unistd.h>
#include <termios.h>
#include <time.h>
#include <sys/select.h>
#include <sys/time.h>

#define TITLE_MAX 96
#define HOF_LINES 12
#define HOF_CHUNK 96

static struct termios g_old_tio;
static int  g_raw = 0;
static int  g_running = 1;

static int  g_crow = 0, g_ccol = 0;        /* board cursor */
static int  g_dirty = 1;
static char g_title[TITLE_MAX];

static char g_msg[128];
static uint64_t g_msg_at = 0;

/* leaderboard overlay (filled from fe_hof_* events) */
static char g_hof[HOF_LINES][HOF_CHUNK];
static int  g_hof_n = 0;
static int  g_hof_done = 0;

/* last-drawn signature to avoid useless redraws */
static char g_sig[1 + MAX_CELLS + 64];
static char g_last_sig[1 + MAX_CELLS + 64];
static int  g_has_sig = 0;

static void term_set_title(const char *title) {
    strncpy(g_title, title, sizeof(g_title) - 1);
    g_title[sizeof(g_title) - 1] = 0;
}

static void term_denied(void) {
    snprintf(g_msg, sizeof(g_msg),
             "solver denied: the simulation server refused a request");
    g_msg_at = ms_now_ms();
    g_dirty = 1;
}

static void term_hof_start(void) {
    g_hof_n = 0;
    g_hof_done = 0;
    g_dirty = 1;
}

static void term_hof_entry(int rank, const char *diff, const char *name,
                           int time_ms, long long ts) {
    char line[HOF_CHUNK];
    (void)ts;
    if (time_ms >= 60000)
        snprintf(line, sizeof(line), "#%-3d %-8s %-16s %d:%02d",
                 rank, diff, name, time_ms / 60000, (time_ms / 1000) % 60);
    else
        snprintf(line, sizeof(line), "#%-3d %-8s %-16s %.1fs",
                 rank, diff, name, time_ms / 1000.0);
    if (g_hof_n < HOF_LINES) {
        size_t n = strlen(line);
        if (n >= HOF_CHUNK) n = HOF_CHUNK - 1;
        memcpy(g_hof[g_hof_n], line, n);
        g_hof[g_hof_n][n] = 0;
        g_hof_n++;
    }
    g_dirty = 1;
}

static void term_hof_end(void) {
    g_hof_done = 1;
    g_dirty = 1;
}

/* ------------------------------------------------------------------ */
/* raw terminal mode                                                   */
/* ------------------------------------------------------------------ */
static void term_raw_on(void) {
    if (g_raw) return;
    if (tcgetattr(STDIN_FILENO, &g_old_tio) != 0) return;
    {
        struct termios raw = g_old_tio;
        raw.c_lflag &= ~(ICANON | ECHO | ISIG);
        raw.c_iflag &= ~(IXON | ICRNL | BRKINT | INPCK | ISTRIP);
        raw.c_oflag &= ~(OPOST);
        raw.c_cc[VMIN] = 0;
        raw.c_cc[VTIME] = 0;
        if (tcsetattr(STDIN_FILENO, TCSANOW, &raw) == 0) g_raw = 1;
    }
}

static void term_raw_off(void) {
    if (!g_raw) return;
    tcsetattr(STDIN_FILENO, TCSANOW, &g_old_tio);
    g_raw = 0;
}

static void term_cleanup(void) {
    term_raw_off();
    printf("\x1b[0m\x1b[?25h\x1b[0J");
    fflush(stdout);
}

static void term_hide_cursor(void) { printf("\x1b[?25l"); }
static void term_show_cursor(void) { printf("\x1b[?25h"); }

/* ------------------------------------------------------------------ */
/* drawing                                                             */
/* ------------------------------------------------------------------ */
static char face_char(int st) {
    switch (st) {
    case 1:  return 'O';
    case 2:  return ')';
    case 3:  return 'x';
    default: return ':';
    }
}

/* bare SGR number color codes (1 blue, 2 green, 3/5 red, 4 navy, 6 teal, 8 gray) */
static const char *num_color(int n) {
    switch (n) {
    case 1: return "34";
    case 2: return "32";
    case 3: return "31";
    case 4: return "34";
    case 5: return "31";
    case 6: return "36";
    case 7: return "37";
    case 8: return "37";
    default: return "";
    }
}

/* Append the board cell at (r,c) to out, honoring the cursor highlight. */
static void term_cell(char *out, size_t cap, size_t *len, int r, int c) {
    Game *g = game_state();
    int i = r * g->cols + c;
    int cur = (r == g_crow && c == g_ccol);
    char tmp[64];

    if (cur) {
        char ch = ' ';
        const char *color = "";
        if (!g->revealed[i]) {
            if (g->mark[i] == 1)      { ch = 'F'; color = ";31"; }
            else if (g->mark[i] == 2) { ch = '?'; color = ";34"; }
            else                      { ch = '.'; }
        } else if (g->mine[i]) {
            ch = '*'; color = ";31";
        } else if (g->adj[i] > 0) {
            ch = (char)('0' + g->adj[i]);
            if (num_color(g->adj[i])[0])
                snprintf(tmp, sizeof(tmp), "\x1b[7;%sm%c\x1b[0m",
                         num_color(g->adj[i]), ch);
            else
                snprintf(tmp, sizeof(tmp), "\x1b[7m%c\x1b[0m", ch);
            snprintf(out + *len, cap - *len, "%s", tmp);
            *len += (size_t)strlen(out + *len);
            return;
        }
        snprintf(tmp, sizeof(tmp), "\x1b[7%s%c\x1b[0m", color, ch);
        snprintf(out + *len, cap - *len, "%s", tmp);
        *len += (size_t)strlen(out + *len);
        return;
    }

    if (!g->revealed[i]) {
        if (g->mark[i] == 1)
            snprintf(tmp, sizeof(tmp), "\x1b[31mF\x1b[0m");
        else if (g->mark[i] == 2)
            snprintf(tmp, sizeof(tmp), "\x1b[34m?\x1b[0m");
        else
            snprintf(tmp, sizeof(tmp), ".");
    } else if (g->mine[i]) {
        snprintf(tmp, sizeof(tmp), "\x1b[31m*\x1b[0m");
    } else if (g->adj[i] > 0) {
        snprintf(tmp, sizeof(tmp), "\x1b[%sm%c\x1b[0m",
                 num_color(g->adj[i]), (char)('0' + g->adj[i]));
    } else {
        snprintf(tmp, sizeof(tmp), " ");
    }
    snprintf(out + *len, cap - *len, "%s", tmp);
    *len += (size_t)strlen(out + *len);
}

static void term_build_signature(void) {
    Game *g = game_state();
    char *p = g_sig;
    int n = 0, r, c;
    n = snprintf(p, sizeof(g_sig), "%d:%d:%d:%d:%d:%llu;",
                 g->mines - g->flags, g->time, game_face_state(), g->over,
                 game_seed_active() ? 0 : 1, (unsigned long long)game_seed_val());
    p += n;
    for (r = 0; r < g->rows; r++)
        for (c = 0; c < g->cols; c++) {
            int i = r * g->cols + c;
            *p++ = (char)(g->revealed[i] ? '1' : '0');
            *p++ = (char)('0' + g->adj[i]);
            *p++ = (char)('0' + g->mark[i]);
            if (g->revealed[i] && g->mine[i]) *p++ = 'M';
        }
    *p = 0;
}

static void term_redraw(void) {
    Game *g = game_state();
    char buf[16384];
    size_t len = 0;
    int r, c;

    buf[0] = 0;
    term_build_signature();
    if (g_has_sig && strcmp(g_sig, g_last_sig) == 0 && !g_dirty) return;

    term_hide_cursor();
    snprintf(buf + len, sizeof(buf) - len, "\x1b[H");
    len = strlen(buf);

    /* LEDs + face */
    snprintf(buf + len, sizeof(buf) - len,
             "\x1b[1;37m MINES \x1b[36m%3d\x1b[0m  %c  "
             "\x1b[1;37mTIME \x1b[36m%3d\x1b[0m  %s\r\n",
             g->mines - g->flags, face_char(game_face_state()), g->time,
             g_title[0] ? g_title : "");
    len = strlen(buf);

    /* grid */
    for (r = 0; r < g->rows; r++) {
        snprintf(buf + len, sizeof(buf) - len, "  ");
        len = strlen(buf);
        for (c = 0; c < g->cols; c++) {
            term_cell(buf, sizeof(buf), &len, r, c);
            if (c + 1 < g->cols) {
                snprintf(buf + len, sizeof(buf) - len, " ");
                len = strlen(buf);
            }
        }
        snprintf(buf + len, sizeof(buf) - len, "\r\n");
        len = strlen(buf);
    }

    /* leaderboard overlay */
    if (g_hof_n > 0) {
        snprintf(buf + len, sizeof(buf) - len,
                 "\x1b[1;37m -- Hall of Fame%s --\x1b[0m\r\n",
                 g_hof_done ? "" : " (loading...)");
        len = strlen(buf);
        for (r = 0; r < g_hof_n; r++) {
            snprintf(buf + len, sizeof(buf) - len, "  %s\r\n", g_hof[r]);
            len = strlen(buf);
        }
    }

    /* status + transient message */
    {
        NetStats st;
        int seeded = game_seed_active();
        int showmsg = g_msg[0] && ms_now_ms() - g_msg_at < 6000;
        net_get_stats(&st);
        snprintf(buf + len, sizeof(buf) - len,
                 " %-10s seed=%s %s%s%s%s%s",
                 g_diff_names[g->diff >= 0 && g->diff < DIFF_COUNT
                                  ? g->diff : DIFF_CUSTOM],
                 seeded ? "on" : "off",
                 net_telemetry_active()
                     ? (st.connected ? "net=connected" : "net=connecting")
                     : "net=off",
                 g->paused ? " paused" : "",
                 g->over == 1 ? " WON!" : (g->over == -1 ? " LOST" : ""),
                 showmsg ? " | " : "",
                 showmsg ? g_msg : "");
        len = strlen(buf);
        snprintf(buf + len, sizeof(buf) - len, "\r\n");
        len = strlen(buf);
    }

    /* help */
    snprintf(buf + len, sizeof(buf) - len,
             " \x1b[1;37mArrows/WASD\x1b[0m move  \x1b[1;37mSpace\x1b[0m "
             "click  \x1b[1;37mF\x1b[0m flag  \x1b[1;37mC\x1b[0m chord  "
             "\x1b[1;37mN\x1b[0m new  \x1b[1;37m1/2/3\x1b[0m difficulty  "
             "\x1b[1;37mQ\x1b[0m quit\r\n");
    len = strlen(buf);

    snprintf(buf + len, sizeof(buf) - len, "\x1b[0m");
    len = strlen(buf);

    if (fwrite(buf, 1, len, stdout) != len) { /* ignore */ }
    fflush(stdout);

    /* place the hardware cursor on the board cursor cell */
    {
        int row = 3 + g_crow;               /* line 0 header, line 1 grid  */
        int col = 2 + g_ccol * 2;
        printf("\x1b[%d;%dH", row, col);
        fflush(stdout);
        term_show_cursor();
    }
    strcpy(g_last_sig, g_sig);
    g_has_sig = 1;
    g_dirty = 0;
}

static void term_redraw_now(void) {
    g_dirty = 1;
    term_redraw();
}

/* ------------------------------------------------------------------ */
/* input                                                               */
/* ------------------------------------------------------------------ */
static void term_clamp_cursor(void) {
    Game *g = game_state();
    if (g_crow >= g->rows) g_crow = g->rows - 1;
    if (g_ccol >= g->cols) g_ccol = g->cols - 1;
    if (g_crow < 0) g_crow = 0;
    if (g_ccol < 0) g_ccol = 0;
}

static void term_status(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(g_msg, sizeof(g_msg), fmt, ap);
    va_end(ap);
    g_msg_at = ms_now_ms();
    g_dirty = 1;
}

static void term_handle_key(int key) {
    Game *g = game_state();
    switch (key) {
    case 0x1b: /* treated as escape below; see term_read_keys */
        return;
    case 'q':
    case 'Q':
        g_running = 0;
        return;
    case 'n':
    case 'N':
        game_new_diff(g->diff);
        term_clamp_cursor();
        term_status("new game");
        return;
    case '1':
        game_new_diff(DIFF_BEGIN);
        term_clamp_cursor();
        return;
    case '2':
        game_new_diff(DIFF_INTERMEDIATE);
        term_clamp_cursor();
        return;
    case '3':
        game_new_diff(DIFF_EXPERT);
        term_clamp_cursor();
        return;
    case ' ':
    case '\r':
    case '\n':
        game_click(g_crow, g_ccol);
        return;
    case 'f':
    case 'F':
        game_mark(g_crow, g_ccol);
        return;
    case 'c':
    case 'C':
        game_chord_at(g_crow, g_ccol);
        return;
    case 'm':
    case 'M':
        g->marks_enabled = !g->marks_enabled;
        term_status(g->marks_enabled ? "question marks on"
                                     : "question marks off");
        return;
    case 'p':
    case 'P':
        g->paused = !g->paused;
        term_status(g->paused ? "paused" : "resumed");
        return;
    case 'h':
    case 'a':
    case 'A':
        if (g_ccol > 0) g_ccol--;
        g_dirty = 1;
        return;
    case 'l':
    case 'd':
    case 'D':
        if (g_ccol < g->cols - 1) g_ccol++;
        g_dirty = 1;
        return;
    case 'k':
    case 'w':
    case 'W':
        if (g_crow > 0) g_crow--;
        g_dirty = 1;
        return;
    case 'j':
    case 's':
    case 'S':
        if (g_crow < g->rows - 1) g_crow++;
        g_dirty = 1;
        return;
    default:
        break;
    }
}

/* Non-blocking read of pending keys from stdin.  Handles ANSI arrow
 * sequences (ESC [ A/B/C/D). */
static void term_read_keys(void) {
    unsigned char c;
    while (read(STDIN_FILENO, &c, 1) > 0) {
        if (c == 0x1b) {
            unsigned char seq[2];
            int n = (int)read(STDIN_FILENO, &seq[0], 1);
            if (n == 1 && seq[0] == '[') {
                unsigned char d;
                if (read(STDIN_FILENO, &d, 1) != 1) continue;
                switch (d) {
                case 'A': if (g_crow > 0) g_crow--; break;
                case 'B': if (g_crow < game_state()->rows - 1) g_crow++; break;
                case 'C': if (g_ccol < game_state()->cols - 1) g_ccol++; break;
                case 'D': if (g_ccol > 0) g_ccol--; break;
                default: break;
                }
                g_dirty = 1;
            } else if (n == 1) {
                term_handle_key(seq[0]);   /* Alt-key etc.: pass through */
            }
        } else {
            term_handle_key(c);
            if (!g_running) break;
        }
    }
}

/* ------------------------------------------------------------------ */
/* main loop                                                           */
/* ------------------------------------------------------------------ */
static void term_run(void) {
    uint64_t last_tick = ms_now_ms();
    term_raw_on();
    term_show_cursor();
    g_dirty = 1;

    while (g_running) {
        fd_set rf;
        struct timeval tv;
        int r;

        FD_ZERO(&rf);
        FD_SET(STDIN_FILENO, &rf);
        tv.tv_sec = 0;
        tv.tv_usec = 100000;               /* 100 ms poll */

        r = select(STDIN_FILENO + 1, &rf, NULL, NULL, &tv);
        if (r > 0 && FD_ISSET(STDIN_FILENO, &rf)) {
            term_read_keys();
            if (!g_running) break;
        }

        ms_loop_pump();

        {
            uint64_t now = ms_now_ms();
            if (now - last_tick >= 1000) {
                last_tick = now;
                game_tick();
            }
        }

        term_redraw();
    }
}

/* ------------------------------------------------------------------ */
/* public                                                              */
/* ------------------------------------------------------------------ */
int term_main(void) {
    if (!isatty(STDIN_FILENO) || !isatty(STDOUT_FILENO)) {
        fprintf(stderr, "terminal frontend requires an interactive tty; "
                        "use --headless for scripted use\n");
        return 1;
    }
    atexit(term_cleanup);
    fe_repaint = term_redraw_now;
    fe_set_title = term_set_title;
    fe_hof_start = term_hof_start;
    fe_hof_entry = term_hof_entry;
    fe_hof_end = term_hof_end;
    fe_denied = term_denied;

    ms_refresh_title();

    term_run();
    return 0;
}
