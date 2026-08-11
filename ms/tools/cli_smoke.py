import socket, sys, time

cmds = [
    "seed beginner 42",
    "seeds",
    "new beginner",
    "state",
    "seedcustom expert hello",
    "seeds",
    "new expert",
    "state",
    "new custom 9 9 12",
    "state",
    "click 1 1",
    "board",
    "chord 1 1",
    "state",
    "seed expert off",
    "seeds",
    "quit",
]

def run(host, port):
    s = socket.create_connection((host, port), timeout=5)
    s.settimeout(3)
    out = b""
    for c in cmds:
        s.sendall((c + "\n").encode())
    try:
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            out += chunk
    except socket.timeout:
        pass
    s.close()
    return out

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 29000
    data = run("127.0.0.1", port).decode(errors="replace")
    sys.stdout.write(data)
