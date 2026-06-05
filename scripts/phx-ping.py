#!/usr/bin/env python3
"""phx-ping: time Phoenix-socket heartbeats. stdlib only.

Connects to a Phoenix socket endpoint, sends N `phoenix`/`heartbeat`
messages, and prints one RTT (ms) or `-` per heartbeat — fping -C style,
so it plugs straight into dabping's exec probe:

    [probes.phx]
    type = "exec"
    command = "scripts/phx-ping.py wss://%host%/socket/websocket %pings%"

usage: phx-ping.py URL [pings] [cookie-header-value]
  URL     e.g. wss://example.org/socket/websocket (vsn=2.0.0 added if absent)
  cookie  session cookie for auth-gated sockets (e.g. "_app_key=…")

Heartbeats ride the reserved "phoenix" topic, so no channel join is
needed; the RTT includes TLS-terminated app-level work, not just TCP.
"""

import base64
import hashlib  # noqa: F401  (kept for parity with server-side testing)
import json
import os
import socket
import ssl
import struct
import sys
import time
import urllib.parse

TIMEOUT = 5.0


def connect(url, cookie=None):
    u = urllib.parse.urlsplit(url)
    tls = u.scheme in ("wss", "https")
    port = u.port or (443 if tls else 80)
    sock = socket.create_connection((u.hostname, port), TIMEOUT)
    if tls:
        sock = ssl.create_default_context().wrap_socket(sock, server_hostname=u.hostname)
    sock.settimeout(TIMEOUT)

    query = u.query or ""
    if "vsn=" not in query:
        query += ("&" if query else "") + "vsn=2.0.0"
    key = base64.b64encode(os.urandom(16)).decode()
    req = [
        f"GET {u.path or '/'}?{query} HTTP/1.1",
        f"Host: {u.hostname}",
        "Upgrade: websocket",
        "Connection: Upgrade",
        f"Sec-WebSocket-Key: {key}",
        "Sec-WebSocket-Version: 13",
    ]
    if cookie:
        req.append(f"Cookie: {cookie}")
    sock.sendall(("\r\n".join(req) + "\r\n\r\n").encode())

    resp = b""
    while b"\r\n\r\n" not in resp:
        chunk = sock.recv(4096)
        if not chunk:
            break
        resp += chunk
    status = resp.split(b"\r\n", 1)[0].decode(errors="replace")
    if " 101 " not in status:
        raise ConnectionError(f"handshake refused: {status!r}")
    return sock


def send_text(sock, text):
    data = text.encode()
    mask = os.urandom(4)  # client frames must be masked
    head = bytearray([0x81])
    n = len(data)
    if n < 126:
        head.append(n | 0x80)
    else:
        head.append(126 | 0x80)
        head += struct.pack(">H", n)
    sock.sendall(bytes(head) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))


def recv_text(sock):
    """Next text frame's payload, transparently answering pings."""

    def need(n):
        buf = b""
        while len(buf) < n:
            chunk = sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("closed")
            buf += chunk
        return buf

    while True:
        b1, b2 = need(2)
        n = b2 & 0x7F
        if n == 126:
            n = struct.unpack(">H", need(2))[0]
        elif n == 127:
            n = struct.unpack(">Q", need(8))[0]
        payload = need(n)  # server→client frames are unmasked
        op = b1 & 0x0F
        if op == 0x9:  # ping → pong
            sock.sendall(bytes([0x8A, 0x80]) + os.urandom(4))
        elif op == 0x8:
            raise ConnectionError("server closed")
        elif op == 0x1:
            return payload.decode()


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    url = sys.argv[1]
    pings = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    cookie = sys.argv[3] if len(sys.argv) > 3 else None

    try:
        sock = connect(url, cookie)
    except Exception as e:
        # total loss: print only '-' tokens so the exec probe records 100%
        print(" ".join(["-"] * pings))
        print(f"phx-ping: {e}", file=sys.stderr)
        sys.exit(1)

    out = []
    for ref in range(1, pings + 1):
        # v2 wire format: [join_ref, ref, topic, event, payload]
        send_text(sock, json.dumps([None, str(ref), "phoenix", "heartbeat", {}]))
        t0 = time.monotonic()
        deadline = t0 + TIMEOUT
        rtt = None
        try:
            while time.monotonic() < deadline:
                msg = json.loads(recv_text(sock))
                if msg[1] == str(ref) and msg[3] == "phx_reply":
                    rtt = (time.monotonic() - t0) * 1000
                    break
        except (ConnectionError, socket.timeout, ssl.SSLError):
            pass
        out.append(f"{rtt:.2f}" if rtt is not None else "-")
        time.sleep(0.1)
    print(" ".join(out))


if __name__ == "__main__":
    main()
