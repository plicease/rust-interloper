# Testing on real hardware

How to test Interloper against INTERLNK on a real DOS machine over a serial
cable. The first target is Phaistos (NuXT, NEC V20); see section 7 of
`interloper-design.md`. Run the tests at 9600 baud first, and try faster
speeds only once 9600 passes.

## Hardware

- A null-modem cable between a Linux serial port (`/dev/ttyS0`, or a USB
  adapter such as `/dev/ttyUSB0`) and the DOS machine's COM1. A full
  null-modem cable is safest: Interloper turns off RTS/CTS flow control and
  ignores the modem lines, but a 3-wire cable has not been tried with
  INTERLNK on the DOS side.
- Your Linux user needs access to the port, usually through the `dialout`
  group.
- Nothing else may hold the port: check with `fuser /dev/ttyUSB0`.
  ModemManager sometimes probes USB adapters.

## Linux side

Build Interloper and the fixture image, a 1.44 MB FAT12 floppy image with
the same files the emulator tests use (see `emu/dos622/README.md`), plus
their SHA-256 sums and the image's own sum:

```sh
cargo build --release
mkdir -p ~/il-hw
PYTHONPATH=emu/dos622 python3 -c 'import sys, build; build.fat12_floppy(sys.argv[1], build.fixture_files(), label="FIXTURE", readonly=["READONLY.TXT"])' ~/il-hw/fixture.img
PYTHONPATH=emu/dos622 python3 -c 'import hashlib, build; [print(hashlib.sha256(d).hexdigest() + "  " + n) for n, d in sorted(build.fixture_files().items()) if d is not None]' > ~/il-hw/FIXTURE.SHA256
sha256sum ~/il-hw/fixture.img > ~/il-hw/image.sha256
```

The image is identical to the emulator's `dos622-server/fixture.img`. Paths
in `FIXTURE.SHA256` are relative to the image's root: `SUB/NESTED.TXT` is
`X:\SUB\NESTED.TXT` on DOS.

Start the server, with a new trace file for each run:

```sh
target/release/interloper serve --port /dev/ttyUSB0 --image ~/il-hw/fixture.img \
    --trace ~/il-hw/9600.trace -v 2> ~/il-hw/9600.stderr
```

## DOS side

Copy `INTERLNK.EXE` from your own MS-DOS 6.22; it is not in this
repository. Load it from `CONFIG.SYS`:

```
DEVICE=C:\DOS\INTERLNK.EXE /DRIVES:1 /NOPRINTER /COM:1 /BAUD:9600
```

`/DRIVES:1` is enough for one image. If Interloper is running when the
machine boots, INTERLNK connects at load and shows its drive table (such as
`E: equals C:`). Otherwise type `INTERLNK` at the prompt to connect. Below,
`X:` is the letter INTERLNK assigned.

## Tests at 9600 baud

1. **Discovery and DIR:** run `INTERLNK`, then `DIR X:\` and
   `DIR X:\SUB\DEEP`. Expect `README.TXT`, `EMPTY.DAT` (0 bytes),
   `SECTOR.BIN` (512), `SECTOR1.BIN` (513), `BIG.BIN` (66,536),
   `READONLY.TXT`, `SUB` and `EMPTYDIR`.
2. **COPY:** `MD C:\CAP`, `COPY X:\*.* C:\CAP` and
   `COPY X:\SUB\NESTED.TXT C:\CAP`. Compare each copy with its remote
   original, such as `FC /B X:\BIG.BIN C:\CAP\BIG.BIN`.
3. **Writes are refused:** `COPY C:\AUTOEXEC.BAT X:\` must fail with a
   write-protect error; answer `F` (Fail). Also try `DEL X:\README.TXT` and
   `MD X:\NEW`.
4. **Client restart:** with Interloper still running, reboot the DOS machine
   (Ctrl-Alt-Del), then run `DIR X:\` again. On a real port nothing closes,
   so Interloper sees a new handshake on the open port. The emulator tests
   do not cover this path; the unit tests do.
5. **Server restart:** stop Interloper with Ctrl-C, start it again with a
   new `--trace` file, and run `DIR X:\`. INTERLNK should find the server
   again after a short timeout.

## Checks afterwards

```sh
sha256sum -c ~/il-hw/image.sha256
emu/dos622/decode.py ~/il-hw/9600.trace --image 2=$HOME/il-hw/fixture.img | head -3
emu/dos622/decode.py ~/il-hw/9600.trace | grep -c "not echoed"
```

- `sha256sum -c` must report the image as unchanged.
- `decode.py --image` checks every sector Interloper sent against the image
  and must report `0 mismatched`.
- Each `not echoed` line is a lost packet. At 9600 there should be none.
- For a full end-to-end check, move `C:\CAP` to Linux (on a floppy or CF
  card) and run `sha256sum -c FIXTURE.SHA256` in it.

## Faster speeds

Once 9600 passes, start Interloper with `--baud 115200` and change
`/BAUD:` to 19200, 38400, 57600 and 115200 in turn, with a new trace file
for each. For each speed, note:

- the `MARK HOST baud` lines in the trace, which show the speed change;
- whether packets are lost and the link falls back to echo mode;
- how long `COPY X:\BIG.BIN C:\CAP` takes.

In the emulator at 115200, the first packet each way is lost and both sides
then stay in echo mode, at about 2.3 KB/s (see "Lost packets and echo mode"
in `protocol.md`). Whether a real machine does the same is an open
question. A V20 with a UART that has no FIFO may well do it too.

Keep the traces and `.stderr` files. They are the first captures from real
hardware, and should settle the guessed speed-change timing in
`protocol.md`.
