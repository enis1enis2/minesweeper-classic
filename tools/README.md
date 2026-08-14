# Developer tooling

Smoke/verification harnesses used during development. All are optional dev
tooling; nothing here ships in the client or server binaries.

| Tool | What it does | Run |
|------|--------------|-----|
| `msys2_root.cmd` | Resolves `MSYS2_ROOT` once for local dev scripts: an already-set env var (CI via `setup-msys2`), else the documented scoop default, else `C:\msys64`. Call with `call "%~dp0msys2_root.cmd"`. | — |
| `run_analyze_diff.cmd` | Builds the C probability harness (`analyze_test.c` + `src/analyze.c`) with the MSYS2 mingw64 gcc, then diffs its per-cell frontier probabilities against the Python reference solver (`archive/minesweeper_bot`). | `tools\run_analyze_diff.cmd` (from repo root; needs Python on PATH) |
| `analyze_test.c` | C harness: reads a board, computes per-cell frontier probabilities, prints the layout for diffing. | built by `run_analyze_diff.cmd` |
| `analyze_test.py` | Python reference: same board format, same layout, using `archive/minesweeper_bot/ms_solver.py`. | invoked by `analyze_diff.py` |
| `analyze_diff.py` | Feeds boards (fixtures + generated) through both harnesses and reports mismatches. | `python tools\analyze_diff.py` |
| `board486.txt` | Regression fixture board (486-cell frontier) used by the diff harness. | — |
| `diag_test.c` | Unit test for the `src/diag.c` crash-text sanitizer (`diag_sanitize`). | `gcc -O2 -Wall -Wextra tools/diag_test.c -o build/diag_test.exe -lwinhttp -ladvapi32` then `build\diag_test.exe` |
| `diag_flow_test.c` | Unit test for the `src/diag.c` opt-out gate (`diag_on_connected` / `diag_set_opt_out`) and the bounded transport retry (`diag_send_thread`). Fakes the HTTPS transport via `-DDIAG_TEST_FAKE_POST` so it never touches the network. | `gcc -O2 -Wall -Wextra -DDIAG_TEST_FAKE_POST -DDIAG_SEND_ATTEMPTS=3 -DDIAG_RETRY_DELAY_MS=1 tools/diag_flow_test.c -o build/diag_flow_test.exe -lwinhttp -ladvapi32` then `build\diag_flow_test.exe` |

Notes:

- `analyze_test.py` expects to run with `archive/minesweeper_bot` importable
  (the harness inserts it on `sys.path`); keep the archive directory in place.
- `diag_test.c` and `diag_flow_test.c` both `#include` `../src/diag.c`
  directly, so never pass `src/diag.c` on the same gcc command line.
- `diag_flow_test.c` needs `-DDIAG_TEST_FAKE_POST` so the real WinHTTP
  transport is compiled out (without it the retry test would dial the real
  diagnostics host); `-DDIAG_RETRY_DELAY_MS=1` keeps the 3-attempt loops fast.
  The `#ifndef` guards in `src/diag.c` keep the production build unchanged.
- `run_analyze_diff.cmd` and `build.cmd` resolve the MSYS2 root through
  `tools\msys2_root.cmd` — an explicit `MSYS2_ROOT` env var always wins, so
  point it at your toolchain if it lives somewhere other than the scoop
  default or `C:\msys64`.
- `tools/` is committed and shipped in the repository; transient outputs
  (`__pycache__` and similar) are gitignored.

### Scripting-server integration tests (`archive/ms/tools/`)

Three socket tests drive a running game over the `--listen` scripting
protocol (default port 29000). They are the regression harness for the
seed-gate invariant (server broadcasts must never reset a passively
connected board, but a `req*` session applies streamed seeds) and the
`reqseed`/`reqbatch` wire layer. Invocation:

    # offline: pure command smoke, no telemetry, no production contact
    build\minesweeper-x64.exe --listen 29000 --no-telemetry
    python archive\ms\tools\cli_smoke.py 29000

    # online (integration): needs the game's telemetry connected to the sim
    # server (135.125.79.15:28571) so broadcast seeds + req* sessions work;
    # run the game WITHOUT --no-telemetry first
    python archive\ms\tools\cli_int.py 29000        # reqseed/reqbatch flow
    python archive\ms\tools\seed_gate_test.py 29000 # asserts PASS/FAIL, exit code

`cli_smoke.py` is a read-only dump (no assertions). `cli_int.py` prints the
interaction trace. `seed_gate_test.py` is the assertion-bearing one. The two
online scripts contact the production sim server by design (they test live
seeding); there is no offline substitute because the sim address is compiled
into the client.
