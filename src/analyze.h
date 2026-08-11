/*
 * analyze.h - in-game scenario analyzer for Minesweeper (Classic).
 *
 * Given the current board (revealed/flag state, adjacency counts and mine
 * count), compute an exact per-cell P(mine) over the frontier using the same
 * algorithm as the Python solver (minesweeper_bot/ms_solver.py): split the
 * frontier into components via shared revealed-number constraints, enumerate
 * every consistent mine placement per component, condition each component on
 * the global mine count with binomial weights over the non-frontier cells,
 * and report P(mine) for every hidden cell.
 *
 * Each hidden cell is a "scenario": clicking it either reveals (win path,
 * P(safe)) or hits a mine (loss path, P(loss) == P(mine)).  The report is
 * ranked best-first so the safest move is first.
 *
 * MIT License
 */
#ifndef ANALYZE_H
#define ANALYZE_H

typedef struct {
    int    cell;          /* r*cols+c */
    int    r, c;
    double p_mine;        /* probability the cell hides a mine */
    double p_safe;        /* 1 - p_mine */
    double p_loss;        /* == p_mine; the loss scenario */
    int    frontier;      /* 1 if the cell borders a revealed number */
    int    reveals;       /* cells that would open if this cell were safe */
} Scenario;

typedef struct {
    int      rows, cols, total_cells, mines;
    int      n_hidden;        /* hidden, un-flagged cells */
    int      n_free;          /* hidden cells not bordering a number */
    double   nonfrontier_p;   /* P(mine) for a free cell */
    int      n_scenarios;     /* == n_hidden when analyzed */
    Scenario *scenarios;      /* sorted best-first (highest p_safe) */
    int      solved;          /* 1 if probabilities were computed */
    char     reason[96];      /* why solved==0 (or "OK") */
} ScenarioReport;

/* Analyze `revealed`/`mine`/`mark`/`adj` (per-cell byte arrays of length
 * rows*cols; mark==1 means flagged).  Fills *out; the caller must free the
 * result with scenario_report_free().  Returns 1 on success (even when the
 * board is so open that every cell is equally likely), 0 if there was
 * nothing to analyze. */
int  scenario_analyze(int rows, int cols, int mines,
                      const unsigned char *revealed,
                      const unsigned char *mine,
                      const unsigned char *mark,
                      const unsigned char *adj,
                      ScenarioReport *out);

void scenario_report_free(ScenarioReport *rep);

#endif /* ANALYZE_H */
