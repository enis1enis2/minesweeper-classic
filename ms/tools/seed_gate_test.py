#!/usr/bin/env python3
"""Seed-gate test: verify a passively-connected client never has its board
reset by the server's broadcast seed stream, but that a req* session still
applies streamed seeds to the live board.

Usage: seed_gate_test.py [port]
"""
import socket, sys, time

def send(s, c):
    s.sendall((c + "\n").encode())

def recv_until_end(s, timeout=4):
    s.settimeout(timeout)
    buf = b""
    try:
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            buf += chunk
            if b"END\n" in buf[-8:]:
                break
    except socket.timeout:
        pass
    return buf.decode(errors="replace")

def state_of(text):
    d = {}
    for line in text.splitlines():
        if "=" in line and not line.startswith("=="):
            k, v = line.split("=", 1)
            d[k.strip()] = v.strip()
    return d

def seeds_stat(text):
    for line in text.splitlines():
        if "seeds=" in line:
            return int(line.split("seeds=", 1)[1].split()[0])
    return -1

def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 29000
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    fails = []

    def check(name, ok, detail=""):
        print(("PASS " if ok else "FAIL ") + name + ("  " + detail if detail else ""))
        if not ok:
            fails.append(name)

    # --- passive phase: board must not change while broadcasts arrive ---
    send(s, "seed beginner off"); recv_until_end(s)   # clear any persistent slot
    send(s, "new beginner"); recv_until_end(s)
    send(s, "state"); s1 = state_of(recv_until_end(s))
    send(s, "telemetry"); seeds_pre = seeds_stat(recv_until_end(s))

    time.sleep(5)  # let several broadcast seeds arrive at rate 5 g/s

    send(s, "telemetry"); seeds_post = seeds_stat(recv_until_end(s))
    send(s, "state"); s2 = state_of(recv_until_end(s))

    check("broadcast seeds still received+counted", seeds_post > seeds_pre,
          f"{seeds_pre} -> {seeds_post}")
    check("board NOT reset by broadcast (seed unchanged)",
          s1.get("seed") == s2.get("seed") and s1.get("seeded") == s2.get("seeded"),
          f"{s1.get('seed')} -> {s2.get('seed')}")
    check("board NOT touched (opened/started unchanged)",
          s1.get("opened") == s2.get("opened") and s1.get("started") == s2.get("started"),
          f"{s1.get('opened')}/{s1.get('started')} -> {s2.get('opened')}/{s2.get('started')}")
    check("broadcast did NOT write the seed slot (board stays unseeded)",
          s2.get("seeded") == "0",
          f"seeded={s2.get('seeded')}")

    # --- reqsession phase: streamed seeds ARE applied to the live board ---
    send(s, "reqseed beginner 42"); recv_until_end(s)
    time.sleep(2)  # let the request's games (seed/outcome) arrive
    send(s, "state"); s3 = state_of(recv_until_end(s))
    check("reqsession applies streamed seed to board",
          s3.get("seeded") == "1" and s3.get("opened") == "0",
          f"seeded={s3.get('seeded')} opened={s3.get('opened')}")

    # --- post-session: broadcast must stop resetting again ---
    time.sleep(3)
    send(s, "state"); s4 = state_of(recv_until_end(s))
    check("session end restores gate (board stable again)",
          s3.get("seed") == s4.get("seed"),
          f"{s3.get('seed')} -> {s4.get('seed')}")

    send(s, "quit")
    try:
        s.recv(64)
    except Exception:
        pass
    s.close()

    print()
    print("RESULT: " + ("PASS" if not fails else "FAIL: " + ", ".join(fails)))
    sys.exit(0 if not fails else 1)

if __name__ == "__main__":
    main()
