/*
 * ms_ini.h - minimal INI-style config persistence for the Linux port.
 *
 * Replaces the Win32 GetPrivateProfileStringA/WritePrivateProfileStringA
 * used by src/leader.c.  State lives in a single file at
 *   $XDG_CONFIG_HOME/minesweeper.ini   (or $HOME/.config/minesweeper.ini)
 *
 * The format matches classic Windows INI files: [section] headers and
 * key=value pairs.  Values are stored/retrieved as plain text; surrounding
 * whitespace and double quotes are trimmed on read.
 *
 * MIT License
 */
#ifndef MS_INI_H
#define MS_INI_H

#include <stddef.h>

/* Resolve the config file path (cached after first call). */
const char *ms_ini_path(void);

/* Read a value from [sec] key=, defaulting to `def` when absent. */
int  ms_ini_get_str(const char *sec, const char *key, const char *def,
                    char *out, size_t outsz);
/* Write (or create) key=value under [sec]. */
void ms_ini_set_str(const char *sec, const char *key, const char *value);

int  ms_ini_get_int(const char *sec, const char *key, int def);
void ms_ini_set_int(const char *sec, const char *key, int value);

#endif /* MS_INI_H */
