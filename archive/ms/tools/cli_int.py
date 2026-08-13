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

def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 29000
    s = socket.create_connection(("127.0.0.1", port), timeout=5)

    print("== telemetry status (pre) ==")
    send(s, "telemetry")
    print(recv_until_end(s), end="")

    print("== reqseed beginner 42 x2 ==")
    send(s, "reqseed beginner 42 2")
    print(recv_until_end(s), end="")

    print("== reqbatch expert 3 ==")
    send(s, "reqbatch expert 3")
    print(recv_until_end(s), end="")

    time.sleep(3)  # let seeds stream in

    print("== telemetry status (post) ==")
    send(s, "telemetry")
    print(recv_until_end(s), end="")

    print("== state after streamed seed ==")
    send(s, "state")
    print(recv_until_end(s), end="")

    print("== seeds slots ==")
    send(s, "seeds")
    print(recv_until_end(s), end="")

    send(s, "quit")
    try:
        s.recv(64)
    except Exception:
        pass
    s.close()

if __name__ == "__main__":
    main()
