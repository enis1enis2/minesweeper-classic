/*
 * ms_main.c - entry point for the Linux Minesweeper client.
 *
 * Parses the command line (seeds, solver credentials, telemetry endpoint,
 * frontend selection), starts the shared core and dispatches to the X11,
 * terminal or headless frontend.  Mirrors the Win32 client's argument
 * semantics (--seed / --seed-custom / --telemetry / --no-telemetry /
 * --listen / --solver-*).
 *
 * MIT License
 */
#include "ms_core.h"
#include "ms_net.h"
#include "ms_endpoint.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int term_main(void);
int x11_main(void);

static char     g_telemetry_host[128];
static unsigned g_telemetry_port;

enum Frontend { FE_AUTO, FE_X11, FE_TERM, FE_HEADLESS };
static enum Frontend g_fe = FE_AUTO;

static void usage(const char *prog) {
    fprintf(stderr,
        "Usage: %s [options]\n"
        "\n"
        "Frontends:\n"
        "  --x11               X11 GUI (default when DISPLAY is set)\n"
        "  --term              terminal UI (default when stdin/stdout are a tty)\n"
        "  --headless          no UI; runs the CLI scripting server\n"
        "\n"
        "Seeds:\n"
        "  --seed <n>                one-shot seed override for the first board\n"
        "  --seed-custom <s>         one-shot hashed seed override\n"
        "  --seed <diff>:<n>         persistent seed slot for a difficulty\n"
        "  --seed-custom <diff>:<s>  persistent hashed slot (beginner/intermediate/expert)\n"
        "\n"
        "Telemetry (on by default):\n"
        "  --telemetry <host:port>   simulation/telemetry endpoint\n"
        "  --no-telemetry            disable the connection\n"
        "\n"
        "Solver credentials (used for the HMAC-SHA256 challenge):\n"
        "  --solver-user <user>  --solver-pass <pass>\n"
        "  --solver-config <file.json>\n"
        "  (or the MS_SOLVER_USER / MS_SOLVER_PASS environment variables)\n"
        "\n"
        "Scripting server:\n"
        "  --listen <port>       start the localhost CLI server\n"
        "\n"
        "  -h, --help            show this help\n",
        prog);
}

static void parse_telemetry_arg(const char *arg) {
    char host[128];
    const char *colon;
    size_t hlen;
    unsigned port;
    if (!arg || !*arg) return;
    colon = strchr(arg, ':');
    if (!colon) return;
    hlen = (size_t)(colon - arg);
    if (hlen == 0 || hlen >= sizeof(host)) return;
    memcpy(host, arg, hlen);
    host[hlen] = 0;
    port = (unsigned)atoi(colon + 1);
    if (port == 0 || port > 65535) return;
    strncpy(g_telemetry_host, host, sizeof(g_telemetry_host) - 1);
    g_telemetry_host[sizeof(g_telemetry_host) - 1] = 0;
    g_telemetry_port = port;
}

static void parse_frontend_and_telemetry(int argc, char **argv) {
    int i;
    /* default endpoint: obfuscated constant, decoded here (never started
     * otherwise means telemetry is off and no defaults were applied). */
    ms_endpoint_default_host(g_telemetry_host, sizeof(g_telemetry_host));
    g_telemetry_port = ms_endpoint_default_port();
    for (i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (strcmp(a, "--x11") == 0)            g_fe = FE_X11;
        else if (strcmp(a, "--term") == 0)      g_fe = FE_TERM;
        else if (strcmp(a, "--headless") == 0)  g_fe = FE_HEADLESS;
        else if (strcmp(a, "--no-telemetry") == 0) g_telemetry_port = 0;
        else if (strcmp(a, "--telemetry") == 0 && i + 1 < argc)
            parse_telemetry_arg(argv[++i]);
        else if (strncmp(a, "--telemetry=", 12) == 0)
            parse_telemetry_arg(a + 12);
    }
}

static int parse_listen_port(int argc, char **argv) {
    int i;
    for (i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (strcmp(a, "--listen") == 0 && i + 1 < argc)
            return atoi(argv[++i]);
        if (strncmp(a, "--listen=", 9) == 0)
            return atoi(a + 9);
    }
    return -1;
}

int main(int argc, char **argv) {
    int cli_port;
    int i;

    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "-h") == 0 || strcmp(argv[i], "--help") == 0) {
            usage(argv[0]);
            return 0;
        }
    }

    ms_events_init();
    ms_net_setup_sinks();

    leader_setup_solver(argc, argv);
    ms_parse_seed_args(argc, argv);

    /* initial board: classic beginner */
    game_reset(&g_presets[DIFF_BEGIN], 1);

    parse_frontend_and_telemetry(argc, argv);

    if (g_fe == FE_AUTO) {
        if (getenv("DISPLAY") && getenv("DISPLAY")[0])
            g_fe = FE_X11;
        else if (isatty(STDIN_FILENO))
            g_fe = FE_TERM;
        else
            g_fe = FE_HEADLESS;
    }

    /* telemetry is forced on by default (matches the Win32 client) */
    if (g_telemetry_port != 0)
        net_telemetry_start(g_telemetry_host, (unsigned short)g_telemetry_port);

    cli_port = parse_listen_port(argc, argv);
    if (cli_port > 0 && !cli_start(cli_port))
        fprintf(stderr, "failed to start the scripting server on port %d\n",
                cli_port);

    {
        int rc = 0;
        switch (g_fe) {
        case FE_X11:
            if (getenv("DISPLAY") && getenv("DISPLAY")[0])
                rc = x11_main();
            else {
                fprintf(stderr, "x11 frontend requested but DISPLAY is not set\n");
                rc = 1;
            }
            break;
        case FE_TERM:
            rc = term_main();
            break;
        case FE_HEADLESS:
        default: {
            /* drain the event pump forever; headless use is driven by the
             * CLI scripting server (--listen) */
            for (;;) {
                ms_loop_pump();
                game_tick();
                usleep(100000);
            }
        }
        }
        cli_stop();
        net_telemetry_stop();
        return rc;
    }
}
