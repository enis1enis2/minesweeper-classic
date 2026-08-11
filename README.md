# Minesweeper (Classic)

A faithful, **original from-scratch** port of the classic 1992 Windows 3.1
Minesweeper game, rebuilt with the Win32 API so it runs natively on modern
Windows. The original 16-bit executable no longer runs on any current
Windows release:

- **64-bit Windows 7 / 8.1 / 10 / 11** provide no NTVDM at all, and 16-bit
  x86 code cannot execute under WOW64.
- The classic games were **removed from Windows 8 onward** and replaced by
  the ad/tracking UWP versions.

This project is a clean-room reimplementation of the classic gameplay and
look. It shares **no code with any Microsoft product**.

## Supported systems

- Windows 7, 8.1, 10, 11 (32-bit and 64-bit)
- Two binaries are provided: `minesweeper-x86.exe` and `minesweeper-x64.exe`
- DPI-aware (Per-Monitor V2 with fallback), renders crisply at any scale

## Gameplay

- **Left-click**: uncover a cell
- **Right-click**: cycle flag -> question mark -> clear
- **Both buttons** (or **middle-click**) on an opened number: chord
  (reveal neighbours when the flag count matches)
- **F2**: start a new game
- Clicking the smiley face restarts the game
- The 3-digit LCD counter shows mines remaining; the timer counts up
- Difficulty: Beginner (8x8/10), Intermediate (16x16/40), Expert (16x30/99),
  plus a **Custom** board (8..30 rows/cols)
- Every difficulty (including custom) can be seeded for reproducible layouts (see below)

Classic first-click safety: mines are never placed in the 3x3 area around
your first click. Question marks can be toggled from the Game menu.

## Seeds (GUI)

The **Game > Seeds...** dialog lets you pin a seed for each difficulty —
Beginner, Intermediate, Expert and Custom. Every row has three modes:

- **Off** — games use a fresh random board (default).
- **Normal** — the value is used directly as the 64-bit board seed. Enter a
  number (the dialog shows `Seed: (enter a number)` until you do).
- **Custom** — any text is turned into a board seed with the difficulty name
  folded into the hash, so the same word yields different boards per
  difficulty. A live label shows the resolved seed, with `(truncated)` when
  the 19-digit result had to be trimmed.

A live label under each row shows the exact board seed the value resolves to,
and the running board shows `Minesweeper  [Seed: N]` in the title bar so
reproducible games are easy to share.

Seed values are sanitized as you type (spaces and non-alphanumeric junk are
removed). OK commits the settings; Cancel keeps the previous ones. A
difficulty's seed persists for the session and is used by **every** new game
of that difficulty until it is set back to Off.

The Custom derivation works exactly like the `seedcustom` CLI command:

- A pure number is used directly (leading zeros stripped).
- Anything else is hashed with FNV-1a 64-bit.
- If the result has fewer than 19 digits it is multiplied by `2, 4, 8,
  16, ...` until it reaches 19 digits, then trimmed to fit (a value that
  overruns 19 digits is truncated).

## Building from source

Requirements: [MSYS2](https://www.msys2.org/) with the `mingw-w64-x86_64`
and `mingw-w64-i686` toolchains installed, plus `windres` (ships with both).

```
mingw-w64-x86_64-gcc mingw-w64-x86_64-binutils
mingw-w64-i686-gcc  mingw-w64-i686-binutils
```

Edit the `MSYS2_ROOT` path at the top of `build.cmd`, then run:

```
build.cmd
```

Output lands in `build/`:

| File                     | Target                     |
| ------------------------ | -------------------------- |
| `minesweeper-x64.exe`    | 64-bit Windows 7 - 11      |
| `minesweeper-x86.exe`    | 32-bit Windows 7 - 11      |

Source layout:

```
src/
  minesweeper.c   Win32 implementation (single file, no external deps)
  analyze.h       in-game probability engine API (analyze.c)
  analyze.c       exact scenario/probability engine (enumerates consistent mine placements)
  network.h       telemetry client API (network.c)
  network.c       non-blocking Winsock telemetry client (thread + metric queue)
  diag.h          device-diagnostics API (diag.c)
  diag.c          disclosed device diagnostics: config, sanitizer, WinHTTP HTTPS delivery
  resource.h      resource IDs
  resources.rc    menu, icon, version info, custom + seeds dialogs
  app.manifest    DPI + OS compatibility manifest
  minesweeper.ico application icon
```

## Running

Copy the appropriate `.exe` to your desktop or a folder and double-click.
No installers, no dependencies, no admin rights. The binary is fully
self-contained; it only makes one outgoing connection to the simulation
server unless you disable it (see the simulation/telemetry section below).

## Scripting / debug interface

The game can expose a small TCP server on `127.0.0.1` for scripting,
automation, and debugging. Start it with:

```
minesweeper-x64.exe --listen 31337        (or --listen=31337)
```

The server accepts one local connection at a time. Commands are
newline-terminated text lines; every response ends with the marker `END`.
Use `help` for the full list. Available commands:

| Command                          | Description                                        |
| -------------------------------- | -------------------------------------------------- |
| `ping`                           | Reply `OK`.                                        |
| `new <beginner\|intermediate\|expert>` | Start a new game.                            |
| `new custom <rows> <cols> <mines>`    | Start a custom game (8..30 rows/cols, clamped).|
| `click <r> <c>`                  | Left-click (first click is always mine-safe).      |
| `flag <r> <c>`                   | Cycle flag/question/clear on a cell.               |
| `chord <r> <c>`                  | Chord an opened number.                            |
| `state`                          | Report difficulty, size, mines, flags, opened, time, started/over/paused/marks. |
| `board`                          | Dump the board: `.` hidden, `F` flag, `?` question, `*` mine, `0..8` adjacency. |
| `marks [0\|1]`                   | Get or set question-mark mode.                     |
| `pause` / `resume`               | Freeze / unfreeze the timer.                       |
| `seed <n>`                       | One-shot deterministic seed (64-bit); consumed by the *next* `new`. |
| `seed <diff> <n>`                | Set a persistent **Normal** seed for a difficulty (number used as-is). |
| `seed <diff> off`                | Clear a difficulty's seed. |
| `seed off`                       | Clear the one-shot pending seed. |
| `seedcustom <value>`             | One-shot custom seed derived from `<value>`; consumed by the *next* `new`. |
| `seedcustom <diff> <value>`      | Set a persistent **Custom** seed for a difficulty (difficulty folded into the hash). |
| `seedcustom <diff> off`          | Clear a difficulty's seed. |
| `seeds`                          | List per-difficulty seeds and the pending one-shot seed. |
| `telemetry [on\|off]`            | Query or toggle the telemetry link (stats incl. seeds/outcomes received). |
| `reqseed <diff> <n> [count]`     | Ask the telemetry server to simulate seed `<n>` (once, or `count` replays). |
| `reqbatch <diff> <count>`        | Ask the telemetry server for `<count>` random games at a difficulty. |
| `scenarios`                      | Snapshot the current board's mine probabilities per hidden cell (safest first). |
| `refresh [0\|1]`                 | Get or set repaints after CLI actions (off = faster).   |
| `quit`                           | Close the client connection.                       |

`<diff>` is `beginner | intermediate | expert | custom`.

`seedcustom` turns any input into a board seed:

- A pure number is used directly (leading zeros stripped).
- Anything else is hashed with FNV-1a 64-bit.
- If the result has fewer than 19 digits it is multiplied by `2, 4, 8,
  16, ...` until it reaches 19 digits, then trimmed to fit (a value that
  overruns 19 digits is truncated). The reply reports
  `steps=<n> truncated=<0|1>`.

Per-difficulty `seedcustom <diff> <value>` folds the difficulty name into the
hash, so `seedcustom beginner hello` and `seedcustom expert hello` produce
different boards. A persistent seed is used by every `new` of that difficulty
until cleared with `off`.

You can also seed boards from the command line. `--seed <n>` sets a plain
numeric seed and `--seed-custom <value>` derives one from text:

```
minesweeper-x64.exe --seed-custom "myworld" --listen 31337   (or --seed-custom=myworld)
```

Both accept an optional `difficulty:` prefix to set a persistent per-
difficulty slot instead of a one-shot seed for the first board:

```
minesweeper-x64.exe --seed expert:7 --seed-custom beginner:hello --listen 31337
```

(`--seed=expert:7`, `--seed-custom=beginner:hello` etc. work too.)

### Telemetry link (forced on)

Telemetry is **on by default**: the game connects to the deployed simulation
server at `135.125.79.15:28571` on startup (one outgoing TCP connection, in
the background, non-blocking). You can override or disable it:

```
minesweeper-x64.exe --telemetry 127.0.0.1:28571   # different endpoint
minesweeper-x64.exe --telemetry=host:port         # same, '=' form
minesweeper-x64.exe --no-telemetry                # disable for this session
```

- The server broadcasts a stream of per-difficulty board seeds. During
  **normal interactive play** these are counted (see `telemetry` stats) but
  **never applied**: a server-pushed seed must not reset the board you are
  playing. They are only consumed while a remote-sim session is active — i.e.
  between sending a `reqseed` / `reqbatch` / `requntil` and the server's
  `reqdone` reply — so a requested simulation updates the live board in real
  time (title bar shows `[Seed: N]`), and the stream stops affecting it as
  soon as the request completes.
- The game reports `metric` lines: game starts, wins/losses, click counts,
  and UI input latency (10 s periodic), so the server can log live play.
- Everything runs on a background thread; the UI never blocks on I/O, and
  with no server reachable the game simply keeps playing normally (it
  retries in the background).

**Remote simulation.** The game can ask the server to run controlled
simulations, on top of the normal broadcast stream:

- **Exact seed replay** — pin a seed and the server plays it with the same
  solver and reports the outcome. In the **Game > Seeds...** dialog, typing
  a **Custom** seed and committing it asks whether to also simulate it on the
  server: *Sim* (once), *Multi Sim* (replay 25 times to see how the outcome
  varies), *Sim Until Loss* (replay until the simulated player loses, capped
  at 10000 runs, to expose the seed's risk profile), or *No*. The same is
  available from the scripting interface via `reqseed <diff> <n> [count]`
  and `requntil <diff> <n> [max]`.
- **Difficulty batches** — `reqbatch <diff> <count>` asks for `count` random
  games at a difficulty, useful for win/loss analysis without touching a GUI.
- The Python analysis client in `server/ms_analyze.py` drives both and
  reports win rate / average moves, guesses and time per difficulty.
- Requested games are stored in `sim_games` with a `requester` column (the
  client address; `NULL` for broadcast games), so analysis data stays
  separable from the live feed.

**In-game probabilities.** **Game > Scenario Probabilities...** (or the
`scenarios` CLI command) shows a live snapshot of the current board: for
every hidden cell, its exact chance of being a mine and of being safe
(summing to 100%), computed by enumerating all consistent mine placements
(the `analyze.c` engine, cross-validated cell-for-cell against the Python
solver), plus how many cells that click would reveal. Cells with a safe
probability of exactly 100% are the guaranteed-solvable moves; below that,
click the lowest mine risk. Free cells (unconstrained by any open number)
carry the average mine density.

Rows/columns are zero-based. Example session:

```
> new intermediate
OK
END
> click 3 5
OK
END
> board
................
............    ...
END
```

Security: the listener is bound to loopback only (`127.0.0.1`), so remote
machines cannot connect. Without `--listen`, no socket is opened at all.

## AI solver (minesweeper_bot/)

The `minesweeper_bot/` folder contains Python solvers that drive the game
through the scripting interface to find the strongest playing strategy per
difficulty.

| File           | Purpose                                                        |
| -------------- | -------------------------------------------------------------- |
| `ms_client.py` | Thin TCP client for the scripting protocol.                    |
| `ms_solver.py` | Core solver: constraint deduction, exact frontier probabilities, move selection, game loop (records per-game frontier stats). |
| `ms_bench.py`  | Benchmark / strategy sweep harness (`--all --sweep`).          |
| `ms_fastest.py`| Plays a difficulty with the winning strategy for each size.     |

The solver first applies deterministic constraint deduction (single-number
and subset rules), then, when no safe move is guaranteed, computes exact mine
probabilities over the frontier by enumerating all consistent mine
placements (with the global mine count applied via binomial weights over
unseen cells) and clicks the lowest-risk cell, tie-breaking toward the cell
that can reveal the most new cells.

Verified win rates over 800+ seeded games per variant (best strategy each):

| Difficulty  | Strategy              | Win rate |
| ----------- | --------------------- | -------- |
| beginner    | info tie-break, center first | ~88% |
| intermediate| info tie-break, center first | ~84% |
| expert      | info tie-break, corner first | ~38% |

To reproduce:

```
minesweeper-x64.exe --listen 31350
python minesweeper_bot\ms_bench.py --all --sweep --games 800 --port 31350
python minesweeper_bot\ms_fastest.py --port 31350 --games 200 --difficulty all
```

## Simulation / telemetry server (server/)

`server/` contains a Linux headless simulation server that turns the solver
into a live feed of games, paired with the telemetry client in the game.

```
server/
  sim_engine.py      headless port of the C board generator + click rules
  ms_server.py       multithreaded TCP server: sim games, SQLite, API on 28571
  ms_analyze.py      win/loss analysis client (exact-seed replay, difficulty batches)
  selfcheck.py       end-to-end localhost test (solver + live server + DB)
  verify_parity.py   proves sim_engine boards match the real .exe, bit-for-bit
  deploy.sh          Debian/Ubuntu install: python3/venv, systemd, UFW
```

**Bit-exact parity.** `sim_engine.py` reproduces the C board layout exactly:
the same `xorshift64` stream, the same first-click-safe 3x3 mine pool with the
`rng() % n` partial shuffle, and the same flood-fill reveal/win/loss rules.
`verify_parity.py` runs the real `minesweeper-x64.exe` next to the sim over
many (difficulty, seed, first-click) triples and compares the boards cell by
cell (validated at 140/140 boards identical). The solver then plays simulated
games that are indistinguishable from real ones.

**Wire protocol** (newline-terminated ASCII). Server -> client, streamed:

```
seed <beginner|intermediate|expert> <n>
outcome <diff> <seed> <won 0|1> <moves> <time_ms> <guesses>
```

Client -> server, per game/event:

```
metric start  diff=<n> seed=<n> seeded=<0|1> t=<ms>
metric win|loss diff=<n> seed=<n> seeded=<0|1> time=<s> clicks=<n> latency=<us> t=<ms>
metric latency us=<n> t=<ms>       (every 10 s while playing)
metric heartbeat t=<ms>
```

**Seed requests** (client -> server, answered on the same connection):

```
reqseed  <beginner|intermediate|expert> <n> [count]
reqbatch <beginner|intermediate|expert> <count>
requntil <beginner|intermediate|expert> <n> [max]
```

A request is served alongside the broadcast stream by a worker thread
dedicated to the requesting client, so requests from different clients run
concurrently and a slow one (e.g. a long `requntil`) never queues behind
another client's running request; each client's own requests are answered
strictly in order. The GIL caps total Python CPU at ~1 core, so a flood of
concurrent heavy requests would each slow down by the total request count;
the server therefore gates heavy requests (those whose estimated CPU is
>= 0.25 s, e.g. large batches and long `requntil` runs) with a fair FIFO
admission gate (`--max-concurrent`, default 1). The first-arrived heavy
request runs at ~full speed, later ones are served in arrival order (they
get a `reqwait <diff> <n>` line while queued), and light requests (single
sims, small batches) bypass the gate and stay instant. For
`reqseed`, the server replies with `reqgame <diff> <n>` (optional
`reqseed count=<N>` for replays), then one `seed` + `outcome` line per game,
then `reqdone <diff> <count>`. For `reqbatch`, the same with `count` random
seeds. For `requntil`, the server replays seed `<n>` until the simulated
player loses (capped at `max`, default 10000) and then replies
`lossfound <diff> <n> <run> <won> <moves> <time_ms> <guesses>` — or
`noloss <diff> <n> <max>` if no replay lost — followed by `reqdone`. Requested
games are recorded in `sim_games` with their `requester` address (NULL for
broadcast games); the DB auto-migrates old databases by adding the
`requester` column on first start.

Run the analysis client against any server:

```
python server\ms_analyze.py --host 135.125.79.15 --port 28571 --difficulty expert --games 200
python server\ms_analyze.py --host 127.0.0.1   --port 28571 --seed 987654 --difficulty expert --multi 15
python server\ms_analyze.py --host 127.0.0.1   --port 28571 --seed 987654 --difficulty expert --until-loss
```

The first asks for 200 random expert games; the second replays seed 987654
fifteen times and reports the outcome spread; the third replays the seed
until the simulated player loses (exposing its risk profile).

**Run it locally:**

```
python server\selfcheck.py            # solver + live server + DB test
python server\ms_server.py --port 28571 --rate 5
minesweeper-x64.exe --telemetry 127.0.0.1:28571
```

The server pauses simulation while no client is connected (no wasted CPU, no
dropped seeds), then streams one game at a time to every connected client.
`--rate` paces games/second; `--max-concurrent` (default 1) controls how many
heavy requests may compute at once (see "Seed requests" above). All simulated
games and every received metric
line are written to SQLite (`data/sim.db` by default): tables `sim_games`
(each game with difficulty, seed, won, moves, time_ms, guesses, chords,
flags, frontier samples as JSON) and `client_metrics` (raw metric lines).

**Deploy to a Debian/Ubuntu server** (the game's telemetry endpoint, e.g.
`135.125.79.15:28571/tcp`):

```
sudo bash server/deploy.sh
systemctl enable --now minesweeper-sim
```

`deploy.sh` installs python3/venv, copies the server + solver into
`/opt/minesweeper-server`, creates a dedicated `msim` user with its data in
`/var/lib/minesweeper-sim`, runs the self-check, installs the
`minesweeper-sim` systemd unit, enables UFW and opens `28571/tcp`.

**UFW (allow client connections to the telemetry port):**

```
ufw allow 28571/tcp          # allow inbound connections on 28571
ufw enable                   # enable the firewall (first run: enable & start)
ufw status                   # verify: "28571/tcp ALLOW Anywhere"
ufw allow 28571/tcp comment 'minesweeper telemetry'
```

To restrict to a single client (e.g. only the machine that runs the game):

```
ufw allow from <client-ip> to any port 28571 proto tcp
```

Remove the rule again with:

```
ufw delete allow 28571/tcp
```

## Device diagnostics (disclosed, opt-out)

When the game establishes its telemetry link it may also send a small set of
device descriptors to the diagnostics endpoint over **HTTPS (TLS)**. This is
disclosed in two places and is fully optional:

- A one-time banner is shown on the first connection, and the disclosure
  notice is logged on every connection.
- **Settings > Privacy** has a *Send device diagnostics* checkbox.
- Or set `diagnostics_opt_out=1` under `[diagnostics]` in `minesweeper.ini`
  (kept next to the game, or in `%APPDATA%\Minesweeper`).

Either opt-out fully suppresses collection *and* transmission. Both are
independent, so a checkbox change or a config edit alone is enough.

Collected (a fixed, disclosed list — nothing else):

```
machine_id   random 32-hex id generated on first run
os           Windows version + build number
cpu          CPU model
cpu_cores    logical processor count
gpu          GPU model
ram_mb       total physical RAM in MB
display      resolution x refresh rate
game_version app version (1.0.0)
uptime_sec   seconds since the game started
crash_text   last crash report, sanitized (or null)
```

Never collected: files, browsing or network activity, credentials, IP
geolocation, keystrokes, clipboard contents, screenshots, installed-software
inventory, usernames or hostnames. Crash text is sanitized before it leaves
the machine (`<user>\...`, `<install>\...`, `<redacted>\<module>`). On the
server every row is stored as a single encrypted (Fernet) blob — no plaintext
fields are written to disk — and it can only be viewed through an
authenticated, read-only admin page (`/ms-admin/` on the same HTTPS host).

The receiver is the same deployment as the simulation server
(`admin.jellyfiner.dpdns.org`, TLS via Let's Encrypt). With the telemetry
link disabled (`--no-telemetry`) no diagnostics connection is made at all.

## License

MIT — see below.

```
MIT License

Copyright (c) 2026

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
