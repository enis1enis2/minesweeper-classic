/*
 * ms_x11.c - X11 GUI frontend for the Linux Minesweeper client.
 *
 * Plain Xlib (no toolkit): renders the LED header, face button and the board
 * into an off-screen pixmap that is blitted on repaint.  Pointer press/release
 * are mapped onto the core's game_pointer_down/up so left+right chord and the
 * pressed-face "new game" behaviour match the Win32 client.
 *
 * MIT License
 */
#include "ms_core.h"
#include "ms_net.h"

#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/keysym.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/select.h>
#include <sys/time.h>

#define CELL     24
#define MARGIN   6
#define HDR_H    (CELL + 14)
#define FONT_H   15
#define HOF_PANEL_W 200
#define HOF_LINES 12
#define HOF_CHUNK 96
#define MAX_PRES 32

typedef struct {
    int region;
    int cell;
    int button;
    int active;
} Press;

static Display *g_dpy;
static int      g_screen;
static Window   g_win;
static Pixmap   g_pm;
static GC       g_gc;
static int      g_win_w, g_win_h;
static XFontStruct *g_font;
static long     g_col_bg, g_col_lite, g_col_dark, g_col_face, g_col_led;
static long     g_col_num[9];
static int      g_running = 1;
static int      g_xdirty = 1;

static Press    g_press;
static char     g_hof[HOF_LINES][HOF_CHUNK];
static int      g_hof_n = 0;
static int      g_hof_done = 0;
static char     g_msg[128];

/* ------------------------------------------------------------------ */
/* color + font setup                                                  */
/* ------------------------------------------------------------------ */
static long x11_color(unsigned long rgb) {
    XColor c;
    c.red   = (rgb >> 16) & 0xffff;
    c.green = (rgb >> 8)  & 0xffff;
    c.blue  = (rgb >> 0)  & 0xffff;
    c.flags = DoRed | DoGreen | DoBlue;
    if (XAllocColor(g_dpy, DefaultColormap(g_dpy, g_screen), &c))
        return c.pixel;
    return BlackPixel(g_dpy, g_screen);
}

static void x11_setup_colors(void) {
    g_col_bg   = x11_color(0xc0c0c0);
    g_col_lite = x11_color(0xffffff);
    g_col_dark = x11_color(0x808080);
    g_col_face = x11_color(0xd0d0d0);
    g_col_led  = x11_color(0x000000);
    g_col_num[1] = x11_color(0x0000ff);
    g_col_num[2] = x11_color(0x008000);
    g_col_num[3] = x11_color(0xff0000);
    g_col_num[4] = x11_color(0x000080);
    g_col_num[5] = x11_color(0x800000);
    g_col_num[6] = x11_color(0x008080);
    g_col_num[7] = x11_color(0x000000);
    g_col_num[8] = x11_color(0x808080);
}

/* ------------------------------------------------------------------ */
/* geometry                                                            */
/* ------------------------------------------------------------------ */
static void x11_layout(Game *g, int *off_x, int *off_y, int *ww, int *wh,
                       int *face_x, int *face_y, int *face_w, int *face_h) {
    *off_x = MARGIN;
    *off_y = MARGIN + HDR_H;
    *ww = MARGIN * 2 + g->cols * CELL;
    *wh = MARGIN + HDR_H + g->rows * CELL + MARGIN;
    if (g_hof_n > 0) *ww += HOF_PANEL_W;
    *face_w = CELL + 8;
    *face_h = CELL;
    *face_x = MARGIN + (g->cols * CELL - *face_w) / 2;
    *face_y = MARGIN + (HDR_H - *face_h) / 2;
}

static void x11_sync_window(void) {
    Game *g = game_state();
    int ox, oy, ww, wh, fx, fy, fw, fh;
    x11_layout(g, &ox, &oy, &ww, &wh, &fx, &fy, &fw, &fh);
    if (ww != g_win_w || wh != g_win_h) {
        if (g_pm) XFreePixmap(g_dpy, g_pm);
        g_pm = XCreatePixmap(g_dpy, g_win, ww, wh,
                             DefaultDepth(g_dpy, g_screen));
        XResizeWindow(g_dpy, g_win, ww, wh);
        g_win_w = ww;
        g_win_h = wh;
    }
}

/* ------------------------------------------------------------------ */
/* drawing primitives                                                  */
/* ------------------------------------------------------------------ */
static void x11_raised(int x, int y, int w, int h, int sunken) {
    XSetForeground(g_dpy, g_gc, g_col_face);
    XFillRectangle(g_dpy, g_pm, g_gc, x, y, w, h);
    if (sunken) {
        XSetForeground(g_dpy, g_gc, g_col_dark);
        XDrawLine(g_dpy, g_pm, g_gc, x, y, x + w - 2, y);
        XDrawLine(g_dpy, g_pm, g_gc, x, y, x, y + h - 2);
        XSetForeground(g_dpy, g_gc, g_col_lite);
        XDrawLine(g_dpy, g_pm, g_gc, x, y + h - 1, x + w - 1, y + h - 1);
        XDrawLine(g_dpy, g_pm, g_gc, x + w - 1, y, x + w - 1, y + h - 1);
    } else {
        XSetForeground(g_dpy, g_gc, g_col_lite);
        XDrawLine(g_dpy, g_pm, g_gc, x, y, x + w - 2, y);
        XDrawLine(g_dpy, g_pm, g_gc, x, y, x, y + h - 2);
        XSetForeground(g_dpy, g_gc, g_col_dark);
        XDrawLine(g_dpy, g_pm, g_gc, x, y + h - 1, x + w - 1, y + h - 1);
        XDrawLine(g_dpy, g_pm, g_gc, x + w - 1, y, x + w - 1, y + h - 1);
    }
}

static void x11_led(int x, int y, int w, int h, int value) {
    char txt[8];
    XSetForeground(g_dpy, g_gc, g_col_led);
    XFillRectangle(g_dpy, g_pm, g_gc, x, y, w, h);
    XSetForeground(g_dpy, g_gc, g_col_num[3]);
    snprintf(txt, sizeof(txt), "%3d", value);
    XDrawString(g_dpy, g_pm, g_gc, x + 6, y + h - 4, txt, 3);
}

static void x11_text(int x, int y, const char *s, long color) {
    XSetForeground(g_dpy, g_gc, color);
    XDrawString(g_dpy, g_pm, g_gc, x, y, s, (int)strlen(s));
}

static void x11_center1(int x, int y, int w, int h, char ch, long color) {
    int cw = XTextWidth(g_font, &ch, 1);
    XSetForeground(g_dpy, g_gc, color);
    XDrawString(g_dpy, g_pm, g_gc, x + (w - cw) / 2,
                y + (h - FONT_H) / 2 + FONT_H - 3, &ch, 1);
}

static void x11_face(int fx, int fy, int fw, int fh, int st) {
    int cx = fx + fw / 2, cy = fy + fh / 2;
    x11_raised(fx, fy, fw, fh, g_press.active && g_press.region == PTR_FACE);
    XSetForeground(g_dpy, g_gc, g_col_num[7]);
    if (st == 3) {
        XDrawLine(g_dpy, g_pm, g_gc, cx - 8, cy - 6, cx - 3, cy - 1);
        XDrawLine(g_dpy, g_pm, g_gc, cx - 3, cy - 6, cx - 8, cy - 1);
        XDrawLine(g_dpy, g_pm, g_gc, cx + 3, cy - 6, cx + 8, cy - 1);
        XDrawLine(g_dpy, g_pm, g_gc, cx + 8, cy - 6, cx + 3, cy - 1);
        XDrawArc(g_dpy, g_pm, g_gc, cx - 7, cy, 14, 10, 180 * 64, 180 * 64);
    } else if (st == 1) {
        XFillRectangle(g_dpy, g_pm, g_gc, cx - 8, cy - 6, 5, 5);
        XFillRectangle(g_dpy, g_pm, g_gc, cx + 3, cy - 6, 5, 5);
        XFillArc(g_dpy, g_pm, g_gc, cx - 5, cy - 4, 10, 10, 0, 360 * 64);
    } else if (st == 2) {
        XFillRectangle(g_dpy, g_pm, g_gc, cx - 9, cy - 6, 18, 4);
        XDrawArc(g_dpy, g_pm, g_gc, cx - 7, cy - 8, 7, 8, 180 * 64, 180 * 64);
        XDrawArc(g_dpy, g_pm, g_gc, cx + 1, cy - 8, 7, 8, 180 * 64, 180 * 64);
        XDrawArc(g_dpy, g_pm, g_gc, cx - 7, cy + 1, 14, 10, 0, 180 * 64);
    } else {
        XFillRectangle(g_dpy, g_pm, g_gc, cx - 8, cy - 6, 4, 6);
        XFillRectangle(g_dpy, g_pm, g_gc, cx + 4, cy - 6, 4, 6);
        XDrawArc(g_dpy, g_pm, g_gc, cx - 6, cy - 1, 12, 9, 0, 180 * 64);
    }
}

static void x11_cell(int ox, int oy, int r, int c) {
    Game *g = game_state();
    int i = r * g->cols + c;
    int x = ox + c * CELL, y = oy + r * CELL;

    if (!g->revealed[i]) {
        int press = g_press.active && g_press.region == PTR_GRID &&
                    g_press.cell == i && !g->revealed[i];
        x11_raised(x, y, CELL, CELL, press);
        if (g->mark[i] == 1)
            x11_center1(x, y, CELL, CELL, 'F', g_col_num[3]);
        else if (g->mark[i] == 2)
            x11_center1(x, y, CELL, CELL, '?', g_col_num[1]);
        return;
    }
    XSetForeground(g_dpy, g_gc, g_col_bg);
    XFillRectangle(g_dpy, g_pm, g_gc, x, y, CELL, CELL);
    if (g->mine[i]) {
        int cc = x + CELL / 2, cy = y + CELL / 2;
        XSetForeground(g_dpy, g_gc, g_col_num[3]);
        XFillArc(g_dpy, g_pm, g_gc, cc - 7, cy - 7, 14, 14, 0, 360 * 64);
        XSetForeground(g_dpy, g_gc, g_col_led);
        XDrawLine(g_dpy, g_pm, g_gc, cc - 7, cy, cc + 7, cy);
        XDrawLine(g_dpy, g_pm, g_gc, cc, cy - 7, cc, cy + 7);
        XDrawLine(g_dpy, g_pm, g_gc, cc - 5, cy - 5, cc + 5, cy + 5);
        XDrawLine(g_dpy, g_pm, g_gc, cc + 5, cy - 5, cc - 5, cy + 5);
    } else if (g->adj[i] > 0) {
        x11_center1(x, y, CELL, CELL, (char)('0' + g->adj[i]), g_col_num[g->adj[i]]);
    }
}

/* ------------------------------------------------------------------ */
/* full render                                                         */
/* ------------------------------------------------------------------ */
static void x11_render(void) {
    Game *g = game_state();
    int ox, oy, ww, wh, fx, fy, fw, fh, r, c;

    x11_sync_window();

    x11_layout(g, &ox, &oy, &ww, &wh, &fx, &fy, &fw, &fh);

    XSetForeground(g_dpy, g_gc, g_col_bg);
    XFillRectangle(g_dpy, g_pm, g_gc, 0, 0, ww, wh);

    /* LEDs */
    x11_led(MARGIN, MARGIN + 2, 58, CELL - 4, g->mines - g->flags);
    x11_led(ww - MARGIN - 58, MARGIN + 2, 58, CELL - 4, g->time);

    /* face */
    x11_face(fx, fy, fw, fh, game_face_state());

    /* board */
    for (r = 0; r < g->rows; r++)
        for (c = 0; c < g->cols; c++)
            x11_cell(ox, oy, r, c);

    /* Hall of Fame panel */
    if (g_hof_n > 0) {
        int px = MARGIN * 2 + g->cols * CELL;
        int py = MARGIN;
        x11_text(px, py + 14, "Hall of Fame", g_col_num[1]);
        py += 10;
        for (r = 0; r < g_hof_n; r++) {
            x11_text(px, py + FONT_H, g_hof[r], g_col_num[7]);
            py += FONT_H + 2;
        }
        if (!g_hof_done)
            x11_text(px, py + FONT_H, "loading...", g_col_dark);
        else
            x11_text(px, py + FONT_H, "done", g_col_dark);
        py += FONT_H + 12;
        if (g_msg[0]) {
            XSetForeground(g_dpy, g_gc, g_col_num[3]);
            XDrawString(g_dpy, g_pm, g_gc, px, py + FONT_H, g_msg,
                        (int)strlen(g_msg));
        }
    }

    XCopyArea(g_dpy, g_pm, g_win, g_gc, 0, 0, ww, wh, 0, 0);
    XFlush(g_dpy);
    g_xdirty = 0;
}

static void x11_repaint(void) {
    g_xdirty = 1;
}

static void x11_set_title(const char *title) {
    if (!g_dpy || !g_win) return;
    XStoreName(g_dpy, g_win, title);
    XFlush(g_dpy);
}

static void x11_denied(void) {
    snprintf(g_msg, sizeof(g_msg),
             "solver denied: the simulation server refused a request");
    g_xdirty = 1;
}

static void x11_hof_start(void) {
    g_hof_n = 0;
    g_hof_done = 0;
    g_xdirty = 1;
}

static void x11_hof_entry(int rank, const char *diff, const char *name,
                          int time_ms, long long ts) {
    char line[HOF_CHUNK];
    (void)ts;
    if (time_ms >= 60000)
        snprintf(line, sizeof(line), "#%-3d %-8s %s %d:%02d",
                 rank, diff, name, time_ms / 60000, (time_ms / 1000) % 60);
    else
        snprintf(line, sizeof(line), "#%-3d %-8s %s %.1fs",
                 rank, diff, name, time_ms / 1000.0);
    if (g_hof_n < HOF_LINES) {
        size_t n = strlen(line);
        if (n >= HOF_CHUNK) n = HOF_CHUNK - 1;
        memcpy(g_hof[g_hof_n], line, n);
        g_hof[g_hof_n][n] = 0;
        g_hof_n++;
    }
    g_xdirty = 1;
}

static void x11_hof_end(void) {
    g_hof_done = 1;
    g_xdirty = 1;
}

/* ------------------------------------------------------------------ */
/* input                                                               */
/* ------------------------------------------------------------------ */
static void x11_hit_test(int x, int y, int *region, int *cell) {
    Game *g = game_state();
    int ox, oy, ww, wh, fx, fy, fw, fh;
    x11_layout(g, &ox, &oy, &ww, &wh, &fx, &fy, &fw, &fh);
    if (x >= fx && x < fx + fw && y >= fy && y < fy + fh) {
        *region = PTR_FACE;
        *cell = -1;
        return;
    }
    *region = PTR_GRID;
    if (x < ox || y < oy) { *cell = -1; return; }
    {
        int c = (x - ox) / CELL, r = (y - oy) / CELL;
        if (r < 0 || c < 0 || r >= g->rows || c >= g->cols) { *cell = -1; return; }
        *cell = r * g->cols + c;
    }
}

static int x11_button_code(unsigned int b) {
    if (b == Button1) return 0;   /* left */
    if (b == Button2) return 1;   /* middle acts as right */
    if (b == Button3) return 1;   /* right */
    return -1;
}

static void x11_event(XEvent *ev) {
    switch (ev->type) {
    case Expose:
        if (ev->xexpose.count == 0) x11_render();
        break;
    case ConfigureNotify:
        g_xdirty = 1;
        break;
    case ButtonPress: {
        int code = x11_button_code(ev->xbutton.button);
        int region, cell;
        if (code < 0) break;
        x11_hit_test(ev->xbutton.x, ev->xbutton.y, &region, &cell);
        g_press.region = region;
        g_press.cell = cell;
        g_press.button = code;
        g_press.active = 1;
        game_pointer_down(region, cell, code);
        x11_render();
        break;
    }
    case ButtonRelease: {
        int code = x11_button_code(ev->xbutton.button);
        int region, cell;
        if (code < 0) break;
        x11_hit_test(ev->xbutton.x, ev->xbutton.y, &region, &cell);
        g_press.active = 0;
        game_pointer_up(region, cell, code);
        x11_render();
        break;
    }
    case MotionNotify:
        if (g_press.active && g_press.region == PTR_GRID) {
            int region, cell;
            x11_hit_test(ev->xmotion.x, ev->xmotion.y, &region, &cell);
            if (cell != g_press.cell) {
                game_pointer_cancel();
                g_press.active = 0;
            }
        }
        break;
    case KeyPress: {
        KeySym ks = XLookupKeysym(&ev->xkey, 0);
        Game *g = game_state();
        switch (ks) {
        case XK_1: game_new_diff(DIFF_BEGIN); break;
        case XK_2: game_new_diff(DIFF_INTERMEDIATE); break;
        case XK_3: game_new_diff(DIFF_EXPERT); break;
        case XK_n:
        case XK_N: game_new_diff(g->diff); break;
        case XK_p:
        case XK_P: g->paused = !g->paused; break;
        case XK_q:
        case XK_Q: g_running = 0; break;
        default: break;
        }
        break;
    }
    case ClientMessage:
        if (ev->xclient.data.l[0] == (long)XInternAtom(g_dpy, "WM_DELETE_WINDOW", False))
            g_running = 0;
        break;
    default:
        break;
    }
}

/* ------------------------------------------------------------------ */
/* main loop                                                           */
/* ------------------------------------------------------------------ */
static void x11_run(void) {
    uint64_t last_tick = ms_now_ms();
    XSelectInput(g_dpy, g_win,
                 ExposureMask | ButtonPressMask | ButtonReleaseMask |
                 PointerMotionMask | KeyPressMask | StructureNotifyMask);
    x11_render();

    while (g_running) {
        fd_set rf;
        struct timeval tv;
        int fd = ConnectionNumber(g_dpy);

        FD_ZERO(&rf);
        FD_SET(fd, &rf);
        tv.tv_sec = 0;
        tv.tv_usec = 100000;

        if (select(fd + 1, &rf, NULL, NULL, &tv) > 0 && FD_ISSET(fd, &rf)) {
            while (XPending(g_dpy)) {
                XEvent ev;
                XNextEvent(g_dpy, &ev);
                x11_event(&ev);
                if (!g_running) break;
            }
        }

        ms_loop_pump();

        {
            uint64_t now = ms_now_ms();
            if (now - last_tick >= 1000) {
                last_tick = now;
                game_tick();
                g_xdirty = 1;
            }
        }

        if (g_xdirty) x11_render();
    }
}

/* ------------------------------------------------------------------ */
/* public                                                              */
/* ------------------------------------------------------------------ */
int x11_main(void) {
    XSetWindowAttributes attrs;
    unsigned long mask;

    g_dpy = XOpenDisplay(NULL);
    if (!g_dpy) {
        fprintf(stderr, "x11: cannot open display\n");
        return 1;
    }
    g_screen = DefaultScreen(g_dpy);

    g_font = XLoadQueryFont(g_dpy, "9x15bold");
    if (!g_font) g_font = XLoadQueryFont(g_dpy, "fixed");
    if (!g_font) {
        fprintf(stderr, "x11: no usable font\n");
        return 1;
    }

    x11_setup_colors();

    {
        Game *g = game_state();
        int ox, oy, ww, wh, fx, fy, fw, fh;
        x11_layout(g, &ox, &oy, &ww, &wh, &fx, &fy, &fw, &fh);
        g_win_w = ww;
        g_win_h = wh;
    }

    attrs.event_mask = ExposureMask;
    attrs.background_pixel = g_col_bg;
    mask = CWBackPixel | CWEventMask;
    g_win = XCreateWindow(g_dpy, DefaultRootWindow(g_dpy),
                          0, 0, g_win_w, g_win_h, 0,
                          CopyFromParent, InputOutput, CopyFromParent, mask,
                          &attrs);
    XStoreName(g_dpy, g_win, "Minesweeper");
    XSetIconName(g_dpy, g_win, "Minesweeper");
    XMapWindow(g_dpy, g_win);

    {
        Atom wm_del = XInternAtom(g_dpy, "WM_DELETE_WINDOW", False);
        XSetWMProtocols(g_dpy, g_win, &wm_del, 1);
    }

    g_gc = XCreateGC(g_dpy, g_win, 0, NULL);
    XSetFont(g_dpy, g_gc, g_font->fid);
    g_pm = XCreatePixmap(g_dpy, g_win, g_win_w, g_win_h,
                         DefaultDepth(g_dpy, g_screen));

    fe_repaint = x11_repaint;
    fe_set_title = x11_set_title;
    fe_hof_start = x11_hof_start;
    fe_hof_entry = x11_hof_entry;
    fe_hof_end = x11_hof_end;
    fe_denied = x11_denied;

    ms_refresh_title();

    x11_run();

    if (g_pm) XFreePixmap(g_dpy, g_pm);
    if (g_gc) XFreeGC(g_dpy, g_gc);
    XDestroyWindow(g_dpy, g_win);
    XCloseDisplay(g_dpy);
    return 0;
}
