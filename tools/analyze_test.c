/* analyze_test.c - compare the C scenario engine against the Python solver.
 *
 * Reads a board from stdin:
 *   line 1: rows cols mines
 *   lines 2..: one row per line; chars:
 *     '0'-'8' revealed number, '*' revealed mine, 'F' flag,
 *     '?' question-mark (hidden, un-flagged), anything else hidden.
 * Prints, for every hidden cell in index order:
 *   cell r c p_mine p_safe reveals frontier
 * then a footer line with n_hidden n_free nonfrontier_p.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../src/analyze.h"

#define MAXC 900

static unsigned char revealed[MAXC], mine[MAXC], mark[MAXC], adj[MAXC];

int main(void) {
    int rows = 0, cols = 0, mines = 0;
    char buf[2048];
    int r, c, i;
    ScenarioReport rep;

    if (!fgets(buf, sizeof(buf), stdin)) return 2;
    if (sscanf(buf, "%d %d %d", &rows, &cols, &mines) != 3) return 2;
    if (rows * cols > MAXC) return 2;

    memset(revealed, 0, sizeof(revealed));
    memset(mine, 0, sizeof(mine));
    memset(mark, 0, sizeof(mark));
    memset(adj, 0, sizeof(adj));

    for (r = 0; r < rows; r++) {
        if (!fgets(buf, sizeof(buf), stdin)) return 2;
        for (c = 0; c < cols; c++) {
            i = r * cols + c;
            char ch = buf[c];
            if (ch >= '0' && ch <= '8') {
                revealed[i] = 1;
                adj[i] = (unsigned char)(ch - '0');
            } else if (ch == '*') {
                revealed[i] = 1;
                mine[i] = 1;
            } else if (ch == 'F') {
                mark[i] = 1;
            } else if (ch == '?') {
                /* hidden, un-flagged */
            }
            /* else '.' or anything: hidden */
        }
    }

    if (!scenario_analyze(rows, cols, mines, revealed, mine, mark, adj,
                          &rep)) {
        printf("FAIL: %s\n", rep.reason);
        return 1;
    }

    for (i = 0; i < rows * cols; i++) {
        int k;
        if (revealed[i] || mark[i] == 1) continue;
        k = i / cols;
        c = i % cols;
        /* rep.scenarios is sorted; find this cell */
        for (r = 0; r < rep.n_scenarios; r++) {
            if (rep.scenarios[r].cell == i) {
                printf("%d %d %d %.12g %.12g %d %d\n", i, k, c,
                       rep.scenarios[r].p_mine, rep.scenarios[r].p_safe,
                       rep.scenarios[r].reveals, rep.scenarios[r].frontier);
                break;
            }
        }
    }
    printf("# %d %d %.12g\n", rep.n_hidden, rep.n_free, rep.nonfrontier_p);
    scenario_report_free(&rep);
    return 0;
}
