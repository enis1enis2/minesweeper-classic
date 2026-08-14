/*
 * diag.c - opt-out device diagnostics for Minesweeper (Classic).
 *
 * See diag.h for the collected-field contract and opt-out surface.
 *
 * Implementation notes:
 *   - Pure Win32 + CRT, no external dependencies (WinHTTP, advapi32 and the
 *     Windows TLS stack are system components).
 *   - The crash filter runs on a possibly-corrupted heap / exhausted stack,
 *     so it uses only stack buffers and allocation-free CRT calls.
 *   - All network I/O happens on a worker thread; the UI thread never blocks.
 *
 * MIT License
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winhttp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <io.h>
#include <fcntl.h>
#include <sys/stat.h>

#include "diag.h"

#ifndef INTERNET_DEFAULT_HTTPS_PORT
#define INTERNET_DEFAULT_HTTPS_PORT 443
#endif
#ifndef NTAPI
#define NTAPI __stdcall
#endif

#define DIAG_INI_SEC     "diagnostics"
#define DIAG_CRASH_MAX   2048          /* crash text buffer (filter stack) */
#define DIAG_STACK_MAX   8             /* stack frames captured at crash   */
#define DIAG_PAYLOAD_MAX 16384         /* JSON body (thread stack)         */
#ifndef DIAG_SEND_ATTEMPTS
#define DIAG_SEND_ATTEMPTS 3           /* bounded retries for transport    */
#endif
#ifndef DIAG_RETRY_DELAY_MS
#define DIAG_RETRY_DELAY_MS 5000       /* pause between retry attempts     */
#endif

static LONG CALLBACK diag_crash_filter(EXCEPTION_POINTERS *ep);

/* ---------- persistent state ---------- */
static char g_cfg_dir[MAX_PATH];
static char g_ini_path[MAX_PATH];
static char g_log_path[MAX_PATH];
static char g_crash_path[MAX_PATH];
static char g_profile_root[MAX_PATH];
static char g_install_dir[MAX_PATH];
static char g_machine_id[40];
static int  g_opt_out = 0;
static int  g_banner_seen = 0;
static int  g_ready = 0;
static ULONGLONG g_start_tick = 0;
static volatile LONG g_send_inflight = 0;

/* ---------- small string helpers ---------- */

static void set_str(char *dst, size_t sz, const char *src) {
    if (sz == 0 || dst == src) return;
    strncpy(dst, src, sz - 1);
    dst[sz - 1] = 0;
}

static void str_append(char *dst, size_t *o, size_t cap, const char *s) {
    while (*s && *o + 1 < cap) dst[(*o)++] = *s++;
    if (cap > 0) dst[*o] = 0;
}

static void str_appendc(char *dst, size_t *o, size_t cap, char c) {
    if (*o + 1 < cap) { dst[(*o)++] = c; dst[*o] = 0; }
}

static size_t ci_len_bound(const char *s) {
    return strlen(s);
}

static int ci_eq(const char *a, const char *b, size_t n) {
    size_t i;
    for (i = 0; i < n; i++) {
        char ca = a[i], cb = b[i];
        if (ca >= 'A' && ca <= 'Z') ca = (char)(ca - 'A' + 'a');
        if (cb >= 'A' && cb <= 'Z') cb = (char)(cb - 'A' + 'a');
        if (ca != cb) return 0;
    }
    return 1;
}

/* Replace every case-insensitive occurrence of `from` with `to`. */
static void replace_ci(const char *in, const char *from, const char *to,
                       char *out, size_t outsz) {
    size_t o = 0, i = 0, flen = ci_len_bound(from);
    size_t inlen = strlen(in);
    while (i < inlen) {
        if (flen > 0 && i + flen <= inlen && ci_eq(in + i, from, flen)) {
            str_append(out, &o, outsz, to);
            i += flen;
        } else {
            str_appendc(out, &o, outsz, in[i++]);
        }
    }
}

/* Reduce remaining absolute-path tokens (drive:\... and \\UNC...\...) to
 * <redacted>\<final-element>, in place. */
static void redact_paths(char *buf, size_t bufsz) {
    char tmp[DIAG_CRASH_MAX];
    size_t o = 0, i = 0, n = strlen(buf);
    while (i < n) {
        int is_path = 0;
        if (i + 2 <= n &&
            ((buf[i] >= 'A' && buf[i] <= 'Z') || (buf[i] >= 'a' && buf[i] <= 'z')) &&
            buf[i + 1] == ':' && buf[i + 2] == '\\') {
            is_path = 1;
        } else if (i + 1 < n && buf[i] == '\\' && buf[i + 1] == '\\') {
            is_path = 1;
        }
        if (is_path) {
            size_t j = (buf[i + 1] == ':') ? i + 3 : i + 2;
            size_t last_slash = (size_t)-1;
            while (j < n) {
                char c = buf[j];
                if (c == '\\' || c == '/') last_slash = j;
                if (c == ' ' || c == '\t' || c == '\r' || c == '\n' ||
                    c == ',' || c == ';' || c == '(' || c == ')' ||
                    c == '<' || c == '>' || c == '"' || c == '\'') break;
                j++;
            }
            if (last_slash != (size_t)-1 && last_slash + 1 < j) {
                str_append(tmp, &o, bufsz, "<redacted>\\");
                while (last_slash + 1 < j && o + 1 < bufsz)
                    tmp[o++] = buf[++last_slash];
                if (o + 1 < bufsz) tmp[o] = 0;
            } else {
                str_append(tmp, &o, bufsz, "<redacted>");
            }
            i = j;
        } else {
            str_appendc(tmp, &o, bufsz, buf[i++]);
        }
    }
    str_appendc(tmp, &o, bufsz, 0);
    memcpy(buf, tmp, o + 1);
}

/* Replace any standalone occurrence of the Windows username (last component
 * of profile_root) with <user>, defense in depth. */
static void strip_user_token(char *buf, size_t bufsz, const char *profile_root) {
    const char *u = strrchr(profile_root ? profile_root : "", '\\');
    u = u ? u + 1 : (profile_root ? profile_root : "");
    if (!u[0]) return;
    {
        char tmp[DIAG_CRASH_MAX];
        size_t o = 0, i = 0, n = strlen(buf), ulen = strlen(u);
        while (i < n) {
            int match = 0;
            if (i + ulen <= n && ci_eq(buf + i, u, ulen)) {
                char pre = i ? buf[i - 1] : ' ';
                char post = (i + ulen < n) ? buf[i + ulen] : ' ';
                int pre_ok = !((pre >= 'A' && pre <= 'Z') ||
                               (pre >= 'a' && pre <= 'z') ||
                               (pre >= '0' && pre <= '9') || pre == '_');
                int post_ok = !((post >= 'A' && post <= 'Z') ||
                                (post >= 'a' && post <= 'z') ||
                                (post >= '0' && post <= '9') || post == '_');
                if (pre_ok && post_ok) match = 1;
            }
            if (match) {
                str_append(tmp, &o, bufsz, "<user>");
                i += ulen;
            } else {
                str_appendc(tmp, &o, bufsz, buf[i++]);
            }
        }
        str_appendc(tmp, &o, bufsz, 0);
        memcpy(buf, tmp, o + 1);
    }
}

void diag_sanitize(const char *profile_root, const char *install_dir,
                   const char *in, char *out, size_t outsz) {
    char stage1[DIAG_CRASH_MAX], stage2[DIAG_CRASH_MAX];
    size_t cap = outsz < DIAG_CRASH_MAX ? outsz : DIAG_CRASH_MAX;
    /* install dir first: it may live under the profile root (portable
     * downloads), in which case profile replacement would mangle it */
    replace_ci(in, install_dir ? install_dir : "", "<install>",
               stage1, cap);
    replace_ci(stage1, profile_root ? profile_root : "", "<user>",
               stage2, cap);
    memcpy(out, stage2, cap);
    out[cap - 1] = 0;
    redact_paths(out, cap);
    strip_user_token(out, cap, profile_root);
}

/* ---------- config directory + persistence ---------- */

static int dir_writable(const char *dir) {
    char tmp[MAX_PATH];
    tmp[0] = 0;
    if (GetTempFileNameA(dir, "msd", 0, tmp)) {
        DeleteFileA(tmp);
        return 1;
    }
    return 0;
}

static void diag_init_paths(void) {
    char exe[MAX_PATH] = "", appdata[MAX_PATH] = "", alt[MAX_PATH] = "";
    char dir[MAX_PATH] = "";
    char *p;
    int used = 0;

    GetModuleFileNameA(NULL, exe, sizeof exe);
    set_str(g_install_dir, sizeof g_install_dir, exe);
    p = strrchr(g_install_dir, '\\');
    if (p) { *p = 0; set_str(g_install_dir, sizeof g_install_dir, g_install_dir); }

    GetEnvironmentVariableA("USERPROFILE", g_profile_root, sizeof g_profile_root);

    set_str(dir, sizeof dir, g_install_dir);
    if (dir_writable(dir)) used = 1;
    if (!used && GetEnvironmentVariableA("APPDATA", appdata, sizeof appdata) &&
        appdata[0]) {
        _snprintf(alt, sizeof alt, "%s\\Minesweeper", appdata);
        CreateDirectoryA(alt, NULL);
        if (dir_writable(alt)) { set_str(dir, sizeof dir, alt); used = 1; }
    }
    if (!used) { g_ready = 0; return; }

    set_str(g_cfg_dir, sizeof g_cfg_dir, dir);
    _snprintf(g_ini_path, sizeof g_ini_path, "%s\\minesweeper.ini", g_cfg_dir);
    _snprintf(g_log_path, sizeof g_log_path, "%s\\minesweeper.log", g_cfg_dir);
    _snprintf(g_crash_path, sizeof g_crash_path, "%s\\crash-last.txt", g_cfg_dir);
    g_ready = 1;
}

static int cfg_get_int(const char *key, int def) {
    char buf[16];
    GetPrivateProfileStringA(DIAG_INI_SEC, key, def ? "1" : "0",
                             buf, sizeof buf, g_ini_path);
    return atoi(buf);
}

static void cfg_set_int(const char *key, int v) {
    char buf[16];
    _snprintf(buf, sizeof buf, "%d", v);
    WritePrivateProfileStringA(DIAG_INI_SEC, key, buf, g_ini_path);
}

typedef BOOLEAN (NTAPI *SystemFunction036Fn)(void *, ULONG);

static void gen_machine_id(void) {
    unsigned char b[16];
    char hex[40];
    int i;
    HMODULE adv = GetModuleHandleA("advapi32.dll");
    SystemFunction036Fn fn = adv ?
        (SystemFunction036Fn)GetProcAddress(adv, "SystemFunction036") : NULL;
    if (!fn) {
        /* weak fallback (no RNG available): time + stack pointer entropy */
        ULONGLONG t = GetTickCount64();
        ULONGLONG a = (ULONGLONG)(DWORD_PTR)&b;
        memcpy(b, &t, 8);
        memcpy(b + 8, &a, 8);
    } else {
        fn(b, sizeof b);
    }
    for (i = 0; i < 16; i++)
        _snprintf(hex + i * 2, 3, "%02X", b[i]);
    set_str(g_machine_id, sizeof g_machine_id, hex);
    WritePrivateProfileStringA(DIAG_INI_SEC, "machine_id", hex, g_ini_path);
}

static void diag_load_config(void) {
    char id[64];
    g_opt_out = cfg_get_int("opt_out", 0) ? 1 : 0;
    g_banner_seen = cfg_get_int("banner_seen", 0) ? 1 : 0;
    if (!GetPrivateProfileStringA(DIAG_INI_SEC, "machine_id", "", id,
                                  sizeof id, g_ini_path) || !id[0]) {
        gen_machine_id();
    } else {
        set_str(g_machine_id, sizeof g_machine_id, id);
    }
}

int diag_init(void) {
    g_start_tick = GetTickCount64();
    diag_init_paths();
    if (!g_ready) return 1;
    diag_load_config();
    SetUnhandledExceptionFilter(diag_crash_filter);
    return 0;
}

int diag_opt_out(void) { return g_opt_out; }

void diag_set_opt_out(int v) {
    g_opt_out = v ? 1 : 0;
    if (g_ready) cfg_set_int("opt_out", g_opt_out);
}

int diag_banner_needed(void) { return g_ready && !g_banner_seen && !g_opt_out; }

void diag_mark_banner_seen(void) {
    g_banner_seen = 1;
    if (g_ready) cfg_set_int("banner_seen", 1);
}

const char *diag_machine_id(void) { return g_machine_id; }

const char *diag_cfg_path(void) { return g_cfg_dir; }

int diag_crash_pending(void) {
    return g_ready &&
           GetFileAttributesA(g_crash_path) != INVALID_FILE_ATTRIBUTES;
}

/* ---------- diagnostics log ---------- */

void diag_log(const char *fmt, ...) {
    char line[1024];
    int n;
    va_list ap;
    va_start(ap, fmt);
    n = vsnprintf(line, sizeof line, fmt, ap);
    va_end(ap);
    if (n < 0) n = 0;
    if ((size_t)n >= sizeof line) n = (int)sizeof line - 1;
    OutputDebugStringA(line);
    if (g_ready && g_log_path[0]) {
        int fd = _open(g_log_path, _O_WRONLY | _O_CREAT | _O_APPEND | _O_BINARY,
                       _S_IREAD | _S_IWRITE);
        if (fd >= 0) {
            _write(fd, line, (unsigned)n);
            _write(fd, "\n", 1);
            _close(fd);
        }
    }
}

/* ---------- crash capture + sanitize (allocation-free) ---------- */

static void crash_append(char *raw, int *len, int cap, const char *fmt, ...) {
    va_list ap;
    int n;
    if (*len >= cap - 1) return;
    va_start(ap, fmt);
    n = vsnprintf(raw + *len, (size_t)(cap - *len), fmt, ap);
    va_end(ap);
    if (n < 0) return;
    if (*len + n > cap - 1) *len = cap - 1; else *len += n;
}

static LONG CALLBACK diag_crash_filter(EXCEPTION_POINTERS *ep) {
    DWORD code = ep && ep->ExceptionRecord ?
        ep->ExceptionRecord->ExceptionCode : 0;
    void *fault = ep && ep->ExceptionRecord ?
        ep->ExceptionRecord->ExceptionAddress : NULL;
    int is_stack_overflow = (code == (DWORD)0xC00000FDUL);
    char raw[DIAG_CRASH_MAX];
    char clean[DIAG_CRASH_MAX];
    int len = 0;
    HMODULE ntdll = GetModuleHandleA("ntdll.dll");

    crash_append(raw, &len, (int)sizeof raw, "Exception 0x%08lX at 0x%p\n",
                 (unsigned long)code, fault);

    if (!is_stack_overflow && len < (int)sizeof raw - 64) {
        typedef USHORT (NTAPI *RtlCaptureStackBackTraceFn)(ULONG, ULONG,
                                                           PVOID *, PULONG);
        RtlCaptureStackBackTraceFn rcsb =
            ntdll ? (RtlCaptureStackBackTraceFn)GetProcAddress(
                        ntdll, "RtlCaptureStackBackTrace") : NULL;
        if (rcsb) {
            PVOID addrs[DIAG_STACK_MAX];
            ULONG n = rcsb(0, DIAG_STACK_MAX, addrs, NULL);
            ULONG i;
            for (i = 0; i < n && len < (int)sizeof raw - 64; i++) {
                HMODULE h = NULL;
                DWORD_PTR base = 0;
                char mod[MAX_PATH] = "<unknown>";
                if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                       GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                       (LPCSTR)addrs[i], &h) && h) {
                    base = (DWORD_PTR)h;
                    GetModuleFileNameA(h, mod, sizeof mod);
                }
                crash_append(raw, &len, (int)sizeof raw, "  %s+0x%llX\n",
                             mod,
                             (unsigned long long)((DWORD_PTR)addrs[i] - base));
            }
        }
    }

    diag_sanitize(g_profile_root, g_install_dir, raw, clean, sizeof clean);
    if (g_ready && g_crash_path[0]) {
        int fd = _open(g_crash_path, _O_WRONLY | _O_CREAT | _O_TRUNC |
                       _O_BINARY, _S_IREAD | _S_IWRITE);
        if (fd >= 0) {
            _write(fd, clean, (unsigned)strlen(clean));
            _close(fd);
        }
    }
    OutputDebugStringA(clean);
    return EXCEPTION_CONTINUE_SEARCH;
}

/* ---------- system info collection (Win32) ---------- */

static void reg_read_sz(HKEY root, const char *subkey, const char *val,
                        char *out, DWORD outsz) {
    HKEY k;
    DWORD type = 0, n = outsz;
    char *p;
    out[0] = 0;
    if (RegOpenKeyExA(root, subkey, 0, KEY_READ, &k) == ERROR_SUCCESS) {
        if (RegQueryValueExA(k, val, NULL, &type, (LPBYTE)out, &n) ==
            ERROR_SUCCESS && type == REG_SZ) {
            if (n >= outsz) n = outsz - 1;
            out[n] = 0;
        }
        RegCloseKey(k);
    }
    for (p = out; *p; p++) {
        if ((unsigned char)*p < 0x20) *p = ' ';
    }
    while (p > out && (p[-1] == ' ')) *--p = 0;
}

typedef LONG (NTAPI *RtlGetVersionFn)(void *);   /* RTL_OSVERSIONINFOW* */

static void collect_os(char *out, size_t sz) {
    struct {
        ULONG dwOSVersionInfoSize;
        ULONG dwMajorVersion;
        ULONG dwMinorVersion;
        ULONG dwBuildNumber;
        ULONG dwPlatformId;
        WCHAR szCSDVersion[128];
    } v;
    char prod[128] = "";
    RtlGetVersionFn fn;
    HMODULE ntdll = GetModuleHandleA("ntdll.dll");
    memset(&v, 0, sizeof v);
    v.dwOSVersionInfoSize = sizeof v;
    fn = ntdll ? (RtlGetVersionFn)GetProcAddress(ntdll, "RtlGetVersion") : NULL;
    if (fn) fn(&v);
    reg_read_sz(HKEY_LOCAL_MACHINE,
                "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
                "ProductName", prod, sizeof prod);
    /* Windows 11 still reports ProductName "Windows 10 ..." in the registry,
     * so the family cannot come from there.  Detect it by build number
     * instead: Windows 11 is NT 10.0 build 22000 and up. */
    if (v.dwMajorVersion == 10 && v.dwBuildNumber >= 22000)
        _snprintf(out, sz, "Windows 11 %lu.%lu (build %lu)",
                  (unsigned long)v.dwMajorVersion,
                  (unsigned long)v.dwMinorVersion,
                  (unsigned long)v.dwBuildNumber);
    else if (v.dwMajorVersion == 10)
        _snprintf(out, sz, "Windows 10 %lu.%lu (build %lu)",
                  (unsigned long)v.dwMajorVersion,
                  (unsigned long)v.dwMinorVersion,
                  (unsigned long)v.dwBuildNumber);
    else
        _snprintf(out, sz, "%s %lu.%lu (build %lu)",
                  prod[0] ? prod : "Windows",
                  (unsigned long)v.dwMajorVersion,
                  (unsigned long)v.dwMinorVersion,
                  (unsigned long)v.dwBuildNumber);
}

static void collect_cpu(char *model, size_t msz, int *cores) {
    SYSTEM_INFO si;
    reg_read_sz(HKEY_LOCAL_MACHINE,
                "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
                "ProcessorNameString", model, (DWORD)msz);
    GetSystemInfo(&si);
    *cores = (int)si.dwNumberOfProcessors;
}

static void collect_gpu(char *out, size_t sz) {
    const char *base =
        "SYSTEM\\CurrentControlSet\\Control\\Class\\"
        "{4d36e968-e325-11ce-bfc1-08002be10318}";
    char sub[160];
    int i;
    out[0] = 0;
    for (i = 0; i < 8 && !out[0]; i++) {
        _snprintf(sub, sizeof sub, "%s\\%04d", base, i);
        reg_read_sz(HKEY_LOCAL_MACHINE, sub, "DriverDesc", out, (DWORD)sz);
    }
}

static void collect_ram(unsigned long long *mb) {
    MEMORYSTATUSEX m;
    memset(&m, 0, sizeof m);
    m.dwLength = sizeof m;
    if (GlobalMemoryStatusEx(&m))
        *mb = m.ullTotalPhys / (1024ULL * 1024ULL);
    else
        *mb = 0;
}

static void collect_display(char *out, size_t sz) {
    DEVMODEA dm;
    int w = GetSystemMetrics(SM_CXSCREEN);
    int h = GetSystemMetrics(SM_CYSCREEN);
    int hz = 0;
    memset(&dm, 0, sizeof dm);
    dm.dmSize = sizeof dm;
    if (EnumDisplaySettingsA(NULL, ENUM_CURRENT_SETTINGS, &dm))
        hz = (int)dm.dmDisplayFrequency;
    if (hz > 0)
        _snprintf(out, sz, "%dx%d@%d", w, h, hz);
    else
        _snprintf(out, sz, "%dx%d", w, h);
}

static void json_escape(const char *in, char *out, size_t outsz) {
    size_t o = 0;
    const unsigned char *p = (const unsigned char *)in;
    while (*p && o + 6 < outsz) {
        switch (*p) {
        case '"':  out[o++] = '\\'; out[o++] = '"';  break;
        case '\\': out[o++] = '\\'; out[o++] = '\\'; break;
        case '\n': out[o++] = '\\'; out[o++] = 'n';  break;
        case '\r': out[o++] = '\\'; out[o++] = 'r';  break;
        case '\t': out[o++] = '\\'; out[o++] = 't';  break;
        default:
            if (*p < 0x20) {
                _snprintf(out + o, outsz - o, "\\u%04x", *p);
                o += 6;
            } else {
                out[o++] = (char)*p;
            }
        }
        p++;
    }
    out[o] = 0;
}

static void read_crash_file(char *out, size_t sz) {
    int fd;
    out[0] = 0;
    if (!g_ready || !g_crash_path[0]) return;
    fd = _open(g_crash_path, _O_RDONLY | _O_BINARY);
    if (fd >= 0) {
        int n = _read(fd, out, (unsigned)(sz - 1));
        if (n < 0) n = 0;
        out[n] = 0;
        _close(fd);
    }
}

static int build_payload(char *out, size_t sz) {
    char os[192], cpu[256], gpu[256], disp[64];
    char crash_raw[DIAG_CRASH_MAX + 16], crash_san[DIAG_CRASH_MAX + 16];
    char os_e[512], cpu_e[640], gpu_e[640], disp_e[192], crash_e[DIAG_PAYLOAD_MAX];
    int cores;
    unsigned long long ram;
    long long up;
    const char *crash_field;

    collect_os(os, sizeof os);
    collect_cpu(cpu, sizeof cpu, &cores);
    collect_gpu(gpu, sizeof gpu);
    collect_ram(&ram);
    collect_display(disp, sizeof disp);
    up = (long long)((GetTickCount64() - g_start_tick) / 1000);

    read_crash_file(crash_raw, sizeof crash_raw);
    /* double safety: never trust on-disk text, re-sanitize on read */
    diag_sanitize(g_profile_root, g_install_dir, crash_raw,
                  crash_san, sizeof crash_san);
    if (crash_raw[0]) {
        json_escape(crash_san, crash_e, sizeof crash_e);
        crash_field = crash_e;
    } else {
        crash_field = "null";
    }

    json_escape(os, os_e, sizeof os_e);
    json_escape(cpu, cpu_e, sizeof cpu_e);
    json_escape(gpu, gpu_e, sizeof gpu_e);
    json_escape(disp, disp_e, sizeof disp_e);

    _snprintf(out, sz,
        "{\"machine_id\":\"%s\",\"os\":\"%s\",\"cpu\":\"%s\",\"cpu_cores\":%d,"
        "\"gpu\":\"%s\",\"ram_mb\":%llu,\"display\":\"%s\","
        "\"game_version\":\"%s\",\"uptime_sec\":%lld,\"crash_text\":%s}",
        g_machine_id, os_e, cpu_e, cores, gpu_e, ram, disp_e,
        APP_VERSION, up, crash_field);
    return 1;
}

/* ---------- HTTPS delivery (worker thread) ---------- */

/* POST one JSON payload over HTTPS.  Returns:
 *    1  delivered (HTTP 2xx)  -- crash file (if any) is deleted
 *    0  server answered but not 2xx (endpoint reachable; terminal, no retry)
 *   -1  no HTTP response at all (DNS / connect / TLS / send / receive
 *       failure) -- transient, caller may retry a bounded number of times */
#ifndef DIAG_TEST_FAKE_POST
static int http_post_json(const char *host, const char *path,
                          const char *body) {
    WCHAR whost[256], wpath[256];
    HINTERNET h = NULL, c = NULL, r = NULL;
    DWORD status = 0, status_len = sizeof status;
    BOOL ok = FALSE, got_status = FALSE;
    LPCWSTR headers = L"Content-Type: application/json\r\n"
                      L"Accept: application/json\r\n";

    MultiByteToWideChar(CP_UTF8, 0, host, -1, whost, 256);
    MultiByteToWideChar(CP_UTF8, 0, path, -1, wpath, 256);

    h = WinHttpOpen(L"Windfall/" APP_VERSION,
                    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                    WINHTTP_NO_PROXY_NAME, WINHTTP_NO_PROXY_BYPASS, 0);
    if (!h) goto done;
    WinHttpSetTimeouts(h, 10000, 10000, 10000, 10000);
    c = WinHttpConnect(h, whost, INTERNET_DEFAULT_HTTPS_PORT, 0);
    if (!c) goto done;
    r = WinHttpOpenRequest(c, L"POST", wpath, NULL, WINHTTP_NO_REFERER,
                           WINHTTP_DEFAULT_ACCEPT_TYPES, WINHTTP_FLAG_SECURE);
    if (!r) goto done;
    if (!WinHttpSendRequest(r, headers, (DWORD)-1L, (LPVOID)body,
                            (DWORD)strlen(body), (DWORD)strlen(body), 0))
        goto done;
    if (!WinHttpReceiveResponse(r, NULL)) goto done;
    got_status = WinHttpQueryHeaders(r, WINHTTP_QUERY_STATUS_CODE |
                                          WINHTTP_QUERY_FLAG_NUMBER,
                                     WINHTTP_HEADER_NAME_BY_INDEX,
                                     &status, &status_len, WINHTTP_NO_HEADER_INDEX);
    ok = got_status && status >= 200 && status < 300;

done:
    if (r) WinHttpCloseHandle(r);
    if (c) WinHttpCloseHandle(c);
    if (h) WinHttpCloseHandle(h);
    if (ok) {
        if (g_ready && g_crash_path[0]) DeleteFileA(g_crash_path);
        diag_log("diag: delivered (HTTP %lu)", (unsigned long)status);
        return 1;
    }
    if (got_status) {
        diag_log("diag: delivery failed (HTTP %lu)", (unsigned long)status);
        return 0;
    }
    diag_log("diag: delivery failed (no response: DNS/connect/TLS)");
    return -1;
}
#endif /* DIAG_TEST_FAKE_POST */

static DWORD WINAPI diag_send_thread(void *arg) {
    char body[DIAG_PAYLOAD_MAX];
    int attempt, r = -1;
    (void)arg;
    if (!build_payload(body, sizeof body)) goto done;
    for (attempt = 1; attempt <= DIAG_SEND_ATTEMPTS; attempt++) {
        r = http_post_json(DIAG_HOST, DIAG_PATH, body);
        if (r >= 0) break;   /* delivered, or the endpoint answered */
        if (attempt < DIAG_SEND_ATTEMPTS) {
            diag_log("diag: transport failure, retry %d/%d in %d ms",
                     attempt, DIAG_SEND_ATTEMPTS, DIAG_RETRY_DELAY_MS);
            Sleep(DIAG_RETRY_DELAY_MS);
        }
    }
    if (r < 0)
        diag_log("diag: giving up after %d attempts (fail-silent)",
                 DIAG_SEND_ATTEMPTS);
done:
    InterlockedExchange(&g_send_inflight, 0);
    return 0;
}

static void diag_send_async(void) {
    HANDLE t;
    if (InterlockedCompareExchange(&g_send_inflight, 1, 0) != 0) return;
    t = CreateThread(NULL, 0, diag_send_thread, NULL, 0, NULL);
    if (t) CloseHandle(t);
    else InterlockedExchange(&g_send_inflight, 0);
}

void diag_on_connected(void) {
    if (!g_ready) return;
    if (g_opt_out) {
        diag_log("[diagnostics] disabled (opt_out=1) \xe2\x80\x94 no device "
                 "diagnostics are sent. Enable in Settings > Privacy.");
    } else {
        diag_log("[diagnostics] sending device diagnostics to " DIAG_HOST
                 " \xe2\x80\x94 disable in Settings > Privacy or via "
                 "diagnostics_opt_out=1 in config");
        diag_send_async();
    }
}
