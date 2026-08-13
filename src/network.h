/*
 * network.h - telemetry client for Minesweeper (Classic).
 *
 * The client connects out to a remote simulation/telemetry server and
 *   1. continuously receives streamed per-difficulty seeds and simulated
 *      board outcomes ("API receiver"), and
 *   2. transmits local game metrics ("metrics sender").
 *
 * Every socket operation runs on a dedicated background thread using
 * non-blocking sockets polled with select().  The UI thread never touches
 * the socket; it only appends metric strings to a lock-protected queue and
 * hands received seeds to the registered sink via PostMessage.  The Win32
 * GDI message pump therefore never blocks on network I/O.
 *
 * Wire protocol (newline-terminated ASCII lines):
 *   Server -> client (streamed, one per line):
 *       seed <beginner|intermediate|expert|custom> <n>
 *       outcome <diff> <seed> <won 0|1> <moves> <time_ms> <guesses>
 *       welcome ... | stats ...
 *   Client -> server (metrics, one per line):
 *       metric start  diff=<n> seed=<n> seeded=<0|1> t=<ms>
 *       metric win|loss diff=<n> seed=<n> seeded=<0|1> time=<s>
 *                       clicks=<n> latency=<us> t=<ms>
 *       metric latency us=<n> t=<ms>
 *       metric heartbeat t=<ms>
 *   Client -> server (seed requests, for the remote-simulation analysis
 *   system; answered with reqgame/seed/outcome/reqdone lines):
 *       reqseed  <beginner|intermediate|expert> <n> [count]
 *       reqbatch <beginner|intermediate|expert> <count>
 *       requntil <beginner|intermediate|expert> <n> [max]
 *   The server gates heavy requests (estimated CPU >= 0.25 s) in FIFO order
 *   because the GIL caps its total Python CPU at ~1 core; a gated request
 *   is answered with `reqwait <diff> <n>` before its reqgame lines.  This
 *   client ignores reqwait and treats a later reqgame as the start marker,
 *   so no code change is required here.
 *   requntil replays seed n until the simulated player loses (capped at max
 *   runs) and, when a loss is seen, replies:
 *       lossfound <diff> <n> <run> <won> <moves> <time_ms> <guesses>
 *   otherwise it replies noloss <diff> <n> <max>; both are followed by reqdone.
 *
 *   The solver (the seed-request system above) is protected on the server:
 *   without credentials configured it is disabled and every request is
 *   answered `reqdenied`; with credentials the client authenticates once per
 *   connection using an HMAC-SHA256 challenge-response (the password is
 *   never sent):
 *       client -> server:  auth <user>
 *       server -> client:  authchal <nonce-hex>      (or autherr)
 *       client -> server:  authresp <hmac-sha256-hex>
 *       server -> client:  authok                    (or autherr)
 *   Credentials come from the game (--solver-user/--solver-pass or the
 *   [solver] section of minesweeper.ini) and are applied automatically after
 *   each connect.  A denied solver request arrives as `reqdenied`.
 *
 *   Leaderboard (best win times): the client submits a finished win and can
 *   request the current top times:
 *       client -> server:  lbscore <name> <diff> <time_ms>
 *       server -> client:  lbstored <rank> <diff> <name> <time_ms> | lbnotop
 *       client -> server:  lbtop <count> | lbtop <diff> <count>
 *       server -> client:  lbtop [<diff>] <count>  header
 *                          lbentry <rank> <diff> <name> <time_ms> <ts>
 *                          lbdone
 *   The top-list entries are handed to the registered lb sink.
 *
 * MIT License
 */
#ifndef NETWORK_H
#define NETWORK_H

/* UI-thread marshalling messages posted to a window that wants to react to
 * the network thread.  The leaderboard dialog registers itself as the
 * recipient of WM_APP_LB_ENTRY / WM_APP_LB_END; WM_APP_SOLVER_DENIED goes to
 * the main window. */
#define WM_APP_LB_ENTRY      (WM_APP + 4)
#define WM_APP_LB_END        (WM_APP + 5)
#define WM_APP_SOLVER_DENIED (WM_APP + 6)

/* Event codes carried in wParam of WM_APP_LB_ENTRY. */
#define LB_EV_START 0               /* lParam 0: a top-list begins */
#define LB_EV_ENTRY 1               /* lParam: NetLbEntryMsg* (free after use) */
#define LB_EV_END   2               /* lParam 0: the top-list is complete */

/* A single leaderboard entry, marshalled off the network thread (heap-
 * allocated, freed by the receiver).  name/diff are NUL-terminated ASCII. */
typedef struct {
    int        rank;
    char       diff[16];
    char       name[17];
    int        time_ms;
    long long  ts;
} NetLbEntryMsg;

/* Called from the UI thread.  diff is a DIFF_* index
 * (beginner=0, intermediate=1, expert=2, custom=3). */
typedef void (*net_seed_sink_fn)(int diff, unsigned long long seed);

/* Start/stop the telemetry background thread.  start() is idempotent for a
 * given host:port; calling it again reconnects if the previous session was
 * stopped.  Returns 1 if the thread was spawned. */
int  net_telemetry_start(const char *host, unsigned short port);
void net_telemetry_stop(void);
int  net_telemetry_active(void);

/* Decode the obfuscated (base64) default telemetry endpoint into host
 * (NUL-terminated, up to hsz bytes) and *port.  Obfuscation only — the value
 * must be recoverable at runtime and is still sent in the clear on the wire.
 * Returns 1 on success. */
int  net_endpoint_default(char *host, size_t hsz, unsigned short *port);

/* Register the callback that receives streamed `seed <diff> <n>` lines.
 * The callback runs on the network thread; keep it cheap or marshal to the
 * UI thread (the game does the latter). */
void net_set_seed_sink(net_seed_sink_fn fn);

/* Set the window that receives the leaderboard top-list messages
 * (WM_APP_LB_ENTRY with LB_EV_* / WM_APP_LB_END).  The leaderboard dialog
 * registers its HWND here on open and resets to NULL on close. */
void net_set_lb_window(HWND hwnd);

/* Set the window that receives WM_APP_SOLVER_DENIED when the server refuses
 * an authenticated solver request (e.g. bad credentials). */
void net_set_notify_hwnd(HWND hwnd);

/* Enqueue a metric line (printf-style).  Thread-safe, never blocks the
 * caller: drops the oldest pending metric if the queue is full. */
void net_send_metric(const char *fmt, ...);

/* Enqueue an outbound line verbatim (printf-style), e.g. a seed request
 * `reqseed beginner 12345`.  Same queue/thread-safety guarantees as
 * net_send_metric(); a no-op while the telemetry session is inactive. */
/* Enqueue one request line.  Returns 1 if queued, 0 if dropped (telemetry
 * off, or an unauthenticated solver request).  Non-blocking. */
int  net_send_request(const char *fmt, ...);

/* Solver credentials.  When both are set, the network thread authenticates
 * (HMAC-SHA256 challenge-response) after every (re)connect so the protected
 * seed-request system is usable.  Clear with user/pass NULL or empty. */
void net_set_solver_creds(const char *user, const char *pass);
int  net_solver_creds_set(void);

/* Enqueue a leaderboard best-time submission (called on a win; best time per
 * name+difficulty is kept by the server).  name is validated by the caller;
 * diff is a DIFF_* index (presets only). */
void net_send_score(const char *name, int diff, int time_ms);

/* Enqueue a top-list query: lbtop <count> across all difficulties, or
 * lbtop <diff> <count> for one.  Results arrive through the lb sink. */
void net_request_lbtop(int count);
void net_request_lbtop_diff(int diff, int count);

typedef struct {
    int      connected;               /* socket currently up */
    int      connected_ms;            /* ms since the current connection */
    int      attempts;                /* connection attempts */
    unsigned long long seeds_recv;    /* seed <diff> <n> lines received */
    unsigned long long outcomes_recv; /* outcome ... lines received */
    unsigned long long wins_recv;     /* outcome ... won=1 lines received */
    unsigned long long metrics_sent;  /* metric lines actually sent */
    unsigned long long metrics_dropped; /* metrics dropped (queue full) */
} NetStats;

void net_get_stats(NetStats *st);

#endif /* NETWORK_H */
