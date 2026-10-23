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
the same files the emulator tests use (see `emu/dos622/README.md`). To
check the copies on the DOS machine itself, the image also carries a DOS
`md5sum`: [md5sum16](https://github.com/plicease/dos-md5sum16) (MIT), whose
releases include a prebuilt `md5sum.exe`.

```sh
cargo build --release
curl -LO https://github.com/plicease/dos-md5sum16/releases/download/v0.01/md5sum.exe
emu/dos622/fixture.py ~/il-hw --md5sum md5sum.exe
sha256sum ~/il-hw/fixture.img > ~/il-hw/image.sha256
```

`fixture.py` writes `fixture.img` and `FIXTURE.SHA256`, the SHA-256 of each
file in it. With `--md5sum`, the image's root also holds `MD5SUM.EXE` and
`MD5SUM`, the MD5 of each root file. Without it, the image is identical to
the emulator's `dos622-server/fixture.img`. Paths in `FIXTURE.SHA256` are
relative to the image's root: `SUB/NESTED.TXT` is `X:\SUB\NESTED.TXT` on
DOS.

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
2. **COPY:** `MD C:\CAP`, `COPY X:\*.* C:\CAP`, then `C:`, `CD \CAP` and
   `MD5SUM -c MD5SUM`. Every file must print `OK`. This checks the copies
   on the DOS machine's own disk, independently of the link.
   (`FC /B X:\*.* C:\CAP\*.*` is weaker: it only shows that two reads
   over the link agree.)
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

## Faster speeds

Once 9600 passes, start Interloper with `--baud 115200` and change
`/BAUD:` to 19200, 38400, 57600 and 115200 in turn, with a new trace file
for each. For each speed, note:

- the `MARK HOST baud` lines in the trace, which show the speed change;
- whether packets are lost and the link falls back to echo mode;
- how long `COPY X:\BIG.BIN C:\CAP` takes.

In the emulator at 115200, the first packet each way is lost and both sides
then stay in echo mode, at about 2.3 KB/s (see "Lost packets and echo mode"
in `protocol.md`). Phaistos does not: it runs at 115200 with no lost
packets, so this is a limit of 86Box's UART.

Keep the traces and `.stderr` files.

## Results

| Date | Machine | Speed | Result |
|------|---------|-------|--------|
| 2026-10-23 | Phaistos (NuXT, NEC V20), MS-DOS 6.22, served from a Linux `/dev/ttyS0` | 9600 | Pass |
| 2026-10-23 | Phaistos | 19200 | Pass |
| 2026-10-23 | Phaistos | 38400 | Pass |
| 2026-10-23 | Phaistos | 57600 | Pass |
| 2026-10-23 | Phaistos | 115200 | Pass, except the client restart |

At 9600 on Phaistos every test above passed: DIR, COPY with
`MD5SUM -c` reporting every file `OK`, four writes refused with the image
unchanged, a client reboot and a server restart. The traces decode with no
anomalies and no lost packets, every READ matched the image, and the
127-sector READ for `BIG.BIN` ran at the full line rate (about 8.5 s per
8 KB block). The traces are in `emu/dos622/captures/hardware/`.

The faster runs started Interloper with the same `--baud` as `/BAUD:`.
Each speed step's test pattern passed, no packets were lost, and every
READ matched the image. Time for each 8 KB block of `BIG.BIN`, against
the line rate:

| Speed | Per 8 KB block | Line rate |
|-------|----------------|-----------|
| 19200 | 4.28 s | 4.27 s |
| 38400 | 2.20 s | 2.13 s |
| 57600 | 1.54 s | 1.42 s |
| 115200 | 0.85 s | 0.71 s |

The client reboot failed at 115200. INTERLNK restarts at 9600, and at
115200 each byte of its handshake arrives as several `00` bytes with
framing errors. Interloper took `00` for a packet's flags byte, so it never
went back to 9600, and DOS could not reconnect until Interloper was
restarted. At the slower speeds the same handshake arrived as bytes that
cannot be flags, which Interloper treats as noise and so goes back to 9600.
That worked there, but only by luck.
