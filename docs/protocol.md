# INTERLNK/INTERSVR serial protocol

**Status:** working notes, updated as captures are decoded. Everything here
comes from captures of the genuine MS-DOS 6.22 programs
(`emu/dos622/captures/`) unless marked otherwise. Guesses are labelled as
such, and open questions are listed at the end.

## Sources

- Captures: `emu/dos622/captures/*.log`, between INTERLNK 1.00 (client) and
  INTERSVR 1.10 (server): reads, writes, reconnects at 9600 baud, and a speed
  change to 115200. The log format and the list of captures are in that
  directory's README.
- Decoder: `emu/dos622/decode.py` applies the rules below to a capture. It
  reports any byte that breaks them as an anomaly. With `--image` it checks
  READ and WRITE data against the server's disk images. The captures decode
  with no anomalies, apart from the client's unanswered idle bytes while
  the server was restarting. All 520 READs and 139 written sectors match the
  images, apart from two sectors read before they were rewritten.
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

INTERLNK also sends a lone `aa` when it loads. If the server answers, it
connects there and then, and fetches DRIVE-INFO for its load-time drive table
(`reconnect-9600.log`, second boot). If nothing answers, it tries again on
the first access to a redirected drive (both `dir-copy` captures).

### Reconnection

There is no disconnect message. Either side may vanish and come back, and
the client simply starts again with the handshake, SERVER-INFO and INIT:

- **Client restarted.** The client had been sending idle bytes for the
  server to echo. When it stopped, the line went quiet, and the server sent
  nothing on its own. When the new client loaded 22 s later, the server
  answered its handshake straight away and ran a normal session start.
- **Server restarted.** The client sent idle `fc` about every 55 ms for
  0.6 s with no echo, then stopped. Nothing more was sent until the next
  drive access 44 s later, which began with a fresh handshake, SERVER-INFO
  and INIT. The client's DOS saw no error. The client then repeated
  MEDIA-CHECK and BUILD-BPB before reading again.

### Speed change

The handshake runs at 9600 baud. If both sides allow a higher speed, the
client then steps up one speed at a time (`baud-default.log`, where neither
side has `/BAUD`, so both allow 115200). Each step is a series of byte
pairs, the client's byte first:

| Client | Server | Notes |
|--------|--------|-------|
| `ff` | `00` | start of a speed change |
| `k` | `k << 4` | speed index `k` |
| `k << 4` | `~(k << 4)` | |
| 15 bytes | 15 bytes | fixed test pattern, below |

Test pattern, client: `ff 00 5a 55 aa f0 0f e7 7e c3 3c 81 18 00 ff`;
server: `00 ff a5 aa 55 f0 0f ff 00 18 81 3c c3 7e e7`.

| Index | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|-------|---|---|---|---|---|---|---|---|
| Baud | 1200 | 2400 | 4800 | 9600 | 19200 | 38400 | 57600 | 115200 |

The capture shows steps to 4, 5, 6 and 7, all passing, with the first
packet straight after. With `/BAUD:9600` on both sides there is no speed
change at all. The relay carries bytes whatever the emulated UART speed is,
so the capture cannot show the moment each side switches, or what happens
when the test pattern fails. The index table and the pattern agree with
`ruby-interlnk`.

### Packet transfer

The sender of a packet does this:

1. It sends a **flags** byte `F`.
2. The receiver acknowledges with `~F ^ 0x04` (the complement of `F` with
   bit 2 flipped back).
3. It sends the **frame**.
4. The receiver echoes `F` once the frame's CRC has checked out.

Flags byte:

| Bits | Meaning |
|------|---------|
| 7 (`0x80`) | set: the frame has a 1-byte length; clear: a 2-byte length |
| 6 (`0x40`) | more packets of the same reply follow (multi-block reads) |
| 3–5 | always 0 in the captures |
| 2 (`0x04`) | echo mode, see below |
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

### Lost packets and echo mode

If the flags echo does not come, the sender waits about 0.57 s and sends the
packet again with the same sequence number and flag bit 2 set. In this
**echo mode** the frame goes one byte at a time: the receiver echoes each
byte straight back, except the last, which it answers with the flags echo.
For example, the client's `00 00` SERVER-INFO announcement:

```
C: 80  S: 7b  C: 02 00 00 5d 03       (no echo: lost)
C: 84  S: 7f  C: 02  S: 02  C: 00  S: 00  C: 00  S: 00  C: 5d  S: 5d  C: 03  S: 84
```

At 115200 baud in the emulator, the first packet in each direction was lost
(the 8 MHz 286 probably cannot keep up), and both sides then used echo mode
for every packet for the rest of the session. Each side only uses echo mode
for packets it sends, after one of its own packets was lost. No packet was
lost at 9600.

### Turnaround and sequence numbers

After finishing what it has to send, a side hands the link over by sending a
bare sequence number `0`–`3`, the **turnaround**. The other side then does
one of two things:

- starts a packet whose flags carry that same sequence number; or
- if it has nothing to send, **idles**.

Every packet uses the next sequence number (mod 4), whichever side sends
it. A turnaround carries the number the next packet will use, without using
it up. The captures have no breaks in this.

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
4, write 8 becomes `05`, and generic IOCTL 0x13 becomes `1d`.

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
| 26 | 4 | varies | client ID |
| 30 | 4 | 0 or ID | server ID from this client's previous session |

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
| 25 | 4 | 0 or ID | client ID from this server's previous session |
| 29 | 4 | varies | server ID |

Each program picks its ID when it starts and keeps it for its lifetime, and
each side reports the ID it last saw from the other. After a client restart
the server's reply carried the old client's ID; after a server restart the
client's request carried the old server's ID. Neither side refused a
mismatch, so the IDs seem to be only for spotting a reconnect. The IDs look
random: client `b010ffd9` (four first boots), `1814b9dd` and `9c1fbfdf`;
server `773e6970`, `773a9434`, `773d419b`, `773d55dd` and `6a2931e7`.

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
| 4 | 1 | 3 | server drive count: units 0 to 2 (A: to C:) |
| 5 | 3 | `ff ff ff` | printer map |
| 8 | 26 | `02 01 fe …` | drive map: entry *i* is the server unit for client drive *i* (D: → server C:, E: → server B:) |
| 34 | 52 | `08c2` ×3, then 0 | attribute word for each server unit (see below) |
| 86 | 1 | 0 | multitasking flag |

The drive count and the attribute table both describe the **server's**
units. INTERSVR's server had A:, B: and C:, so both say three, which happens
to equal the client's `/DRIVES:3`. With Interloper serving units 2 and 3,
INTERLNK sent no request at all to unit 3 while the reply had attribute
words at indexes 0 to 2 and a count of 3. Moving the attribute words to
indexes 2 and 3 was not enough on its own; with the count also raised to 4,
the drive worked. Whether the attribute words matter once the count is
right has not been tested.

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

#### `05` WRITE

The request has the same 11-byte layout as READ, with command `05`. The
client sends the sector data straight after the request, before turning
around, in the same packets of at most 8192 bytes: a 127-sector write was
seven 8192-byte packets and one of 7680. The server then answers with a
4-byte status packet (status word, sectors written) and nothing else.

A write to the server's write-protected B: was refused with status `8100`
and a count of 0: the error bit, the done bit, and DOS error code 0, "write
protect violation". The client's DOS then raised its usual critical error.
The reply came 0.6 s after the request, the same as a successful one-sector
write, so the server does not appear to retry.

DOS writes FAT copies itself, so creating or deleting a file is a series of
one-sector WRITEs to the directory and both FATs (for example sectors 127, 9
and 72 on the server's C:, where the second FAT starts 63 sectors after the
first). No other command appears for file creation, deletion or `MD`.

#### `1c` DRIVE-INFO

Request: `1c unit`. Sent only when INTERLNK connects at load, once per
mapped drive, to fill in its "D: equals C: (32Mb) DOS622" table. The reply
is 23 bytes: a NUL-terminated size string in a 10-byte field, then a
NUL-terminated volume label in a 13-byte field.

| Unit | Reply |
|------|-------|
| 2 (C:) | `(32Mb)`, `DOS622` |
| 1 (B:, floppy) | both empty |

The floppy reply has only the first byte of each field zeroed. The rest is
the hard disk's reply left in the buffer, so the server seems to leave these
fields empty for removable drives rather than touch the disk.

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

Replies start with a 16-bit status word, which matches the status field of
a DOS device driver request header: `0x8000` is the error bit, `0x0100` is
"done", and the low byte is a DOS device error code when the error bit is
set. READ, WRITE and GENERIC-IOCTL return `0100` on success, and a refused
write returned `8100` (error 0, write protect). MEDIA-CHECK and BUILD-BPB
return `01xx`, where the low byte was `01`, `02` or `09`. DOS ignores the low
byte unless the error bit is set, so these are probably left over in a
register on the server.

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
- The server must answer a fresh handshake at any time, since a client that
  restarts or loses the server just starts again. Remembering the last
  client ID is optional, as INTERLNK accepts anything there.
- With no `/BAUD` limit, the client changes speed right after the handshake.
  The server has to follow the speed change and must handle echo-mode
  packets, which the client switches to after any lost packet.
- DRIVE-INFO needs a size string and a label for each drive.
- INIT's drive count and attribute table must cover every unit served, or
  INTERLNK never uses the higher units.
- A sender must allow for a frame's time on the line before deciding its
  echo is lost. Resending too early (Interloper did, on a pseudo-terminal
  where writes return at once) confuses the client, which has already
  received the frame.
- A read-only server can refuse every WRITE with status `8100` and count 0,
  after taking the data packets that follow the request. The client then
  shows "Write protect error", exactly as for INTERSVR with a write-protected
  disk.

## Open questions

- Whether `06` is a write with verify, as DOS has separate device commands
  for the two. Only `05` has been seen.
- Speed change: when each side switches its UART, and what the client does
  when the test pattern fails at some speed. This needs real serial ports
  or an emulated link that enforces the line speed.
- Whether echo mode is ever switched off again within a session.
- The 6 bytes after the BPB in BUILD-BPB replies.
- The 2-byte `00 00` packet before the SERVER-INFO request: is it a command
  header, or a separate announcement?
- How the client and server IDs are derived. The first client boot always
  had `b010ffd9`.
- Error replies: a missing sector or a bad unit. A lost frame is resent
  after a timeout, but whether a receiver can reject a frame with a bad CRC
  sooner (a NAK) is unknown.
- INIT field meanings beyond the drive map (drive count, attribute table,
  multitasking flag).
- Other commands INTERLNK can send: open/close, removable-media check,
  get/set logical drive, printer redirection.
