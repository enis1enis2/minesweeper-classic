/*
 * network.c - telemetry client for Minesweeper (Classic).
 *
 * Implements the "API receiver" (streamed seeds + simulated outcomes) and
 * "metrics sender" described in network.h.  All socket I/O is confined to a
 * single background thread using non-blocking sockets polled with select():
 *
 *   - the UI thread only ever calls net_send_metric(), which appends to a
 *     lock-protected ring queue and returns immediately, and
 *   - received `seed <diff> <n>` lines are handed to the sink callback,
 *     which the game marshals to the UI thread via PostMessage.
 *
 * The GDI message pump therefore never blocks on network I/O.
 *
 * The solver seed-request system is gated behind an HMAC-SHA256 challenge-
 * response handshake when credentials are configured (see network.h): the
 * network thread authenticates once per connection and only then forwards
 * `reqseed`/`reqbatch`/`requntil` lines from the game.  Leaderboard lines
 * (`lbscore`, `lbtop`) are open and share the same outbound queue; the
 * replies are marshalled to the leaderboard dialog via PostMessage.
 *
 * MIT License
 */
#define WIN32_LEAN_AND_MEAN
#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0600
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <winhttp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>

#include "network.h"

#define Q_CAP         256          /* max pending metric lines        */
#define LINE_MAX      512          /* max serialised metric line      */
#define SEND_MAX      16384        /* outbound byte buffer            */
#define CONNECT_TO    5000         /* ms to complete the TCP handshake*/
#define LOOP_TV_MS    100          /* select() poll interval (ms)     */
#define BEAT_MS       10000        /* heartbeat interval (ms)         */
#define RETRY_MS      3000         /* backoff between connect retries */
#define AUTH_PREFIX   "ms-auth:"   /* HMAC challenge-response domain   */

#define HTTP_LOOP_MS  1000         /* HTTP(S) session poll interval    */

/* solver auth state (LONG, read/written across threads via Interlocked) */
#define AUTH_NONE      0           /* not authenticated / no creds     */
#define AUTH_WAIT_CHAL 1           /* auth sent, awaiting authchal     */
#define AUTH_WAIT_OK   2           /* authresp sent, awaiting authok   */
#define AUTH_OK        3           /* authenticated                    */

static const char *const g_diff_names_t[] = {
    "beginner", "intermediate", "expert", "custom"
};
static const int g_diff_count = (int)(sizeof(g_diff_names_t) / sizeof(g_diff_names_t[0]));

/* ---- connection state (written by the network thread only) ---- */
static SOCKET g_sock = INVALID_SOCKET;
static char  g_host[64];
static unsigned short g_port = 0;
static int   g_connected = 0;

/* A remote-simulation session is active from the moment a req* line is
 * queued until the server closes it out (reqdone / reqdenied / lossfound /
 * noloss) or the connection is (re)established.  Broadcast `seed` lines are
 * only applied to the live board while a session is active; outside one the
 * game is interactive and a server-pushed seed must never reset it. */
static volatile LONG g_sim_session = 0;

/* ---- control ---- */
static volatile LONG g_running = 0;      /* telemetry session active   */
static HANDLE g_thread = NULL;
static int    g_wsa = 0;                 /* we own a WSAStartup         */
static net_seed_sink_fn g_sink = NULL;

/* ---- solver auth (creds guarded by g_qlock; state by Interlocked) ---- */
static char  g_solver_user[64];
static char  g_solver_pass[128];
static volatile LONG g_auth_state = AUTH_NONE;

/* ---- UI marshalling targets ---- */
static HWND   g_ui_hwnd = NULL;          /* WM_APP_SOLVER_DENIED        */
static HWND   g_lb_hwnd = NULL;          /* WM_APP_LB_ENTRY / _END      */

/* forward declarations (defined below) */
static void solver_creds(char *user, size_t usz, char *pass, size_t psz);
static int  solver_ready(void);
static int  sendbuf_append(const char *line, int metric_lines);

/* ---- metric queue (UI thread producer, network thread consumer) ---- */
static CRITICAL_SECTION g_qlock;
static volatile LONG    g_qlock_ready = 0;   /* g_qlock initialized yet */
static char *g_q[Q_CAP];
static int g_q_head = 0;
static int g_q_count = 0;

/* ---- outbound byte buffer (network thread only) ---- */
static char  g_send[SEND_MAX];
static int   g_send_len = 0;
static int   g_send_off = 0;
/* metric lines currently resident in g_send that have not been confirmed
 * fully flushed to the socket (network thread only).  g_stats_sent is only
 * credited once these bytes have actually left via flush_sendbuf(). */
static int   g_send_metric_lines = 0;

/* ---- inbound line buffer (network thread only) ---- */
static char  g_recv[4096];
static int   g_recv_len = 0;

/* ---- stats (64-bit fields, atomic increments; readers tolerate the
 * occasional torn read, so reads are done with a compare-exchange helper) */
static volatile LONG64 g_stats_seeds = 0;
static volatile LONG64 g_stats_outcomes = 0;
static volatile LONG64 g_stats_wins = 0;
static volatile LONG64 g_stats_sent = 0;
static volatile LONG64 g_stats_dropped = 0;
static volatile LONG64 g_stats_attempts = 0;
static volatile unsigned long long g_connected_at_ms = 0;

/* HTTP(S) transport (WinHTTP) state.  g_http_mode / g_https_insecure are
 * set by the UI thread before the network thread starts; the rest are
 * touched only on the network thread. */
static int   g_http_mode = 0;        /* 0=raw TCP, 1=plain HTTP, 2=HTTPS */
static int   g_https_insecure = 0;   /* 1 = disable WinHTTP cert checks   */
static char  g_auth_nonce[129];      /* latest authchal nonce (hex)       */
static unsigned long long g_seed_cursor = 0;   /* x-ms-cursor watermark   */
static volatile LONG g_session_gen = 0;   /* bumps per net_telemetry_start */

static unsigned long long net_now_ms(void) {
    return (unsigned long long)GetTickCount64();
}

static unsigned long long net_atomic_read(const volatile LONG64 *v) {
    LONG64 cur;
    do { cur = *v; } while (InterlockedCompareExchange64((LONG64 *)v, cur, cur) != cur);
    return (unsigned long long)cur;
}

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
        /* drop the oldest pending metric */
        char *old = g_q[g_q_head];
        g_q_head = (g_q_head + 1) % Q_CAP;
        g_q_count--;
        free(old);
        InterlockedIncrement64(&g_stats_dropped);
    }
    {
        int idx = (g_q_head + g_q_count) % Q_CAP;
        g_q[idx] = net_strdup(line);
        if (g_q[idx]) {
            g_q_count++;
        } else {
            InterlockedIncrement64(&g_stats_dropped);
        }
    }
}

/* Common outbound-queue entry point: format a line and append it to the
 * metric/request queue.  Never blocks the caller. */
static void net_queue_line(const char *line) {
    EnterCriticalSection(&g_qlock);
    queue_push(line);
    LeaveCriticalSection(&g_qlock);
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

/* Queue one request line.  Returns 1 if it was actually queued, 0 if it was
 * dropped (telemetry off, or a solver request sent before the handshake
 * succeeded -- without credentials such lines are never forwarded because the
 * server would deny them anyway). */
int net_send_request(const char *fmt, ...) {
    char line[LINE_MAX];
    va_list ap;
    if (!g_running) return 0;
    va_start(ap, fmt);
    vsnprintf(line, sizeof(line), fmt, ap);
    va_end(ap);
    /* the solver seed-request system is gated behind authentication: forward
     * req* lines only once the handshake succeeded (without credentials they
     * are never sent; the server would deny them anyway). */
    if (_strnicmp(line, "req", 3) == 0 && !solver_ready())
        return 0;
    net_queue_line(line);
    /* a queued req* line starts a remote-sim session: broadcast seeds now
     * apply to the live board until the server replies reqdone/reqdenied. */
    if (_strnicmp(line, "req", 3) == 0)
        InterlockedExchange(&g_sim_session, 1);
    return 1;
}

/* Take every queued line out from under the lock into a caller buffer.
 * Lines leave the queue here but are NOT counted as sent yet: delivery is
 * only confirmed once flush_sendbuf() hands their bytes to the socket. */
static int queue_drain(char *dst, size_t cap) {
    size_t used = 0;
    EnterCriticalSection(&g_qlock);
    while (g_q_count > 0) {
        const char *line = g_q[g_q_head];
        size_t n = strlen(line);
        if (used + n + 2 > cap) break;      /* leave the rest for later */
        memcpy(dst + used, line, n);
        used += n;
        dst[used++] = '\n';
        free((void *)line);
        g_q_head = (g_q_head + 1) % Q_CAP;
        g_q_count--;
    }
    LeaveCriticalSection(&g_qlock);
    return (int)used;
}

/* ------------------------------------------------------------------ */
/* SHA-256 / HMAC-SHA256 (self-contained, no external crypto lib)      */
/* ------------------------------------------------------------------ */
typedef struct {
    uint32_t h[8];
    uint64_t len;                  /* total bytes hashed so far */
    uint8_t  buf[64];
    size_t   buflen;
} Sha256Ctx;

static const uint32_t K256[64] = {
    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u, 0x3956c25bu,
    0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u, 0xd807aa98u, 0x12835b01u,
    0x243185beu, 0x550c7dc3u, 0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u,
    0xc19bf174u, 0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu,
    0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau, 0x983e5152u,
    0xa831c66du, 0xb00327c8u, 0xbf597fc7u, 0xc6e00bf3u, 0xd5a79147u,
    0x06ca6351u, 0x14292967u, 0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu,
    0x53380d13u, 0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
    0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u, 0xd192e819u,
    0xd6990624u, 0xf40e3585u, 0x106aa070u, 0x19a4c116u, 0x1e376c08u,
    0x2748774cu, 0x34b0bcb5u, 0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu,
    0x682e6ff3u, 0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u,
    0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u
};

static uint32_t rotr32(uint32_t x, int n) { return (x >> n) | (x << (32 - n)); }

static void sha256_init(Sha256Ctx *c) {
    c->h[0] = 0x6a09e667u; c->h[1] = 0xbb67ae85u; c->h[2] = 0x3c6ef372u;
    c->h[3] = 0xa54ff53au; c->h[4] = 0x510e527fu; c->h[5] = 0x9b05688cu;
    c->h[6] = 0x1f83d9abu; c->h[7] = 0x5be0cd19u;
    c->len = 0; c->buflen = 0;
}

static void sha256_block(Sha256Ctx *c, const uint8_t *p) {
    uint32_t w[64];
    for (int i = 0; i < 16; i++)
        w[i] = ((uint32_t)p[i*4] << 24) | ((uint32_t)p[i*4+1] << 16) |
               ((uint32_t)p[i*4+2] << 8) | (uint32_t)p[i*4+3];
    for (int i = 16; i < 64; i++) {
        uint32_t s0 = rotr32(w[i-15], 7) ^ rotr32(w[i-15], 18) ^ (w[i-15] >> 3);
        uint32_t s1 = rotr32(w[i-2], 17) ^ rotr32(w[i-2], 19) ^ (w[i-2] >> 10);
        w[i] = w[i-16] + s0 + w[i-7] + s1;
    }
    uint32_t a=c->h[0], b=c->h[1], cc=c->h[2], d=c->h[3],
             e=c->h[4], f=c->h[5], g=c->h[6], h=c->h[7];
    for (int i = 0; i < 64; i++) {
        uint32_t S1 = rotr32(e, 6) ^ rotr32(e, 11) ^ rotr32(e, 25);
        uint32_t ch = (e & f) ^ ((~e) & g);
        uint32_t t1 = h + S1 + ch + K256[i] + w[i];
        uint32_t S0 = rotr32(a, 2) ^ rotr32(a, 13) ^ rotr32(a, 22);
        uint32_t maj = (a & b) ^ (a & cc) ^ (b & cc);
        uint32_t t2 = S0 + maj;
        h = g; g = f; f = e; e = d + t1;
        d = cc; cc = b; b = a; a = t1 + t2;
    }
    c->h[0]+=a; c->h[1]+=b; c->h[2]+=cc; c->h[3]+=d;
    c->h[4]+=e; c->h[5]+=f; c->h[6]+=g; c->h[7]+=h;
}

static void sha256_update(Sha256Ctx *c, const void *data, size_t n) {
    const uint8_t *p = (const uint8_t *)data;
    c->len += n;
    while (n > 0) {
        size_t take = 64 - c->buflen;
        if (take > n) take = n;
        memcpy(c->buf + c->buflen, p, take);
        c->buflen += take; p += take; n -= take;
        if (c->buflen == 64) { sha256_block(c, c->buf); c->buflen = 0; }
    }
}

static void sha256_final(Sha256Ctx *c, uint8_t out[32]) {
    uint64_t bits = c->len * 8;
    uint8_t  pad = 0x80;
    size_t   rem = c->buflen;
    sha256_update(c, &pad, 1);
    uint8_t zero = 0;
    while (c->buflen != 56)
        sha256_update(c, &zero, 1);
    uint8_t blen[8];
    for (int i = 0; i < 8; i++)
        blen[i] = (uint8_t)(bits >> (56 - i * 8));
    sha256_update(c, blen, 8);
    (void)rem;
    for (int i = 0; i < 8; i++) {
        out[i*4]   = (uint8_t)(c->h[i] >> 24);
        out[i*4+1] = (uint8_t)(c->h[i] >> 16);
        out[i*4+2] = (uint8_t)(c->h[i] >> 8);
        out[i*4+3] = (uint8_t)c->h[i];
    }
}

static void hmac_sha256(const uint8_t *key, size_t klen,
                        const uint8_t *msg, size_t mlen, uint8_t out[32]) {
    uint8_t k[64], ipad[64], opad[64], inner[32];
    Sha256Ctx c;
    if (klen > 64) {
        sha256_init(&c); sha256_update(&c, key, klen); sha256_final(&c, k);
        klen = 32;
    } else {
        memcpy(k, key, klen);
    }
    if (klen < 64) memset(k + klen, 0, 64 - klen);
    for (int i = 0; i < 64; i++) { ipad[i] = k[i] ^ 0x36; opad[i] = k[i] ^ 0x5c; }
    sha256_init(&c); sha256_update(&c, ipad, 64); sha256_update(&c, msg, mlen);
    sha256_final(&c, inner);
    sha256_init(&c); sha256_update(&c, opad, 64); sha256_update(&c, inner, 32);
    sha256_final(&c, out);
}

static void hex_encode(const uint8_t *src, size_t n, char *dst) {
    static const char hexd[] = "0123456789abcdef";
    for (size_t i = 0; i < n; i++) {
        dst[i*2]   = hexd[src[i] >> 4];
        dst[i*2+1] = hexd[src[i] & 15];
    }
    dst[n*2] = 0;
}

/* Reply to a received authchal <nonce-hex> challenge with the HMAC-SHA256
 * of "ms-auth:"+nonce keyed by the solver password (which never leaves the
 * machine).  Runs on the network thread. */
static void net_answer_challenge(const char *nonce_hex) {
    char pass[128];
    uint8_t mac[32];
    char resp[65];
    char msg[256];
    char line[384];
    solver_creds(NULL, 0, pass, sizeof(pass));
    _snprintf(msg, sizeof(msg), AUTH_PREFIX "%s", nonce_hex);
    hmac_sha256((const uint8_t *)pass, strlen(pass),
                (const uint8_t *)msg, strlen(msg), mac);
    hex_encode(mac, 32, resp);
    _snprintf(line, sizeof(line), "authresp %s", resp);
    sendbuf_append(line, 0);
}

static void solver_creds(char *user, size_t usz, char *pass, size_t psz) {
    EnterCriticalSection(&g_qlock);
    if (user && usz > 0) {
        strncpy(user, g_solver_user, usz - 1); user[usz - 1] = 0;
    }
    if (pass && psz > 0) {
        strncpy(pass, g_solver_pass, psz - 1); pass[psz - 1] = 0;
    }
    LeaveCriticalSection(&g_qlock);
}

static int solver_wanted(void) {
    int wanted = 0;
    EnterCriticalSection(&g_qlock);
    if (g_solver_user[0] && g_solver_pass[0]) wanted = 1;
    LeaveCriticalSection(&g_qlock);
    return wanted;
}

/* A solver request may be forwarded only once the challenge-response
 * handshake succeeded (or when no credentials are configured, in which case
 * the server denies it anyway, so we do not even send it). */
static int solver_ready(void) {
    if (!solver_wanted()) return 0;
    return InterlockedCompareExchange(&g_auth_state, AUTH_OK, AUTH_OK) == AUTH_OK;
}

/* Begin the auth handshake on a freshly connected socket. */
static void auth_start(void) {
    char user[64];
    char line[80];
    solver_creds(user, sizeof(user), NULL, 0);
    _snprintf(line, sizeof(line), "auth %s", user);
    sendbuf_append(line, 0);
    InterlockedExchange(&g_auth_state, AUTH_WAIT_CHAL);
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
            InterlockedIncrement64(&g_stats_seeds);
            /* Only apply a server-pushed seed to the live board while a
             * remote-sim session (reqseed/reqbatch/requntil) is active.
             * The server broadcasts seeds at the producer rate to every
             * connected client; applying them passively would reset the
             * player's board mid-game.  Outside a session they are still
             * counted (g_stats_seeds) but never touch the game. */
            if (g_sim_session && g_sink)
                g_sink(diff, seed);
        }
    } else if (stricmp(argv[0], "outcome") == 0 && argc >= 4) {
        InterlockedIncrement64(&g_stats_outcomes);
        if (atoi(argv[3]) == 1)
            InterlockedIncrement64(&g_stats_wins);
    } else if (stricmp(argv[0], "authchal") == 0 && argc == 2) {
        if (InterlockedCompareExchange(&g_auth_state, AUTH_WAIT_OK, AUTH_WAIT_CHAL) == AUTH_WAIT_CHAL) {
            net_answer_challenge(argv[1]);
        }
    } else if (stricmp(argv[0], "authok") == 0) {
        if (InterlockedCompareExchange(&g_auth_state, AUTH_OK, AUTH_WAIT_OK) == AUTH_WAIT_OK) {
            OutputDebugStringA("network.c: solver authenticated\n");
        }
    } else if (stricmp(argv[0], "autherr") == 0) {
        InterlockedExchange(&g_auth_state, AUTH_NONE);
        OutputDebugStringA("network.c: solver auth rejected by server\n");
    } else if (stricmp(argv[0], "reqdone") == 0 ||
               stricmp(argv[0], "reqdenied") == 0 ||
               stricmp(argv[0], "lossfound") == 0 ||
               stricmp(argv[0], "noloss") == 0) {
        /* the server closed out a request (or refused it): end the sim
         * session so subsequent broadcast seeds stop resetting the board. */
        InterlockedExchange(&g_sim_session, 0);
        if (stricmp(argv[0], "reqdenied") == 0 && g_ui_hwnd)
            PostMessage(g_ui_hwnd, WM_APP_SOLVER_DENIED, 0, 0);
    } else if (stricmp(argv[0], "lbtop") == 0 && (argc == 2 || argc == 3)) {
        /* header of a top-list reply: lbtop <count> | lbtop <diff> <count> */
        if (g_lb_hwnd)
            PostMessage(g_lb_hwnd, WM_APP_LB_ENTRY, LB_EV_START, 0);
    } else if (stricmp(argv[0], "lbentry") == 0 && argc == 6) {
        NetLbEntryMsg *m = (NetLbEntryMsg *)malloc(sizeof(NetLbEntryMsg));
        if (m) {
            memset(m, 0, sizeof(*m));
            m->rank = atoi(argv[1]);
            strncpy(m->diff, argv[2], sizeof(m->diff) - 1);
            strncpy(m->name, argv[3], sizeof(m->name) - 1);
            m->time_ms = atoi(argv[4]);
            m->ts = _strtoi64(argv[5], NULL, 10);
            if (g_lb_hwnd) {
                if (!PostMessage(g_lb_hwnd, WM_APP_LB_ENTRY, LB_EV_ENTRY,
                                 (LPARAM)m))
                    free(m);
            } else {
                free(m);
            }
        }
    } else if (stricmp(argv[0], "lbdone") == 0) {
        if (g_lb_hwnd)
            PostMessage(g_lb_hwnd, WM_APP_LB_END, 0, 0);
    }
    /* welcome / stats / auth / anything else: ignored or handled above */
}

/* Extract and dispatch complete lines; keep any partial line buffered. */
static void dispatch_lines(char *buf, int *used) {
    int i = 0;
    while (i < *used) {
        int j = i;
        while (j < *used && buf[j] != '\n') j++;
        if (j >= *used) break;              /* partial line: wait for more */
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
/* outbound sending (non-blocking, WSAEWOULDBLOCK-safe)                */
/* ------------------------------------------------------------------ */
static int sendbuf_append(const char *line, int metric_lines) {
    size_t n = strlen(line);
    if (g_send_len + (int)n + 2 > SEND_MAX) {
        /* the whole batch does not fit: count it dropped, same accounting
         * path as queue_push()'s overflow case, instead of a bare return */
        if (metric_lines > 0)
            InterlockedExchangeAdd64(&g_stats_dropped, metric_lines);
        return 0;
    }
    memcpy(g_send + g_send_len, line, n);
    g_send_len += (int)n;
    g_send[g_send_len++] = '\n';
    g_send_metric_lines += metric_lines;
    return 1;
}

/* Send as much of g_send as the socket accepts.  Returns 0 on socket error.
 * When the buffer is fully drained, every metric line in it is confirmed
 * delivered and credited to g_stats_sent. */
static int flush_sendbuf(SOCKET s) {
    while (g_send_off < g_send_len) {
        int n = send(s, g_send + g_send_off, g_send_len - g_send_off, 0);
        if (n == SOCKET_ERROR) {
            int e = WSAGetLastError();
            if (e == WSAEWOULDBLOCK) return 1;   /* try again later */
            return 0;
        }
        g_send_off += n;
    }
    g_send_off = g_send_len = 0;
    if (g_send_metric_lines > 0) {
        /* all bytes flushed, so these metric lines reached the socket */
        InterlockedExchangeAdd64(&g_stats_sent, g_send_metric_lines);
        g_send_metric_lines = 0;
    }
    return 1;
}

/* Pull queued metrics into the send buffer and flush. */
static int flush_pending(SOCKET s) {
    char batch[SEND_MAX];
    int n = queue_drain(batch, sizeof(batch));
    if (n > 0) {
        int m = 0;
        for (int i = 0; i < n; i++)
            if (batch[i] == '\n') m++;
        batch[n] = 0;
        sendbuf_append(batch, m);   /* dropped+counted on overflow */
    }
    return flush_sendbuf(s);
}

static void heartbeat(SOCKET s) {
    char line[96];
    _snprintf(line, sizeof(line), "metric heartbeat t=%llu", net_now_ms());
    sendbuf_append(line, 0);        /* heartbeats are not counted as metrics */
    (void)s;
}

/* ------------------------------------------------------------------ */
/* HTTP(S) transport (WinHTTP)                                         */
/* ------------------------------------------------------------------ */

static const char *const HTTP_POST_HEADERS =
    "Content-Type: application/octet-stream\r\n";

/* Growable response buffer (heap): http_exchange reallocs as the body is
 * read, so no response is truncated. */
typedef struct {
    char  *data;
    size_t len;
    size_t cap;
} HttpBuf;

static void http_buf_free(HttpBuf *b) {
    free(b->data);
    b->data = NULL;
    b->len = b->cap = 0;
}

/* One synchronous WinHTTP request/response on the network thread.  Returns
 *   1  a 2xx response; its body is copied into *out (NUL-terminated).
 *      *cursor is set from the X-Ms-Cursor header when non-NULL and present.
 *   0  the server answered with a non-2xx status (endpoint reachable).
 *  -1  transport failure (DNS / connect / TLS / send / receive): the caller
 *      tears the session down and retries.
 * WinHTTP does all TLS via SChannel; HTTP/2 is never negotiated (the server
 * requires HTTP/1.x), and the connection is closed after each exchange,
 * matching the per-request Connection: close contract. */
static int http_exchange(const char *method, const char *path,
                         const char *headers, const char *body,
                         unsigned long long *cursor, HttpBuf *out) {
    WCHAR wmethod[8], wpath[320], whost[256], wheaders[512];
    HINTERNET h = NULL, c = NULL, r = NULL;
    DWORD status = 0, status_len = sizeof status;
    DWORD flags = (g_http_mode == 2) ? WINHTTP_FLAG_SECURE : 0;
    DWORD body_len = body ? (DWORD)strlen(body) : 0;
    BOOL got_status = FALSE;
    int ret = -1;

    if (!method || !path || !out) return -1;
    MultiByteToWideChar(CP_UTF8, 0, method, -1, wmethod, 8);
    MultiByteToWideChar(CP_UTF8, 0, path, -1, wpath, 320);
    MultiByteToWideChar(CP_UTF8, 0, g_host, -1, whost, 256);

    h = WinHttpOpen(L"MinesweeperClassic/1.0",
                    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                    WINHTTP_NO_PROXY_NAME, WINHTTP_NO_PROXY_BYPASS, 0);
    if (!h) return -1;
    WinHttpSetTimeouts(h, 10000, 10000, 20000, 30000);
    c = WinHttpConnect(h, whost, (INTERNET_PORT)g_port, 0);
    if (!c) goto done;
    r = WinHttpOpenRequest(c, wmethod, wpath, NULL, WINHTTP_NO_REFERER,
                           WINHTTP_DEFAULT_ACCEPT_TYPES, flags);
    if (!r) goto done;

    if (g_https_insecure) {
        DWORD secflags = SECURITY_FLAG_IGNORE_UNKNOWN_CA |
                         SECURITY_FLAG_IGNORE_CERT_DATE_INVALID |
                         SECURITY_FLAG_IGNORE_CERT_CN_INVALID |
                         SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE;
        WinHttpSetOption(r, WINHTTP_OPTION_SECURITY_FLAGS,
                         &secflags, sizeof secflags);
    }

    {
        LPCWSTR wh = WINHTTP_NO_ADDITIONAL_HEADERS;
        if (headers && *headers) {
            MultiByteToWideChar(CP_UTF8, 0, headers, -1, wheaders, 512);
            wh = wheaders;
        }
        if (!WinHttpSendRequest(r, wh, (DWORD)-1L,
                                body ? (LPVOID)body : WINHTTP_NO_REQUEST_DATA,
                                body_len, body_len, 0))
            goto done;
    }
    if (!WinHttpReceiveResponse(r, NULL)) goto done;

    got_status = WinHttpQueryHeaders(r, WINHTTP_QUERY_STATUS_CODE |
                                          WINHTTP_QUERY_FLAG_NUMBER,
                                     WINHTTP_HEADER_NAME_BY_INDEX,
                                     &status, &status_len,
                                     WINHTTP_NO_HEADER_INDEX);
    if (!got_status || status < 200 || status >= 300) {
        ret = got_status ? 0 : -1;
        goto done;
    }
    ret = 1;

    if (cursor) {
        WCHAR wval[64];
        DWORD vsz = sizeof wval;
        if (WinHttpQueryHeaders(r, WINHTTP_QUERY_CUSTOM, L"X-Ms-Cursor",
                                wval, &vsz, WINHTTP_NO_HEADER_INDEX))
            *cursor = _wcstoui64(wval, NULL, 10);
    }

    /* read the whole body (Connection: close ends it) */
    {
        size_t used = 0;
        for (;;) {
            DWORD avail = 0, got = 0;
            if (!WinHttpQueryDataAvailable(r, &avail) || avail == 0) break;
            if (avail > 4096) avail = 4096;
            if (used + avail + 1 > out->cap) {
                size_t ncap = out->cap ? out->cap * 2 : 8192;
                while (ncap < used + avail + 1) ncap *= 2;
                if (ncap > (1u << 24)) break;       /* 16 MB hard cap */
                char *np = (char *)realloc(out->data, ncap);
                if (!np) break;
                out->data = np;
                out->cap = ncap;
            }
            if (!WinHttpReadData(r, out->data + used, avail, &got) || got == 0)
                break;
            used += got;
        }
        if (out->data) out->data[used] = 0;
        out->len = used;
    }

done:
    if (r) WinHttpCloseHandle(r);
    if (c) WinHttpCloseHandle(c);
    if (h) WinHttpCloseHandle(h);
    return ret;
}

/* Append one protocol line + '\n' to an outbound HTTP body.  Counts the
 * line; on overflow it is dropped (same accounting as the raw-TCP queue). */
static void http_body_append(char *buf, size_t cap, int *n, const char *line) {
    size_t len = strlen(line);
    size_t cur = strlen(buf);
    if (cur + len + 2 > cap) {
        InterlockedIncrement64(&g_stats_dropped);
        return;
    }
    memcpy(buf + cur, line, len);
    cur += len;
    buf[cur++] = '\n';
    buf[cur] = 0;
    (*n)++;
}

/* Build the /ms-sim/lbtop query from a `lbtop [<diff>] <count>` line. */
static void http_lbtop_query(const char *line, char *out, size_t cap) {
    char tmp[64];
    char *save = NULL, *tok;
    char *diff = NULL, *count = NULL;
    strncpy(tmp, line, sizeof(tmp) - 1);
    tmp[sizeof(tmp) - 1] = 0;
    tok = strtok_r(tmp, " \t", &save);      /* "lbtop" */
    if (tok) {
        tok = strtok_r(NULL, " \t", &save);
        if (tok) {
            if (parse_diff_name_t(tok) >= 0) {
                diff = tok;
                tok = strtok_r(NULL, " \t", &save);
                if (tok) count = tok;
            } else {
                count = tok;
            }
        }
    }
    if (count)
        _snprintf(out, cap, "?count=%s", count);
    else
        _snprintf(out, cap, "?count=10");
    if (diff)
        _snprintf(out, cap, "?diff=%s&count=%s", diff, count ? count : "10");
}

/* Hand a complete HTTP response body to the shared line parser. */
static void dispatch_response(char *body) {
    int used = (int)strlen(body);
    if (used > 0) dispatch_lines(body, &used);
}

/* Capture an `authchal <nonce>` response body into g_auth_nonce. */
static int http_parse_authchal(const char *body) {
    const char *p;
    size_t len;
    if (_strnicmp(body, "authchal ", 9) != 0) return 0;
    p = body + 9;
    while (*p == ' ' || *p == '\t') p++;
    len = strlen(p);
    while (len && (p[len - 1] == '\n' || p[len - 1] == '\r' ||
                   p[len - 1] == ' ' || p[len - 1] == '\t')) len--;
    if (len == 0 || len >= sizeof(g_auth_nonce)) return 0;
    memcpy(g_auth_nonce, p, len);
    g_auth_nonce[len] = 0;
    return 1;
}

/* Run the HTTP(S) session: poll once per HTTP_LOOP_MS.  Returns when the
 * session must be torn down (transport failure, shutdown, or a newer
 * session superseded it via net_telemetry_start). */
static void http_run(LONG gen) {
    unsigned long long last_beat = 0;
    int wanted = solver_wanted();

    g_connected = 1;
    g_connected_at_ms = net_now_ms();
    InterlockedExchange(&g_sim_session, 0);
    InterlockedExchange(&g_auth_state, AUTH_NONE);

    while (g_running) {
        char batch[SEND_MAX];
        char headers[512];
        HttpBuf resp = {0};
        int n, transport_ok = 1;
        int metric_n = 0, req_n = 0, lb_n = 0;
        char metric_body[SEND_MAX + 64], req_body[SEND_MAX + 64];
        char lb_body[SEND_MAX + 64], lbtop_query[96];

        /* a telemetry off/on toggle may have superseded this session while
         * a WinHTTP call was blocking: bail so we never run two senders */
        if (InterlockedCompareExchange(&g_session_gen, gen, gen) != gen) break;

        Sleep(HTTP_LOOP_MS);

        /* connectivity probe: with credentials this also refreshes the
         * challenge nonce (the server's nonce is single-use) */
        if (wanted) {
            char user[64];
            solver_creds(user, sizeof(user), NULL, 0);
            _snprintf(batch, sizeof(batch), "auth %s", user);
            int r = http_exchange("POST", "/ms-sim/auth", HTTP_POST_HEADERS,
                                  batch, NULL, &resp);
            if (r == -1) {
                transport_ok = 0;
            } else if (r == 1) {
                if (http_parse_authchal(resp.data)) {
                    InterlockedExchange(&g_auth_state, AUTH_OK);
                } else if (_strnicmp(resp.data, "autherr", 7) == 0) {
                    InterlockedExchange(&g_auth_state, AUTH_NONE);
                }
            }
            if (!transport_ok) break;
        }

        /* drain the outbound queue once; lines leave the queue now and, if
         * a request below fails, are counted dropped (mirrors the raw-TCP
         * session where unflushed bytes die with the connection) */
        n = queue_drain(batch, sizeof(batch));
        metric_body[0] = req_body[0] = lb_body[0] = lbtop_query[0] = 0;
        if (n > 0) {
            batch[n] = 0;
            char *save = NULL;
            char *line = strtok_r(batch, "\n", &save);
            while (line) {
                if (_strnicmp(line, "metric ", 7) == 0) {
                    http_body_append(metric_body, sizeof(metric_body),
                                     &metric_n, line);
                } else if (_strnicmp(line, "req", 3) == 0) {
                    http_body_append(req_body, sizeof(req_body), &req_n, line);
                } else if (_strnicmp(line, "lbscore ", 8) == 0) {
                    http_body_append(lb_body, sizeof(lb_body), &lb_n, line);
                } else if (_strnicmp(line, "lbtop", 5) == 0) {
                    if (lbtop_query[0] == 0)
                        http_lbtop_query(line, lbtop_query, sizeof(lbtop_query));
                } else {
                    InterlockedIncrement64(&g_stats_dropped);
                }
                line = strtok_r(NULL, "\n", &save);
            }
        }

        if (metric_n > 0) {
            int r = http_exchange("POST", "/ms-sim/metrics", HTTP_POST_HEADERS,
                                  metric_body, NULL, &resp);
            if (r == 1) {
                InterlockedExchangeAdd64(&g_stats_sent, metric_n);
            } else {
                InterlockedExchangeAdd64(&g_stats_dropped, metric_n);
                if (r == -1) { transport_ok = 0; break; }
            }
        }

        if (req_n > 0) {
            char user[64], pass[128], resp_hex[65], msg[256];
            uint8_t mac[32];
            int authed = 0;
            if (wanted) {
                solver_creds(user, sizeof(user), pass, sizeof(pass));
                _snprintf(batch, sizeof(batch), "auth %s", user);
                int r = http_exchange("POST", "/ms-sim/auth", HTTP_POST_HEADERS,
                                      batch, NULL, &resp);
                if (r == -1) {
                    InterlockedExchangeAdd64(&g_stats_dropped, req_n);
                    transport_ok = 0;
                    break;
                } else if (r == 1) {
                    if (http_parse_authchal(resp.data)) {
                        authed = 1;
                    } else if (_strnicmp(resp.data, "autherr", 7) == 0) {
                        InterlockedExchange(&g_auth_state, AUTH_NONE);
                        if (g_ui_hwnd)
                            PostMessage(g_ui_hwnd, WM_APP_SOLVER_DENIED, 0, 0);
                    }
                }
            }
            if (authed && g_auth_nonce[0]) {
                _snprintf(msg, sizeof(msg), AUTH_PREFIX "%s", g_auth_nonce);
                hmac_sha256((const uint8_t *)pass, strlen(pass),
                            (const uint8_t *)msg, strlen(msg), mac);
                hex_encode(mac, 32, resp_hex);
                _snprintf(headers, sizeof(headers),
                          "Content-Type: application/octet-stream\r\n"
                          "X-Ms-User: %s\r\n"
                          "X-Ms-Auth: %s\r\n", user, resp_hex);
                int r = http_exchange("POST", "/ms-sim/req", headers, req_body,
                                      NULL, &resp);
                if (r == 1) {
                    dispatch_response(resp.data);
                    InterlockedExchangeAdd64(&g_stats_sent, req_n);
                } else {
                    InterlockedExchangeAdd64(&g_stats_dropped, req_n);
                    if (r == -1) { transport_ok = 0; break; }
                    if (g_ui_hwnd)
                        PostMessage(g_ui_hwnd, WM_APP_SOLVER_DENIED, 0, 0);
                }
            } else {
                /* no (valid) credentials: the request is never delivered */
                InterlockedExchangeAdd64(&g_stats_dropped, req_n);
            }
        }

        if (lb_n > 0) {
            int r = http_exchange("POST", "/ms-sim/lbscore", HTTP_POST_HEADERS,
                                  lb_body, NULL, &resp);
            if (r == 1) {
                dispatch_response(resp.data);
                InterlockedExchangeAdd64(&g_stats_sent, lb_n);
            } else {
                InterlockedExchangeAdd64(&g_stats_dropped, lb_n);
                if (r == -1) { transport_ok = 0; break; }
            }
        }

        if (lbtop_query[0]) {
            char path[320];
            _snprintf(path, sizeof(path), "/ms-sim/lbtop%s", lbtop_query);
            int r = http_exchange("GET", path, NULL, NULL, NULL, &resp);
            if (r == 1) {
                dispatch_response(resp.data);
                InterlockedExchangeAdd64(&g_stats_sent, 1);
            } else {
                InterlockedExchangeAdd64(&g_stats_dropped, 1);
                if (r == -1) { transport_ok = 0; break; }
            }
        }

        /* seeds poll: pull any feed entries past the last cursor; the
         * cursor watermark keeps this lossless across reconnects */
        {
            char path[64];
            unsigned long long new_cursor = 0;
            _snprintf(path, sizeof(path), "/ms-sim/seeds?since=%llu",
                      g_seed_cursor);
            int r = http_exchange("GET", path, NULL, NULL, &new_cursor, &resp);
            if (r == 1) {
                dispatch_response(resp.data);
                if (new_cursor > g_seed_cursor) g_seed_cursor = new_cursor;
            } else if (r == -1) {
                transport_ok = 0;
            }
            if (!transport_ok) break;
        }

        /* heartbeat: a periodic metrics POST keeps the session alive even
         * when the player produced no metrics this round */
        if (net_now_ms() - last_beat >= BEAT_MS) {
            last_beat = net_now_ms();
            if (metric_n == 0) {
                char hb[96];
                _snprintf(hb, sizeof(hb), "metric heartbeat t=%llu",
                          net_now_ms());
                int r = http_exchange("POST", "/ms-sim/metrics",
                                      HTTP_POST_HEADERS, hb, NULL, &resp);
                if (r == -1) { transport_ok = 0; break; }
            }
        }
        http_buf_free(&resp);
    }
    g_connected = 0;
}

/* ------------------------------------------------------------------ */
/* connect + session loop (network thread)                             */
/* ------------------------------------------------------------------ */
static int tcp_connect(const char *host, unsigned short port, SOCKET *out) {
    struct addrinfo hints, *res = NULL, *ai;
    char portstr[16];
    SOCKET s = INVALID_SOCKET;
    int err = 0;

    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_INET;          /* IPv4, like the rest of the game */
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    _snprintf(portstr, sizeof(portstr), "%u", (unsigned)port);

    if (getaddrinfo(host, portstr, &hints, &res) != 0) return 0;

    for (ai = res; ai; ai = ai->ai_next) {
        u_long nb = 1;
        s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
        if (s == INVALID_SOCKET) continue;
        ioctlsocket(s, FIONBIO, &nb);          /* non-blocking */
        if (connect(s, ai->ai_addr, (int)ai->ai_addrlen) == SOCKET_ERROR) {
            if (WSAGetLastError() == WSAEWOULDBLOCK) {
                fd_set wf;
                struct timeval tv = { CONNECT_TO / 1000, (CONNECT_TO % 1000) * 1000 };
                FD_ZERO(&wf);
                FD_SET(s, &wf);
                if (select(0, NULL, &wf, NULL, &tv) > 0) {
                    int soerr = 0, len = sizeof(soerr);
                    getsockopt(s, SOL_SOCKET, SO_ERROR, (char *)&soerr, &len);
                    if (soerr == 0) err = 0;
                    else { err = soerr; }
                } else {
                    err = WSAETIMEDOUT;
                }
            } else {
                err = WSAGetLastError();
            }
        }
        if (err == 0 && s != INVALID_SOCKET) {
            char one = 1;
            setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
            freeaddrinfo(res);
            *out = s;
            return 1;
        }
        closesocket(s);
        s = INVALID_SOCKET;
        err = 0;
    }
    freeaddrinfo(res);
    *out = INVALID_SOCKET;
    return 0;
}

/* Run the connected session until EOF, an error, or shutdown. */
static int conn_loop(SOCKET s) {
    unsigned long long last_beat = 0;
    g_connected = 1;
    g_connected_at_ms = net_now_ms();
    /* a fresh connection has no in-flight request: never carry a stale
     * remote-sim session across a reconnect */
    InterlockedExchange(&g_sim_session, 0);
    InterlockedExchange(&g_auth_state, AUTH_NONE);
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
        have_out = (g_send_off < g_send_len) || (g_q_count > 0);
        if (have_out) FD_SET(s, &wf);

        r = select(0, &rf, &wf, NULL, &tv);
        if (r == SOCKET_ERROR) {
            if (!g_running) break;
            break;
        }
        if (r > 0 && FD_ISSET(s, &wf)) {
            if (!flush_pending(s)) break;
        } else if (have_out) {
            if (!flush_sendbuf(s)) break;   /* drain leftovers */
        }
        if (r > 0 && FD_ISSET(s, &rf)) {
            if (g_recv_len >= (int)sizeof(g_recv) - 1) {
                /* one inbound line filled the buffer without a newline.  The
                 * socket is healthy, but recv(...,0) below would read as a
                 * graceful close and tear the whole session down (looking
                 * like random disconnects).  Discard the poisoned partial
                 * line and keep the connection. */
                OutputDebugStringA("network.c: inbound line exceeded recv "
                                   "buffer; discarding\n");
                g_recv_len = 0;
            }
            int n = recv(s, g_recv + g_recv_len,
                         (int)sizeof(g_recv) - 1 - g_recv_len, 0);
            if (n <= 0) break;
            g_recv_len += n;
            g_recv[g_recv_len] = 0;
            dispatch_lines(g_recv, &g_recv_len);
        }

        if (net_now_ms() - last_beat >= BEAT_MS) {
            last_beat = net_now_ms();
            heartbeat(s);
            if (!flush_sendbuf(s)) break;
        }
    }
    g_connected = 0;
    return 0;
}

static DWORD WINAPI telemetry_thread(LPVOID arg) {
    LONG gen = (LONG)(intptr_t)arg;
    while (g_running) {
        InterlockedIncrement64(&g_stats_attempts);
        if (g_http_mode != 0) {
            http_run(gen);
            g_send_off = g_send_len = 0;
            if (g_send_metric_lines > 0) {
                InterlockedExchangeAdd64(&g_stats_dropped, g_send_metric_lines);
                g_send_metric_lines = 0;
            }
            if (!g_running) break;
        } else {
            SOCKET s = INVALID_SOCKET;
            if (tcp_connect(g_host, g_port, &s)) {
                g_sock = s;                 /* visible to net_telemetry_stop */
                conn_loop(s);
                if (g_sock == s) g_sock = INVALID_SOCKET;
                if (g_running) closesocket(s);  /* natural disconnect */
                /* else net_telemetry_stop() already closed the socket */
                g_send_off = g_send_len = 0;
                if (g_send_metric_lines > 0) {
                    /* these lines left the queue but never reached the socket
                     * before the connection died: they are lost, not "sent",
                     * so count them as dropped (metrics_dropped is then a real
                     * "did we lose data" signal). */
                    InterlockedExchangeAdd64(&g_stats_dropped, g_send_metric_lines);
                    g_send_metric_lines = 0;
                }
                if (!g_running) break;
            }
        }
        /* back off between attempts; also yields on shutdown */
        for (int i = 0; i < RETRY_MS / 50 && g_running; i++)
            Sleep(50);
    }
    return 0;
}

/* ------------------------------------------------------------------ */
/* public API                                                          */
/* ------------------------------------------------------------------ */
int net_telemetry_start(const char *host, unsigned short port) {
    if (g_running) return 1;                 /* already running */
    if (!host || !*host || port == 0) return 0;

    if (!g_wsa) {
        WSADATA wsa;
        if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return 0;
        g_wsa = 1;
    }

    strncpy(g_host, host, sizeof(g_host) - 1);
    g_host[sizeof(g_host) - 1] = 0;
    g_port = port;
    g_sock = INVALID_SOCKET;
    g_connected = 0;
    g_send_off = g_send_len = 0;
    g_recv_len = 0;

    InitializeCriticalSection(&g_qlock);
    InterlockedExchange(&g_qlock_ready, 1);
    g_q_head = g_q_count = 0;

    InterlockedExchange(&g_running, 1);
    {
        LONG gen = InterlockedIncrement(&g_session_gen);
        g_thread = CreateThread(NULL, 0, telemetry_thread,
                                (LPVOID)(intptr_t)gen, 0, NULL);
    }
    if (!g_thread) {
        InterlockedExchange(&g_running, 0);
        DeleteCriticalSection(&g_qlock);
        if (g_wsa) { WSACleanup(); g_wsa = 0; }
        return 0;
    }
    return 1;
}

void net_telemetry_stop(void) {
    if (!g_running) return;
    InterlockedExchange(&g_running, 0);
    if (g_sock != INVALID_SOCKET) {
        closesocket(g_sock);            /* wake the select loop */
        g_sock = INVALID_SOCKET;
    }
    if (g_thread) {
        WaitForSingleObject(g_thread, 3000);
        CloseHandle(g_thread);
        g_thread = NULL;
    }
    DeleteCriticalSection(&g_qlock);
    if (g_wsa) { WSACleanup(); g_wsa = 0; }
}

int net_telemetry_active(void) {
    return g_running ? 1 : 0;
}

/* HTTP(S) transport selection for the telemetry session.  mode 0 (default)
 * is the raw TCP stream; 1 is plain HTTP and 2 is HTTPS (both via WinHTTP,
 * which terminates TLS with SChannel).  Set before net_telemetry_start(). */
void net_set_http_mode(int mode) {
    g_http_mode = (mode == 1 || mode == 2) ? mode : 0;
}

/* When non-zero, --telemetry-https skips WinHTTP certificate validation
 * (debug/testing only; production endpoints use a CA-signed certificate). */
void net_set_https_insecure(int on) {
    g_https_insecure = on ? 1 : 0;
}

/* ---------- obfuscated default endpoint ---------- */

static int b64_val(unsigned char c) {
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}

static size_t b64_decode(const char *in, char *out, size_t outsz) {
    size_t o = 0, i;
    unsigned v = 0;
    int bits = 0;
    for (i = 0; in[i] && in[i] != '='; i++) {
        int d = b64_val((unsigned char)in[i]);
        if (d < 0) continue;
        v = (v << 6) | (unsigned)d;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            if (o < outsz) out[o++] = (char)((v >> bits) & 0xFF);
        }
    }
    return o;
}

int net_endpoint_default(char *host, size_t hsz, unsigned short *port) {
    /* "135.125.79.15" / "28571", base64 so the deployed server address does
     * not sit in the source or binary as a readable string.  Obfuscation
     * only: the value is recovered at runtime and sent in the clear. */
    static const char HOST_B64[] = "MTM1LjEyNS43OS4xNQ==";
    static const char PORT_B64[] = "Mjg1NzE=";
    char pbuf[16];
    size_t n, i;
    unsigned p = 0;
    if (!host || hsz == 0) return 0;
    n = b64_decode(HOST_B64, host, hsz - 1);
    host[n] = 0;
    if (n == 0) return 0;
    if (port) {
        n = b64_decode(PORT_B64, pbuf, sizeof(pbuf) - 1);
        pbuf[n] = 0;
        for (i = 0; i < n; i++) {
            if (pbuf[i] < '0' || pbuf[i] > '9') return 0;
            p = p * 10 + (unsigned)(pbuf[i] - '0');
        }
        *port = (unsigned short)p;
    }
    return 1;
}

void net_set_seed_sink(net_seed_sink_fn fn) {
    g_sink = fn;
}

void net_set_lb_window(HWND hwnd) {
    g_lb_hwnd = hwnd;
}

void net_set_notify_hwnd(HWND hwnd) {
    g_ui_hwnd = hwnd;
}

void net_set_solver_creds(const char *user, const char *pass) {
    /* g_qlock may not be initialized yet (called before net_telemetry_start).
     * Before the telemetry thread exists there is no other writer, so a
     * lock-free copy is safe; once started the lock is ready. */
    if (InterlockedCompareExchange(&g_qlock_ready, 1, 1)) {
        EnterCriticalSection(&g_qlock);
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
        LeaveCriticalSection(&g_qlock);
    } else {
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
    }
}

int net_solver_creds_set(void) {
    return solver_wanted();
}

void net_send_score(const char *name, int diff, int time_ms) {
    char line[128];
    if (!g_running || !name || !*name) return;
    /* leaderboard is scored for the three preset difficulties only */
    if (diff < 0 || diff >= 3) return;
    _snprintf(line, sizeof(line), "lbscore %s %s %d",
              name, g_diff_names_t[diff], time_ms);
    net_queue_line(line);
}

void net_request_lbtop(int count) {
    char line[64];
    if (!g_running || count <= 0) return;
    _snprintf(line, sizeof(line), "lbtop %d", count);
    net_queue_line(line);
}

void net_request_lbtop_diff(int diff, int count) {
    char line[64];
    if (!g_running || diff < 0 || diff >= 3 || count <= 0) return;
    _snprintf(line, sizeof(line), "lbtop %s %d", g_diff_names_t[diff], count);
    net_queue_line(line);
}

void net_get_stats(NetStats *st) {
    if (!st) return;
    memset(st, 0, sizeof(*st));
    st->connected = g_connected;
    st->connected_ms = g_connected
        ? (int)(net_now_ms() - g_connected_at_ms) : 0;
    st->attempts = (int)net_atomic_read(&g_stats_attempts);
    st->seeds_recv = net_atomic_read(&g_stats_seeds);
    st->outcomes_recv = net_atomic_read(&g_stats_outcomes);
    st->wins_recv = net_atomic_read(&g_stats_wins);
    st->metrics_sent = net_atomic_read(&g_stats_sent);
    st->metrics_dropped = net_atomic_read(&g_stats_dropped);
}
