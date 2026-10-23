# MS-DOS 6.22 INTERLNK/INTERSVR test VMs

`build.py` builds a pair of [86Box](https://86box.net/) VMs running genuine
MS-DOS 6.22, wired together over COM1 for protocol captures:

| VM              | Role                                                                |
|-----------------|---------------------------------------------------------------------|
| `dos622-client` | Loads `INTERLNK.EXE /DRIVES:3 /NOPRINTER /COM:1 /BAUD:9600` from `CONFIG.SYS` |
| `dos622-server` | Runs `INTERSVR C: B: /COM:1 /BAUD:9600` from `AUTOEXEC.BAT`         |

## Machine

- 86Box `ibmatami`: an AT-compatible 286 with a 1989 AMI BIOS (86Box labels
  it "[ISA] Trangg Bow Unknown 286"), 8 MHz, no FPU, 1MB RAM (the board's
  maximum: 640K base + 384K extended).
- VGA, AT keyboard, no mouse.
- 3.5" 1.44MB A: drive; the server also has a B: drive holding the fixture.
- Generic ISA IDE controller with a 32MB disk: 65 cylinders, 16 heads,
  63 sectors (BIOS user drive type 47).
- COM1 at 3F8 and COM2 at 2F8, both host pseudo-terminals. While the VM
  runs, 86Box symlinks them to `<vm>/com1.pty` and `<vm>/com2.pty`. COM1
  carries INTERLNK; capture scenarios write step markers to the client's COM2.

## Building

Requirements: Python 3, `qemu-system-i386`, and the three MS-DOS 6.22
(Upgrade) 3.5" floppy images named `Disk1.img`, `Disk2.img` and `Disk3.img`.
Microsoft files are never committed to this repository.

```sh
emu/dos622/build.py \
    --floppies "$HOME/dev/dos-floppy/images/Microsoft DOS 6.22 (Upgrade) (3.5)" \
    --out "$HOME/.local/share/86Box/Virtual Machines"
```

`--out` may be repeated to write the same VMs to several places. Existing
VMs are only replaced with `--force`. A build takes a few minutes and needs
no interaction.

What it does:

1. Boots Setup Disk 1 in headless QEMU. The script watches the VGA text
   screen and types the answers over QMP.
2. The Upgrade edition refuses to install on a blank disk, so it first runs
   `FDISK` and `FORMAT C: /S`, then the normal `SETUP` with default options,
   swapping floppies when asked.
3. Copies the installed disk once per role, boots it with a generated
   floppy in A: holding the role's files (`roles/<role>/`), and runs
   `A:\INSTALL.BAT`.
4. Writes each VM directory: `86box.cfg` (from `86box.cfg.in`), the disk
   image `dos622.img`, and CMOS settings in `nvr/ibmatami.nvr`. The CMOS is
   generated directly so the BIOS setup screen never has to be used.

## Test files

The server shares two drives with the same generated files:

- `C:\FIXTURE` on its FAT16 hard disk;
- `B:`, a FAT12 1.44MB image (`dos622-server/fixture.img`) written directly
  by `build.py`, mounted write-protected in 86Box.

Files: `README.TXT`, `EMPTY.DAT` (0 bytes), `SECTOR.BIN` (512 bytes),
`SECTOR1.BIN` (513 bytes), `BIG.BIN` (66,536 bytes), `READONLY.TXT` (DOS
read-only attribute), `SUB\NESTED.TXT`, `SUB\DEEP\DEEP.TXT` and the empty
directory `EMPTYDIR`. Their SHA-256 sums are in
`dos622-server/FIXTURE.SHA256`.

## Capturing traffic

### Scripted session

`capture.py` copies a built VM pair into a new work directory, installs a
scenario, starts the server and then the client, relays and logs COM1, and
stops both VMs when the client finishes:

```sh
xvfb-run emu/dos622/capture.py --vms "$VMS" --scenario dir-copy \
    --work /tmp/run-dir-copy --log capture.log
```

A scenario is a directory under `scenarios/` with `client/` and `server/`
files that are written into the root of that VM's C: drive (`fatimg.py`
does this without booting the VM). Each has a `client/CAPTURE.BAT`, which
the client's `AUTOEXEC.BAT` runs under `COMMAND /F` so a critical error is
answered with Fail rather than waiting for a key. It announces each step on
COM2, which `capture.py` logs as `MARK` lines. A `HOST-RESTART-CLIENT` or
`HOST-RESTART-SERVER` marker makes `capture.py` restart that VM while the
relay keeps logging.

| Scenario | Steps |
|----------|-------|
| `dir-copy` | `DIR` and `COPY` from both redirected drives. |
| `write` | Create, overwrite and delete files and make a directory on D:, then a copy to the write-protected E:. |
| `reconnect` | Restart the client mid-session, then restart the server. |
| `baud-default` | `dir-copy` reads with no `/BAUD` on either side. |

The work directory is kept so READ data can be checked against the server's
images with `decode.py --image`. Saved captures are in `captures/`.

### Against Interloper

With `--interloper BINARY`, the server VM is not started. Interloper serves
the server VM's `dos622.img` and `fixture.img` as units 2 and 3 on the
client's COM1, so the client's D: and E: are the same files as with
INTERSVR, and the scenarios run unchanged:

```sh
cargo build
xvfb-run emu/dos622/capture.py --vms "$VMS" --scenario dir-copy \
    --work /tmp/run-il --log il.log --interloper target/debug/interloper
emu/dos622/decode.py il.log --image 2=/tmp/run-il/dos622-server/dos622.img@63 \
    --image 3=/tmp/run-il/dos622-server/fixture.img
emu/dos622/fatimg.py --get /tmp/run-il/dos622-client/dos622.img@63 'CAP\BIG.BIN' BIG.BIN
```

The log is Interloper's `--trace` (kept as `il.log.trace.0`) with the
client's markers merged in, and Interloper's own messages are in
`il.log.stderr`. `fatimg.py --get` copies a file out of the client's disk to
compare with `FIXTURE.SHA256`. `--interloper-baud` sets Interloper's
`--baud` (default 9600); use 115200 with the `baud-default` scenario.

`HOST-RESTART-CLIENT` restarts the client VM as usual; Interloper notices
its pty closing and reopens `com1.pty` once the new VM has replaced it.
`HOST-RESTART-SERVER` stops Interloper and starts a new process, which
traces to `il.log.trace.1` and so on; their times are shifted onto one clock
in the merged log.

### Decoding

`decode.py` splits a capture into protocol events, and can check READ data
against the server's images; see `captures/README.md` and
`docs/protocol.md`. It also reads the byte traces of `interloper serve --trace FILE`, where
`MARK HOST baud` lines show Interloper's speed changes.

### Interactive session

1. Start the server VM, then the client VM. Each creates its `com1.pty`
   symlink when it starts.
2. Start the relay, which forwards bytes between the two PTYs and logs them:

   ```sh
   VMS="$HOME/.local/share/86Box/Virtual Machines"
   emu/dos622/sniff.py "$VMS/dos622-client/com1.pty" \
       "$VMS/dos622-server/com1.pty" --log capture.log
   ```

3. INTERLNK tries to connect once when `CONFIG.SYS` loads it. If the relay
   was not running yet, type `INTERLNK` on the client to connect.

Each log line is the time in seconds, the direction (`C>S` or `S>C`), the
byte count and the bytes in hex. Add `--marks <client>/com2.pty` to log lines
the client writes to COM2.

The relay does not pace bytes at 9600 baud: timing comes from each VM's
emulated UART. Restart the relay whenever a VM restarts, because 86Box
creates a new PTY each time.
