/*
 * leader.h - Hall of Fame (leaderboard) for Minesweeper (Classic).
 *
 * A player-chosen alias (stored in the [leaderboard] section of
 * minesweeper.ini) is attached to every finished win when auto-submit is on,
 * and the fastest time per name+difficulty is kept on the simulation server.
 * The Hall of Fame dialog pulls the current top times via the telemetry
 * stream (`lbtop`/`lbentry`/`lbdone`, implemented in network.c) and renders
 * them in a ListView.
 *
 * The solver credentials that unlock the protected remote-simulation seed
 * requests are configured through the command line (--solver-user/
 * --solver-pass or --solver-config <file>), falling back to the
 * MS_SOLVER_USER/MS_SOLVER_PASS environment variables.  See
 * leader_set_solver_creds().
 *
 * MIT License
 */
#ifndef LEADER_H
#define LEADER_H

#include <windows.h>

#define LEADER_NAME_MAX  16            /* [A-Za-z0-9_-]{1,16} */
#define LEADER_HOF_MAX   50            /* top-list rows requested */

/* 1 if name matches [A-Za-z0-9_-]{1,16} (pure function). */
int leader_name_valid(const char *name);

/* Persisted [leaderboard] name= alias (defaults to "Player").  Only valid
 * after diag_init() has resolved the config directory. */
const char *leader_player_name(void);

/* Validate and persist the player alias.  Returns 1 on success. */
int leader_set_player_name(const char *name);

/* [leaderboard] auto_submit= (default 1): attach the alias to new best times
 * automatically when a game is won. */
int leader_auto_submit(void);
void leader_set_auto_submit(int on);

/* Submit a finished win (preset difficulty index diff, elapsed time_ms) if
 * auto-submit is on, a name is set, and telemetry is live.  Cheap and safe
 * to call from end_game_win(). */
void leader_submit_win(int diff, int time_ms);

/* Open the Hall of Fame leaderboard dialog (modal). */
void leader_show_hof(HWND parent);

/* Open the player-name chooser dialog (modal). */
void leader_show_player(HWND parent);

/* Apply the auto-submit check state to the menu.  Call from
 * update_menu_checks(). */
void leader_sync_menu(HMENU menu);

/* Read solver credentials and configure the network layer.  Precedence:
 * explicit args > --solver-config <json file> > MS_SOLVER_USER/_PASS env.
 * With no credentials the solver stays disabled. */
void leader_setup_solver(LPSTR lpCmd);

#endif /* LEADER_H */
