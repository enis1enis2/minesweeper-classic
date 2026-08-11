/* diag_test.c - unit test for the crash-text sanitizer.
 *
 * Build (console, from the repo root, with mingw64 gcc on PATH):
 *   gcc -O2 -Wall -Wextra tools/diag_test.c -o build/diag_test.exe ^
 *       -lwinhttp -ladvapi32
 *
 * Note: this TU #includes ../src/diag.c directly, so diag.c must NOT be
 * passed on the command line too.
 */
#include <stdio.h>
#include <string.h>

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

static void expect(const char *profile_root, const char *install_dir,
                   const char *in, const char *want) {
    char out[DIAG_CRASH_MAX];
    diag_sanitize(profile_root, install_dir, in, out, sizeof out);
    if (strcmp(out, want) != 0) {
        printf("FAIL: sanitize\n  in:   %s\n  want: %s\n  got:  %s\n",
               in, want, out);
        failures++;
    }
}

int main(void) {
    const char *profile = "C:\\Users\\Bob";
    const char *install = "D:\\games\\classic\\Minesweeper";

    /* path under the user profile root -> <user>\... */
    expect(profile, install,
        "Exception 0xC0000005 at minesweeper-x64.exe+0x1234\n"
        "  C:\\Users\\Bob\\AppData\\Roaming\\Minesweeper\\logs\\crash.txt",
        "Exception 0xC0000005 at minesweeper-x64.exe+0x1234\n"
        "  <user>\\AppData\\Roaming\\Minesweeper\\logs\\crash.txt");

    /* system path not under profile/install -> <redacted>\<module> */
    expect(profile, install,
        "  C:\\Windows\\System32\\KERNELBASE.dll+0x3f2a1",
        "  <redacted>\\KERNELBASE.dll+0x3f2a1");

    /* install-dir path -> <install>\... */
    expect(profile, install,
        "  D:\\games\\classic\\Minesweeper\\minesweeper-x64.exe+0x9ab1",
        "  <install>\\minesweeper-x64.exe+0x9ab1");

    /* arbitrary other absolute path keeps only the final element */
    expect(profile, install,
        "in module D:\\other\\tools\\solver.dll offset 0x7c10",
        "in module <redacted>\\solver.dll offset 0x7c10");

    /* username appearing mid-path outside the profile root */
    expect(profile, install,
        "  E:\\stuff\\Bob\\file.txt",
        "  <redacted>\\file.txt");

    /* standalone username token -> <user> */
    expect(profile, install,
        "running as Bob now",
        "running as <user> now");

    /* lone drive root */
    expect(profile, install,
        "path C:\\ end",
        "path <redacted> end");

    /* UNC share */
    expect(profile, install,
        "load \\\\server\\share\\mod.dll",
        "load <redacted>\\mod.dll");

    /* plain text passes through untouched */
    expect(profile, install,
        "hello world 0x1234",
        "hello world 0x1234");

    /* install dir inside profile (portable run) still resolves */
    expect(profile, "C:\\Users\\Bob\\Downloads\\Minesweeper",
        "0xC0000409 in module C:\\Users\\Bob\\Downloads\\Minesweeper\\minesweeper-x64.exe at 0x7c10",
        "0xC0000409 in module <install>\\minesweeper-x64.exe at 0x7c10");

    if (failures) {
        printf("%d FAILURE(S)\n", failures);
        return 1;
    }
    printf("all sanitize tests passed\n");
    return 0;
}
