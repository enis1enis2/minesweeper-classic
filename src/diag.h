/*
 * diag.h - opt-in device diagnostics for Minesweeper (Classic).
 *
 * Collects a fixed, disclosed set of device descriptors (OS version/build,
 * CPU model + core count, GPU model, RAM total, screen resolution/refresh,
 * game version, session uptime) plus a sanitized crash log when one exists,
 * and delivers them to the diagnostics server over HTTPS (TLS) using WinHTTP.
 *
 * Nothing else is ever collected: no file listings, browsing or network
 * activity, credentials, IP geolocation, keystrokes, clipboard contents,
 * screenshots, installed-software inventory, usernames or hostnames.
 *
 * Two independent opt-outs (either one fully suppresses collection and send):
 *   - config flag  diagnostics_opt_out=1  in [diagnostics] of minesweeper.ini
 *   - Settings > Privacy > "Send device diagnostics" checkbox
 *
 * MIT License
 */
#ifndef DIAG_H
#define DIAG_H

#define APP_VERSION "1.0.0"

/* HTTPS (TLS) diagnostics endpoint, terminated by nginx + Let's Encrypt. */
#define DIAG_HOST "admin.jellyfiner.dpdns.org"
#define DIAG_PATH "/ms-diag/ingest"

/* Resolve the config directory (exe dir, falling back to %APPDATA%\
 * Minesweeper), load the persisted flags and machine id, and install the
 * unhandled-exception crash filter.  Returns 0 on success (diagnostics
 * operational), nonzero if no writable config location exists (all
 * diagnostics are then silently disabled). */
int diag_init(void);

/* Is device-diagnostics collection currently opted out? */
int diag_opt_out(void);

/* Set and persist the opt-out flag (1 = off, 0 = on). */
void diag_set_opt_out(int v);

/* Should the first-connect disclosure banner be shown? */
int diag_banner_needed(void);
void diag_mark_banner_seen(void);

/* Machine-scoped random id (32 hex chars), generated on first run. */
const char *diag_machine_id(void);

/* Resolved config directory (exe dir, falling back to %APPDATA%\
 * Minesweeper) with a trailing backslash.  Only valid after diag_init(). */
const char *diag_cfg_path(void);

/* Called on every rising telemetry-connect edge.  Logs the disclosure notice
 * (or the opt-out notice) and, when enabled, queues an HTTPS delivery of the
 * current device diagnostics on a background thread. */
void diag_on_connected(void);

/* A crash-last.txt is pending upload (crash file not yet delivered). */
int diag_crash_pending(void);

/* Sanitize arbitrary crash text: the user-profile root is replaced with
 * <user>, the install directory with <install>, any remaining absolute path
 * is reduced to <redacted>\<final-element>, and any standalone Windows
 * username is replaced with <user>.  Output is bounded by outsz.  Pure
 * function (unit-testable). */
void diag_sanitize(const char *profile_root, const char *install_dir,
                   const char *in, char *out, size_t outsz);

/* Append a line to the diagnostics log (minesweeper.log in the config dir)
 * and emit it via OutputDebugStringA. */
void diag_log(const char *fmt, ...);

#endif /* DIAG_H */
