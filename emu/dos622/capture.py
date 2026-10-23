#!/usr/bin/env python3
"""Run a scripted INTERLNK session between the two VMs and log the traffic.

    capture.py --vms DIR --scenario NAME --work DIR --log FILE
               [--86box APPIMAGE] [--roms DIR] [--timeout SECONDS]

DIR must hold dos622-client and dos622-server from build.py. They are copied
into the --work directory, which is kept afterwards so READ data can be
checked against the server's images (decode.py --image). The files in
scenarios/NAME/client and scenarios/NAME/server are written into the root
of each copy's C: drive, and the client's AUTOEXEC.BAT is made to run
C:\\CAPTURE.BAT under COMMAND /F, which answers critical errors with Fail.

The server starts first, then the client. sniff.py relays COM1 between them
and labels each step from the client's COM2 markers. A marker of
HOST-RESTART-SERVER or HOST-RESTART-CLIENT makes this script stop and
restart that VM, keeping the relay running. Both VMs are stopped when the
client reports CAPTURE-DONE. 86Box windows open on $DISPLAY; use xvfb-run to
run headless.

With --interloper BINARY, Interloper replaces the server VM: it serves the
server's C: (dos622.img) and B: (fixture.img) as units 2 and 3 on the
client's COM1, and the log is its --trace output with the client's markers
merged in. Restart markers are not supported in this mode.
"""

import argparse
import os
import select
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import build
import fatimg
import sniff

HERE = Path(__file__).resolve().parent
PARTITION = 63      # first sector of C: on the VM disks
PTYS = {"client": ["com1.pty", "com2.pty"], "server": ["com1.pty"]}


def copy_vm(src, dest):
    """Copy a VM directory, pointing its 86box.cfg at the new location."""
    shutil.copytree(src, dest, ignore=shutil.ignore_patterns("*.pty"))
    cfg = dest / "86box.cfg"
    text = cfg.read_text()
    if "com2.pty" not in text:
        sys.exit(f"{src} has no COM2 console; rebuild it with build.py")
    cfg.write_text(text.replace(str(src.resolve()), str(dest.resolve())))


def install_scenario(scenario, vms):
    hook = b"C:\\DOS\\COMMAND.COM /F /C C:\\CAPTURE.BAT\r\n"
    files = {"client": {"AUTOEXEC.BAT": build.crlf(HERE / "roles/client/AUTOEXEC.BAT") + hook},
             "server": {}}
    for role in files:
        role_dir = scenario / role
        if role_dir.is_dir():
            files[role].update({f.name.upper(): build.crlf(f)
                                for f in sorted(role_dir.iterdir()) if f.is_file()})
    if "CAPTURE.BAT" not in files["client"]:
        sys.exit(f"{scenario}/client/CAPTURE.BAT is missing")
    for role, role_files in files.items():
        for name, data in role_files.items():
            fatimg.put(vms[role] / "dos622.img", name, data, PARTITION)


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
    except ProcessLookupError:
        pass
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()


def run_interloper(binary, vms, log):
    """Serve the client from Interloper until CAPTURE-DONE; write the merged log."""
    server = vms["server"]
    trace = log.with_name(log.name + ".trace")
    stderr = open(log.with_name(log.name + ".stderr"), "w")
    cmd = [str(binary), "serve", "--port", str(vms["client"] / "com1.pty"),
           "--image", str(server / "dos622.img"), "--image", str(server / "fixture.img"),
           "--trace", str(trace), "--verbose"]
    proc = subprocess.Popen(cmd, stdout=stderr, stderr=stderr)
    start = time.monotonic()
    marks, pending = [], b""
    fd = sniff.open_pty(vms["client"] / "com2.pty")
    try:
        while True:
            if proc.poll() is not None:
                raise RuntimeError(f"interloper exited with {proc.returncode}")
            ready, _, _ = select.select([fd], [], [], 0.5)
            if not ready:
                continue
            pending += os.read(fd, 4096)
            while b"\n" in pending:
                raw, pending = pending.split(b"\n", 1)
                text = raw.decode("ascii", "replace").strip()
                if not text:
                    continue
                if text.startswith("HOST-"):
                    raise ValueError(f"{text} is not supported with --interloper")
                marks.append(f"{time.monotonic() - start:12.6f} MARK {text}")
                print(marks[-1], flush=True)
                if text == "CAPTURE-DONE":
                    return
    finally:
        os.close(fd)
        proc.terminate()
        proc.wait()
        lines = trace.read_text().splitlines() if trace.exists() else []
        merged = sorted(lines + marks, key=lambda line: float(line.split()[0]))
        log.write_text("".join(line + "\n" for line in merged))


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--vms", type=Path, required=True,
                   help="directory holding dos622-client and dos622-server")
    p.add_argument("--scenario", required=True,
                   help="scenario name (a directory under scenarios/) or path")
    p.add_argument("--work", type=Path, required=True,
                   help="new directory for this run's copies of the VMs")
    p.add_argument("--log", type=Path, required=True, help="capture log to write")
    p.add_argument("--86box", dest="appimage", type=Path,
                   default=Path.home() / "opt/86box/86Box-Linux-x86_64-b9001.AppImage")
    p.add_argument("--roms", type=Path, default=Path.home() / "opt/86box/roms")
    p.add_argument("--timeout", type=int, default=900,
                   help="give up after this many seconds (default 900)")
    p.add_argument("--interloper", type=Path, metavar="BINARY",
                   help="serve the client from Interloper instead of the server VM")
    args = p.parse_args()

    scenario = Path(args.scenario)
    if not scenario.is_dir():
        scenario = HERE / "scenarios" / args.scenario
    if not scenario.is_dir():
        sys.exit(f"no scenario {args.scenario}")
    for path in (args.log, args.work):
        if path.exists():
            sys.exit(f"{path} exists; choose a new name")

    vms = {role: args.work / f"dos622-{role}" for role in PTYS}
    for role, vm in vms.items():
        copy_vm(args.vms / f"dos622-{role}", vm)
    install_scenario(scenario, vms)

    def expired(signum, frame):
        raise TimeoutError(f"no CAPTURE-DONE within {args.timeout}s")
    signal.signal(signal.SIGALRM, expired)
    signal.alarm(args.timeout)

    procs = {}

    def start(role):
        procs[role] = start_vm(args.appimage, args.roms, vms[role], PTYS[role])

    def on_mark(text, note):
        if not text.startswith("HOST-RESTART-"):
            return ()
        role = text.removeprefix("HOST-RESTART-").lower()
        if role not in procs:
            raise ValueError(f"unknown marker {text}")
        stop_vm(procs[role])
        note(f"{role} stopped")
        start(role)
        note(f"{role} started")
        return (role,)

    try:
        if args.interloper:
            start("client")
            run_interloper(args.interloper.resolve(), vms, args.log)
            return
        start("server")
        start("client")
        sniff.relay(vms["client"] / "com1.pty", vms["server"] / "com1.pty", args.log,
                    marks_path=vms["client"] / "com2.pty", until="CAPTURE-DONE",
                    on_mark=on_mark)
    finally:
        for proc in procs.values():
            stop_vm(proc)


if __name__ == "__main__":
    main()
