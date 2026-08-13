// hub.js - connection registry, per-client request queues and the admission
// gate (ports of ms_server.py ClientHub / RequestWorkers / AdmissionGate).

import crypto from "node:crypto";

export const NONCE_TTL = 60;

const nowSec = () => Math.floor(Date.now() / 1000);

function timingSafeEqualHex(a, b) {
  if (a.length !== b.length) return false;
  return crypto.timingSafeEqual(Buffer.from(a, "utf8"), Buffer.from(b, "utf8"));
}

export class ClientHub {
  constructor(db) {
    this.db = db;
    this.clients = new Map(); // addr -> client record
  }

  add(addr, conn) {
    const now = nowSec();
    const cl = {
      conn,
      last: now,
      seeds: 0,
      outcomes: 0,
      user: null,
      nonce: null,
      nonceTs: 0,
      fails: 0,
      authed: false,
      dead: false,
    };
    this.clients.set(addr, cl);
    conn.on("error", () => {
      cl.dead = true;
    });
    this.db.upsert_client(addr, now);
  }

  get(addr) {
    return this.clients.get(addr) || null;
  }

  is_authed(addr) {
    const cl = this.clients.get(addr);
    return !!(cl && cl.authed);
  }

  auth_begin(addr, user) {
    const cl = this.clients.get(addr);
    if (!cl) return null;
    cl.user = user;
    cl.nonce = crypto.randomBytes(16).toString("hex");
    cl.nonceTs = nowSec();
    return cl.nonce;
  }

  auth_resolve(addr, digestHex, expectedHex) {
    const cl = this.clients.get(addr);
    if (!cl) return [false, 0];
    const nonce = cl.nonce;
    const nonceTs = cl.nonceTs;
    if (nonce === null || nowSec() - nonceTs > NONCE_TTL) {
      cl.fails += 1;
      return [false, cl.fails];
    }
    const ok = timingSafeEqualHex(digestHex.toLowerCase(), expectedHex);
    cl.nonce = null;
    if (ok) {
      cl.authed = true;
      cl.fails = 0;
    } else {
      cl.fails += 1;
    }
    return [ok, cl.fails];
  }

  remove(addr) {
    this.clients.delete(addr);
    this.db.upsert_client(addr, nowSec(), false);
  }

  count() {
    return this.clients.size;
  }

  send_to(addr, line) {
    const cl = this.clients.get(addr);
    if (!cl) return false;
    if (cl.dead || cl.conn.destroyed) {
      this.remove(addr);
      return false;
    }
    try {
      cl.conn.write(line + "\n");
      cl.last = nowSec();
      if (line.startsWith("seed ")) cl.seeds += 1;
      else if (line.startsWith("outcome ")) cl.outcomes += 1;
      const seeds = cl.seeds;
      const outcomes = cl.outcomes;
      this.db.client_touch(addr, seeds, outcomes);
      return true;
    } catch {
      this.remove(addr);
      return false;
    }
  }

  broadcast(line) {
    let sent = 0;
    const dead = [];
    const touched = [];
    for (const [addr, cl] of this.clients) {
      if (cl.dead || cl.conn.destroyed) {
        dead.push(addr);
        continue;
      }
      try {
        cl.conn.write(line + "\n");
        cl.last = nowSec();
        if (line.startsWith("seed ")) cl.seeds += 1;
        else if (line.startsWith("outcome ")) cl.outcomes += 1;
        sent += 1;
        touched.push([addr, cl.seeds, cl.outcomes]);
      } catch {
        dead.push(addr);
      }
    }
    for (const a of dead) this.remove(a);
    this.db.client_touch_many(touched);
    return sent;
  }
}

export class RequestWorkers {
  constructor(server) {
    this.server = server;
    this.states = new Map(); // addr -> { q: [], running: bool, closed: bool }
  }

  enqueue(addr, line) {
    let st = this.states.get(addr);
    if (!st) {
      st = { q: [], running: false, closed: false };
      this.states.set(addr, st);
    }
    st.q.push(line);
    if (!st.running) {
      st.running = true;
      void this._drain(addr, st);
    }
  }

  drop(addr) {
    const st = this.states.get(addr);
    if (st) {
      st.closed = true;
      st.q = [];
    }
  }

  async _drain(addr, st) {
    try {
      while (st.q.length && !st.closed && !this.server.stop) {
        const line = st.q.shift();
        await this.server.handleRequest(addr, line);
      }
    } catch (e) {
      console.error(
        `  request worker error for ${addr}: ${e && e.stack ? e.stack : e}`
      );
    } finally {
      st.running = false;
      if (st.q.length && !st.closed && !this.server.stop) {
        void this._drain(addr, st);
      } else if (st.closed) {
        this.states.delete(addr);
      }
    }
  }
}

export class AdmissionGate {
  constructor(maxConcurrent) {
    this.maxConcurrent = Math.max(1, maxConcurrent);
    this.free = this.maxConcurrent;
    this.head = 0;
    this.nextTicket = 0;
    this._waiters = [];
  }

  acquire() {
    return new Promise((resolve) => {
      const ticket = this.nextTicket++;
      if (this.free > 0 && ticket === this.head) {
        this.free -= 1;
        this.head += 1;
        resolve();
        return;
      }
      this._waiters.push({ ticket, resolve });
    });
  }

  release() {
    this.free += 1;
    while (
      this.free > 0 &&
      this._waiters.length &&
      this._waiters[0].ticket === this.head
    ) {
      const w = this._waiters.shift();
      this.free -= 1;
      this.head += 1;
      w.resolve();
    }
  }
}
