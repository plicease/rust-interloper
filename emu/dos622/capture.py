#!/usr/bin/env python3
"""Run a scripted INTERLNK session between the two VMs and log the traffic.

    capture.py --vms DIR --log FILE [--86box APPIMAGE] [--roms DIR]

DIR must hold dos622-client and dos622-server built with
`build.py --capture`. The server starts first, then the client, whose
AUTOEXEC.BAT runs CAPTURE.BAT. sniff.py relays COM1 between them and labels
each step from the client's COM2 markers. Both VMs are stopped when the
client reports CAPTURE-DONE. 86Box windows open on $DISPLAY; use xvfb-run to
run headless.
"""

import argparse
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import sniff


def start_vm(appimage, roms, vm, ptys):
    for pty in ptys:
        (vm / pty).unlink(missing_ok=True)   # 86Box recreates it on start
    proc = subprocess.Popen([str(appimage), "-N", "-R", str(roms), "-P", str(vm)],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            start_new_session=True)
    deadline = time.monotonic() + 30
    while not all((vm / pty).exists() for pty in ptys):
        if time.monotonic() > deadline or proc.poll() is not None:
            stop_vm(proc)
            sys.exit(f"86Box did not create {', '.join(ptys)} in {vm}")
        time.sleep(0.2)
    return proc


def stop_vm(proc):
    try:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=15)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        os.killpg(proc.pid, signal.SIGKILL)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--vms", type=Path, required=True,
                   help="directory holding dos622-client and dos622-server")
    p.add_argument("--log", type=Path, required=True, help="capture log to write")
    p.add_argument("--86box", dest="appimage", type=Path,
                   default=Path.home() / "opt/86box/86Box-Linux-x86_64-b9001.AppImage")
    p.add_argument("--roms", type=Path, default=Path.home() / "opt/86box/roms")
    p.add_argument("--timeout", type=int, default=900,
                   help="give up after this many seconds (default 900)")
    args = p.parse_args()

    if args.log.exists():
        sys.exit(f"{args.log} exists; choose a new name")
    client, server = args.vms / "dos622-client", args.vms / "dos622-server"

    def expired(signum, frame):
        raise TimeoutError(f"no CAPTURE-DONE within {args.timeout}s")
    signal.signal(signal.SIGALRM, expired)
    signal.alarm(args.timeout)

    procs = [start_vm(args.appimage, args.roms, server, ["com1.pty"])]
    try:
        procs.append(start_vm(args.appimage, args.roms, client, ["com1.pty", "com2.pty"]))
        sniff.relay(client / "com1.pty", server / "com1.pty", args.log,
                    marks_path=client / "com2.pty", until="CAPTURE-DONE")
    finally:
        for proc in procs:
            stop_vm(proc)


if __name__ == "__main__":
    main()
