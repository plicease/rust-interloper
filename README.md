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
- If the port fails, as a pseudo-terminal does when its emulator exits,
  Interloper keeps running and reopens it when it can: a symbolic link
  (such as 86Box's `com1.pty`) once it has been replaced, any other path as
  soon as it opens. A restarted INTERLNK then reconnects.
- `--trace FILE` records every byte, in the format
  `emu/dos622/decode.py` decodes.
- `--verbose` logs each request.

On the DOS side, load the driver from `CONFIG.SYS`, for example
`DEVICE=C:\DOS\INTERLNK.EXE /DRIVES:3 /NOPRINTER /COM:1 /BAUD:9600`.

## Making images

```sh
interloper image create share.img ~/dos-files
interloper image list share.img
```

`image create` builds an image of a directory's contents. The image is a
1.44MB floppy if the files fit, otherwise the smallest FAT12 or FAT16 volume
that holds them, up to just under 32MB. Names must already be DOS 8.3 names
(`readme.txt` is stored as `README.TXT`). Nothing is renamed or truncated:
a name that cannot be stored, two names that differ only in case, a
symbolic link or a special file is an error, and every problem is listed
before anything is written. Entries whose names start with `.` are left
out. Modification times are kept, in local time to the even second. The
image file must not already exist.

`image list` shows every file and directory in an image that `serve`
accepts, with sizes and times. `interloper image create --help` gives the
full rules.

## Testing against the real INTERLNK

`emu/dos622/` builds MS-DOS 6.22 VMs for 86Box and runs scripted
sessions. With `--interloper`, `capture.py` serves the client VM from
Interloper instead of the INTERSVR VM; see `emu/dos622/README.md`.
`docs/hardware-test.md` covers testing with a real DOS machine over a
serial cable.
