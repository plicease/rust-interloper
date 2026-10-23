#!/usr/bin/env python3
"""Decode a sniff.py capture log into INTERLNK protocol events.

    decode.py CAPTURE.log [--link] [--hex] [--image UNIT=PATH[@SECTOR] ...]

Consecutive bytes in the same direction are joined, then split into packets
(length, data, CRC-16 that validates) and single control bytes. Each control
byte is given its link-layer role, and any byte that breaks the rules in
docs/protocol.md is reported as an anomaly. Packets are decoded as requests
and replies. The exit status is non-zero if there are anomalies or, with
--image, READ data that does not match the image.
"""

import argparse
import sys
from dataclasses import dataclass, field


# --- CRC ----------------------------------------------------------------------

def _crc_table():
    table = []
    for i in range(256):
        crc = i << 8
        for _ in range(8):
            crc = (crc << 1) ^ 0x1021 if crc & 0x8000 else crc << 1
        table.append(crc & 0xFFFF)
    return table


CRC_TABLE = _crc_table()


def crc16(data, crc=0xFFFF):
    """CRC-16/CCITT (poly 0x1021, init 0xFFFF), MSB first."""
    for byte in data:
        crc = ((crc << 8) & 0xFF00) ^ CRC_TABLE[(crc >> 8) ^ byte]
    return crc


def crc_ok(frame):
    """A frame is its length bytes, data, and the complemented CRC, high byte first."""
    return len(frame) >= 3 and crc16(frame[:-2]) ^ 0xFFFF == frame[-2] << 8 | frame[-1]


# --- capture log --------------------------------------------------------------

@dataclass
class Run:
    time: float
    direction: str      # "C>S", "S>C" or "MARK"
    data: bytearray = field(default_factory=bytearray)
    times: list = field(default_factory=list)   # arrival time of each byte
    text: str = ""


def read_runs(path):
    """Join consecutive chunks in the same direction."""
    runs = []
    with open(path) as f:
        for line in f:
            parts = line.split()
            if not parts:
                continue
            time, kind = float(parts[0]), parts[1]
            if kind == "MARK":
                runs.append(Run(time, "MARK", text=" ".join(parts[2:])))
                continue
            data = bytes.fromhex("".join(parts[3:]))
            if not (runs and runs[-1].direction == kind):
                runs.append(Run(time, kind))
            runs[-1].data += data
            runs[-1].times += [time] * len(data)
    return runs


# --- framing ------------------------------------------------------------------

@dataclass
class Token:
    time: float
    direction: str
    kind: str           # "ctl", "pkt" or "mark"
    data: bytes = b""   # control byte, or packet payload without length/CRC
    wide: bool = False  # packet used a 16-bit length
    text: str = ""


def tokens(runs):
    for run in runs:
        if run.direction == "MARK":
            yield Token(run.time, "MARK", "mark", text=run.text)
            continue
        b, pos = run.data, 0
        while pos < len(b):
            at = run.times[pos]
            n = b[pos]
            if pos + 3 + n <= len(b) and crc_ok(b[pos:pos + 3 + n]):
                yield Token(at, run.direction, "pkt", bytes(b[pos + 1:pos + 1 + n]))
                pos += 3 + n
                continue
            if pos + 1 < len(b):
                n = b[pos] | b[pos + 1] << 8
                if pos + 4 + n <= len(b) and crc_ok(b[pos:pos + 4 + n]):
                    yield Token(at, run.direction, "pkt",
                                bytes(b[pos + 2:pos + 2 + n]), wide=True)
                    pos += 4 + n
                    continue
            yield Token(at, run.direction, "ctl", bytes(b[pos:pos + 1]))
            pos += 1


# --- link layer ---------------------------------------------------------------

HELLO = [("C>S", 0xAA), ("S>C", 0x00), ("C>S", 0x55),
         ("S>C", 0xFF), ("C>S", 0x5A), ("S>C", 0x11)]

BAUD = [1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200]
SPEED_TEST = list(zip(
    [0xFF, 0x00, 0x5A, 0x55, 0xAA, 0xF0, 0x0F, 0xE7, 0x7E, 0xC3, 0x3C, 0x81, 0x18, 0x00, 0xFF],
    [0x00, 0xFF, 0xA5, 0xAA, 0x55, 0xF0, 0x0F, 0xFF, 0x00, 0x18, 0x81, 0x3C, 0xC3, 0x7E, 0xE7]))


def speed_change(k):
    """Control bytes of a change to speed index k, as (direction, value)."""
    pairs = [(0xFF, 0x00), (k, k << 4), (k << 4, ~(k << 4) & 0xFF)] + SPEED_TEST
    return [x for c, s in pairs for x in (("C>S", c), ("S>C", s))]


def ack_of(flags):
    return ~flags & 0xFF ^ 0x04


def frame_length(frame, byte_length):
    """Total length of a frame from its first bytes, or None if not known yet."""
    if byte_length:
        return frame[0] + 3 if frame else None
    return frame[0] + (frame[1] << 8) + 4 if len(frame) >= 2 else None


@dataclass
class Event:
    time: float
    kind: str           # hello, probe, speed, idle, turn, packet, lost, mark, anomaly
    direction: str = ""
    flags: int = 0
    data: bytes = b""
    count: int = 1
    text: str = ""


def events(toks):
    """Assign every control byte a role, checking it against the link rules."""
    toks = list(toks)
    marks = [Event(t.time, "mark", text=t.text) for t in toks if t.kind == "mark"]
    seq = [t for t in toks if t.kind != "mark"]
    out, i, n = [], 0, len(seq)

    def ctl(j, direction=None, value=None):
        return (j < n and seq[j].kind == "ctl"
                and (direction is None or seq[j].direction == direction)
                and (value is None or seq[j].data[0] == value))

    def echoed_frame(j, sender, receiver, flags):
        """A frame sent one byte at a time from j, each byte echoed by the
        receiver except the last, which is answered by the flags echo.
        Returns (frame, index after the flags echo) or None."""
        frame = bytearray()
        while ctl(j, sender):
            frame.append(seq[j].data[0])
            if len(frame) == frame_length(frame, flags & 0x80):
                if ctl(j + 1, receiver, flags) and crc_ok(frame):
                    return frame, j + 2
                return None
            if not ctl(j + 1, receiver, frame[-1]):
                return None
            j += 2
        return None

    while i < n:
        t = seq[i]
        if all(ctl(i + k, d, v) for k, (d, v) in enumerate(HELLO)):
            out.append(Event(t.time, "hello"))
            i += len(HELLO)
            continue
        if ctl(i, "C>S", 0xFF) and ctl(i + 2, "C>S") and seq[i + 2].data[0] < len(BAUD):
            change = speed_change(seq[i + 2].data[0])
            if all(ctl(i + k, d, v) for k, (d, v) in enumerate(change)):
                out.append(Event(t.time, "speed", flags=seq[i + 2].data[0]))
                i += len(change)
                continue
        if t.kind == "ctl":
            v = t.data[0]
            other = "S>C" if t.direction == "C>S" else "C>S"
            if ctl(i + 1, other):
                w = seq[i + 1].data[0]
                if w == ack_of(v) and v & 0x04:
                    echoed = echoed_frame(i + 2, t.direction, other, v)
                    if echoed:
                        frame, i = echoed
                        data = frame[1:-2] if v & 0x80 else frame[2:-2]
                        out.append(Event(t.time, "packet", t.direction, v, bytes(data)))
                        continue
                if w == ack_of(v):
                    pkt = seq[i + 2] if i + 2 < n else None
                    if (pkt and pkt.kind == "pkt" and pkt.direction == t.direction
                            and ctl(i + 3, other, v)):
                        out.append(Event(t.time, "packet", t.direction, v, pkt.data))
                        if bool(v & 0x80) == pkt.wide:
                            out.append(Event(t.time, "anomaly", t.direction, v,
                                             text="length width does not match flags"))
                        i += 4
                        continue
                    if pkt and pkt.kind == "pkt" and pkt.direction == t.direction:
                        # The receiver lost the frame; the sender resends it.
                        out.append(Event(t.time, "lost", t.direction, v, pkt.data))
                        i += 3
                        continue
                    out.append(Event(t.time, "anomaly", t.direction, v,
                                     text="flags/ack without packet and echo"))
                    i += 2
                    continue
                if v & 0xFC == 0xFC and w == v:
                    if out and out[-1].kind == "idle" and out[-1].flags == v:
                        out[-1].count += 1
                    else:
                        out.append(Event(t.time, "idle", t.direction, v))
                    i += 2
                    continue
                # A turnaround hands over seq v: the peer answers with flags
                # for seq v, or idles with 0xFC | (v ^ 1) if it has nothing.
                if v <= 3 and (w & 3 == v or w == 0xFC | (v ^ 1)):
                    out.append(Event(t.time, "turn", t.direction, v))
                    i += 1
                    continue
            if v <= 3 and i + 1 == n:
                out.append(Event(t.time, "turn", t.direction, v, text="unanswered"))
                i += 1
                continue
            if t.direction == "C>S" and v == 0xAA:
                out.append(Event(t.time, "probe", t.direction, v))
                i += 1
                continue
        out.append(Event(t.time, "anomaly", t.direction,
                         data=t.data, text=f"unexpected {t.kind}"))
        i += 1
    return sorted(out + marks, key=lambda e: e.time)


# --- application layer --------------------------------------------------------

def u16(d, o):
    return int.from_bytes(d[o:o + 2], "little")


def u32(d, o):
    return int.from_bytes(d[o:o + 4], "little")


def cstr(d):
    return d.split(b"\0", 1)[0].decode("ascii", "replace")


def drive_map(d):
    """Client drive index -> server unit, skipping unused (0xFE) entries."""
    return " ".join(f"{i}->{b}" for i, b in enumerate(d) if b != 0xFE) or "none"


def bpb(d):
    return (f"bps={u16(d, 0)} spc={d[2]} res={u16(d, 3)} fats={d[5]} root={u16(d, 6)} "
            f"sectors={u16(d, 8) or u32(d, 21)} media={d[10]:02x} fatsz={u16(d, 11)} "
            f"spt={u16(d, 13)} heads={u16(d, 15)} hidden={u32(d, 17)}")


def describe_request(d):
    cmd = d[0]
    if cmd == 0x00 and len(d) == 2:
        return "SERVER-INFO (announce)"
    if cmd == 0x00 and len(d) == 34:
        return (f"SERVER-INFO request version={d[7]}.{d[6]:02} driver={d[8]} "
                f"crc={d[11]} max_block={u16(d, 12)} client_id={u32(d, 26):08x} "
                f"last_server={u32(d, 30):08x}")
    if cmd == 0x01:
        return (f"INIT version={d[1]}.{d[2]:02} dos={d[4]}.{d[3]:02} drives={d[5]} "
                f"first_unit={d[6]} printers={d[7:10].hex()} map={drive_map(d[10:36])}")
    if cmd == 0x02:
        return f"MEDIA-CHECK unit={d[1]} media={d[2]:02x}"
    if cmd == 0x03:
        return f"BUILD-BPB unit={d[1]} media={d[2]:02x}"
    if cmd in (0x04, 0x05):
        name = "READ" if cmd == 0x04 else "WRITE"
        return f"{name} unit={d[1]} media={d[2]:02x} count={u16(d, 3)} start={sector_start(d)}"
    if cmd == 0x1C:
        return f"DRIVE-INFO unit={d[1]}"
    if cmd == 0x1D:
        return f"GENERIC-IOCTL unit={d[1]} category={d[2]:02x} function={d[3]:02x}"
    return f"command {cmd:02x} (unknown)"


def describe_reply(cmd, d, first):
    """Decode a server packet given the client command it answers."""
    if cmd == 0x00 and len(d) == 33:
        return (f"SERVER-INFO reply version={d[7]}.{d[6]:02} server={d[8]} "
                f"max_block={u16(d, 11)} last_client={u32(d, 25):08x} server_id={u32(d, 29):08x}")
    if cmd == 0x01 and len(d) == 87:
        attrs = " ".join(f"{u16(d, 34 + 2 * i):04x}" for i in range(26) if u16(d, 34 + 2 * i))
        return (f"INIT reply version={d[0]}.{d[1]:02} dos={d[3]}.{d[2]:02} drives={d[4]} "
                f"printers={d[5:8].hex()} map={drive_map(d[8:34])} attrs={attrs} "
                f"multitasking={d[86]}")
    if cmd == 0x02 and len(d) == 19:
        change = {0x01: "unchanged", 0xFF: "changed", 0x00: "unknown"}.get(d[2], f"{d[2]:02x}")
        return (f"MEDIA-CHECK reply status={u16(d, 0):04x} {change} "
                f"label={cstr(d[3:15])!r} serial={u32(d, 15):08x}")
    if cmd == 0x03 and len(d) == 33:
        return f"BUILD-BPB reply status={u16(d, 0):04x} {bpb(d[2:27])} extra={d[27:].hex(' ')}"
    if cmd in (0x04, 0x05) and first and len(d) == 4:
        name = "READ" if cmd == 0x04 else "WRITE"
        return f"{name} reply status={u16(d, 0):04x} count={u16(d, 2)}"
    if cmd == 0x04:
        return f"READ data {len(d)} bytes"
    if cmd == 0x1C and len(d) == 23:
        return f"DRIVE-INFO reply size={cstr(d[0:10])!r} label={cstr(d[10:23])!r}"
    if cmd == 0x1D and first and len(d) == 2:
        return f"GENERIC-IOCTL reply status={u16(d, 0):04x}"
    if cmd == 0x1D and len(d) == 25:
        return (f"GENERIC-IOCTL data level={u16(d, 0):04x} serial={u32(d, 2):08x} "
                f"label={d[6:17].decode('ascii', 'replace')!r} fs={d[17:25].decode('ascii', 'replace')!r}")
    return f"reply to {cmd:02x} ({len(d)} bytes)"


def sector_start(d):
    """Start sector of a READ or WRITE request: 16-bit, or 32-bit if that is ffff."""
    start = u16(d, 5)
    return u32(d, 7) if start == 0xFFFF else start


def flags_text(f):
    return (f"seq={f & 3}" + (" byte" if f & 0x80 else " word")
            + (" more" if f & 0x40 else "") + (" echo" if f & 0x04 else "")
            + (f" bits={f & 0x38:02x}" if f & 0x38 else ""))


# --- output -------------------------------------------------------------------

def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("log")
    p.add_argument("--hex", action="store_true", help="show packet bytes")
    p.add_argument("--link", action="store_true", help="show turnarounds and flags")
    p.add_argument("--image", action="append", default=[], metavar="UNIT=PATH[@SECTOR]",
                   help="check READ data, and the last WRITE to each sector, for server "
                        "UNIT against a disk image whose volume starts at SECTOR "
                        "(default 0); repeatable")
    args = p.parse_args()

    images = {}
    for spec in args.image:
        unit, _, path = spec.partition("=")
        path, _, first_sector = path.partition("@")
        images[int(unit)] = (open(path, "rb").read(), int(first_sector or 0))

    reads = {"checked": 0, "bad": 0}
    pending = None
    written = {}        # (unit, sector) -> last successfully written data
    write = None

    def check_read():
        """Compare a completed READ with the volume image for its unit."""
        if not pending or pending["unit"] not in images:
            return
        image, base = images[pending["unit"]]
        offset = (base + pending["start"]) * 512
        expected = image[offset:offset + pending["count"] * 512]
        reads["checked"] += 1
        if bytes(pending["data"]) != expected:
            reads["bad"] += 1
            print(f"# READ unit={pending['unit']} start={pending['start']} "
                  f"count={pending['count']} does NOT match the image", file=sys.stderr)

    cmd, first, anomalies = None, False, 0
    write_left = 0      # WRITE data bytes still to come from the client
    for e in events(tokens(read_runs(args.log))):
        at = f"{e.time:10.3f}"
        if e.kind == "mark":
            print(f"{at}  ==== {e.text}")
        elif e.kind == "hello":
            print(f"{at}  C<>S HELLO aa/00 55/ff 5a/11")
        elif e.kind == "speed":
            print(f"{at}  C<>S SPEED {e.flags} ({BAUD[e.flags]} baud), test pattern ok")
        elif e.kind == "lost":
            print(f"{at}  {e.direction}  packet not echoed, to be resent "
                  f"[{flags_text(e.flags)}] {e.data.hex(' ')}")
        elif e.kind == "probe":
            print(f"{at}  C>S  probe aa (no answer)")
        elif e.kind == "idle":
            print(f"{at}  C<>S idle {e.flags:02x} x{e.count}")
        elif e.kind == "turn":
            if args.link:
                print(f"{at}  {e.direction}  turn seq={e.flags}")
        elif e.kind == "anomaly":
            anomalies += 1
            print(f"{at}  {e.direction}  ANOMALY {e.text} {e.data.hex(' ')}")
        else:
            if e.direction == "C>S" and write_left > 0:
                write_left -= len(e.data)
                write["data"] += e.data
                text = f"WRITE data {len(e.data)} bytes"
            elif e.direction == "C>S":
                check_read()
                cmd, first, pending = e.data[0], True, None
                text = describe_request(e.data)
                if cmd == 0x04:
                    pending = {"unit": e.data[1], "start": sector_start(e.data),
                               "count": u16(e.data, 3), "data": bytearray()}
                if cmd == 0x05:
                    write_left = u16(e.data, 3) * 512
                    write = {"unit": e.data[1], "start": sector_start(e.data),
                             "data": bytearray()}
            else:
                text = describe_reply(cmd, e.data, first)
                if cmd == 0x05 and first and u16(e.data, 0) == 0x0100:
                    data = write["data"]
                    for i in range(u16(e.data, 2)):
                        written[write["unit"], write["start"] + i] = data[i * 512:(i + 1) * 512]
                if cmd == 0x04 and not first:
                    pending["data"] += e.data
                first = False
            link = f" [{flags_text(e.flags)}]" if args.link else ""
            print(f"{at}  {e.direction}  {text}{link}")
            if args.hex:
                print(f"{'':12}{e.data.hex(' ')}")
    check_read()
    bad_writes = 0
    checked_writes = [(unit, sector, data) for (unit, sector), data in sorted(written.items())
                      if unit in images]
    for unit, sector, data in checked_writes:
        image, base = images[unit]
        if image[(base + sector) * 512:(base + sector + 1) * 512] != data:
            bad_writes += 1
            print(f"# WRITE unit={unit} sector={sector} does NOT match the image",
                  file=sys.stderr)
    print(f"# {anomalies} anomalies", file=sys.stderr)
    if images:
        print(f"# {reads['checked']} reads checked, {reads['bad']} mismatched", file=sys.stderr)
        print(f"# {len(checked_writes)} written sectors checked, {bad_writes} mismatched",
              file=sys.stderr)
    return 1 if anomalies or reads["bad"] or bad_writes else 0


if __name__ == "__main__":
    sys.exit(main())
