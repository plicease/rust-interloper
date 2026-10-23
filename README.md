# interloper

A Linux server for the MS-DOS 6.22 `INTERLNK.EXE` client. It replaces
`INTERSVR.EXE`: a DOS machine running the unmodified INTERLNK driver sees
FAT disk images on Linux as drives, over a serial null-modem cable.

Status: early. Images are served **read-only**; writes are refused with a
"Write protect" error. See `docs/interloper-design.md` for the plan and
`docs/protocol.md` for the protocol as reverse-engineered from captures.

## Usage

```sh
interloper serve --port /dev/ttyS0 --image share.img
```

- `--image` takes a FAT12 or FAT16 image: either a bare volume (such as a
  floppy image) or a whole disk with an MBR, in which case the first FAT
  partition is served. Repeat it for more drives. INTERLNK lists them as
  `C:`, `D:` and so on on the server, and maps them to its own drive
  letters.
- `--baud` is the highest line speed to accept (default 9600). Every
  connection starts at 9600; an INTERLNK loaded without `/BAUD` then steps
  up to the highest speed both sides allow.
- `--trace FILE` records every byte, in the format
  `emu/dos622/decode.py` decodes.
- `--verbose` logs each request.

On the DOS side, load the driver from `CONFIG.SYS`, for example
`DEVICE=C:\DOS\INTERLNK.EXE /DRIVES:3 /NOPRINTER /COM:1 /BAUD:9600`.

## Testing against the real INTERLNK

`emu/dos622/` builds MS-DOS 6.22 VMs for 86Box and runs scripted
sessions. With `--interloper`, `capture.py` serves the client VM from
Interloper instead of the INTERSVR VM; see `emu/dos622/README.md`.
