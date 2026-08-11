/*
 * leader.c - Hall of Fame (leaderboard) for Minesweeper (Classic).
 *
 * Persisted state lives in the [leaderboard] section of minesweeper.ini
 * (alias name, auto-submit flag).  The leaderboard itself is kept server-
 * side: wins are submitted with `lbscore <name> <diff> <time_ms>` and the
 * top list is pulled with `lbtop ...` through network.c.
 *
 * MIT License
 */
#define WIN32_LEAN_AND_MEAN
#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0600
#endif
#include <windows.h>
#include <commctrl.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

#include "resource.h"
#include "network.h"
#include "diag.h"
#include "leader.h"

#define LB_INI_SEC "leaderboard"

/* ---------- persisted config ---------- */
static char g_ini_path[MAX_PATH];
static char g_name[LEADER_NAME_MAX + 1];

static void leader_ini_path(void) {
    if (g_ini_path[0]) return;
    _snprintf(g_ini_path, sizeof(g_ini_path), "%s\\minesweeper.ini",
              diag_cfg_path());
}

static int cfg_get_int(const char *key, int def) {
    char buf[16];
    leader_ini_path();
    GetPrivateProfileStringA(LB_INI_SEC, key, def ? "1" : "0",
                             buf, sizeof(buf), g_ini_path);
    return atoi(buf);
}

static void cfg_set_int(const char *key, int v) {
    char buf[16];
    leader_ini_path();
    _snprintf(buf, sizeof(buf), "%d", v);
    WritePrivateProfileStringA(LB_INI_SEC, key, buf, g_ini_path);
}

static void cfg_get_str(const char *key, const char *def, char *out, size_t outsz) {
    leader_ini_path();
    GetPrivateProfileStringA(LB_INI_SEC, key, def, out, (DWORD)outsz, g_ini_path);
}

static void cfg_set_str(const char *key, const char *v) {
    leader_ini_path();
    WritePrivateProfileStringA(LB_INI_SEC, key, v, g_ini_path);
}

/* ---------- player name ---------- */

int leader_name_valid(const char *name) {
    size_t i, n;
    if (!name) return 0;
    n = strlen(name);
    if (n < 1 || n > LEADER_NAME_MAX) return 0;
    for (i = 0; i < n; i++) {
        char c = name[i];
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
              (c >= '0' && c <= '9') || c == '-' || c == '_'))
            return 0;
    }
    return 1;
}

const char *leader_player_name(void) {
    if (!g_name[0]) {
        cfg_get_str("name", "Player", g_name, sizeof(g_name));
        if (!leader_name_valid(g_name)) strcpy(g_name, "Player");
    }
    return g_name;
}

int leader_set_player_name(const char *name) {
    if (!leader_name_valid(name)) return 0;
    strncpy(g_name, name, sizeof(g_name) - 1);
    g_name[sizeof(g_name) - 1] = 0;
    cfg_set_str("name", g_name);
    return 1;
}

int leader_auto_submit(void) { return cfg_get_int("auto_submit", 1) != 0; }
void leader_set_auto_submit(int on) { cfg_set_int("auto_submit", on ? 1 : 0); }

void leader_sync_menu(HMENU menu) {
    if (!menu) return;
    CheckMenuItem(menu, IDM_HOF_AUTO,
                  MF_BYCOMMAND | (leader_auto_submit() ? MF_CHECKED : MF_UNCHECKED));
}

/* A finished win is submitted as a best-time attempt when auto-submit is on.
 * The server keeps only the fastest time per name+difficulty, so a worse
 * replay simply does not replace anything (lbnotop). */
void leader_submit_win(int diff, int time_ms) {
    const char *name;
    if (!leader_auto_submit()) return;
    name = leader_player_name();
    if (!name || !name[0]) return;
    if (diff < 0 || diff >= 3) return;          /* preset difficulties only */
    if (time_ms <= 0 || time_ms > 3600000) return;
    net_send_score(name, diff, time_ms);
}

/* ---------- player-name chooser ---------- */

static INT_PTR CALLBACK player_proc(HWND hDlg, UINT msg, WPARAM wp, LPARAM lp) {
    (void)lp;
    switch (msg) {
    case WM_INITDIALOG: {
        HWND ed = GetDlgItem(hDlg, IDC_PLAYER_NAME);
        SendMessageA(ed, EM_LIMITTEXT, LEADER_NAME_MAX, 0);
        SetDlgItemTextA(hDlg, IDC_PLAYER_NAME, leader_player_name());
        SendMessageA(ed, EM_SETSEL, 0, -1);
        return TRUE;
    }
    case WM_COMMAND:
        switch (LOWORD(wp)) {
        case IDC_PLAYER_OK: {
            char buf[LEADER_NAME_MAX + 1];
            GetDlgItemTextA(hDlg, IDC_PLAYER_NAME, buf, sizeof(buf));
            if (!leader_set_player_name(buf)) {
                MessageBoxA(hDlg,
                            "Names may only contain letters, digits, '-' and "
                            "'_', and must be 1-16 characters.",
                            "Player Name", MB_OK | MB_ICONWARNING);
                return TRUE;
            }
            EndDialog(hDlg, IDOK);
            return TRUE;
        }
        case IDCANCEL:
            EndDialog(hDlg, IDCANCEL);
            return TRUE;
        }
        break;
    }
    return FALSE;
}

void leader_show_player(HWND parent) {
    DialogBoxParamA(GetModuleHandle(NULL), MAKEINTRESOURCEA(IDD_PLAYER),
                    parent, player_proc, 0);
}

/* ---------- Hall of Fame leaderboard ---------- */

static int hof_diff = -1;            /* -1 = all boards, else DIFF_* preset */

static void format_time(int ms, char *out, size_t outsz) {
    if (ms < 0) ms = 0;
    if (ms >= 60000)
        _snprintf(out, outsz, "%d:%02d", ms / 60000, (ms / 1000) % 60);
    else
        _snprintf(out, outsz, "%.1fs", ms / 1000.0);
}

static const char *board_label(const char *diff) {
    if (stricmp(diff, "beginner") == 0) return "Beginner";
    if (stricmp(diff, "intermediate") == 0) return "Intermediate";
    if (stricmp(diff, "expert") == 0) return "Expert";
    return diff;
}

static void list_set_col(HWND list, int row, int col, const char *text) {
    LVITEMA item;
    memset(&item, 0, sizeof(item));
    item.mask = LVIF_TEXT;
    item.iItem = row;
    item.iSubItem = col;
    item.pszText = (char *)text;
    SendMessageA(list, LVM_SETITEMTEXT, 0, (LPARAM)&item);
}

static void hof_add_entry(HWND list, const NetLbEntryMsg *m) {
    LVITEMA item;
    char rank[16], time[32];
    int row;
    memset(&item, 0, sizeof(item));
    item.mask = LVIF_TEXT;
    item.pszText = rank;
    _snprintf(rank, sizeof(rank), "#%d", m->rank);
    row = (int)SendMessageA(list, LVM_INSERTITEMA, 0, (LPARAM)&item);
    list_set_col(list, row, 1, (char *)m->name);
    list_set_col(list, row, 2, board_label(m->diff));
    format_time(m->time_ms, time, sizeof(time));
    list_set_col(list, row, 3, time);
}

static void hof_query(HWND hDlg) {
    HWND list = GetDlgItem(hDlg, IDC_HOF_LIST);
    ListView_DeleteAllItems(list);
    if (!net_telemetry_active()) {
        SetDlgItemTextA(hDlg, IDC_HOF_STATUS, "Telemetry is off.");
        return;
    }
    SetDlgItemTextA(hDlg, IDC_HOF_STATUS, "Loading...");
    if (hof_diff < 0)
        net_request_lbtop(LEADER_HOF_MAX);
    else
        net_request_lbtop_diff(hof_diff, LEADER_HOF_MAX);
}

static void hof_init_list(HWND list) {
    LVCOLUMNA col;
    const char *titles[4] = { "Place", "Name", "Board", "Time" };
    int widths[4] = { 40, 120, 100, 60 };
    int i;
    ListView_SetExtendedListViewStyle(list, LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES);
    for (i = 0; i < 4; i++) {
        memset(&col, 0, sizeof(col));
        col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM;
        col.pszText = (char *)titles[i];
        col.cx = widths[i];
        col.iSubItem = i;
        SendMessageA(list, LVM_INSERTCOLUMNA, i, (LPARAM)&col);
    }
}

static INT_PTR CALLBACK hof_proc(HWND hDlg, UINT msg, WPARAM wp, LPARAM lp) {
    switch (msg) {
    case WM_INITDIALOG: {
        HWND combo = GetDlgItem(hDlg, IDC_HOF_FILTER);
        const char *labels[4] = { "All boards", "Beginner", "Intermediate",
                                  "Expert" };
        int i;
        hof_diff = -1;
        hof_init_list(GetDlgItem(hDlg, IDC_HOF_LIST));
        for (i = 0; i < 4; i++)
            SendMessageA(combo, CB_ADDSTRING, 0, (LPARAM)labels[i]);
        SendMessageA(combo, CB_SETCURSEL, 0, 0);
        net_set_lb_window(hDlg);
        hof_query(hDlg);
        return TRUE;
    }
    case WM_APP_LB_ENTRY:
        switch (wp) {
        case LB_EV_START:
            ListView_DeleteAllItems(GetDlgItem(hDlg, IDC_HOF_LIST));
            SetDlgItemTextA(hDlg, IDC_HOF_STATUS, "Loading...");
            break;
        case LB_EV_ENTRY: {
            NetLbEntryMsg *m = (NetLbEntryMsg *)lp;
            if (m) {
                hof_add_entry(GetDlgItem(hDlg, IDC_HOF_LIST), m);
                free(m);
            }
            break;
        }
        case LB_EV_END:
            break;
        }
        return TRUE;
    case WM_APP_LB_END: {
        int n = ListView_GetItemCount(GetDlgItem(hDlg, IDC_HOF_LIST));
        char st[64];
        if (n == 0) {
            SetDlgItemTextA(hDlg, IDC_HOF_STATUS,
                            net_telemetry_active() ? "No scores yet."
                                                   : "Telemetry is off.");
        } else {
            _snprintf(st, sizeof(st), "%d entr%s", n, n == 1 ? "y" : "ies");
            SetDlgItemTextA(hDlg, IDC_HOF_STATUS, st);
        }
        return TRUE;
    }
    case WM_COMMAND:
        switch (LOWORD(wp)) {
        case IDC_HOF_FILTER:
            if (HIWORD(wp) == CBN_SELCHANGE) {
                int sel = (int)SendMessageA(GetDlgItem(hDlg, IDC_HOF_FILTER),
                                            CB_GETCURSEL, 0, 0);
                hof_diff = (sel <= 0) ? -1 : sel - 1;
                hof_query(hDlg);
            }
            return TRUE;
        case IDC_HOF_SUBMIT:
            hof_query(hDlg);
            return TRUE;
        case IDOK:
        case IDCANCEL:
            EndDialog(hDlg, IDOK);
            return TRUE;
        }
        break;
    case WM_DESTROY:
        net_set_lb_window(NULL);
        break;
    }
    return FALSE;
}

void leader_show_hof(HWND parent) {
    DialogBoxParamA(GetModuleHandle(NULL), MAKEINTRESOURCEA(IDD_HOF),
                    parent, hof_proc, 0);
}

/* ---------- solver credentials (--solver-user/--solver-pass/--solver-config,
 * falling back to MS_SOLVER_USER/MS_SOLVER_PASS) ---------- */

/* Extract the value of "key":"..." from a tiny hand-written JSON file. */
static void json_str(const char *buf, const char *key, char *out, size_t outsz,
                     int *found) {
    const char *k = buf;
    size_t klen = strlen(key);
    if (outsz == 0) return;
    out[0] = 0;
    while ((k = strchr(k, '"')) != NULL) {
        k++;
        if (_strnicmp(k, key, klen) == 0 && k[klen] == '"') {
            k += klen + 1;
            while (*k == ' ' || *k == '\t') k++;
            if (*k != ':') continue;
            k++;
            while (*k == ' ' || *k == '\t') k++;
            if (*k != '"') continue;
            k++;
            {
                size_t o = 0;
                while (*k && *k != '"' && o + 1 < outsz) out[o++] = *k++;
                out[o] = 0;
            }
            *found = 1;
            return;
        }
    }
}

static void solver_config_read(const char *path, char *user, char *pass,
                               int *have_user, int *have_pass) {
    FILE *f;
    char *buf = NULL;
    long len;
    if (!path || !*path) return;
    f = fopen(path, "rb");
    if (!f) return;
    if (fseek(f, 0, SEEK_END) == 0 && (len = ftell(f)) >= 0 &&
        len <= 65536) {
        if (fseek(f, 0, SEEK_SET) == 0) {
            buf = (char *)malloc((size_t)len + 1);
            if (buf) {
                size_t rd = fread(buf, 1, (size_t)len, f);
                buf[rd] = 0;
                json_str(buf, "user", user, 64, have_user);
                json_str(buf, "pass", pass, 128, have_pass);
                free(buf);
            }
        }
    }
    fclose(f);
}

void leader_setup_solver(LPSTR lpCmd) {
    char user[64] = "";
    char pass[128] = "";
    int have_user = 0, have_pass = 0;
    char buf[1024];
    char *tok, *save = NULL;

    if (lpCmd && *lpCmd) {
        strncpy(buf, lpCmd, sizeof(buf) - 1);
        buf[sizeof(buf) - 1] = 0;
        tok = strtok_r(buf, " \t\r", &save);
        while (tok) {
            if (stricmp(tok, "--solver-user") == 0) {
                tok = strtok_r(NULL, " \t\r", &save);
                if (tok) { strncpy(user, tok, sizeof(user) - 1); have_user = 1; }
            } else if (stricmp(tok, "--solver-pass") == 0) {
                tok = strtok_r(NULL, " \t\r", &save);
                if (tok) { strncpy(pass, tok, sizeof(pass) - 1); have_pass = 1; }
            } else if (stricmp(tok, "--solver-config") == 0) {
                tok = strtok_r(NULL, " \t\r", &save);
                if (tok) solver_config_read(tok, user, pass, &have_user, &have_pass);
            } else if (_strnicmp(tok, "--solver-user=", 14) == 0) {
                strncpy(user, tok + 14, sizeof(user) - 1); have_user = 1;
            } else if (_strnicmp(tok, "--solver-pass=", 14) == 0) {
                strncpy(pass, tok + 14, sizeof(pass) - 1); have_pass = 1;
            } else if (_strnicmp(tok, "--solver-config=", 16) == 0) {
                solver_config_read(tok + 16, user, pass, &have_user, &have_pass);
            }
            tok = strtok_r(NULL, " \t\r", &save);
        }
    }

    /* fall back to the environment */
    if (!have_user || !have_pass) {
        const char *eu = getenv("MS_SOLVER_USER");
        const char *ep = getenv("MS_SOLVER_PASS");
        if (!have_user && eu && *eu) { strncpy(user, eu, sizeof(user) - 1); have_user = 1; }
        if (!have_pass && ep && *ep) { strncpy(pass, ep, sizeof(pass) - 1); have_pass = 1; }
    }

    net_set_solver_creds(have_user && have_pass ? user : NULL,
                         have_user && have_pass ? pass : NULL);
}
