#!/usr/bin/env python3
"""Relay bytes between the client and server COM1 pseudo-terminals and log them.

    sniff.py CLIENT_PTY SERVER_PTY [--log FILE]

Each chunk is logged as one line: seconds since start, direction (C>S is
client to server), byte count, and the bytes in hex. The relay exits when
either 86Box VM closes its terminal.
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


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("client", help="client COM1 PTY, e.g. dos622-client/com1.pty")
    p.add_argument("server", help="server COM1 PTY, e.g. dos622-server/com1.pty")
    p.add_argument("--log", help="also append the log to this file")
    args = p.parse_args()

    client, server = open_pty(args.client), open_pty(args.server)
    route = {client: (server, "C>S"), server: (client, "S>C")}
    out = open(args.log, "a") if args.log else None
    start = time.monotonic()

    while True:
        ready, _, _ = select.select(list(route), [], [])
        for src in ready:
            try:
                data = os.read(src, 4096)
            except OSError:
                data = b""
            if not data:
                print("terminal closed", file=sys.stderr)
                return
            dest, direction = route[src]
            sent = 0
            while sent < len(data):
                sent += os.write(dest, data[sent:])
            line = f"{time.monotonic() - start:12.6f} {direction} {len(data):4d} {data.hex(' ')}"
            print(line, flush=True)
            if out:
                print(line, file=out, flush=True)


if __name__ == "__main__":
    main()
