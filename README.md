# Minesweeper Classic — Linux Client

The Linux port of the classic 1992 Windows 3.1 Minesweeper game. A faithful,
from-scratch reimplementation sharing the same board generator, click rules,
scenario-probability engine, and wire protocol as the Windows client (see the
`main` branch of this repository for the Windows build and the telemetry /
simulation backend).

## Features

- Same gameplay as the Windows client: flag/question cycles, chord, F2 new
  game, seeded boards, custom size.
- Three frontends in one binary:
  - **X11 GUI** (`--x11`) — full graphical play.
  - **Terminal** — plain-text board in your console.
  - **Headless** (`--headless`) — no display; used for automation and as a
    telemetry/solver client.
- Exact mine-probability engine (`src/analyze.c`) with in-game `scenarios`.
- Scripting interface on `127.0.0.1` (`--listen <port>`), compatible with the
  Windows scripting protocol (see the main README).
- Telemetry link to the deployed simulation server (on by default;
  `--no-telemetry` disables it).

## Building

Requires a C11 compiler, GNU make, and the X11 static libraries for a fully
static build:

```
sudo apt install gcc make libx11-dev libxcb-dev libxdmcp-dev libxau-dev
```

```
make          # fully static X11 client (minesweeper)
make dynamic  # dynamically linked client (minesweeper-dyn, needs libX11)
make headless # static client without X11 (terminal + headless)
```

The build pulls the shared scenario analyzer from `../src/analyze.c`.

## Usage

```
./minesweeper --x11                 # graphical
./minesweeper                       # terminal
./minesweeper --headless --listen 29000
./minesweeper --no-telemetry --seed 12345 --x11
```

Type `help` over the scripting interface (or run `./minesweeper --help`) for
the full command and script-command list.

## Source layout

```
linux/
  ms_core.c / ms_core.h   board generator + click rules (portable)
  ms_net.c  / ms_net.h    telemetry / scripting sockets (POSIX)
  ms_ini.c  / ms_ini.h    config file handling
  ms_sha256.c/.h          SHA-256 (diagnostics)
  ms_term.c               terminal frontend
  ms_x11.c                X11 frontend
  ms_main.c               entry point, argument parsing
  Makefile                build
src/
  analyze.c / analyze.h   shared exact probability engine
```

## License

MIT — see `LICENSE`.
