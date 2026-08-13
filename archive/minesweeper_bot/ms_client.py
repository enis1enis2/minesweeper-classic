"""Minimal client for the Minesweeper (Classic) scripting server.

The game exposes a newline-terminated text protocol on a loopback TCP port
when started with:  minesweeper-x64.exe --listen <port>

Every command produces a response terminated by the marker line END.
This module wraps that protocol.
"""

import socket

END = "END"


class MSClient:
    def __init__(self, port, host="127.0.0.1", timeout=10.0):
        self.host = host
        self.port = port
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.buf = b""

    # ------------------------------------------------------------------ io
    def _read_line(self):
        while b"\n" not in self.buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("connection closed by server")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("ascii", "replace").rstrip("\r")

    def cmd(self, text):
        """Send one command line, return the list of reply lines (no END)."""
        self.sock.sendall(text.encode("ascii") + b"\n")
        lines = []
        while True:
            line = self._read_line()
            if line == END:
                return lines
            lines.append(line)

    def ping(self):
        return self.cmd("ping") == ["OK"]

    def close(self):
        try:
            self.cmd("quit")
        except Exception:
            pass
        try:
            self.sock.close()
        except Exception:
            pass

    # ---------------------------------------------------------- high level
    def new(self, difficulty):
        """difficulty: beginner | intermediate | expert | custom r c m"""
        return self.cmd("new " + difficulty)

    def click(self, r, c):
        return self.cmd(f"click {r} {c}")

    def flag(self, r, c):
        return self.cmd(f"flag {r} {c}")

    def chord(self, r, c):
        return self.cmd(f"chord {r} {c}")

    def state(self):
        """Return a dict of key=value fields."""
        out = {}
        for line in self.cmd("state"):
            if "=" in line:
                k, v = line.split("=", 1)
                out[k] = v
        return out

    def board(self):
        """Return the board as a list of strings (rows)."""
        return self.cmd("board")

    def seed(self, n):
        """One-shot numeric seed, consumed by the next new."""
        return self.cmd(f"seed {n}")

    def seedcustom(self, value):
        """One-shot custom seed (text hashed), consumed by the next new."""
        return self.cmd(f"seedcustom {value}")

    def seed_diff(self, diff, n):
        """Persistent Normal seed for a difficulty, used by every new
        until cleared. diff: beginner | intermediate | expert | custom."""
        return self.cmd(f"seed {diff} {n}")

    def seed_diff_off(self, diff):
        return self.cmd(f"seed {diff} off")

    def seedcustom_diff(self, diff, value):
        """Persistent Custom seed for a difficulty (difficulty folded into
        the hash), used by every new until cleared."""
        return self.cmd(f"seedcustom {diff} {value}")

    def seedcustom_diff_off(self, diff):
        return self.cmd(f"seedcustom {diff} off")

    def seed_off(self):
        """Clear the one-shot pending seed."""
        return self.cmd("seed off")

    def seeds(self):
        """List per-difficulty seeds as a dict, plus the pending seed.

        Slots read 'off', 'normal:<n>', or 'custom:<resolved-seed>'.
        """
        out = {}
        for line in self.cmd("seeds"):
            if "=" in line:
                k, v = line.split("=", 1)
                out[k] = v
        return out

    def refresh(self, on):
        return self.cmd(f"refresh {1 if on else 0}")
