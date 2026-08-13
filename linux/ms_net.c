/*
 * ms_net.c - telemetry client + CLI scripting server for the Linux port.
 *
 * POSIX port of src/network.c.  Wire protocol is byte-identical to the Win32
 * client (see src/network.h).  One background thread drives the telemetry
 * socket with non-blocking connect + select(); the main thread only appends
 * metric lines to a lock-protected queue and consumes marshalled MsEvents
 * from the core event queue.  The localhost CLI scripting server (--listen)
 * also lives here; commands are executed on the main thread through
 * ms_event_push_cli() (a synchronous, condvar-based SendMessage equivalent).
 *
 * MIT License
 */
#include "ms_net.h"
#include "ms_sha256.h"

#include <sys/types.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/select.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <netdb.h>
#include <fcntl.h>
#include <errno.h>
#include <pthread.h>
#include <unistd.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>

#define Q_CAP         256          /* max pending metric lines        */
#define LINE_MAX      512          /* max serialised metric line      */
#define SEND_MAX      16384        /* outbound byte buffer            */
#define CONNECT_TO    5000         /* ms to complete the TCP handshake*/
#define LOOP_TV_MS    100          /* select() poll interval (ms)     */
#define BEAT_MS       10000        /* heartbeat interval (ms)         */
#define RETRY_MS      3000         /* backoff between connect retries */
#define AUTH_PREFIX   "ms-auth:"   /* HMAC challenge-response domain   */

/* solver auth state */
#define AUTH_NONE      0           /* not authenticated / no creds     */
#define AUTH_WAIT_CHAL 1           /* auth sent, awaiting authchal     */
#define AUTH_WAIT_OK   2           /* authresp sent, awaiting authok   */
#define AUTH_OK        3           /* authenticated                    */

static const char *const g_diff_names_t[] = {
    "beginner", "intermediate", "expert", "custom"
};
static const int g_diff_count = (int)(sizeof(g_diff_names_t) / sizeof(g_diff_names_t[0]));

/* ---- connection state (written by the network thread only) ---- */
static int   g_sock = -1;
static char  g_host[64] = "135.125.79.15";
static unsigned short g_port = 28571;
static int   g_connected = 0;

/* A remote-simulation session is active from the moment a req* line is
 * queued until the server closes it out (reqdone / reqdenied / lossfound /
 * noloss) or the connection is (re)established.  Broadcast `seed` lines are
 * only applied to the live board while a session is active; outside one the
 * game is interactive and a server-pushed seed must never reset it.
 * (atomic; mirrors network.c's g_sim_session) */
static volatile int g_sim_session = 0;

/* ---- control ---- */
static volatile int g_running = 0;       /* telemetry session active   */
static pthread_t    g_thread;
static int          g_thread_up = 0;

/* ---- solver auth (creds guarded by g_qlock; auth state atomic) ---- */
static char  g_solver_user[64];
static char  g_solver_pass[128];
static int   g_auth_state = AUTH_NONE;   /* atomic via __atomic builtins */

/* ---- metric queue (main thread producer, network thread consumer) ---- */
static pthread_mutex_t g_qlock = PTHREAD_MUTEX_INITIALIZER;
static char *g_q[Q_CAP];
static int g_q_head = 0;
static int g_q_count = 0;

/* ---- outbound byte buffer (network thread only) ---- */
static char  g_send[SEND_MAX];
static int   g_send_len = 0;
static int   g_send_off = 0;
static int   g_send_metric_lines = 0;

/* ---- inbound line buffer (network thread only) ---- */
static char  g_recv[4096];
static int   g_recv_len = 0;

/* ---- stats (atomic) ---- */
static volatile unsigned long long g_stats_seeds = 0;
static volatile unsigned long long g_stats_outcomes = 0;
static volatile unsigned long long g_stats_wins = 0;
static volatile unsigned long long g_stats_sent = 0;
static volatile unsigned long long g_stats_dropped = 0;
static volatile unsigned long long g_stats_attempts = 0;
static volatile unsigned long long g_connected_at_ms = 0;

static void stats_inc(volatile unsigned long long *v) {
    __atomic_add_fetch(v, 1, __ATOMIC_RELAXED);
}
static void stats_add(volatile unsigned long long *v,
                      unsigned long long n) {
    __atomic_add_fetch(v, n, __ATOMIC_RELAXED);
}
static unsigned long long stats_read(const volatile unsigned long long *v) {
    return __atomic_load_n(v, __ATOMIC_RELAXED);
}

/* forward declarations */
static int  solver_ready(void);
static int  sendbuf_append(const char *line, int metric_lines);
static void net_queue_line(const char *line);
static void net_answer_challenge(const char *nonce_hex);

/* ------------------------------------------------------------------ */
/* metric queue                                                        */
/* ------------------------------------------------------------------ */
static char *net_strdup(const char *s) {
    size_t n = strlen(s) + 1;
    char *p = (char *)malloc(n);
    if (p) memcpy(p, s, n);
    return p;
}

static void queue_push(const char *line) {
    if (g_q_count >= Q_CAP) {
        char *old = g_q[g_q_head];
        g_q_head = (g_q_head + 1) % Q_CAP;
        g_q_count--;
        free(old);
        stats_inc(&g_stats_dropped);
    }
    {
        int idx = (g_q_head + g_q_count) % Q_CAP;
        g_q[idx] = net_strdup(line);
        if (g_q[idx]) {
            g_q_count++;
        } else {
            stats_inc(&g_stats_dropped);
        }
    }
}

static void net_queue_line(const char *line) {
    pthread_mutex_lock(&g_qlock);
    queue_push(line);
    pthread_mutex_unlock(&g_qlock);
}

void net_send_metric(const char *fmt, ...) {
    char line[LINE_MAX];
    va_list ap;
    if (!g_running) return;
    va_start(ap, fmt);
    vsnprintf(line, sizeof(line), fmt, ap);
    va_end(ap);
    net_queue_line(line);
}

static int solver_wanted(void) {
    int wanted = 0;
    pthread_mutex_lock(&g_qlock);
    if (g_solver_user[0] && g_solver_pass[0]) wanted = 1;
    pthread_mutex_unlock(&g_qlock);
    return wanted;
}

static int solver_ready(void) {
    if (!solver_wanted()) return 0;
    return __atomic_load_n(&g_auth_state, __ATOMIC_ACQUIRE) == AUTH_OK;
}

int net_send_request(const char *fmt, ...) {
    char line[LINE_MAX];
    va_list ap;
    if (!g_running) return 0;
    va_start(ap, fmt);
    vsnprintf(line, sizeof(line), fmt, ap);
    va_end(ap);
    /* the solver seed-request system is gated behind authentication */
    if (strnicmp(line, "req", 3) == 0 && !solver_ready())
        return 0;
    net_queue_line(line);
    /* a queued req* line starts a remote-sim session: broadcast seeds now
     * apply to the live board until the server replies reqdone/reqdenied. */
    if (strnicmp(line, "req", 3) == 0)
        __atomic_store_n(&g_sim_session, 1, __ATOMIC_RELEASE);
    return 1;
}

/* Take every queued line out from under the lock into a caller buffer. */
static int queue_drain(char *dst, size_t cap) {
    size_t used = 0;
    pthread_mutex_lock(&g_qlock);
    while (g_q_count > 0) {
        const char *line = g_q[g_q_head];
        size_t n = strlen(line);
        if (used + n + 2 > cap) break;
        memcpy(dst + used, line, n);
        used += n;
        dst[used++] = '\n';
        free((void *)line);
        g_q_head = (g_q_head + 1) % Q_CAP;
        g_q_count--;
    }
    pthread_mutex_unlock(&g_qlock);
    return (int)used;
}

/* ------------------------------------------------------------------ */
/* inbound line dispatch                                               */
/* ------------------------------------------------------------------ */
static int parse_diff_name_t(const char *s) {
    int i;
    for (i = 0; i < g_diff_count; i++)
        if (stricmp(s, g_diff_names_t[i]) == 0) return i;
    return -1;
}

/* Push a leaderboard entry to the core event queue. */
static void push_lb_entry(int rank, const char *diff, const char *name,
                          int time_ms, long long ts) {
    MsEvent e;
    memset(&e, 0, sizeof(e));
    e.kind = EV_LB_ENTRY;
    e.rank = rank;
    strncpy(e.diffname, diff, sizeof(e.diffname) - 1);
    strncpy(e.name, name, sizeof(e.name) - 1);
    e.time_ms = time_ms;
    e.ts = ts;
    ms_event_push(&e);
}

static void handle_line(char *line) {
    char *argv[8];
    int argc = 0;
    char *tok, *save = NULL;

    /* strip trailing CR for CRLF robustness */
    {
        size_t n = strlen(line);
        if (n && line[n - 1] == '\r') line[n - 1] = 0;
    }

    tok = strtok_r(line, " \t", &save);
    while (tok && argc < 8) { argv[argc++] = tok; tok = strtok_r(NULL, " \t", &save); }
    if (argc == 0) return;

    if (stricmp(argv[0], "seed") == 0 && argc == 3) {
        int diff = parse_diff_name_t(argv[1]);
        if (diff >= 0) {
            unsigned long long seed = strtoull(argv[2], NULL, 10);
            stats_inc(&g_stats_seeds);
            /* Only apply a server-pushed seed to the live board while a
             * remote-sim session (reqseed/reqbatch/requntil) is active.
             * The server broadcasts seeds at the producer rate to every
             * connected client; applying them passively would reset the
             * player's board mid-game.  Outside a session they are still
             * counted (g_stats_seeds) but never touch the game. */
            if (__atomic_load_n(&g_sim_session, __ATOMIC_ACQUIRE)) {
                MsEvent e;
                memset(&e, 0, sizeof(e));
                e.kind = EV_TELEMETRY_SEED;
                e.diff = diff;
                e.seed = seed;
                ms_event_push(&e);
            }
        }
    } else if (stricmp(argv[0], "outcome") == 0 && argc >= 4) {
        stats_inc(&g_stats_outcomes);
        if (atoi(argv[3]) == 1)
            stats_inc(&g_stats_wins);
    } else if (stricmp(argv[0], "authchal") == 0 && argc == 2) {
        if (__atomic_compare_exchange_n(&g_auth_state, &(int){AUTH_WAIT_CHAL},
                                        AUTH_WAIT_OK, 0,
                                        __ATOMIC_ACQ_REL,
                                        __ATOMIC_ACQUIRE)) {
            net_answer_challenge(argv[1]);
        }
    } else if (stricmp(argv[0], "authok") == 0) {
        __atomic_compare_exchange_n(&g_auth_state, &(int){AUTH_WAIT_OK},
                                    AUTH_OK, 0, __ATOMIC_ACQ_REL,
                                    __ATOMIC_ACQUIRE);
    } else if (stricmp(argv[0], "autherr") == 0) {
        __atomic_store_n(&g_auth_state, AUTH_NONE, __ATOMIC_RELEASE);
    } else if (stricmp(argv[0], "reqdone") == 0 ||
               stricmp(argv[0], "reqdenied") == 0 ||
               stricmp(argv[0], "lossfound") == 0 ||
               stricmp(argv[0], "noloss") == 0) {
        /* the server closed out a request (or refused it): end the sim
         * session so subsequent broadcast seeds stop resetting the board. */
        __atomic_store_n(&g_sim_session, 0, __ATOMIC_RELEASE);
        if (stricmp(argv[0], "reqdenied") == 0) {
            MsEvent e;
            memset(&e, 0, sizeof(e));
            e.kind = EV_SOLVER_DENIED;
            ms_event_push(&e);
        }
    } else if (stricmp(argv[0], "lbtop") == 0 && (argc == 2 || argc == 3)) {
        MsEvent e;
        memset(&e, 0, sizeof(e));
        e.kind = EV_LB_START;
        ms_event_push(&e);
    } else if (stricmp(argv[0], "lbentry") == 0 && argc == 6) {
        push_lb_entry(atoi(argv[1]), argv[2], argv[3], atoi(argv[4]),
                      strtoll(argv[5], NULL, 10));
    } else if (stricmp(argv[0], "lbdone") == 0) {
        MsEvent e;
        memset(&e, 0, sizeof(e));
        e.kind = EV_LB_END;
        ms_event_push(&e);
    }
    /* welcome / stats / auth / anything else: ignored or handled above */
}

/* Extract and dispatch complete lines; keep any partial line buffered. */
static void dispatch_lines(char *buf, int *used) {
    int i = 0;
    while (i < *used) {
        int j = i;
        while (j < *used && buf[j] != '\n') j++;
        if (j >= *used) break;
        buf[j] = 0;
        handle_line(buf + i);
        i = j + 1;
    }
    if (i >= *used) {
        *used = 0;
    } else if (i > 0) {
        memmove(buf, buf + i, (size_t)(*used - i));
        *used -= i;
    }
}

/* ------------------------------------------------------------------ */
/* solver auth                                                         */
/* ------------------------------------------------------------------ */
static void solver_creds(char *user, size_t usz, char *pass, size_t psz) {
    pthread_mutex_lock(&g_qlock);
    if (user && usz > 0) {
        size_t n = strlen(g_solver_user);
        if (n >= usz) n = usz - 1;
        memcpy(user, g_solver_user, n);
        user[n] = 0;
    }
    if (pass && psz > 0) {
        size_t n = strlen(g_solver_pass);
        if (n >= psz) n = psz - 1;
        memcpy(pass, g_solver_pass, n);
        pass[n] = 0;
    }
    pthread_mutex_unlock(&g_qlock);
}

/* Reply to a received authchal <nonce-hex> challenge.  Runs on the network
 * thread.  The password never leaves the machine. */
static void net_answer_challenge(const char *nonce_hex) {
    char pass[128];
    uint8_t mac[32];
    char resp[65];
    char msg[256];
    char line[384];
    solver_creds(NULL, 0, pass, sizeof(pass));
    snprintf(msg, sizeof(msg), AUTH_PREFIX "%s", nonce_hex);
    ms_hmac_sha256((const uint8_t *)pass, strlen(pass),
                   (const uint8_t *)msg, strlen(msg), mac);
    ms_hex_encode(mac, 32, resp);
    snprintf(line, sizeof(line), "authresp %s", resp);
    sendbuf_append(line, 0);
}

/* Begin the auth handshake on a freshly connected socket. */
static void auth_start(void) {
    char user[64];
    char line[80];
    solver_creds(user, sizeof(user), NULL, 0);
    snprintf(line, sizeof(line), "auth %s", user);
    sendbuf_append(line, 0);
    __atomic_store_n(&g_auth_state, AUTH_WAIT_CHAL, __ATOMIC_RELEASE);
}

/* ------------------------------------------------------------------ */
/* outbound sending (non-blocking)                                     */
/* ------------------------------------------------------------------ */
static int sendbuf_append(const char *line, int metric_lines) {
    size_t n = strlen(line);
    if (g_send_len + (int)n + 2 > SEND_MAX) {
        if (metric_lines > 0)
            stats_add(&g_stats_dropped, (unsigned long long)metric_lines);
        return 0;
    }
    memcpy(g_send + g_send_len, line, n);
    g_send_len += (int)n;
    g_send[g_send_len++] = '\n';
    g_send_metric_lines += metric_lines;
    return 1;
}

static int flush_sendbuf(int s) {
    while (g_send_off < g_send_len) {
        int n = send(s, g_send + g_send_off, (size_t)(g_send_len - g_send_off), 0);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) return 1;
            return 0;
        }
        if (n == 0) return 0;
        g_send_off += n;
    }
    g_send_off = g_send_len = 0;
    if (g_send_metric_lines > 0) {
        stats_add(&g_stats_sent, (unsigned long long)g_send_metric_lines);
        g_send_metric_lines = 0;
    }
    return 1;
}

static int flush_pending(int s) {
    char batch[SEND_MAX];
    int n = queue_drain(batch, sizeof(batch));
    if (n > 0) {
        int m = 0;
        for (int i = 0; i < n; i++)
            if (batch[i] == '\n') m++;
        batch[n] = 0;
        sendbuf_append(batch, m);
    }
    return flush_sendbuf(s);
}

static void heartbeat(int s) {
    char line[96];
    snprintf(line, sizeof(line), "metric heartbeat t=%llu",
             (unsigned long long)ms_now_ms());
    sendbuf_append(line, 0);
    (void)s;
}

/* ------------------------------------------------------------------ */
/* connect + session loop (network thread)                             */
/* ------------------------------------------------------------------ */
static int set_nonblock(int s) {
    int fl = fcntl(s, F_GETFL, 0);
    if (fl < 0) return -1;
    return fcntl(s, F_SETFL, fl | O_NONBLOCK);
}

static int tcp_connect(const char *host, unsigned short port, int *out) {
    struct addrinfo hints, *res = NULL, *ai;
    char portstr[16];
    int s = -1;

    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    snprintf(portstr, sizeof(portstr), "%u", (unsigned)port);

    if (getaddrinfo(host, portstr, &hints, &res) != 0) return 0;

    for (ai = res; ai; ai = ai->ai_next) {
        int err = 0;
        s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
        if (s < 0) continue;
        set_nonblock(s);
        if (connect(s, ai->ai_addr, (socklen_t)ai->ai_addrlen) < 0) {
            if (errno == EINPROGRESS || errno == EWOULDBLOCK) {
                fd_set wf;
                struct timeval tv = { CONNECT_TO / 1000,
                                      (CONNECT_TO % 1000) * 1000 };
                FD_ZERO(&wf);
                FD_SET(s, &wf);
                if (select(s + 1, NULL, &wf, NULL, &tv) > 0) {
                    int soerr = 0;
                    socklen_t len = sizeof(soerr);
                    if (getsockopt(s, SOL_SOCKET, SO_ERROR, &soerr, &len) < 0)
                        err = errno;
                    else
                        err = soerr;
                } else {
                    err = ETIMEDOUT;
                }
            } else {
                err = errno;
            }
        }
        if (err == 0) {
            int one = 1;
            setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
            freeaddrinfo(res);
            *out = s;
            return 1;
        }
        close(s);
        s = -1;
    }
    freeaddrinfo(res);
    *out = -1;
    return 0;
}

/* Run the connected session until EOF, an error, or shutdown. */
static int conn_loop(int s) {
    unsigned long long last_beat = 0;
    g_connected = 1;
    g_connected_at_ms = ms_now_ms();
    /* a fresh connection has no in-flight request: never carry a stale
     * remote-sim session across a reconnect */
    __atomic_store_n(&g_sim_session, 0, __ATOMIC_RELEASE);
    __atomic_store_n(&g_auth_state, AUTH_NONE, __ATOMIC_RELEASE);
    if (solver_wanted()) auth_start();
    while (g_running) {
        fd_set rf, wf;
        struct timeval tv;
        int r, have_out;

        tv.tv_sec = 0;
        tv.tv_usec = LOOP_TV_MS * 1000;

        FD_ZERO(&rf);
        FD_SET(s, &rf);
        FD_ZERO(&wf);
        pthread_mutex_lock(&g_qlock);
        have_out = (g_send_off < g_send_len) || (g_q_count > 0);
        pthread_mutex_unlock(&g_qlock);
        if (have_out) FD_SET(s, &wf);

        r = select(s + 1, &rf, &wf, NULL, &tv);
        if (r < 0) {
            if (errno == EINTR && g_running) continue;
            break;
        }
        if (r > 0 && FD_ISSET(s, &wf)) {
            if (!flush_pending(s)) break;
        } else if (have_out) {
            if (!flush_sendbuf(s)) break;
        }
        if (r > 0 && FD_ISSET(s, &rf)) {
            if (g_recv_len >= (int)sizeof(g_recv) - 1) {
                g_recv_len = 0;
            }
            int n = recv(s, g_recv + g_recv_len,
                         (size_t)((int)sizeof(g_recv) - 1 - g_recv_len), 0);
            if (n <= 0) break;
            g_recv_len += n;
            g_recv[g_recv_len] = 0;
            dispatch_lines(g_recv, &g_recv_len);
        }

        if (ms_now_ms() - last_beat >= BEAT_MS) {
            last_beat = ms_now_ms();
            heartbeat(s);
            if (!flush_sendbuf(s)) break;
        }
    }
    g_connected = 0;
    return 0;
}

static void *telemetry_thread(void *arg) {
    (void)arg;
    while (g_running) {
        int s = -1;
        stats_inc(&g_stats_attempts);
        if (tcp_connect(g_host, g_port, &s)) {
            g_sock = s;
            conn_loop(s);
            if (g_sock == s) g_sock = -1;
            if (g_running) close(s);
            g_send_off = g_send_len = 0;
            if (g_send_metric_lines > 0) {
                stats_add(&g_stats_dropped,
                          (unsigned long long)g_send_metric_lines);
                g_send_metric_lines = 0;
            }
            if (!g_running) break;
        }
        for (int i = 0; i < RETRY_MS / 50 && g_running; i++)
            usleep(50 * 1000);
    }
    return NULL;
}

/* ------------------------------------------------------------------ */
/* public API                                                          */
/* ------------------------------------------------------------------ */
int net_telemetry_start(const char *host, unsigned short port) {
    if (g_running) return 1;
    if (!host || !*host || port == 0) return 0;

    strncpy(g_host, host, sizeof(g_host) - 1);
    g_host[sizeof(g_host) - 1] = 0;
    g_port = port;
    g_sock = -1;
    g_connected = 0;
    g_send_off = g_send_len = 0;
    g_recv_len = 0;
    g_q_head = g_q_count = 0;

    __atomic_store_n(&g_running, 1, __ATOMIC_RELEASE);
    if (pthread_create(&g_thread, NULL, telemetry_thread, NULL) != 0) {
        __atomic_store_n(&g_running, 0, __ATOMIC_RELEASE);
        return 0;
    }
    g_thread_up = 1;
    return 1;
}

void net_telemetry_stop(void) {
    if (!g_running) return;
    __atomic_store_n(&g_running, 0, __ATOMIC_RELEASE);
    if (g_sock != -1) {
        shutdown(g_sock, SHUT_RDWR);
        close(g_sock);
        g_sock = -1;
    }
    if (g_thread_up) {
        pthread_join(g_thread, NULL);
        g_thread_up = 0;
    }
}

int net_telemetry_active(void) {
    return g_running ? 1 : 0;
}

void net_telemetry_endpoint(char *host, size_t hsz, unsigned short *port) {
    if (host && hsz) {
        strncpy(host, g_host, hsz - 1);
        host[hsz - 1] = 0;
    }
    if (port) *port = g_port;
}

void ms_net_setup_sinks(void) {
    /* seeds/leaderboard/denied are pushed straight to the core event queue
     * by handle_line(); no additional sink registration is needed. */
}

void net_set_solver_creds(const char *user, const char *pass) {
    pthread_mutex_lock(&g_qlock);
    if (user && *user) {
        strncpy(g_solver_user, user, sizeof(g_solver_user) - 1);
        g_solver_user[sizeof(g_solver_user) - 1] = 0;
    } else {
        g_solver_user[0] = 0;
    }
    if (pass && *pass) {
        strncpy(g_solver_pass, pass, sizeof(g_solver_pass) - 1);
        g_solver_pass[sizeof(g_solver_pass) - 1] = 0;
    } else {
        g_solver_pass[0] = 0;
    }
    pthread_mutex_unlock(&g_qlock);
}

int net_solver_creds_set(void) {
    return solver_wanted();
}

void net_send_score(const char *name, int diff, int time_ms) {
    char line[128];
    if (!g_running || !name || !*name) return;
    if (diff < 0 || diff >= 3) return;
    snprintf(line, sizeof(line), "lbscore %s %s %d",
             name, g_diff_names_t[diff], time_ms);
    net_queue_line(line);
}

void net_request_lbtop(int count) {
    char line[64];
    if (!g_running || count <= 0) return;
    snprintf(line, sizeof(line), "lbtop %d", count);
    net_queue_line(line);
}

void net_request_lbtop_diff(int diff, int count) {
    char line[64];
    if (!g_running || diff < 0 || diff >= 3 || count <= 0) return;
    snprintf(line, sizeof(line), "lbtop %s %d", g_diff_names_t[diff], count);
    net_queue_line(line);
}

void net_get_stats(NetStats *st) {
    if (!st) return;
    memset(st, 0, sizeof(*st));
    st->connected = g_connected;
    st->connected_ms = g_connected
        ? (int)(ms_now_ms() - g_connected_at_ms) : 0;
    st->attempts = (int)stats_read(&g_stats_attempts);
    st->seeds_recv = stats_read(&g_stats_seeds);
    st->outcomes_recv = stats_read(&g_stats_outcomes);
    st->wins_recv = stats_read(&g_stats_wins);
    st->metrics_sent = stats_read(&g_stats_sent);
    st->metrics_dropped = stats_read(&g_stats_dropped);
}

/* ==================================================================== */
/* CLI scripting server (localhost only)                                */
/* ==================================================================== */

static int g_cli_sock = -1;
static pthread_t g_cli_thread;
static volatile int g_cli_running = 0;
static volatile int g_cli_started = 0;

static int cli_send_all(int s, const char *buf, int len) {
    int off = 0;
    while (off < len) {
        int n = (int)send(s, buf + off, (size_t)(len - off), 0);
        if (n <= 0) return 0;
        off += n;
    }
    return 1;
}

static void cli_handle_client(int s) {
    char line[512];
    int used = 0;
    int done = 0;

    while (!done) {
        char c;
        int n = (int)recv(s, &c, 1, 0);
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
            cmd.have_u = 0;

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
                    if (cmd.a < 0) cmd.a = -3;
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
                    if (cmd.a < 0) cmd.a = -3;
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
                    else cmd.a = -2;
                } else cmd.a = -1;
            }
            else if (stricmp(argv[0], "reqseed") == 0) {
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

            /* execute on the main thread where the game state is safe */
            {
                pthread_mutex_init(&cmd.lock, NULL);
                pthread_cond_init(&cmd.cv, NULL);
                cmd.ready = 0;
                ms_event_push_cli(&cmd);          /* blocks until dispatched */
                if (cmd.reply) {
                    cli_send_all(s, cmd.reply, (int)strlen(cmd.reply));
                    free(cmd.reply);
                }
                pthread_cond_destroy(&cmd.cv);
                pthread_mutex_destroy(&cmd.lock);
            }
        } else if (used < (int)sizeof(line) - 1) {
            line[used++] = c;
        }
    }
}

static void *cli_server_thread(void *arg) {
    (void)arg;
    while (g_cli_running) {
        int client = accept(g_cli_sock, NULL, NULL);
        if (client < 0) {
            if (errno == EINTR) continue;
            break;
        }
        {
            int one = 1;
            setsockopt(client, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
        }
        cli_handle_client(client);
        close(client);
    }
    return NULL;
}

int cli_start(int port) {
    struct sockaddr_in addr;
    int s;
    int one = 1;

    s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (s < 0) return 0;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));

    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);   /* localhost only */
    addr.sin_port = htons((unsigned short)port);

    if (bind(s, (struct sockaddr *)&addr, sizeof(addr)) < 0 ||
        listen(s, 4) < 0) {
        close(s);
        return 0;
    }

    g_cli_sock = s;
    __atomic_store_n(&g_cli_started, 1, __ATOMIC_RELEASE);
    __atomic_store_n(&g_cli_running, 1, __ATOMIC_RELEASE);
    if (pthread_create(&g_cli_thread, NULL, cli_server_thread, NULL) != 0) {
        __atomic_store_n(&g_cli_running, 0, __ATOMIC_RELEASE);
        close(s);
        __atomic_store_n(&g_cli_started, 0, __ATOMIC_RELEASE);
        g_cli_sock = -1;
        return 0;
    }
    return 1;
}

void cli_stop(void) {
    __atomic_store_n(&g_cli_running, 0, __ATOMIC_RELEASE);
    if (g_cli_sock != -1) {
        shutdown(g_cli_sock, SHUT_RDWR);
        close(g_cli_sock);
        g_cli_sock = -1;
    }
    if (g_cli_started) {
        pthread_join(g_cli_thread, NULL);
        __atomic_store_n(&g_cli_started, 0, __ATOMIC_RELEASE);
    }
}
