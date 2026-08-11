/*
 * ms_net.h - telemetry client for the Linux Minesweeper client.
 *
 * POSIX port of src/network.h.  Identical wire protocol: the client connects
 * out to the simulation/telemetry server, receives streamed seeds + simulated
 * outcomes ("API receiver") and transmits local game metrics ("metrics
 * sender").  All socket I/O runs on a single background thread using
 * non-blocking sockets polled with select(); the main thread only appends
 * metric strings to a lock-protected queue and drains marshalled events from
 * the core event queue (ms_core.h), so the UI loop never blocks on the
 * network.
 *
 * MIT License
 */
#ifndef MS_NET_H
#define MS_NET_H

#include "ms_core.h"

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

/* Start/stop the telemetry background thread.  start() reconnects if the
 * previous session was stopped.  Returns 1 if the thread was spawned. */
int  net_telemetry_start(const char *host, unsigned short port);
void net_telemetry_stop(void);
int  net_telemetry_active(void);

/* Enqueue a metric line (printf-style).  Thread-safe, never blocks the
 * caller: drops the oldest pending metric if the queue is full. */
void net_send_metric(const char *fmt, ...);

/* Enqueue one request line (reqseed/reqbatch/requntil).  Returns 1 if queued,
 * 0 if dropped (telemetry off, or an unauthenticated solver request). */
int  net_send_request(const char *fmt, ...);

/* Solver credentials for the HMAC-SHA256 challenge-response.  When both are
 * set, the network thread authenticates after every (re)connect. */
void net_set_solver_creds(const char *user, const char *pass);
int  net_solver_creds_set(void);

/* Leaderboard: submit a best-time and query the top list. */
void net_send_score(const char *name, int diff, int time_ms);
void net_request_lbtop(int count);
void net_request_lbtop_diff(int diff, int count);

void net_get_stats(NetStats *st);

/* Register the event-queue sinks: streamed seeds, leaderboard entries and
 * solver-denied notices are pushed to the core event queue as MsEvents.
 * Call once at startup. */
void ms_net_setup_sinks(void);

#endif /* MS_NET_H */
