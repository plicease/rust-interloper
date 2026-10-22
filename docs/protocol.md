# INTERLNK/INTERSVR serial protocol

**Status:** working notes, updated as captures are decoded. Everything here
comes from captures of the genuine MS-DOS 6.22 programs
(`emu/dos622/captures/`) unless marked otherwise. Guesses are labelled as
such, and open questions are listed at the end.

## Sources

- Captures: `emu/dos622/captures/*.log`, recorded at 9600 baud between
  INTERLNK 1.00 (client) and INTERSVR 1.10 (server). The log format is
  described in that directory's README.
- Decoder: `emu/dos622/decode.py` applies the rules below to a capture. It
  reports any byte that breaks them as an anomaly. With `--image` it checks
  every READ against the server's disk images. Both captures decode with no
  anomalies, and all 170 READs match the images byte for byte.
- Reference: [sarahemm/ruby-interlnk](https://github.com/sarahemm/ruby-interlnk)
  (MIT), a client-side implementation. Used as a lead only; every rule here
  was checked against the captures. Where it differs from the captures, the
  captures win.

## Summary

INTERLNK is a DOS block device driver that forwards device requests to the
server. **The server answers with raw sectors from the volume**, numbered
from the volume's first sector (the partition, for a hard disk); it does not
handle files or directories at all. The client's DOS does all FAT
interpretation: it reads the BPB, FATs, directories and data sectors itself.

The serial link is half duplex. Every packet travels with a short exchange
of single control bytes, and the side that just sent a packet hands the turn
to the other side.

## Link layer

### Connection handshake

The client sends three bytes and the server answers each one:

| Client | Server |
|--------|--------|
| `aa`   | `00`   |
| `55`   | `ff`   |
| `5a`   | `11`   |

INTERLNK also sends a lone `aa` when it loads. If nothing answers, it tries
again on the first access to a redirected drive. Both captures connect that
way.

`ruby-interlnk` follows the handshake with a baud-rate negotiation (`ff`,
then a speed index). That negotiation does not appear at 9600 baud (see open
questions).

### Packet transfer

The sender of a packet does this:

1. It sends a **flags** byte `F`.
2. The receiver acknowledges with `~F & 0xFB`.
3. It sends the **frame**.
4. The receiver echoes `F` once the frame's CRC has checked out.

Flags byte:

| Bits | Meaning |
|------|---------|
| 7 (`0x80`) | set: the frame has a 1-byte length; clear: a 2-byte length |
| 6 (`0x40`) | more packets of the same reply follow (multi-block reads) |
| 2–5 | always 0 in the captures |
| 0–1 | sequence number, modulo 4 |

Frame:

| Field | Size | Notes |
|-------|------|-------|
| length | 1 or 2 | data length; 2-byte lengths are little-endian |
| data | length | the application payload |
| CRC | 2 | high byte first |

The CRC is CRC-16/GENIBUS: poly `0x1021`, init `0xFFFF`, not reflected,
output complemented (`xorout 0xFFFF`), check value `0xD64E`. It covers the
length bytes and the data. Feeding a whole frame, CRC included, through the
same CRC register (without the final complement) leaves `0x1D0F`.

Requests and short replies use 1-byte lengths. Sector data uses 2-byte
lengths. A packet carries at most 8192 bytes, which is the `max_block` both
sides announce in SERVER-INFO (see Commands). Larger replies are split into
8192-byte packets with flag bit 6 set on all but the last.

### Turnaround and sequence numbers

After finishing what it has to send, a side hands the link over by sending a
bare sequence number `0`–`3`, the **turnaround**. The other side then does
one of two things:

- starts a packet whose flags carry that same sequence number; or
- if it has nothing to send, **idles**.

Every packet and every turnaround uses the next sequence number (mod 4). The
captures have no breaks in this.

A request and its reply look like this, with sequence numbers in brackets:

```
C: flags 81 [1]   S: ack 7a   C: frame   S: echo 81
C: turnaround 02 [2]
S: flags 82 [2]   C: ack 79   S: frame   C: echo 82
S: turnaround 03 [3]
```

For a READ, the server sends its status packet and then the data packets
before turning around. Each of those packets has its own flags, ack and echo.

### Idle

With nothing to send after receiving turnaround `v`, a side sends
`0xFC | (v ^ 1)`, and the other side echoes it. The pair repeats until the
idling side sends flags for sequence `v` and starts a packet. Idle runs about
every 50 ms; a 5-second pause produced about 80 idle pairs.

## Application layer

Each request is one packet from the client, and its first byte is the
command. `unit` is the server's unit number: 0 = A:, 1 = B:, 2 = C:, and so
on. Multi-byte fields are little-endian.

The command numbers are INTERLNK's own. Some sit one above the matching DOS
device driver command (media check 1→`02`, build BPB 2→`03`), but read keeps
4, and generic IOCTL 0x13 becomes `1d`.

### Session start

Once connected, the client sends `SERVER-INFO` and `INIT` before any drive
access.

#### `00` SERVER-INFO

The client sends a 2-byte packet `00 00`, then a 34-byte request; the server
answers with 33 bytes.

Request (34 bytes):

| Offset | Size | Value seen | Meaning |
|--------|------|------------|---------|
| 0 | 6 | 0 | OS type, developer, product (per ruby-interlnk) |
| 6 | 2 | `0100` | client version 1.00 |
| 8 | 1 | 1 | "device driver" (the client side) |
| 9 | 1 | 0 | wanted features |
| 10 | 1 | 0 | checksum supported? |
| 11 | 1 | 1 | CRC supported? |
| 12 | 2 | 8192 | maximum serial block size |
| 14 | 12 | 0 | reserved |
| 26 | 4 | `b010ffd9` | client ID |
| 30 | 4 | 0 | last server ID |

Reply (33 bytes):

| Offset | Size | Value seen | Meaning |
|--------|------|------------|---------|
| 0 | 6 | 0 | OS type, developer, product |
| 6 | 2 | `010a` | server version 1.10 |
| 8 | 1 | 1 | "device server" |
| 9 | 1 | 0 | checksum? |
| 10 | 1 | 1 | CRC? |
| 11 | 2 | 8192 | maximum serial block size |
| 13 | 12 | 0 | reserved |
| 25 | 4 | 0 | last client ID |
| 29 | 4 | varies | server ID (different in each capture) |

#### `01` INIT

Request (36 bytes):

| Offset | Size | Value seen | Meaning |
|--------|------|------------|---------|
| 0 | 1 | `01` | command |
| 1 | 2 | `01 00` | client version major, minor (1.00) |
| 3 | 2 | `16 06` | DOS version minor, major (6.22) |
| 5 | 1 | 3 | drive letters reserved (`/DRIVES:3`) |
| 6 | 1 | 3 | first client unit (3 = D:) |
| 7 | 3 | `ff ff ff` | printer map |
| 10 | 26 | all `fe` | drive map (`fe` = none) |

Reply (87 bytes):

| Offset | Size | Value seen | Meaning |
|--------|------|------------|---------|
| 0 | 2 | `01 0a` | server version major, minor (1.10) |
| 2 | 2 | `16 06` | DOS version minor, major |
| 4 | 1 | 3 | drive count (echoes the request?) |
| 5 | 3 | `ff ff ff` | printer map |
| 8 | 26 | `02 01 fe …` | drive map: entry *i* is the server unit for client drive *i* (D: → server C:, E: → server B:) |
| 34 | 52 | `08c2` ×3, then 0 | 26 attribute words (see below) |
| 86 | 1 | 0 | multitasking flag |

`0x08C2` matches a DOS block device attribute word. Its bits mean
open/close/removable supported (`0x0800`), IOCTL queries (`0x0080`), generic
IOCTL (`0x0040`), and 32-bit sector numbers (`0x0002`). The 32-bit bit fits
the READ addressing below.

### Drive access

#### `02` MEDIA-CHECK

Request: `02 unit media`, where `media` is the media descriptor the client
last saw (`00` before the first BPB).

Reply (19 bytes):

| Offset | Size | Meaning |
|--------|------|---------|
| 0 | 2 | status (see below) |
| 2 | 1 | `01` unchanged, `ff` changed, `00` don't know |
| 3 | 12 | volume label, NUL padded |
| 15 | 4 | volume serial number |

The first check of the fixture floppy answered "changed" with an empty label
and serial. Later checks answered "unchanged" with `FIXTURE`. The client
sends a MEDIA-CHECK before almost every access.

#### `03` BUILD-BPB

Request: `03 unit media`. The reply is 33 bytes: a status word, the 25-byte
DOS 4 BPB, and 6 more bytes.

| Field | C: (FAT16) | B: (FAT12) |
|-------|------------|------------|
| bytes/sector | 512 | 512 |
| sectors/cluster | 4 | 1 |
| reserved | 1 | 1 |
| FATs | 2 | 2 |
| root entries | 512 | 224 |
| sectors | 64449 | 2880 |
| media | `f8` | `f0` |
| sectors/FAT | 63 | 9 |
| sectors/track | 63 | 18 |
| heads | 16 | 2 |
| hidden sectors | 63 | 0 |
| extra 6 bytes | `40 00 00 05 21 00` | `00 00 00 07 22 00` |

The extra bytes are a guess: `05` and `07` match DOS device types (fixed
disk, 3.5" 1.44MB), and the rest is unknown.

#### `04` READ

Request (11 bytes):

| Offset | Size | Meaning |
|--------|------|---------|
| 0 | 1 | `04` |
| 1 | 1 | server unit |
| 2 | 1 | media descriptor |
| 3 | 2 | sector count |
| 5 | 2 | start sector, or `ffff` to use the 32-bit field |
| 7 | 4 | 32-bit start sector |

Every READ in the captures uses `ffff` plus the 32-bit field. The start
sector is relative to the volume: sector 0 is the volume's boot sector. For
the server's C: that is sector 63 of the disk, the first sector of the
partition. The largest single request is 127 sectors.

The server first sends a 4-byte status packet (status word, then the
sector count transferred). The data follows in packets of at most 8192 bytes,
with 2-byte lengths. For example, a 127-sector read came back as seven 8192-byte
packets and one of 7680 bytes.

#### `1d` GENERIC-IOCTL

Request: `1d unit category function`. Only category `08`, function `66`
(Get Media ID) appears. The server sends a 2-byte status packet, then 25
bytes:

| Offset | Size | Meaning |
|--------|------|---------|
| 0 | 2 | info level (`601d` seen, should be 0; probably uninitialised) |
| 2 | 4 | volume serial number |
| 6 | 11 | volume label from the boot sector |
| 17 | 8 | file system type (`FAT16   `, `FAT12   `) |

### Status word

Replies start with a 16-bit status word. READ and GENERIC-IOCTL return
`0100`, the DOS "done" bit. MEDIA-CHECK and BUILD-BPB return `01xx`, where
the low byte was `01`, `02` or `09`. DOS ignores the low byte unless the
error bit (`0x8000`) is set, so these are probably left over from the server
(see open questions).

## Consequences for Interloper

- The server is a **sector server**. A backend needs a BPB, media
  descriptor, volume label and serial, and sector reads by volume-relative
  number. FAT knowledge is needed only to fill in those metadata fields from
  the boot sector.
- A hard-disk image is served as its **partition**, not as a whole disk:
  sector 0 of the protocol is the partition's boot sector. A whole-disk image
  therefore needs a partition offset. A bare volume image (like
  `fixture.img`) is served from offset 0.
- Reads arrive as 32-bit sector numbers, so volumes over 32MB should not need
  special handling. This is unverified.

## Open questions

- Writes: command number, request layout, and how the server reports
  write-protect. No write is captured yet.
- The baud negotiation `ruby-interlnk` performs at 115200 is absent at 9600.
  Is it only sent when changing speed?
- What the low byte of the `01xx` status word in MEDIA-CHECK and BUILD-BPB
  replies means.
- The 6 bytes after the BPB in BUILD-BPB replies.
- The 2-byte `00 00` packet before the SERVER-INFO request: is it a command
  header, or a separate announcement?
- The client ID was `b010ffd9` in both captures, even though they came from
  different builds; the server ID changed. How are they derived, and does
  the server check them on reconnect?
- Error replies: a missing sector, a bad unit, or a CRC failure. What does a
  receiver send instead of the echo (a NAK)?
- Disconnect and reconnect behaviour.
- INIT field meanings beyond the drive map (drive count, attribute table,
  multitasking flag).
- Other commands INTERLNK can send: open/close, removable-media check,
  get/set logical drive, printer redirection.
