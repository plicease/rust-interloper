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
import queue
import select
import sys
import threading
import time
import tty


def open_pty(path):
    fd = os.open(path, os.O_RDWR | os.O_NOCTTY)
    tty.setraw(fd)
    return fd


def relay(client_path, server_path, log=None, marks_path=None, until=None,
          on_mark=None):
    """Relay until the `until` mark or a terminal closes.

    on_mark(text, note) is called in a thread for each mark and may restart
    VMs; it returns the ends ("client", "server") it restarted, which are
    then reopened. While it runs, a closed terminal does not end the relay,
    and bytes sent towards it are logged but dropped, as on a cable with
    nothing at the far end. note(text) logs a "MARK HOST <text>" line.
    """
    paths = {"client": client_path, "server": server_path, "marks": marks_path}
    fds = {}
    peer = {"client": ("server", "C>S"), "server": ("client", "S>C")}
    pending = b""
    out = open(log, "a") if log else None
    start = time.monotonic()
    done = queue.Queue()
    busy = threading.Event()

    def emit(line):
        print(line, flush=True)
        if out:
            print(line, file=out, flush=True)

    def note(text):
        emit(f"{time.monotonic() - start:12.6f} MARK HOST {text}")

    def reopen(names):
        for name in names:
            if name in fds:
                os.close(fds.pop(name))
            if paths[name]:
                fds[name] = open_pty(paths[name])

    def run_hook(text):
        try:
            done.put(on_mark(text, note) or ())
        except Exception as e:      # report it and end the relay
            done.put(e)

    reopen(["client", "server", "marks"])
    while True:
        try:
            result = done.get_nowait()
        except queue.Empty:
            pass
        else:
            busy.clear()
            if isinstance(result, Exception):
                raise result
            reopen(list(result) + (["marks"] if "client" in result else []))

        by_fd = {fd: name for name, fd in fds.items()}
        ready, _, _ = select.select(list(by_fd), [], [], 0.2)
        for src in ready:
            name = by_fd[src]
            try:
                data = os.read(src, 4096)
            except OSError:
                data = b""
            if not data:
                if not busy.is_set():
                    print(f"{name} terminal closed", file=sys.stderr)
                    return
                os.close(fds.pop(name))
                continue
            now = time.monotonic() - start
            if name == "marks":
                pending += data
                while b"\n" in pending:
                    raw, pending = pending.split(b"\n", 1)
                    text = raw.decode("ascii", "replace").strip()
                    if not text:
                        continue
                    emit(f"{now:12.6f} MARK {text}")
                    if until and until in text:
                        return
                    if on_mark and not busy.is_set():
                        busy.set()
                        threading.Thread(target=run_hook, args=(text,), daemon=True).start()
                continue
            dest, direction = peer[name]
            if dest in fds:
                sent = 0
                while sent < len(data):
                    sent += os.write(fds[dest], data[sent:])
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
