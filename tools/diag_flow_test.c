/* diag_flow_test.c - unit test for the diagnostics opt-out gate and the
 * bounded transport retry loop.
 *
 * Build (console, from the repo root, with mingw64 gcc on PATH):
 *   gcc -O2 -Wall -Wextra -DDIAG_TEST_FAKE_POST \
 *       -DDIAG_SEND_ATTEMPTS=3 -DDIAG_RETRY_DELAY_MS=1 \
 *       tools/diag_flow_test.c -o build/diag_flow_test.exe \
 *       -lwinhttp -ladvapi32
 *
 * DIAG_TEST_FAKE_POST compiles out the real WinHTTP transport inside
 * ../src/diag.c so this test never touches the network (no production
 * contact).  The retry delay is shortened so the 3-attempt loops finish
 * in milliseconds instead of 10 s.
 *
 * Note: this TU #includes ../src/diag.c directly, so diag.c must NOT be
 * passed on the command line too.
 */
#include <stdio.h>
#include <string.h>

#define DIAG_TEST_FAKE_POST 1

/* diag.c's diag_send_thread calls http_post_json; the prod definition is
 * compiled out (DIAG_TEST_FAKE_POST) so we forward-declare our static fake. */
static int http_post_json(const char *host, const char *path,
                          const char *body);

#include "../src/diag.h"
#include "../src/diag.c"

static int failures = 0;

#define CHECK(cond)                                                       \
    do {                                                                  \
        if (!(cond)) {                                                    \
            printf("FAIL line %d: %s\n", __LINE__, #cond);                \
            failures++;                                                   \
        }                                                                 \
    } while (0)

/* ---------- fake transport ---------- */

static int fake_calls = 0;      /* total http_post_json invocations        */
static int fake_host_ok = 0;    /* calls that passed the real DIAG_HOST    */
static int fake_path_ok = 0;    /* calls that passed the real DIAG_PATH    */
static int fake_body_ok = 0;    /* calls with a plausible JSON body        */
static int fake_seq[8];
static int fake_n = 0;

static void fake_reset(const int *seq, int n) {
    int i;
    fake_calls = 0;
    fake_host_ok = fake_path_ok = fake_body_ok = 0;
    fake_n = n < (int)(sizeof fake_seq / sizeof fake_seq[0]) ? n : 8;
    for (i = 0; i < fake_n; i++) fake_seq[i] = seq[i];
}

/* Stand-in for diag.c's http_post_json (compiled out via
 * DIAG_TEST_FAKE_POST).  Returns scripted results so retry behaviour can
 * be asserted deterministically. */
static int http_post_json(const char *host, const char *path,
                          const char *body) {
    int res;
    fake_calls++;
    if (host && strcmp(host, DIAG_HOST) == 0) fake_host_ok++;
    if (path && strcmp(path, DIAG_PATH) == 0) fake_path_ok++;
    if (body && body[0] && strstr(body, "\"machine_id\":\"")) fake_body_ok++;
    res = (fake_calls <= fake_n) ? fake_seq[fake_calls - 1] : -1;
    return res;
}

/* Poll until the async send worker has drained (g_send_inflight back to 0). */
static int wait_idle(int timeout_ms) {
    while (timeout_ms > 0) {
        if (InterlockedCompareExchange(&g_send_inflight, 0, 0) == 0)
            return 1;
        Sleep(5);
        timeout_ms -= 5;
    }
    return 0;
}

static void test_opt_out_gate(void) {
    const int seq[3] = { -1, -1, -1 };
    fake_reset(seq, 3);
    diag_set_opt_out(1);
    diag_on_connected();                    /* must not send anything */
    CHECK(fake_calls == 0);
    CHECK(diag_opt_out() == 1);
    CHECK(InterlockedCompareExchange(&g_send_inflight, 0, 0) == 0);
}

static void test_opt_in_transient_retry(void) {
    const int seq[3] = { -1, -1, 1 };       /* 2 transport failures, then ok */
    fake_reset(seq, 3);
    diag_set_opt_out(0);
    diag_on_connected();
    CHECK(wait_idle(10000));
    CHECK(fake_calls == 3);                 /* retried after transient fail  */
    CHECK(fake_host_ok == 3 && fake_path_ok == 3);
    CHECK(fake_body_ok == 3);               /* every attempt carried a body  */
}

static void test_bounded_give_up(void) {
    const int seq[3] = { -1, -1, -1 };      /* never answers                */
    fake_reset(seq, 3);
    diag_on_connected();
    CHECK(wait_idle(10000));
    CHECK(fake_calls == DIAG_SEND_ATTEMPTS); /* bounded, not infinite        */
    CHECK(InterlockedCompareExchange(&g_send_inflight, 0, 0) == 0);
}

static void test_terminal_failure_no_retry(void) {
    const int seq[3] = { 0, -1, -1 };       /* HTTP non-2xx = endpoint reach */
    fake_reset(seq, 3);
    diag_on_connected();
    CHECK(wait_idle(10000));
    CHECK(fake_calls == 1);                 /* terminal, no retry            */
}

int main(void) {
    /* Bypass diag_init(): drive the module state directly.  A scratch ini
     * keeps cfg_get_int/cfg_set_int off the empty path. */
    g_ready = 1;
    _snprintf(g_ini_path, sizeof g_ini_path, "diag_flow_test.ini");

    test_opt_out_gate();
    test_opt_in_transient_retry();
    test_bounded_give_up();
    test_terminal_failure_no_retry();

    DeleteFileA(g_ini_path);
    if (failures) {
        printf("%d FAILURE(S)\n", failures);
        return 1;
    }
    printf("all diag flow tests passed\n");
    return 0;
}
