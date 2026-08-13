# Developer tooling

Smoke/verification harnesses used during development. All are optional dev
tooling; nothing here ships in the client or server binaries.

| Tool | What it does | Run |
|------|--------------|-----|
| `run_analyze_diff.cmd` | Builds the C probability harness (`analyze_test.c` + `src/analyze.c`) with the scoop MSYS2 mingw64 gcc, then diffs its per-cell frontier probabilities against the Python reference solver (`archive/minesweeper_bot`). | `tools\run_analyze_diff.cmd` (from repo root; needs Python on PATH) |
| `analyze_test.c` | C harness: reads a board, computes per-cell frontier probabilities, prints the layout for diffing. | built by `run_analyze_diff.cmd` |
| `analyze_test.py` | Python reference: same board format, same layout, using `archive/minesweeper_bot/ms_solver.py`. | invoked by `analyze_diff.py` |
| `analyze_diff.py` | Feeds boards (fixtures + generated) through both harnesses and reports mismatches. | `python tools\analyze_diff.py` |
| `board486.txt` | Regression fixture board (486-cell frontier) used by the diff harness. | — |
| `diag_test.c` | Unit test for the `src/diag.c` crash-text sanitizer (`diag_sanitize`). | `gcc -O2 -Wall -Wextra tools/diag_test.c -o build/diag_test.exe -lwinhttp -ladvapi32` then `build\diag_test.exe` |

Notes:

- `analyze_test.py` expects to run with `archive/minesweeper_bot` importable
  (the harness inserts it on `sys.path`); keep the archive directory in place.
- `run_analyze_diff.cmd` hardcodes the scoop MSYS2 root; adjust
  `MSYS2_ROOT` if your toolchain lives elsewhere.
- `tools/` is committed and shipped in the repository; transient outputs
  (`__pycache__`, `gcc.log`, `compile_test.cmd`) are gitignored.
