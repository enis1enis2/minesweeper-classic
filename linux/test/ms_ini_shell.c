/*
 * ms_ini_shell.c - regression test: config path must never be shelled out.
 *
 * ms_ini.c derives its config path from $XDG_CONFIG_HOME / $HOME, which the
 * user controls.  The original mkdirs_for_path() built
 *   system("mkdir -p '<dir>'")
 * so a single quote in the home directory broke out of the quoting and
 * executed arbitrary shell commands on the first config write.  This test
 * sets a hostile XDG_CONFIG_HOME containing a `;touch MARKER;` payload and
 * verifies that:
 *   1. the config directory is still created (mkdir works),
 *   2. the config file round-trips,
 *   3. MARKER is never created in the working directory (no shell ran).
 *
 * Build/run:
 *   gcc -std=c11 -I.. ms_ini.c ms_ini_shell.c -o ms_ini_shell_test
 *   ./ms_ini_shell_test
 * (also wired up as `make test` in linux/Makefile).
 *
 * MIT License
 */
#include "ms_ini.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

static int failures = 0;
#define CHECK(cond, msg)                                                        \
    do {                                                                        \
        if (!(cond)) {                                                          \
            fprintf(stderr, "FAIL: %s\n", msg);                                 \
            failures++;                                                         \
        } else {                                                                \
            printf("ok:   %s\n", msg);                                          \
        }                                                                       \
    } while (0)

int main(void) {
    /* A slash-free marker name: if the old system() path ran, this file
     * would appear in the process CWD.  (No slashes, so it cannot confuse
     * the mkdir() component loop.) */
    const char *marker = "MS_INI_PWNED";
    char env[2048];
    char ini_path[4096];

    remove(marker);
    /* Hostile XDG_CONFIG_HOME: single quote + shell payload.  ms_ini_path()
     * caches on first use, so the whole test runs against this path. */
    snprintf(env, sizeof(env), "XDG_CONFIG_HOME=/tmp/ms_ini_shell_test/we'ird;touch %s;&dir", marker);
    putenv(env);
    putenv("HOME=/tmp/ms_ini_shell_test/we'ird;touch MS_INI_PWNED;&dir");

    ms_ini_set_str("settings", "theme", "dark");

    snprintf(ini_path, sizeof(ini_path), "%s/minesweeper.ini", getenv("XDG_CONFIG_HOME"));
    struct stat st;
    CHECK(stat(ini_path, &st) == 0, "config file created under hostile home dir");

    {
        char buf[64];
        int got = ms_ini_get_str("settings", "theme", NULL, buf, sizeof(buf));
        CHECK(got && strcmp(buf, "dark") == 0, "roundtrip theme=dark");
    }

    CHECK(stat(marker, &st) != 0, "no shell command executed (marker absent)");

    /* Cleanup the hostile dirs (innermost first). */
    remove(ini_path);
    {
        char d1[2048];
        snprintf(d1, sizeof(d1), "%s", getenv("XDG_CONFIG_HOME"));
        rmdir(d1);
    }
    rmdir("/tmp/ms_ini_shell_test");
    remove(marker);

    if (failures) {
        fprintf(stderr, "%d failure(s)\n", failures);
        return 1;
    }
    printf("ms_ini shell-injection test: PASS\n");
    return 0;
}
