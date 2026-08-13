// worker-pool.js - small FIFO pool of game-simulation worker threads.
//
// The server runs every simulated game in a worker so a CPU-bound game never
// blocks the event loop.  The pool size is max-concurrent + headroom for
// light requests and the broadcast producer (see server.js).

import { Worker } from "node:worker_threads";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));

export class WorkerPool {
  constructor(size) {
    this.size = Math.max(1, size);
    this.idle = [];
    this.waiters = [];
    this.nextId = 0;
    for (let i = 0; i < this.size; i++) this.idle.push(new Worker(path.join(here, "worker.js")));
  }

  submit(task) {
    return new Promise((resolve, reject) => {
      this.waiters.push({ task, resolve, reject });
      this._pump();
    });
  }

  _pump() {
    while (this.waiters.length && this.idle.length) {
      const { task, resolve, reject } = this.waiters.shift();
      const w = this.idle.pop();
      const id = this.nextId++;
      const msg = { id, ...task };
      const onMsg = (m) => {
        w.removeListener("error", onErr);
        this._free(w);
        if (m && m.error) reject(new Error(m.error));
        else resolve(m);
      };
      const onErr = (e) => {
        w.removeListener("message", onMsg);
        this._repair();
        reject(e);
      };
      w.once("message", onMsg);
      w.once("error", onErr);
      w.postMessage(msg);
    }
  }

  _free(w) {
    this.idle.push(w);
    this._pump();
  }

  _repair() {
    this.idle.push(new Worker(path.join(here, "worker.js")));
    this._pump();
  }

  async close() {
    for (const w of [...this.idle]) {
      await w.terminate();
    }
    this.idle = [];
  }
}
