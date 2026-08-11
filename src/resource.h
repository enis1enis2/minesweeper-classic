#ifndef RESOURCE_H
#define RESOURCE_H

#define IDR_MANIFEST     1
#define IDR_MENU       101
#define IDI_MAIN       102

#define IDD_CUSTOM     200
#define IDC_ROWS      1001
#define IDC_COLS      1002
#define IDC_MINES     1003
#define IDC_STATIC    -1

#define IDD_SEEDS     201

/* remote-simulation prompt shown after a custom seed is committed */
#define IDD_REMSIM    202
#define IDC_REMSIM_MSG    1020
#define IDC_REMSIM_MULTI  1021
#define IDC_REMSIM_UNTIL  1032

/* in-game scenario-probability dialog */
#define IDD_SCENARIO  203
#define IDC_SCEN_INFO 1030
#define IDC_SCEN_LIST 1031

/* per-difficulty seed rows; every row uses IDC_SEED_ROW_STRIDE consecutive
 * ids: Off radio, Normal radio, Custom radio, value edit, result label. */
#define IDC_SEED_OFF_BEGIN    1010
#define IDC_SEED_NORMAL_BEGIN 1011
#define IDC_SEED_CUSTOM_BEGIN 1012
#define IDC_SEED_VALUE_BEGIN  1013
#define IDC_SEED_RESULT_BEGIN 1014
#define IDC_SEED_ROW_STRIDE   5

#define IDM_NEW        40001
#define IDM_BEGIN      40002
#define IDM_INTERMEDIATE 40003
#define IDM_EXPERT     40004
#define IDM_CUSTOM     40005
#define IDM_MARKS      40006
#define IDM_EXIT       40007
#define IDM_ABOUT      40008
#define IDM_SEEDS      40009
#define IDM_SCENARIOS  40010
#define IDM_PRIVACY    40011

/* Hall of Fame (leaderboard) */
#define IDM_HALL_OF_FAME 40012

/* privacy (device-diagnostics) dialog */
#define IDD_PRIVACY      204
#define IDC_PRIV_DIAG    1040

/* first-connect device-diagnostics disclosure banner */
#define IDD_DIAG_BANNER  205

/* Player-name chooser + Hall of Fame leaderboard */
#define IDD_PLAYER      206
#define IDC_PLAYER_NAME  1050
#define IDC_PLAYER_OK    1051

#define IDD_HOF         207
#define IDC_HOF_FILTER  1060
#define IDC_HOF_LIST    1061
#define IDC_HOF_SUBMIT  1062
#define IDC_HOF_STATUS  1063

/* Hall of Fame menu (popup): choose player, view, auto-submit */
#define IDM_HOF_PLAYER  41001
#define IDM_HOF_VIEW    41002
#define IDM_HOF_AUTO    41003

#endif
