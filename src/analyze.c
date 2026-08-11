/*
 * analyze.c - in-game scenario analyzer for Minesweeper (Classic).
 *
 * Exact per-cell mine probabilities over the frontier, ported from the
 * Python solver (minesweeper_bot/ms_solver.py):
 *
 *   1. Build constraints from revealed numbers: each revealed cell with a
 *      non-zero count contributes `hidden neighbours must contain need mines`
 *      where need = adj - flags_around.
 *   2. Frontier = union of all constrained hidden cells.  Split it into
 *      components via union-find over cells that share a constraint.
 *   3. For every component, enumerate ALL consistent mine placements
 *      (subset search with constraint-sum pruning, ordered by the most
 *      constrained cell first) and tally, per total mines placed, the number
 *      of solutions (S) and the per-cell mine counts (T).
 *   4. Condition the whole frontier on the global mine count: the hidden
 *      non-frontier ("free") cells make C(n_free, M - t) completions for a
 *      frontier that uses t mines, so each t is weighted by that binomial.
 *   5. P(mine) per frontier cell = expected count of solutions with that
 *      cell a mine, over all weighted completions.  Free cells all share
 *      P(mine) = (M - E[frontier mines]) / n_free.
 *
 * Every hidden cell is a "scenario": click it and you either win the cell
 * (P(safe)) or hit a mine (P(loss) == P(mine)).  The report is sorted
 * best-first.
 *
 * Components that exceed the node budget are treated as free cells (the
 * same fallback the Python solver uses).
 *
 * MIT License
 */

#include <math.h>
#include <stdlib.h>
#include <string.h>

#include "analyze.h"

#define A_MAX_CELLS 900
#define A_MAX_NEIGH 8
#define A_NODE_BUDGET 2000000

/* ------------------------------------------------ module state (per call) */
static int a_rows, a_cols, a_n;
static unsigned char a_hidden[A_MAX_CELLS];
static unsigned char a_flagged[A_MAX_CELLS];

static inline int a_idx(int r, int c) { return r * a_cols + c; }
static inline int a_inb(int r, int c) {
    return r >= 0 && r < a_rows && c >= 0 && c < a_cols;
}

/* one revealed-number constraint */
typedef struct {
    int cells[A_MAX_NEIGH];
    int count;
    int need;
} ACons;

/* -------------------------------------------------------- union-find over */
static int a_parent[A_MAX_CELLS];

static int uf_find(int x) {
    while (a_parent[x] != x) {
        a_parent[x] = a_parent[a_parent[x]];
        x = a_parent[x];
    }
    return x;
}

static void uf_union(int a, int b) {
    a = uf_find(a);
    b = uf_find(b);
    if (a != b) a_parent[a] = b;
}

/* --------------------------------------------- log C(n,k) with lgamma() */
static double logcomb(int n, int k) {
    if (n < 0 || k < 0 || k > n) return -INFINITY;
    return lgamma((double)n + 1.0) - lgamma((double)k + 1.0)
         - lgamma((double)(n - k) + 1.0);
}

/* --------------------------------------------------- component solving */
typedef struct {
    double *S;   /* S[t] = solutions with t mines in the component */
    double *T;   /* T[li*(m+1)+t] = solutions with cell li a mine, t mines */
    int m;
    int solved;
} AComp;

static int comp_solve(const int *cells, int m, const ACons *cons, int ncons,
                      AComp *out) {
    int li, ci, p;
    int *cell_pos, *order, *assigned;
    int *con_mines, *con_left;
    int member_len[900];
    int member[900][9];
    double *S, *T;
    long long nodes = 0;

    if (m == 0) {
        out->m = 0;
        out->S = calloc(1, sizeof(double));
        out->T = calloc(1, sizeof(double));
        if (!out->S || !out->T) { free(out->S); free(out->T); return 0; }
        out->S[0] = 1.0;
        out->solved = 1;
        return 1;
    }

    cell_pos = malloc(sizeof(int) * A_MAX_CELLS);
    order = malloc(sizeof(int) * m);
    assigned = malloc(sizeof(int) * m);
    con_mines = calloc(ncons, sizeof(int));
    con_left = malloc(sizeof(int) * ncons);
    if (!cell_pos || !order || !assigned || !con_mines || !con_left) {
        free(cell_pos); free(order); free(assigned);
        free(con_mines); free(con_left);
        return 0;
    }

    for (li = 0; li < m; li++) cell_pos[cells[li]] = li;
    for (ci = 0; ci < ncons; ci++) {
        con_mines[ci] = 0;
        con_left[ci] = cons[ci].count;
    }
    memset(member_len, 0, sizeof(member_len));
    for (ci = 0; ci < ncons; ci++) {
        for (p = 0; p < cons[ci].count; p++) {
            li = cell_pos[cons[ci].cells[p]];
            member[li][member_len[li]++] = ci;
        }
    }

    /* order: most-constrained cell first */
    for (li = 0; li < m; li++) order[li] = li;
    for (p = 0; p < m; p++)
        for (int q = p + 1; q < m; q++) {
            int a = order[p], b = order[q];
            if (member_len[b] > member_len[a] ||
                (member_len[b] == member_len[a] && b < a)) {
                order[p] = b;
                order[q] = a;
            }
        }
    for (li = 0; li < m; li++) assigned[li] = -1;

    S = calloc((size_t)m + 1, sizeof(double));
    T = calloc((size_t)m * (m + 1), sizeof(double));
    if (!S || !T) {
        free(S); free(T);
        free(cell_pos); free(order); free(assigned);
        free(con_mines); free(con_left);
        return 0;
    }

    {
        int *stack_p = malloc(sizeof(int) * (m + 1));
        int *stack_v = malloc(sizeof(int) * (m + 1)); /* next value to try */
        int *cur_v = malloc(sizeof(int) * (m + 1));   /* value applied now */
        int depth = 1;
        int ok = 1;
        if (!stack_p || !stack_v || !cur_v) {
            ok = 0;
        } else {
            stack_p[0] = -1;
            stack_v[0] = 0;
            cur_v[0] = -1;
            while (depth > 0 && nodes <= A_NODE_BUDGET) {
                int top = depth - 1;
                int pp = stack_p[top];
                if (pp == -1) {
                    /* fresh frame: pick the cell for this level */
                    if (top == m) {
                        /* leaf: count the assignment */
                        int total = 0;
                        nodes++;
                        for (li = 0; li < m; li++)
                            if (assigned[li] == 1) total++;
                        S[total] += 1.0;
                        for (li = 0; li < m; li++)
                            if (assigned[li] == 1)
                                T[li * (m + 1) + total] += 1.0;
                        depth--;   /* pop; the parent undoes its value */
                        continue;
                    }
                    stack_p[top] = order[top];
                    stack_v[top] = 0;
                    cur_v[top] = -1;
                    continue;
                }
                if (cur_v[top] == -1) {
                    /* choose the next value to try for pp */
                    int v = stack_v[top];
                    if (v >= 2) {
                        depth--;   /* frame done, nothing to undo */
                        continue;
                    }
                    {
                        int feasible = 1;
                        for (int mmi = 0; mmi < member_len[pp]; mmi++) {
                            int cidx = member[pp][mmi];
                            int need = cons[cidx].need;
                            int nm = con_mines[cidx] + v;
                            if (nm > need) { feasible = 0; break; }
                            int nl = con_left[cidx] - 1;
                            if (need - nm > nl) { feasible = 0; break; }
                        }
                        if (!feasible) {
                            stack_v[top] = v + 1;
                            continue;
                        }
                    }
                    assigned[pp] = v;
                    for (int mmi = 0; mmi < member_len[pp]; mmi++) {
                        int cidx = member[pp][mmi];
                        con_mines[cidx] += v;
                        con_left[cidx] -= 1;
                    }
                    cur_v[top] = v;
                    stack_v[top] = v + 1;
                    /* descend */
                    depth++;
                    stack_p[top + 1] = -1;
                    stack_v[top + 1] = 0;
                    cur_v[top + 1] = -1;
                    nodes++;
                } else {
                    /* child subtree done: undo our applied value */
                    int v = cur_v[top];
                    for (int mmi = 0; mmi < member_len[pp]; mmi++) {
                        int cidx = member[pp][mmi];
                        con_mines[cidx] -= v;
                        con_left[cidx] += 1;
                    }
                    assigned[pp] = -1;
                    cur_v[top] = -1;
                }
            }
            if (nodes > A_NODE_BUDGET) ok = 0;
        }
        free(stack_p);
        free(stack_v);
        free(cur_v);
        if (!ok) {
            free(S); free(T);
            free(cell_pos); free(order); free(assigned);
            free(con_mines); free(con_left);
            return 0;
        }
    }

    free(cell_pos); free(order); free(assigned);
    free(con_mines); free(con_left);
    out->S = S;
    out->T = T;
    out->m = m;
    out->solved = 1;
    return 1;
}

/* ---------------------------------------------------- distributions */
typedef struct {
    double *v;
    int len;
} Dist;

/* convolution: out[i] = sum A[j]*B[i-j], length la+lb-1 */
static void conv_into(const double *A, int la, const double *B, int lb,
                      double *out) {
    int i, j;
    for (i = 0; i < la + lb - 1; i++) out[i] = 0.0;
    for (j = 0; j < lb; j++)
        if (B[j] != 0.0)
            for (i = 0; i < la; i++)
                out[i + j] += A[i] * B[j];
}

/* ------------------------------------------------------------- analysis */
static unsigned char flood_reveals_scratch[A_MAX_CELLS];

static int flood_reveals(const unsigned char *revealed,
                         const unsigned char *adj,
                         const unsigned char *mark, int c) {
    /* number of hidden cells that would open if c were revealed */
    int stack[A_MAX_CELLS];
    int sp = 0, cnt = 0, i;
    memset(flood_reveals_scratch, 0, sizeof(flood_reveals_scratch));
    flood_reveals_scratch[c] = 1;
    stack[sp++] = c;
    while (sp > 0) {
        int x = stack[--sp];
        cnt++;
        if (adj[x] == 0) {
            int r = x / a_cols, cc = x % a_cols, dr, dc;
            for (dr = -1; dr <= 1; dr++)
                for (dc = -1; dc <= 1; dc++) {
                    int rr = r + dr, c2 = cc + dc;
                    if (!a_inb(rr, c2)) continue;
                    i = a_idx(rr, c2);
                    if (!revealed[i] && mark[i] != 1 &&
                        !flood_reveals_scratch[i]) {
                        flood_reveals_scratch[i] = 1;
                        stack[sp++] = i;
                    }
                }
        }
    }
    return cnt;
}

static int scen_cmp(const void *A, const void *B) {
    const Scenario *a = (const Scenario *)A, *b = (const Scenario *)B;
    if (b->p_safe > a->p_safe) return 1;
    if (b->p_safe < a->p_safe) return -1;
    if (b->reveals != a->reveals) return b->reveals - a->reveals;
    if (a->r != b->r) return a->r - b->r;
    return a->c - b->c;
}

int scenario_analyze(int rows, int cols, int mines,
                     const unsigned char *revealed,
                     const unsigned char *mine,
                     const unsigned char *mark,
                     const unsigned char *adj,
                     ScenarioReport *out) {
    ACons cons[A_MAX_CELLS];
    int ncons = 0, i, j, r, c, dr, dc;
    int *frontier, n_frontier = 0, *in_frontier;
    int free_count;
    AComp *comps;
    int ncomps = 0;
    int *root_of;
    int *comp_root;
    int *comp_cells_ofs;
    int *comp_cells;
    Dist *dists;
    int ok = 1;

    memset(out, 0, sizeof(*out));
    out->rows = rows;
    out->cols = cols;
    out->total_cells = rows * cols;
    out->mines = mines;
    strcpy(out->reason, "OK");

    if (rows <= 0 || cols <= 0 || rows * cols > A_MAX_CELLS) {
        strcpy(out->reason, "invalid board");
        return 0;
    }
    a_rows = rows;
    a_cols = cols;
    a_n = rows * cols;

    /* visibility */
    for (i = 0; i < a_n; i++) {
        a_hidden[i] = (!revealed[i] && mark[i] != 1) ? 1 : 0;
        a_flagged[i] = (mark[i] == 1) ? 1 : 0;
        if (a_hidden[i]) out->n_hidden++;
    }
    if (out->n_hidden == 0) {
        strcpy(out->reason, "no hidden cells");
        return 0;
    }

    /* constraints from revealed numbers */
    for (i = 0; i < a_n; i++) {
        int cnt = 0, need, fl;
        if (!revealed[i] || mark[i] == 1 || mine[i]) continue;
        r = i / cols;
        c = i % cols;
        fl = 0;
        for (dr = -1; dr <= 1; dr++)
            for (dc = -1; dc <= 1; dc++) {
                int rr = r + dr, cc = c + dc;
                if (!a_inb(rr, cc)) continue;
                j = a_idx(rr, cc);
                if (a_flagged[j]) fl++;
            }
        need = (int)adj[i] - fl;
        for (dr = -1; dr <= 1; dr++)
            for (dc = -1; dc <= 1; dc++) {
                int rr = r + dr, cc = c + dc;
                if (!a_inb(rr, cc)) continue;
                j = a_idx(rr, cc);
                if (a_hidden[j]) cons[ncons].cells[cnt++] = j;
            }
        if (cnt == 0) continue;
        cons[ncons].count = cnt;
        cons[ncons].need = need;
        if (need < 0 || need > cnt) continue;   /* impossible; skip */
        ncons++;
    }

    /* union-find over frontier cells */
    frontier = malloc(sizeof(int) * a_n);
    in_frontier = malloc(sizeof(int) * a_n);
    if (!frontier || !in_frontier) {
        free(frontier); free(in_frontier);
        strcpy(out->reason, "out of memory");
        return 0;
    }
    for (i = 0; i < a_n; i++) { a_parent[i] = i; in_frontier[i] = 0; }
    for (i = 0; i < ncons; i++)
        for (j = 1; j < cons[i].count; j++)
            uf_union(cons[i].cells[0], cons[i].cells[j]);
    for (i = 0; i < ncons; i++) {
        if (!in_frontier[cons[i].cells[0]]) {
            in_frontier[cons[i].cells[0]] = 1;
            frontier[n_frontier++] = cons[i].cells[0];
        }
        for (j = 1; j < cons[i].count; j++)
            if (!in_frontier[cons[i].cells[j]]) {
                in_frontier[cons[i].cells[j]] = 1;
                frontier[n_frontier++] = cons[i].cells[j];
            }
    }
    free_count = out->n_hidden - n_frontier;
    out->n_free = free_count;

    /* group frontier cells into components by root */
    root_of = malloc(sizeof(int) * a_n);
    comp_root = malloc(sizeof(int) * a_n);
    comp_cells_ofs = malloc(sizeof(int) * (a_n + 1));
    comp_cells = malloc(sizeof(int) * (n_frontier > 0 ? n_frontier : 1));
    if (!root_of || !comp_root || !comp_cells_ofs || !comp_cells) {
        free(frontier); free(in_frontier); free(root_of); free(comp_root);
        free(comp_cells_ofs); free(comp_cells);
        strcpy(out->reason, "out of memory");
        return 0;
    }
    ncomps = 0;
    for (i = 0; i < n_frontier; i++) {
        int rt = uf_find(frontier[i]);
        int found = -1;
        for (j = 0; j < ncomps; j++)
            if (comp_root[j] == rt) { found = j; break; }
        if (found < 0) {
            comp_root[ncomps] = rt;
            root_of[frontier[i]] = ncomps;
            ncomps++;
        } else {
            root_of[frontier[i]] = found;
        }
    }
    {
        int *sizes = malloc(sizeof(int) * (ncomps > 0 ? ncomps : 1));
        if (!sizes) {
            free(frontier); free(in_frontier); free(root_of); free(comp_root);
            free(comp_cells_ofs); free(comp_cells);
            strcpy(out->reason, "out of memory");
            return 0;
        }
        for (i = 0; i < ncomps; i++) sizes[i] = 0;
        for (i = 0; i < n_frontier; i++) sizes[root_of[frontier[i]]]++;
        comp_cells_ofs[0] = 0;
        for (i = 1; i <= ncomps; i++)
            comp_cells_ofs[i] = comp_cells_ofs[i - 1] + sizes[i - 1];
        for (i = 0; i < n_frontier; i++) {
            int ci = root_of[frontier[i]];
            comp_cells[comp_cells_ofs[ci] + (--sizes[ci])] = frontier[i];
        }
        free(sizes);
    }

    /* solve each component */
    comps = calloc((size_t)ncomps, sizeof(AComp));
    dists = calloc((size_t)ncomps, sizeof(Dist));
    if (!comps || !dists) {
        ok = 0;
    } else {
        int si = 0;
        for (i = 0; i < ncomps; i++) {
            int m = comp_cells_ofs[i + 1] - comp_cells_ofs[i];
            int *cc = &comp_cells[comp_cells_ofs[i]];
            ACons local[512];
            int nlocal = 0, k;
            for (k = 0; k < ncons; k++) {
                /* constraint belongs to this component if its root matches */
                if (uf_find(cons[k].cells[0]) == comp_root[i]) nlocal++;
            }
            if (nlocal > 512) {
                /* too many constraints to enumerate: treat as free cells */
                for (k = 0; k < m; k++) {
                    in_frontier[cc[k]] = 0;
                }
                free_count += m;
                continue;
            }
            nlocal = 0;
            for (k = 0; k < ncons; k++) {
                if (uf_find(cons[k].cells[0]) == comp_root[i]) {
                    local[nlocal++] = cons[k];
                }
            }
            if (comp_solve(cc, m, local, nlocal, &comps[si])) {
                double tot = 0.0;
                dists[si].len = m + 1;
                dists[si].v = calloc((size_t)(m + 1), sizeof(double));
                if (!dists[si].v) { ok = 0; break; }
                for (j = 0; j <= m; j++) tot += comps[si].S[j];
                for (j = 0; j <= m; j++)
                    dists[si].v[j] = (tot > 0.0) ? comps[si].S[j] / tot : 0.0;
                si++;
            } else {
                /* unsolvable component: drop its cells to the free pool */
                for (k = 0; k < m; k++) {
                    in_frontier[cc[k]] = 0;
                }
                free_count += m;
            }
        }
        ncomps = si;   /* only solved components remain */
    }
    free(comp_root);

    if (!ok) {
        free(frontier); free(in_frontier); free(root_of);
        free(comp_cells_ofs); free(comp_cells);
        for (i = 0; i < ncomps; i++) { free(comps[i].S); free(comps[i].T); }
        free(comps); free(dists);
        strcpy(out->reason, "out of memory");
        return 0;
    }

    /* per-cell mine probabilities, then assemble the scenario list */
    {
        double *p_cell = malloc(sizeof(double) * a_n);
        if (!p_cell) {
            free(frontier); free(in_frontier); free(root_of);
            free(comp_cells_ofs); free(comp_cells);
            for (i = 0; i < ncomps; i++) { free(comps[i].S); free(comps[i].T); }
            free(comps); free(dists);
            strcpy(out->reason, "out of memory");
            return 0;
        }
        if (ncomps == 0) {
            /* no solvable frontier (no numbers, or every component was too
             * complex to enumerate): every hidden cell is a fair guess */
            goto uniform_path;
        } else {
            int total = 0;
            double *D, *w, Z, E_front;
            Dist *prefix, *suffix;
            double *d_except;

            for (i = 0; i < ncomps; i++) total += comps[i].m;
            out->n_free = free_count;
            out->nonfrontier_p = 0.0;

            /* prefix/suffix convolutions of component mine-count dists */
            prefix = calloc((size_t)ncomps + 1, sizeof(Dist));
            suffix = calloc((size_t)ncomps + 1, sizeof(Dist));
            d_except = malloc(sizeof(double) * (total + 1));
            D = calloc((size_t)total + 1, sizeof(double));
            w = calloc((size_t)total + 1, sizeof(double));
            if (!prefix || !suffix || !d_except || !D || !w) {
                ok = 0;
            } else {
                prefix[0].v = calloc(1, sizeof(double));
                prefix[0].len = 1;
                prefix[0].v[0] = 1.0;
                for (i = 0; i < ncomps; i++) {
                    int la = prefix[i].len, lb = dists[i].len;
                    prefix[i + 1].len = la + lb - 1;
                    prefix[i + 1].v =
                        calloc((size_t)(la + lb - 1), sizeof(double));
                    if (!prefix[i + 1].v) { ok = 0; break; }
                    conv_into(prefix[i].v, la, dists[i].v, lb,
                              prefix[i + 1].v);
                }
                if (ok) {
                    suffix[ncomps].v = calloc(1, sizeof(double));
                    suffix[ncomps].len = 1;
                    suffix[ncomps].v[0] = 1.0;
                    for (i = ncomps - 1; i >= 0 && ok; i--) {
                        int la = dists[i].len, lb = suffix[i + 1].len;
                        suffix[i].len = la + lb - 1;
                        suffix[i].v = calloc((size_t)(la + lb - 1),
                                             sizeof(double));
                        if (!suffix[i].v) { ok = 0; break; }
                        conv_into(dists[i].v, la, suffix[i + 1].v, lb,
                                  suffix[i].v);
                    }
                }
            }
            if (!ok) {
                free(prefix); free(suffix); free(d_except); free(D); free(w);
                free(p_cell);
                free(frontier); free(in_frontier); free(root_of);
                free(comp_cells_ofs); free(comp_cells);
                for (i = 0; i < ncomps; i++) {
                    free(comps[i].S); free(comps[i].T);
                }
                free(comps); free(dists);
                strcpy(out->reason, "out of memory");
                return 0;
            }

            for (i = 0; i <= total; i++) D[i] = prefix[ncomps].v[i];

            /* binomial weights over free cells: C(free_count, M - t), taken
             * only over the achievable frontier totals (D's support), exactly
             * like the Python solver iterating the D dict */
            {
                double maxl = -INFINITY;
                for (i = 0; i <= total; i++) {
                    if (D[i] <= 0.0) { w[i] = 0.0; continue; }
                    double lc = logcomb(free_count, mines - i);
                    if (lc > maxl) maxl = lc;
                    w[i] = lc;
                }
                if (maxl == -INFINITY) {
                    /* no free-cell split is possible for any achievable
                     * frontier total (e.g. the frontier already accounts for
                     * every mine, or a wrong flag overconstrains the board):
                     * treat all splits as equally weighted, like the Python
                     * solver's mx <= 0 branch */
                    for (i = 0; i <= total; i++) w[i] = 1.0;
                } else {
                    for (i = 0; i <= total; i++)
                        w[i] = (w[i] == -INFINITY) ? 0.0 : exp(w[i] - maxl);
                }
            }

            Z = 0.0;
            E_front = 0.0;
            for (i = 0; i <= total; i++) {
                Z += D[i] * w[i];
                E_front += (double)i * D[i] * w[i];
            }
            if (Z <= 0.0) {
                /* no consistent placement exists (e.g. wrong flags made the
                 * constraints unsatisfiable): fall back to a fair guess */
                free(prefix); free(suffix); free(d_except); free(D); free(w);
                goto uniform_path;
            }
            E_front /= Z;
            if (free_count > 0) {
                double p = ((double)mines - E_front) / free_count;
                if (p < 0.0) p = 0.0;
                if (p > 1.0) p = 1.0;
                out->nonfrontier_p = p;
            }

            /* frontier cells: sum over other-component mine counts */
            for (i = 0; i < a_n; i++) p_cell[i] = 0.0;
            for (i = 0; i < ncomps; i++) {
                int m = comps[i].m;
                int la = prefix[i].len, lb = suffix[i + 1].len;
                double tot_i = 0.0;
                int li, t, o;
                for (t = 0; t <= m; t++) tot_i += comps[i].S[t];
                memset(d_except, 0, sizeof(double) * (total + 1));
                conv_into(prefix[i].v, la, suffix[i + 1].v, lb, d_except);
                for (li = 0; li < m; li++) {
                    int cell = comp_cells[comp_cells_ofs[i] + li];
                    double num = 0.0;
                    for (t = 0; t <= m; t++) {
                        double cnt = comps[i].T[li * (m + 1) + t];
                        if (cnt == 0.0) continue;
                        double u = 0.0;
                        for (o = 0; o < la + lb - 1; o++) {
                            int tt = t + o;
                            if (tt <= total && w[tt] > 0.0)
                                u += d_except[o] * w[tt];
                        }
                        num += (cnt / tot_i) * u;
                    }
                    p_cell[cell] = num / Z;
                }
            }
            free(prefix); free(suffix); free(d_except); free(D); free(w);
        }
        goto after_uniform;

uniform_path:
        /* fair-guess fallback: every hidden cell has the same P(mine) */
        {
            double p = (double)mines / out->n_hidden;
            if (p < 0.0) p = 0.0;
            if (p > 1.0) p = 1.0;
            out->n_free = out->n_hidden;
            out->nonfrontier_p = p;
            for (i = 0; i < a_n; i++) {
                in_frontier[i] = 0;
                p_cell[i] = 0.0;
            }
        }
after_uniform:
        ;

        /* assemble scenarios (one per hidden cell, sorted best-first) */
        out->n_scenarios = out->n_hidden;
        out->scenarios = malloc(sizeof(Scenario) * out->n_scenarios);
        if (!out->scenarios) {
            free(p_cell);
            free(frontier); free(in_frontier); free(root_of);
            free(comp_cells_ofs); free(comp_cells);
            for (i = 0; i < ncomps; i++) { free(comps[i].S); free(comps[i].T); }
            free(comps); free(dists);
            strcpy(out->reason, "out of memory");
            return 0;
        }
        {
            int k = 0;
            for (i = 0; i < a_n; i++) {
                Scenario *s;
                if (!a_hidden[i]) continue;
                s = &out->scenarios[k++];
                s->cell = i;
                s->r = i / cols;
                s->c = i % cols;
                s->frontier = in_frontier[i];
                s->p_mine = in_frontier[i] ? p_cell[i] : out->nonfrontier_p;
                s->p_loss = s->p_mine;
                s->p_safe = 1.0 - s->p_mine;
                if (s->p_safe < 0.0) s->p_safe = 0.0;
                if (s->p_safe > 1.0) s->p_safe = 1.0;
                s->p_mine = 1.0 - s->p_safe;
                s->p_loss = s->p_mine;
                s->reveals = flood_reveals(revealed, adj, mark, i);
            }
            qsort(out->scenarios, out->n_scenarios, sizeof(Scenario),
                  scen_cmp);
        }
        free(p_cell);
    }

    free(frontier); free(in_frontier); free(root_of);
    free(comp_cells_ofs); free(comp_cells);
    for (i = 0; i < ncomps; i++) { free(comps[i].S); free(comps[i].T); }
    free(comps); free(dists);

    out->solved = 1;
    return 1;
}

void scenario_report_free(ScenarioReport *rep) {
    if (!rep) return;
    free(rep->scenarios);
    rep->scenarios = NULL;
    rep->n_scenarios = 0;
}
