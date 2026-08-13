/*
 * ms_ini.c - minimal INI-style config persistence.
 *
 * The whole file is read on every get/set; it is tiny and the operation is
 * only performed on startup, on settings changes and on leaderboard wins, so
 * the simplicity is worth it.  Writes are atomic (temp file + rename).
 *
 * MIT License
 */
#include "ms_ini.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#if defined(_WIN32)
#include <direct.h> /* _mkdir */
#endif

static char g_path[1024];
static int  g_path_ready = 0;

const char *ms_ini_path(void) {
    if (g_path_ready) return g_path;
    {
        const char *xdg = getenv("XDG_CONFIG_HOME");
        const char *home = getenv("HOME");
        if (xdg && *xdg) {
            snprintf(g_path, sizeof(g_path), "%s/minesweeper.ini", xdg);
        } else if (home && *home) {
            snprintf(g_path, sizeof(g_path), "%s/.config/minesweeper.ini",
                     home);
        } else {
            snprintf(g_path, sizeof(g_path), "./minesweeper.ini");
        }
        g_path_ready = 1;
    }
    return g_path;
}

/* POSIX mkdir(path, mode); the Win32 CRT only takes the path. */
#if defined(_WIN32)
static int mkdir1(const char *path) { return _mkdir(path); }
#else
static int mkdir1(const char *path) { return mkdir(path, 0755); }
#endif

static void mkdirs_for_path(const char *path) {
    /* Create the parent directory if missing, one component at a time.
     * Never shell out (no system()/popen()): the path is derived from
     * user-controlled $HOME / $XDG_CONFIG_HOME, so quoting it into a shell
     * command line would be a command-injection hole on the first config
     * write (a single quote in the home dir breaks out of the quotes). */
    const char *slash = strrchr(path, '/');
    if (!slash || slash == path) return;
    char dir[1024];
    size_t n = (size_t)(slash - path);
    if (n >= sizeof(dir)) return;
    memcpy(dir, path, n);
    dir[n] = 0;
    if (!dir[0]) return;
    {
        char tmp[1024];
        size_t i = 0;
        if (n >= sizeof(tmp)) return;
        memcpy(tmp, dir, n + 1);
        /* skip any leading slashes so we mkdir absolute paths in place */
        while (tmp[i] == '/') i++;
        for (; tmp[i]; i++) {
            if (tmp[i] == '/') {
                tmp[i] = 0;
                if (mkdir1(tmp) != 0 && errno != EEXIST) return;
                tmp[i] = '/';
            }
        }
        if (mkdir1(tmp) != 0 && errno != EEXIST) return;
    }
}

/* Locate the [sec] header line index in `lines` (array of NUL-terminated
 * section names), or -1. */
static const char *section_header(const char *sec, const char *buf) {
    const char *p = buf;
    size_t seclen = strlen(sec);
    while (p && *p) {
        const char *nl = strchr(p, '\n');
        const char *line_end = nl ? nl : p + strlen(p);
        size_t linelen = (size_t)(line_end - p);
        if (linelen >= 3 && p[0] == '[') {
            const char *closing = strchr(p + 1, ']');
            if (closing && closing < line_end) {
                size_t namelen = (size_t)(closing - (p + 1));
                if (namelen == seclen && strncmp(p + 1, sec, seclen) == 0)
                    return p;
            }
        }
        if (!nl) break;
        p = nl + 1;
    }
    return NULL;
}

int ms_ini_get_str(const char *sec, const char *key, const char *def,
                   char *out, size_t outsz) {
    const char *path = ms_ini_path();
    FILE *f;
    char *buf = NULL;
    long flen;
    int found = 0;
    if (!outsz) return 0;
    out[0] = 0;

    f = fopen(path, "rb");
    if (!f) {
        if (def) { strncpy(out, def, outsz - 1); out[outsz - 1] = 0; }
        return 0;
    }
    if (fseek(f, 0, SEEK_END) != 0 || (flen = ftell(f)) < 0 ||
        flen > 1 << 20) {
        fclose(f);
        if (def) { strncpy(out, def, outsz - 1); out[outsz - 1] = 0; }
        return 0;
    }
    if (fseek(f, 0, SEEK_SET) != 0) { fclose(f); return 0; }
    buf = (char *)malloc((size_t)flen + 2);
    if (!buf) { fclose(f); return 0; }
    {
        size_t rd = fread(buf, 1, (size_t)flen, f);
        buf[rd] = 0;
    }
    fclose(f);

    {
        const char *secp = section_header(sec, buf);
        if (secp) {
            const char *p = secp;
            size_t klen = strlen(key);
            /* skip the [sec] header line itself, then scan its keys */
            {
                const char *nl = strchr(p, '\n');
                if (nl) p = nl + 1;
            }
            while (p && *p) {
                const char *nl = strchr(p, '\n');
                const char *line_end = nl ? nl : p + strlen(p);
                size_t linelen = (size_t)(line_end - p);
                if (linelen >= 3 && p[0] == '[') break;   /* next section */
                if (linelen > klen && strncmp(p, key, klen) == 0 &&
                    p[klen] == '=') {
                    const char *v = p + klen + 1;
                    const char *ve = line_end;
                    while (v < ve && (*v == ' ' || *v == '\t')) v++;
                    while (ve > v && (ve[-1] == ' ' || ve[-1] == '\t' ||
                                      ve[-1] == '\r')) ve--;
                    if (ve > v && *v == '"' && ve[-1] == '"') { v++; ve--; }
                    {
                        size_t n = (size_t)(ve - v);
                        if (n >= outsz) n = outsz - 1;
                        memcpy(out, v, n);
                        out[n] = 0;
                    }
                    found = 1;
                    break;
                }
                if (!nl) break;
                p = nl + 1;
            }
        }
    }
    free(buf);
    if (!found) {
        if (def) { strncpy(out, def, outsz - 1); out[outsz - 1] = 0; }
    }
    return found;
}

int ms_ini_get_int(const char *sec, const char *key, int def) {
    char buf[32];
    ms_ini_get_str(sec, key, NULL, buf, sizeof(buf));
    if (!buf[0]) return def;
    return atoi(buf);
}

/* Rewrite the file with key=value set (or added) under [sec]. */
static void ini_rewrite(const char *sec, const char *key, const char *value) {
    const char *path = ms_ini_path();
    FILE *f;
    char *buf = NULL;
    long flen;
    char *tmp = NULL;
    size_t tmpcap = 0, tmplen = 0;

    mkdirs_for_path(path);

    f = fopen(path, "rb");
    if (f) {
        if (fseek(f, 0, SEEK_END) == 0 && (flen = ftell(f)) >= 0 &&
            flen <= (1 << 20)) {
            if (fseek(f, 0, SEEK_SET) == 0) {
                buf = (char *)malloc((size_t)flen + 2);
                if (buf) {
                    size_t rd = fread(buf, 1, (size_t)flen, f);
                    buf[rd] = 0;
                }
            }
        }
        fclose(f);
    }
    if (!buf) { buf = (char *)calloc(1, 1); }

    {
        const char *secp = section_header(sec, buf);
        const char *p;
        int wrote_key = 0;

        /* helper to append bytes */
#define APPEND(s_, n_) do { \
            if (tmplen + (n_) + 2 > tmpcap) { \
                size_t nc = tmpcap ? tmpcap * 2 : 256; \
                char *nb = (char *)realloc(tmp, nc); \
                if (!nb) { free(buf); free(tmp); return; } \
                tmp = nb; tmpcap = nc; \
            } \
            memcpy(tmp + tmplen, (s_), (n_)); \
            tmplen += (n_); \
        } while (0)

        if (!secp) {
            /* no section: append a new one at the end (ensure trailing \n) */
            size_t blen = strlen(buf);
            if (blen && buf[blen - 1] != '\n') APPEND("\n", 1);
            char hdr[64];
            int hn = snprintf(hdr, sizeof(hdr), "\n[%s]\n", sec);
            if (hn > 0) APPEND(hdr, (size_t)hn);
            APPEND(key, strlen(key));
            APPEND("=", 1);
            APPEND(value, strlen(value));
            APPEND("\n", 1);
            wrote_key = 1;
        } else {
            /* walk the section, copying lines; replace the target key */
            p = buf;
            const char *sec_start = secp;
            while (p < secp) {   /* copy everything before [sec] */
                const char *nl = strchr(p, '\n');
                size_t n = nl ? (size_t)(nl - p + 1) : strlen(p);
                APPEND(p, n);
                p += n;
            }
            (void)sec_start;
            p = secp;
            /* copy [sec] header line */
            {
                const char *nl = strchr(p, '\n');
                size_t n = nl ? (size_t)(nl - p + 1) : strlen(p);
                APPEND(p, n);
                p += n;
            }
            /* walk the section body until the next section or EOF */
            while (p && *p && !(p[0] == '[')) {
                const char *nl = strchr(p, '\n');
                const char *line_end = nl ? nl : p + strlen(p);
                size_t linelen = (size_t)(line_end - p);
                int skip = 0;
                if (linelen > strlen(key) &&
                    strncmp(p, key, strlen(key)) == 0 &&
                    p[strlen(key)] == '=') {
                    skip = 1;   /* replace below */
                }
                if (!skip) APPEND(p, linelen);
                if (nl) APPEND("\n", 1);
                if (!nl) break;
                p = nl + 1;
            }
            if (!wrote_key) {
                APPEND(key, strlen(key));
                APPEND("=", 1);
                APPEND(value, strlen(value));
                APPEND("\n", 1);
                wrote_key = 1;
            }
            /* copy the remainder after the section */
            if (p && *p) APPEND(p, strlen(p));
#undef APPEND
        }
        if (!wrote_key) {   /* defensive: nothing written, bail */
            free(buf);
            free(tmp);
            return;
        }
    }

    {
        char tmp_path[1100];
        snprintf(tmp_path, sizeof(tmp_path), "%s.tmp", path);
        f = fopen(tmp_path, "wb");
        if (f) {
            if (fwrite(tmp, 1, tmplen, f) == tmplen)
                fclose(f);
            else {
                fclose(f);
                remove(tmp_path);
                free(buf);
                free(tmp);
                return;
            }
            (void)rename(tmp_path, path);
        } else {
            /* fall back to writing the real file directly */
            f = fopen(path, "wb");
            if (f) {
                if (fwrite(tmp, 1, tmplen, f) != tmplen) { /* ignore */ }
                fclose(f);
            }
        }
    }
    free(buf);
    free(tmp);
}

void ms_ini_set_str(const char *sec, const char *key, const char *value) {
    if (!sec || !key || !value) return;
    ini_rewrite(sec, key, value);
}

void ms_ini_set_int(const char *sec, const char *key, int value) {
    char buf[16];
    snprintf(buf, sizeof(buf), "%d", value);
    ms_ini_set_str(sec, key, buf);
}
