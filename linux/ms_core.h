/*
 * ms_core.h - platform-independent core for the Linux Minesweeper client.
 *
 * This is the POSIX port of the game logic, seed system, metrics, leaderboard
 * persistence, solver credentials and the CLI scripting server from the Win32
 * sources (src/minesweeper.c, src/leader.c).  Both the terminal frontend
 * (ms_term.c), the X11 frontend (ms_x11.c) and the headless mode
 * (ms_main.c) drive this core.
 *
 * MIT License
 */
#ifndef MS_CORE_H
#define MS_CORE_H

#include <stdint.h>
#include <stddef.h>
#include <pthread.h>
#include <strings.h>

#define stricmp  strcasecmp
#define strnicmp strncasecmp

#define MAX_ROWS 30
#define MAX_COLS 30
#define MAX_CELLS (MAX_ROWS * MAX_COLS)
#define CUSTOM_SEED_INPUT_MAX 64
#define LEADER_NAME_MAX 16
#define LEADER_HOF_MAX 50

enum { DIFF_BEGIN, DIFF_INTERMEDIATE, DIFF_EXPERT, DIFF_CUSTOM, DIFF_COUNT };
enum { SEED_OFF = 0, SEED_NORMAL = 1, SEED_CUSTOM = 2 };

typedef struct {
    int rows, cols, mines;
    int diff;
} Difficulty;

extern const Difficulty g_presets[3];
extern const char *const g_diff_names[DIFF_COUNT];

typedef struct {
    int rows, cols, mines;
    unsigned char *mine;      /* 1 = mine */
    unsigned char *adj;       /* adjacent mine count 0..8 */
    unsigned char *revealed;
    unsigned char *mark;      /* 0 none, 1 flag, 2 question */
    int flags;
    int opened;
    int started;              /* first click done */
    int over;                 /* -1 lost, 0 playing, 1 won */
    int time;
    int marks_enabled;
    int paused;
    int diff;
    uint64_t rng;
} Game;

typedef struct {
    int  mode;                /* SEED_OFF / SEED_NORMAL / SEED_CUSTOM */
    char value[CUSTOM_SEED_INPUT_MAX + 1];
} SeedSlot;

/* ------------------------------------------------------------------ */
/* UI-thread marshalling events.  The telemetry thread and the CLI      */
/* server thread push events here; the frontend loop drains them on the */
/* main thread where the game state is safe.                           */
/* ------------------------------------------------------------------ */
typedef enum {
    EV_TELEMETRY_SEED = 1,
    EV_LB_START,
    EV_LB_ENTRY,
    EV_LB_END,
    EV_SOLVER_DENIED,
    EV_CLI
} EventKind;

typedef struct CliCmd CliCmd;

enum {
    CLI_PING, CLI_HELP, CLI_NEW, CLI_CLICK, CLI_FLAG, CLI_CHORD,
    CLI_STATE, CLI_BOARD, CLI_MARKS, CLI_PAUSE, CLI_RESUME,
    CLI_SEED, CLI_REFRESH, CLI_SEEDCUSTOM, CLI_SEEDS,
    CLI_TELEMETRY, CLI_REQSEED, CLI_REQBATCH, CLI_SCENARIOS, CLI_QUIT
};

struct CliCmd {
    int op;
    int a, b, c;
    int have_u;
    int rows, cols, mines;
    uint64_t u;
    char *s;                      /* string arg (points into the CLI line) */
    char *reply;                  /* malloc'd reply, built by the main thread */
    pthread_mutex_t lock;
    pthread_cond_t  cv;
    int ready;
};

typedef struct {
    EventKind kind;
    int diff;
    uint64_t seed;
    int rank;
    char diffname[16];
    char name[17];
    int time_ms;
    long long ts;
    CliCmd *cli;
} MsEvent;

void ms_events_init(void);
int  ms_event_poll(MsEvent *e);
void ms_event_push(const MsEvent *e);
void ms_event_push_cli(CliCmd *cmd);

/* ------------------------------------------------------------------ */
/* Frontend hooks.  Set by the active frontend; all may be NULL.       */
/* ------------------------------------------------------------------ */
extern void (*fe_repaint)(void);
extern void (*fe_set_title)(const char *title);
extern void (*fe_hof_start)(void);
extern void (*fe_hof_entry)(int rank, const char *diff, const char *name,
                            int time_ms, long long ts);
extern void (*fe_hof_end)(void);
extern void (*fe_denied)(void);

/* ------------------------------------------------------------------ */
/* Game.  All functions must be called on the main thread only.        */
/* ------------------------------------------------------------------ */
Game *game_state(void);
void  ms_refresh_title(void);
void  game_reset(const Difficulty *d, int marks);
void  game_new_diff(int diff);
void  game_click(int r, int c);
void  game_mark(int r, int c);
void  game_chord_at(int r, int c);
void  game_pointer_down(int region, int cell, int button);
void  game_pointer_up(int region, int cell, int button);
void  game_pointer_cancel(void);
void  game_tick(void);
int   game_seed_active(void);
uint64_t game_seed_val(void);
void  game_apply_telemetry_seed(int diff, uint64_t seed);
int   game_face_state(void);

/* pointer regions (mapped by the frontends) */
#define PTR_GRID 0
#define PTR_FACE 1

/* ------------------------------------------------------------------ */
/* Seeds.                                                              */
/* ------------------------------------------------------------------ */
int  custom_seed_all_digits(const char *s);
int  custom_seed_generate(const char *input, uint64_t *out,
                          int *steps_out, int *truncated_out);
int  diff_custom_seed_generate(int diff, const char *input, uint64_t *out,
                               int *steps_out, int *truncated_out);
void resolve_board_seed(int diff, uint64_t *out, int *seeded);
const SeedSlot *diff_seed_slot(int diff);
void diff_seed_set_normal(int diff, uint64_t v);
void diff_seed_set_custom(int diff, const char *value);
void diff_seed_clear(int diff);
void seed_override_set(uint64_t v);
void seed_override_clear(void);
int  parse_diff_name(const char *s);
void ms_parse_seed_args(int argc, char **argv);

/* ------------------------------------------------------------------ */
/* Metrics (UI thread).                                                */
/* ------------------------------------------------------------------ */
void metrics_reset(void);
void metrics_note_click(void);
void metrics_note_ui_latency(long us);

/* ------------------------------------------------------------------ */
/* Leaderboard persistence + solver credentials.                       */
/* ------------------------------------------------------------------ */
int  leader_name_valid(const char *name);
const char *leader_player_name(void);
int  leader_set_player_name(const char *name);
int  leader_auto_submit(void);
void leader_set_auto_submit(int on);
void leader_submit_win(int diff, int time_ms);
void leader_setup_solver(int argc, char **argv);

/* ------------------------------------------------------------------ */
/* Time (monotonic).                                                   */
/* ------------------------------------------------------------------ */
uint64_t ms_now_ms(void);
uint64_t ms_now_us(void);

/* ------------------------------------------------------------------ */
/* Event pump + CLI scripting server.                                  */
/* ------------------------------------------------------------------ */
void ms_loop_pump(void);
void ms_net_setup_sinks(void);
int  cli_start(int port);
void cli_stop(void);

#endif /* MS_CORE_H */
