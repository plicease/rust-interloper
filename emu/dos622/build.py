#!/usr/bin/env python3
"""Build MS-DOS 6.22 INTERLNK client and INTERSVR server VMs for 86Box.

The genuine MS-DOS 6.22 Setup is run unattended in QEMU (which can swap
floppies and read the text screen from a script), then each role is
customised and wrapped in an 86Box VM directory:

    <out>/dos622-client/   INTERLNK.EXE loaded from CONFIG.SYS
    <out>/dos622-server/   INTERSVR.EXE started from AUTOEXEC.BAT

Both VMs are an AMI-BIOS 286 AT with 1MB RAM, VGA, a 1.44MB A: drive and a
32MB IDE disk (65 cylinders, 16 heads, 63 sectors, BIOS drive type 47).
COM1 is a host pseudo-terminal, symlinked to <vm>/com1.pty while 86Box runs.

Microsoft files are never stored in this repository; point --floppies at a
directory holding the three MS-DOS 6.22 (Upgrade) 3.5" images.
"""

import argparse
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent

CYLS, HEADS, SPT = 65, 16, 63
DISK_BYTES = CYLS * HEADS * SPT * 512
FLOPPY_BYTES = 1474560
ROLES = ("client", "server")


def log(msg):
    print(f"[dos622] {msg}", flush=True)


# --- QEMU control ---------------------------------------------------------

KEYS = {
    " ": "spc", "\n": "ret", "\\": "backslash", ":": "shift-semicolon",
    ".": "dot", "/": "slash", "-": "minus", "_": "shift-minus",
}


class Qemu:
    """A headless QEMU PC driven over QMP, reading the VGA text screen."""

    def __init__(self, workdir, drives, boot):
        self.workdir = workdir
        (workdir / "qmp.sock").unlink(missing_ok=True)
        args = [
            "qemu-system-i386", "-M", "isapc", "-cpu", "486", "-m", "4",
            "-display", "none", "-vga", "std", "-boot", boot,
            "-qmp", "unix:qmp.sock,server=on,wait=off",
        ] + drives
        self.proc = subprocess.Popen(args, cwd=workdir)
        self.sock = self._connect()
        self.rfile = self.sock.makefile("r")
        self.rfile.readline()  # greeting
        self._execute("qmp_capabilities")

    def _connect(self):
        # Connect by relative path: tempdir paths can exceed the AF_UNIX limit.
        deadline = time.monotonic() + 10
        while True:
            sock = socket.socket(socket.AF_UNIX)
            cwd = os.getcwd()
            try:
                os.chdir(self.workdir)
                sock.connect("qmp.sock")
                return sock
            except (FileNotFoundError, ConnectionRefusedError):
                sock.close()
                if time.monotonic() > deadline or self.proc.poll() is not None:
                    raise RuntimeError("could not connect to QEMU QMP socket")
                time.sleep(0.1)
            finally:
                os.chdir(cwd)

    def _execute(self, command, **arguments):
        msg = {"execute": command}
        if arguments:
            msg["arguments"] = arguments
        self.sock.sendall((json.dumps(msg) + "\n").encode())
        while True:
            reply = json.loads(self.rfile.readline())
            if "return" in reply:
                return reply["return"]
            if "error" in reply:
                raise RuntimeError(f"QMP {command}: {reply['error']}")

    def hmp(self, line):
        return self._execute("human-monitor-command", **{"command-line": line})

    def key(self, name):
        self.hmp(f"sendkey {name}")
        time.sleep(0.1)

    def type(self, text):
        for ch in text:
            if ch in KEYS:
                self.key(KEYS[ch])
            elif ch.isupper():
                self.key("shift-" + ch.lower())
            else:
                self.key(ch)

    def insert(self, drive, image):
        if image is None:
            self.hmp(f"eject -f {drive}")
        else:
            self.hmp(f"change {drive} {image} raw")

    def screen(self):
        """Return the 80x25 text screen as a list of lines."""
        out = self.hmp("xp /2000hx 0xb8000")
        chars = []
        for line in out.splitlines():
            for word in line.split(":", 1)[1].split():
                c = int(word, 16) & 0xFF
                chars.append(chr(c) if 32 <= c < 127 else " ")
        text = "".join(chars)
        return [text[i:i + 80].rstrip() for i in range(0, 2000, 80)]

    def wait(self, predicate, what, timeout=300):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            lines = self.screen()
            if predicate(lines):
                return lines
            if self.proc.poll() is not None:
                raise RuntimeError(f"QEMU exited while waiting for {what}")
            time.sleep(0.5)
        raise RuntimeError(f"timed out waiting for {what}:\n" + "\n".join(self.screen()))

    def wait_text(self, text, timeout=300):
        return self.wait(lambda lines: text in "\n".join(lines), repr(text), timeout)

    def wait_prompt(self, prompt, timeout=300):
        def ready(lines):
            filled = [line for line in lines if line.strip()]
            return bool(filled) and filled[-1] == prompt
        return self.wait(ready, f"prompt {prompt}", timeout)

    def run(self, prompt, command, timeout=300):
        self.type(command + "\n")
        time.sleep(1)
        return self.wait_prompt(prompt, timeout)

    def quit(self):
        try:
            self._execute("quit")
        except (OSError, ValueError):
            pass
        self.proc.wait(timeout=30)


# --- disk images ------------------------------------------------------------

# 1.44MB FAT12 layout: boot sector, two 9-sector FATs, 14 root directory
# sectors, then one sector per cluster starting at cluster 2.
FAT_SECTORS, ROOT_SECTOR, ROOT_ENTRIES, DATA_SECTOR = 9, 19, 224, 33
DOS_DATE = (1994 - 1980) << 9 | 5 << 5 | 31     # 1994-05-31
DOS_TIME = 6 << 11 | 22 << 5                    # 06:22:00


def fat12_floppy(path, files=None, label=None, readonly=()):
    """Write a 1.44MB FAT12 image holding files.

    files maps "DIR/NAME.EXT" to bytes; a path ending in "/" with None
    creates an empty directory. Paths in readonly get the read-only attribute.
    """
    img = bytearray(FLOPPY_BYTES)
    vol = (label or "NO NAME").ljust(11)[:11].encode("ascii")
    boot = bytearray(512)
    boot[0:3] = b"\xEB\x3C\x90"
    boot[3:11] = b"MSDOS5.0"
    boot[11:13] = (512).to_bytes(2, "little")   # bytes per sector
    boot[13] = 1                                # sectors per cluster
    boot[14:16] = (1).to_bytes(2, "little")     # reserved sectors
    boot[16] = 2                                # FATs
    boot[17:19] = ROOT_ENTRIES.to_bytes(2, "little")
    boot[19:21] = (2880).to_bytes(2, "little")  # total sectors
    boot[21] = 0xF0                             # media descriptor
    boot[22:24] = FAT_SECTORS.to_bytes(2, "little")
    boot[24:26] = (18).to_bytes(2, "little")    # sectors per track
    boot[26:28] = (2).to_bytes(2, "little")     # heads
    boot[38] = 0x29                             # extended boot signature
    boot[39:43] = (0x1622_0F1C).to_bytes(4, "little")
    boot[43:54] = vol
    boot[54:62] = b"FAT12   "
    boot[510:512] = b"\x55\xAA"
    img[0:512] = boot

    # Build a tree: directories are dicts, files are bytes.
    root = {}
    for name, data in (files or {}).items():
        parts = name.strip("/").split("/")
        node = root
        for part in parts[:-1]:
            node = node.setdefault(part, {})
        node[parts[-1]] = {} if data is None else data

    fat = [0xFF0, 0xFFF]
    readonly = {name.upper() for name in readonly}

    def allocate(nbytes):
        count = max(1, -(-nbytes // 512))
        first = len(fat)
        fat.extend(range(first + 1, first + count))
        fat.append(0xFFF)
        return first

    def entry(name, attr, cluster, size):
        if name in (".", ".."):
            raw = name.ljust(11)
        else:
            base, _, ext = name.upper().partition(".")
            raw = base.ljust(8) + ext.ljust(3)
        e = bytearray(32)
        e[0:11] = raw.encode("ascii")
        e[11] = attr
        e[22:24] = DOS_TIME.to_bytes(2, "little")
        e[24:26] = DOS_DATE.to_bytes(2, "little")
        e[26:28] = cluster.to_bytes(2, "little")
        e[28:32] = size.to_bytes(4, "little")
        return bytes(e)

    def cluster_offset(cluster):
        return (DATA_SECTOR + cluster - 2) * 512

    def write_dir(node, prefix, entries):
        """Allocate and write node's children; return its directory entries."""
        children = []
        for name in sorted(node):
            child, path = node[name], prefix + name
            if isinstance(child, dict):
                cluster = allocate((len(child) + 2) * 32)
                children.append((name, child, path, cluster))
                entries.append(entry(name, 0x10, cluster, 0))
            elif child:
                cluster = allocate(len(child))
                start = cluster_offset(cluster)
                img[start:start + len(child)] = child
                attr = 0x20 | (0x01 if path.upper() in readonly else 0)
                entries.append(entry(name, attr, cluster, len(child)))
            else:
                attr = 0x20 | (0x01 if path.upper() in readonly else 0)
                entries.append(entry(name, attr, 0, 0))
        return children

    def fill_dir(name, node, path, cluster, parent):
        entries = [entry(".", 0x10, cluster, 0), entry("..", 0x10, parent, 0)]
        for sub in write_dir(node, path + "/", entries):
            fill_dir(*sub, parent=cluster)
        start = cluster_offset(cluster)
        data = b"".join(entries)
        img[start:start + len(data)] = data

    root_entries = [vol + b"\x08" + bytes(20)] if label else []
    for sub in write_dir(root, "", root_entries):
        fill_dir(*sub, parent=0)
    if len(root_entries) > ROOT_ENTRIES:
        raise ValueError("too many root directory entries")
    data = b"".join(root_entries)
    img[ROOT_SECTOR * 512:ROOT_SECTOR * 512 + len(data)] = data

    if len(fat) > 2849:
        raise ValueError("files do not fit on a 1.44MB floppy")
    table = bytearray(FAT_SECTORS * 512)
    for i in range(0, len(fat), 2):
        a, b = fat[i], fat[i + 1] if i + 1 < len(fat) else 0
        table[i * 3 // 2:i * 3 // 2 + 3] = bytes(
            (a & 0xFF, (a >> 8) | (b & 0x0F) << 4, b >> 4))
    for copy in (1, 1 + FAT_SECTORS):
        img[copy * 512:(copy + FAT_SECTORS) * 512] = table
    Path(path).write_bytes(img)


def fixture_files():
    """Deterministic test files served by INTERSVR; path -> contents."""
    def text(*lines):
        return ("\r\n".join(lines) + "\r\n").encode("ascii")

    lcg, big = 0x1622, bytearray()
    for _ in range(66536):
        lcg = (lcg * 1103515245 + 12345) & 0x7FFFFFFF
        big.append(lcg >> 16 & 0xFF)

    return {
        "README.TXT": text(
            "Interloper protocol fixture.",
            "These files are generated by emu/dos622/build.py.",
        ),
        "EMPTY.DAT": b"",
        "SECTOR.BIN": bytes(range(256)) * 2,
        "SECTOR1.BIN": bytes(range(256)) * 2 + b"\xAA",
        "BIG.BIN": bytes(big),
        "READONLY.TXT": text("This file has the DOS read-only attribute."),
        "SUB/NESTED.TXT": text("One directory down."),
        "SUB/DEEP/DEEP.TXT": text("Two directories down."),
        "EMPTYDIR/": None,
    }


def crlf(path):
    return path.read_text().replace("\r\n", "\n").replace("\n", "\r\n").encode("ascii")


def payload_floppy(role, path, capture=False):
    """Write the floppy DOS sees as A: while a role is customised."""
    files = {}
    for name in ("CONFIG.SYS", "AUTOEXEC.BAT", "INSTALL.BAT", "CAPTURE.BAT"):
        src = HERE / "roles" / role / name
        if src.exists():
            files[name] = crlf(src)
    if capture and role == "client":
        files["AUTOEXEC.BAT"] += b"CALL C:\\CAPTURE.BAT\r\n"
    if role == "server":
        files.update({f"FIXTURE/{name}": data for name, data in fixture_files().items()})
    fat12_floppy(path, files, label="PAYLOAD")


# --- build steps ------------------------------------------------------------

def install_base(floppies, work):
    """Run MS-DOS 6.22 Setup onto a blank disk and return its path."""
    disks = []
    for n in (1, 2, 3):
        src = floppies / f"Disk{n}.img"
        if not src.is_file():
            sys.exit(f"missing {src}")
        dest = work / f"Disk{n}.img"
        shutil.copyfile(src, dest)
        disks.append(dest)
    uninstall = work / "Uninstall.img"
    fat12_floppy(uninstall)
    disk = work / "base.img"
    with open(disk, "wb") as f:
        f.truncate(DISK_BYTES)

    qemu = Qemu(work, [
        "-drive", f"if=floppy,index=0,file={disks[0]},format=raw",
        "-drive", f"if=none,id=hd,file={disk},format=raw",
        "-device", f"ide-hd,drive=hd,bus=ide.0,unit=0,cyls={CYLS},heads={HEADS},"
                   f"secs={SPT},bios-chs-trans=none",
    ], boot="a")
    try:
        def leave_setup():
            qemu.wait_text("Welcome to Setup.")
            qemu.key("f3")
            time.sleep(1)
            qemu.key("f3")
            qemu.wait_prompt("A:\\>")

        # The Upgrade edition refuses a blank disk, so partition and format
        # it first; Setup then upgrades the bare MS-DOS 6.22 system on C:.
        log("partitioning")
        leave_setup()
        qemu.type("FDISK\n")
        qemu.wait_text("Enter choice:")
        qemu.type("1\n")
        qemu.wait_text("Create Primary DOS Partition")
        qemu.type("1\n")
        qemu.wait_text("Do you wish to use the maximum available size")
        qemu.key("ret")
        qemu.wait_text("Press any key when ready")
        qemu.key("ret")

        log("formatting C:")
        leave_setup()
        qemu.type("FORMAT C: /S\n")
        qemu.wait_text("Proceed with Format (Y/N)?")
        qemu.type("Y\n")
        qemu.wait_text("Volume label (11 characters", timeout=600)
        qemu.type("DOS622\n")
        qemu.wait_prompt("A:\\>")

        log("running Setup")
        qemu.type("SETUP\n")
        qemu.wait_text("Welcome to Setup.")
        qemu.key("ret")
        qemu.wait_text("Label the disk(s) as follows")
        qemu.key("ret")
        qemu.wait_text("The settings are correct.")
        qemu.key("ret")
        qemu.wait_text("Install the listed programs.")
        qemu.key("ret")
        qemu.wait_text("To install MS-DOS 6.22 files now, press Y")
        qemu.key("y")

        def prompt(lines):
            screen = "\n".join(lines)
            for marker in ("Please label a floppy", "Please insert",
                           "Remove disks from all floppy", "MS-DOS Setup Complete"):
                if marker in screen:
                    return screen
            return None

        while True:
            screen = prompt(qemu.wait(prompt, "a Setup prompt", timeout=900))
            if "MS-DOS Setup Complete" in screen:
                break
            if "Remove disks" in screen:
                qemu.insert("floppy0", None)
            elif "UNINSTALL #1" in screen:
                qemu.insert("floppy0", uninstall)
            else:
                n = next(n for n in (1, 2, 3) if f"Setup Disk #{n}" in screen)
                log(f"inserting Setup Disk #{n}")
                qemu.insert("floppy0", disks[n - 1])
            qemu.key("ret")
            qemu.wait(lambda lines: prompt(lines) != screen, "Setup to continue")

        qemu.key("ret")
        qemu.wait_text("HIMEM is testing extended memory")
        qemu.wait_prompt("C:\\>")
        log("Setup complete")
    finally:
        qemu.quit()
    return disk


def customise(role, base, work, capture=False):
    """Copy the base disk and apply a role's files; return (disk, floppy)."""
    log(f"customising {role}")
    disk = work / f"{role}.img"
    shutil.copyfile(base, disk)
    payload = work / f"{role}-payload.img"
    payload_floppy(role, payload, capture)
    drives = [
        "-drive", f"if=floppy,index=0,file={payload},format=raw,readonly=on",
        "-drive", f"if=none,id=hd,file={disk},format=raw",
        "-device", f"ide-hd,drive=hd,bus=ide.0,unit=0,cyls={CYLS},heads={HEADS},"
                   f"secs={SPT},bios-chs-trans=none",
    ]
    floppy = None
    if role == "server":
        floppy = work / "fixture.img"
        fat12_floppy(floppy, fixture_files(), label="FIXTURE", readonly=["READONLY.TXT"])

    qemu = Qemu(work, drives, boot="c")
    try:
        qemu.wait_prompt("C:\\>")
        lines = qemu.run("C:\\>", "A:\\INSTALL.BAT", timeout=600)
        if not any("INSTALL-OK" in line for line in lines):
            raise RuntimeError(f"{role} INSTALL.BAT failed:\n" + "\n".join(lines))
    finally:
        qemu.quit()
    return disk, floppy


def cmos(floppies, ext_kb=384):
    """AT CMOS for the AMI 286 BIOS: 1.44MB floppies, VGA, drive C: type 47."""
    c = bytearray(b"\x00" * 0x30 + b"\x80\x01\x20" + b"\xff" * 6 + b"\x00\x32" + b"\xff" * 5)
    c[0x0A], c[0x0B], c[0x0D] = 0x26, 0x02, 0x80   # RTC registers A, B, D
    c[0x10] = 0x44 if floppies == 2 else 0x40      # A: (and B:) 1.44MB
    c[0x12] = 0xF0                                 # C: type in 0x19, D: none
    c[0x14] = (floppies - 1) << 6 | 0x01           # floppies, EGA/VGA, no FPU
    c[0x15:0x17] = (640).to_bytes(2, "little")
    c[0x17:0x19] = ext_kb.to_bytes(2, "little")
    c[0x19] = 47
    c[0x1B:0x1D] = CYLS.to_bytes(2, "little")      # AMI user type: cylinders
    c[0x1D] = HEADS
    c[0x20] = 0x08                                 # control byte: > 8 heads
    c[0x21:0x23] = CYLS.to_bytes(2, "little")      # landing zone
    c[0x23] = SPT
    checksum = sum(c[0x10:0x2E])
    c[0x2E:0x30] = checksum.to_bytes(2, "big")
    c[0x30:0x32] = ext_kb.to_bytes(2, "little")
    return bytes(c)


def write_vm(out, role, disk, floppy, force):
    vm = (out / f"dos622-{role}").resolve()
    if vm.exists():
        if not force:
            sys.exit(f"{vm} exists; use --force to replace it")
        shutil.rmtree(vm)
    (vm / "nvr").mkdir(parents=True)
    shutil.copyfile(disk, vm / "dos622.img")
    (vm / "nvr" / "ibmatami.nvr").write_bytes(cmos(2 if floppy else 1))

    # The fixture goes in B: so the BIOS still boots from C:.
    fdd = "fdd_01_type = 35_2hd\n"
    if floppy:
        shutil.copyfile(floppy, vm / "fixture.img")
        fdd += ("fdd_02_type = 35_2hd\n"
                f"fdd_02_fn = {vm / 'fixture.img'}\nfdd_02_writeprot = 1\n")
    else:
        fdd += "fdd_02_type = none\n"
    template = (HERE / "86box.cfg.in").read_text()
    (vm / "86box.cfg").write_text(
        template.replace("@FDD@", fdd).replace("@PTY@", str(vm / "com1.pty"))
        .replace("@PTY2@", str(vm / "com2.pty")))
    log(f"wrote {vm}")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--floppies", type=Path, required=True,
                   help="directory with MS-DOS 6.22 Disk1.img, Disk2.img, Disk3.img")
    p.add_argument("--out", type=Path, action="append", required=True,
                   help="directory to write the VMs into (repeatable)")
    p.add_argument("--force", action="store_true", help="replace existing VMs")
    p.add_argument("--capture", action="store_true",
                   help="make the client run CAPTURE.BAT from AUTOEXEC.BAT (see capture.py)")
    args = p.parse_args()

    for out in args.out:
        for role in ROLES:
            if (out / f"dos622-{role}").exists() and not args.force:
                sys.exit(f"{out / f'dos622-{role}'} exists; use --force to replace it")

    with tempfile.TemporaryDirectory(prefix="dos622-") as tmp:
        work = Path(tmp)
        base = install_base(args.floppies.resolve(), work)
        built = {role: customise(role, base, work, args.capture) for role in ROLES}
        manifest = "".join(
            f"{hashlib.sha256(data).hexdigest()}  {name}\n"
            for name, data in sorted(fixture_files().items()) if data is not None)
        for out in args.out:
            out.mkdir(parents=True, exist_ok=True)
            for role, (disk, floppy) in built.items():
                write_vm(out, role, disk, floppy, args.force)
            (out / "dos622-server" / "FIXTURE.SHA256").write_text(manifest)


if __name__ == "__main__":
    main()
