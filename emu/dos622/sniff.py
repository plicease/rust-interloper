#!/usr/bin/env python3
"""Relay bytes between the client and server COM1 pseudo-terminals and log them.

    sniff.py CLIENT_PTY SERVER_PTY [--log FILE] [--marks PTY] [--until TEXT]

Each chunk is logged as one line: seconds since start, direction (C>S is
client to server), byte count, and the bytes in hex. With --marks, lines the
client writes to that PTY (its COM2) are logged as "MARK <text>" so a capture
can be split into steps; --until stops the relay at a matching mark. The
relay also exits when either 86Box VM closes its terminal.
"""

import argparse
import os
import select
import sys
import time
import tty


def open_pty(path):
    fd = os.open(path, os.O_RDWR | os.O_NOCTTY)
    tty.setraw(fd)
    return fd


def relay(client_path, server_path, log=None, marks_path=None, until=None):
    client, server = open_pty(client_path), open_pty(server_path)
    route = {client: (server, "C>S"), server: (client, "S>C")}
    marks = open_pty(marks_path) if marks_path else None
    pending = b""
    out = open(log, "a") if log else None
    start = time.monotonic()

    def emit(line):
        print(line, flush=True)
        if out:
            print(line, file=out, flush=True)

    while True:
        ready, _, _ = select.select(list(route) + ([marks] if marks else []), [], [])
        for src in ready:
            try:
                data = os.read(src, 4096)
            except OSError:
                data = b""
            if not data:
                print("terminal closed", file=sys.stderr)
                return
            now = time.monotonic() - start
            if src == marks:
                pending += data
                while b"\n" in pending:
                    raw, pending = pending.split(b"\n", 1)
                    text = raw.decode("ascii", "replace").strip()
                    if text:
                        emit(f"{now:12.6f} MARK {text}")
                        if until and until in text:
                            return
                continue
            dest, direction = route[src]
            sent = 0
            while sent < len(data):
                sent += os.write(dest, data[sent:])
            emit(f"{now:12.6f} {direction} {len(data):4d} {data.hex(' ')}")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("client", help="client COM1 PTY, e.g. dos622-client/com1.pty")
    p.add_argument("server", help="server COM1 PTY, e.g. dos622-server/com1.pty")
    p.add_argument("--log", help="also append the log to this file")
    p.add_argument("--marks", help="client COM2 PTY carrying step markers")
    p.add_argument("--until", help="stop when a marker contains this text")
    args = p.parse_args()
    relay(args.client, args.server, args.log, args.marks, args.until)


if __name__ == "__main__":
    main()
