// client.js - minimal client for the Minesweeper (Classic) scripting server.
//
// 1:1 port of minesweeper_bot/ms_client.py.  Wraps the newline-terminated
// text protocol on a loopback TCP port: every command produces a response
// terminated by the marker line END.

import net from "node:net";

export const END = "END";

export class MSClient {
  constructor(port, host = "127.0.0.1", timeout = 10.0) {
    this.host = host;
    this.port = port;
    this.sock = new net.Socket();
    this.sock.setTimeout(Math.round(timeout * 1000));
    this._connected = new Promise((resolve, reject) => {
      this.sock.once("connect", resolve);
      this.sock.once("error", reject);
      this.sock.once("timeout", () => reject(new Error("connect timeout")));
    });
    this.sock.connect(port, host);
    this.buf = "";
    this._closed = false;
    this._error = null;
    this._waiters = [];
    this.sock.on("error", (e) => {
      this._error = e;
    });
    this.sock.on("data", (chunk) => {
      this.buf += chunk.toString("ascii");
      while (this._waiters.length && this.buf.indexOf("\n") >= 0) {
        this._waiters.shift().resolve();
      }
    });
    this.sock.on("close", () => {
      this._closed = true;
      for (const w of this._waiters.splice(0)) {
        w.reject(new Error("connection closed by server"));
      }
    });
  }

  async _connect() {
    if (!this._connected) return;
    await this._connected;
    this._connected = null;
  }

  _readLine() {
    while (this.buf.indexOf("\n") < 0) {
      if (this._closed) throw new Error("connection closed by server");
      if (this._error) throw this._error;
      throw new Error("no data buffered (use readLineAsync)");
    }
    const idx = this.buf.indexOf("\n");
    const line = this.buf.slice(0, idx);
    this.buf = this.buf.slice(idx + 1);
    return line.replace(/\r$/, "");
  }

  async _readLineAsync() {
    for (;;) {
      const idx = this.buf.indexOf("\n");
      if (idx >= 0) {
        const line = this.buf.slice(0, idx);
        this.buf = this.buf.slice(idx + 1);
        return line.replace(/\r$/, "");
      }
      if (this._closed) throw new Error("connection closed by server");
      if (this._error) throw this._error;
      await new Promise((resolve, reject) => {
        this._waiters.push({ resolve, reject });
      });
    }
  }

  async cmd(text) {
    await this._connect();
    this.sock.write(text + "\n");
    const lines = [];
    for (;;) {
      const line = await this._readLineAsync();
      if (line === END) return lines;
      lines.push(line);
    }
  }

  async ping() {
    const r = await this.cmd("ping");
    return r.length === 1 && r[0] === "OK";
  }

  async close() {
    try {
      await this.cmd("quit");
    } catch {
      // ignore
    }
    try {
      this.sock.destroy();
    } catch {
      // ignore
    }
    this._closed = true;
  }

  // ---------------------------------------------------------- high level
  async new(difficulty) {
    // difficulty: beginner | intermediate | expert | custom r c m
    return this.cmd("new " + difficulty);
  }

  async click(r, c) {
    return this.cmd(`click ${r} ${c}`);
  }

  async flag(r, c) {
    return this.cmd(`flag ${r} ${c}`);
  }

  async chord(r, c) {
    return this.cmd(`chord ${r} ${c}`);
  }

  async state() {
    const out = {};
    for (const line of await this.cmd("state")) {
      const eq = line.indexOf("=");
      if (eq >= 0) out[line.slice(0, eq)] = line.slice(eq + 1);
    }
    return out;
  }

  async board() {
    return this.cmd("board");
  }

  async seed(n) {
    return this.cmd(`seed ${n}`);
  }

  async seedcustom(value) {
    return this.cmd(`seedcustom ${value}`);
  }

  async seedDiff(diff, n) {
    return this.cmd(`seed ${diff} ${n}`);
  }

  async seedDiffOff(diff) {
    return this.cmd(`seed ${diff} off`);
  }

  async seedcustomDiff(diff, value) {
    return this.cmd(`seedcustom ${diff} ${value}`);
  }

  async seedcustomDiffOff(diff) {
    return this.cmd(`seedcustom ${diff} off`);
  }

  async seedOff() {
    return this.cmd("seed off");
  }

  async seeds() {
    const out = {};
    for (const line of await this.cmd("seeds")) {
      const eq = line.indexOf("=");
      if (eq >= 0) out[line.slice(0, eq)] = line.slice(eq + 1);
    }
    return out;
  }

  async refresh(on) {
    return this.cmd(`refresh ${on ? 1 : 0}`);
  }
}
