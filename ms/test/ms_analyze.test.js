import { test } from "node:test";
import assert from "node:assert/strict";
import net from "node:net";
import crypto from "node:crypto";
import { Analyzer } from "../cli/ms_analyze.js";

// Stub of the solver/telemetry protocol, enough to exercise the client's
// auth, request draining, lossfound/noloss and reqdenied paths.
function stubServer(handler) {
  return new Promise((resolve) => {
    const server = net.createServer((sock) => {
      sock.setEncoding("ascii");
      let buf = "";
      const send = (s) => sock.write(s + "\n");
      const h = (line) => handler(line, send, sock);
      sock.on("data", (chunk) => {
        buf += chunk;
        let nl;
        while ((nl = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, nl).trim();
          buf = buf.slice(nl + 1);
          if (line) h(line);
        }
      });
    });
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      resolve({
        port,
        close: () => server.close(),
      });
    });
  });
}

test("reqbatch drains until reqdone and only counts reqgame games", async () => {
  const stub = await stubServer((line, send) => {
    if (line.startsWith("reqbatch")) {
      send("reqgame beginner 12345");
      send("seed beginner 12345");
      send("outcome beginner 12345 1 22 410 0");
      // broadcast game on the same connection, must not pollute results
      send("seed beginner 999");
      send("outcome beginner 999 0 8 90 2");
      send("reqgame beginner 12346");
      send("outcome beginner 12346 0 7 80 1");
      send("reqdone beginner 2");
    }
  });
  try {
    const a = new Analyzer("127.0.0.1", stub.port);
    await a.connect();
    const games = await a.request("reqbatch beginner 2", 2);
    assert.equal(games.length, 2);
    assert.deepEqual(
      games.map((g) => [String(g.seed), g.won]),
      [["12345", 1], ["12346", 0]],
    );
    assert.equal(a.lossInfo, null);
    a.close();
  } finally {
    stub.close();
  }
});

test("solver auth uses HMAC-SHA256 challenge-response", async () => {
  const stub = await stubServer((line, send) => {
    if (line === "auth solverbot") {
      send("authchal abc123");
    } else if (line.startsWith("authresp")) {
      const expect = crypto
        .createHmac("sha256", "s3cret")
        .update("ms-auth:abc123")
        .digest("hex");
      send(line === "authresp " + expect ? "authok" : "autherr");
    }
  });
  try {
    const a = new Analyzer("127.0.0.1", stub.port);
    await a.connect();
    assert.equal(await a.auth("solverbot", "s3cret"), true);
    a.close();
  } finally {
    stub.close();
  }
});

test("reqdenied raises instead of returning games", async () => {
  const stub = await stubServer((line, send) => {
    if (line.startsWith("reqbatch")) send("reqdenied");
  });
  try {
    const a = new Analyzer("127.0.0.1", stub.port);
    await a.connect();
    await assert.rejects(
      a.request("reqbatch beginner 1", 1),
      /denied the request/,
    );
    a.close();
  } finally {
    stub.close();
  }
});

test("requntil records lossInfo for lossfound", async () => {
  const stub = await stubServer((line, send) => {
    if (line.startsWith("requntil")) {
      send("reqgame expert 42");
      send("seed expert 42");
      send("outcome expert 42 1 500 9000 3");
      send("reqgame expert 42");
      send("seed expert 42");
      send("outcome expert 42 0 200 3000 2");
      send("lossfound expert 42 2 0 200 3000 2");
      send("reqdone expert 2");
    }
  });
  try {
    const a = new Analyzer("127.0.0.1", stub.port);
    await a.connect();
    const games = await a.request("requntil expert 42 5", 5);
    assert.equal(games.length, 2);
    assert.deepEqual(
      a.lossInfo,
      { kind: "loss", run: 2, won: 0, moves: 200, time_ms: 3000, guesses: 2 },
    );
    a.close();
  } finally {
    stub.close();
  }
});

test("noloss is reported when the seed never loses", async () => {
  const stub = await stubServer((line, send) => {
    if (line.startsWith("requntil")) {
      send("reqgame expert 7");
      send("outcome expert 7 1 500 9000 3");
      send("noloss expert 7 3");
      send("reqdone expert 1");
    }
  });
  try {
    const a = new Analyzer("127.0.0.1", stub.port);
    await a.connect();
    await a.request("requntil expert 7 3", 3);
    assert.deepEqual(a.lossInfo, { kind: "noloss", max: 3 });
    a.close();
  } finally {
    stub.close();
  }
});

test("server closing the connection surfaces as ConnectionError", async () => {
  const { ConnectionError } = await import("../cli/ms_analyze.js");
  const stub = await stubServer((line, send, sock) => {
    if (line.startsWith("reqbatch")) {
      send("reqgame beginner 1");
      sock.end(); // server drops mid-request
    }
  });
  try {
    const a = new Analyzer("127.0.0.1", stub.port);
    await a.connect();
    await assert.rejects(
      a.request("reqbatch beginner 1", 1),
      ConnectionError,
    );
    a.close();
  } finally {
    stub.close();
  }
});
