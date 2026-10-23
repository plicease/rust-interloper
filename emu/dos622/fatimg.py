#!/usr/bin/env python3
"""Write a file into a FAT12/FAT16 volume inside a disk image, in place.

    fatimg.py IMAGE[@SECTOR] DOSPATH SRC

The volume starts at SECTOR (default 0; 63 for the first partition of the
VM disks). DOSPATH is an 8.3 path such as AUTOEXEC.BAT or DOS\\X.BAT whose
directories must already exist. An existing file is replaced and its old
clusters freed. Long file names are not supported.
"""

import argparse
import struct
import sys
from pathlib import Path


class Volume:
    def __init__(self, f, offset):
        self.f, self.offset = f, offset
        bs = self.read(0, 512)
        (self.bps, self.spc, reserved, self.fats, root_entries, total16,
         _, fatsz, _, _, _, total32) = struct.unpack_from("<HBHBHHBHHHII", bs, 11)
        total = total16 or total32
        self.fat_start = reserved * self.bps
        self.fat_bytes = fatsz * self.bps
        self.root_start = self.fat_start + self.fats * self.fat_bytes
        self.root_bytes = root_entries * 32
        self.data_start = self.root_start + -(-self.root_bytes // self.bps) * self.bps
        self.cluster_bytes = self.spc * self.bps
        self.clusters = (total * self.bps - self.data_start) // self.cluster_bytes
        self.fat16 = self.clusters >= 4085
        self.fat = bytearray(self.read(self.fat_start, self.fat_bytes))

    def read(self, pos, n):
        self.f.seek(self.offset + pos)
        return self.f.read(n)

    def write(self, pos, data):
        self.f.seek(self.offset + pos)
        self.f.write(data)

    def get(self, n):
        if self.fat16:
            return struct.unpack_from("<H", self.fat, n * 2)[0]
        v = struct.unpack_from("<H", self.fat, n * 3 // 2)[0]
        return v >> 4 if n & 1 else v & 0xFFF

    def set(self, n, value):
        if self.fat16:
            struct.pack_into("<H", self.fat, n * 2, value)
            return
        pos = n * 3 // 2
        v = struct.unpack_from("<H", self.fat, pos)[0]
        v = (v & 0x000F) | value << 4 if n & 1 else (v & 0xF000) | value
        struct.pack_into("<H", self.fat, pos, v)

    def end(self):
        return 0xFFFF if self.fat16 else 0xFFF

    def chain(self, first):
        n = first
        while 2 <= n < self.clusters + 2:
            yield n
            n = self.get(n)

    def cluster_pos(self, n):
        return self.data_start + (n - 2) * self.cluster_bytes

    def dir_slots(self, first):
        """Byte positions of every 32-byte entry in a directory (0 = root)."""
        if first == 0:
            return range(self.root_start, self.root_start + self.root_bytes, 32)
        return [self.cluster_pos(n) + i
                for n in self.chain(first) for i in range(0, self.cluster_bytes, 32)]

    def lookup(self, first, name):
        """Return (slot of name or None, first free slot or None)."""
        free = None
        for pos in self.dir_slots(first):
            entry = self.read(pos, 32)
            if entry[0] == 0:
                return None, free if free is not None else pos
            if entry[0] == 0xE5:
                free = pos if free is None else free
            elif entry[:11] == name and not entry[11] & 0x08:
                return pos, free
        return None, free

    def put(self, path, data):
        parts = [short_name(p) for p in path.replace("/", "\\").split("\\") if p]
        first = 0
        for part in parts[:-1]:
            pos, _ = self.lookup(first, part)
            if pos is None or not self.read(pos + 11, 1)[0] & 0x10:
                raise FileNotFoundError(f"no directory {path}")
            first = struct.unpack("<H", self.read(pos + 26, 2))[0]
        pos, free = self.lookup(first, parts[-1])
        if pos is None:
            if free is None:
                raise OSError(f"directory full for {path}")
            pos = free
            attr, time, date = 0x20, 0, (2026 - 1980) << 9 | 1 << 5 | 1
        else:
            old = self.read(pos, 32)
            attr, time, date = old[11], *struct.unpack_from("<HH", old, 22)
            for n in list(self.chain(struct.unpack_from("<H", old, 26)[0])):
                self.set(n, 0)

        need = -(-len(data) // self.cluster_bytes)
        free_clusters = (n for n in range(2, self.clusters + 2) if self.get(n) == 0)
        clusters = [next(free_clusters, None) for _ in range(need)]
        if None in clusters:
            raise OSError("volume full")
        for i, n in enumerate(clusters):
            self.set(n, clusters[i + 1] if i + 1 < need else self.end())
            chunk = data[i * self.cluster_bytes:(i + 1) * self.cluster_bytes]
            self.write(self.cluster_pos(n), chunk.ljust(self.cluster_bytes, b"\0"))

        entry = parts[-1] + struct.pack("<B10xHHHI", attr, time, date,
                                        clusters[0] if clusters else 0, len(data))
        self.write(pos, entry)
        for i in range(self.fats):
            self.write(self.fat_start + i * self.fat_bytes, self.fat)


def short_name(part):
    base, _, ext = part.upper().partition(".")
    if not 1 <= len(base) <= 8 or len(ext) > 3:
        raise ValueError(f"not an 8.3 name: {part}")
    return (base.ljust(8) + ext.ljust(3)).encode("ascii")


def put(image, path, data, sector=0):
    with open(image, "r+b") as f:
        Volume(f, sector * 512).put(path, data)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("image", help="disk image, optionally IMAGE@SECTOR")
    p.add_argument("dospath")
    p.add_argument("src", type=Path)
    args = p.parse_args()
    image, _, sector = args.image.partition("@")
    try:
        put(image, args.dospath, args.src.read_bytes(), int(sector or 0))
    except (OSError, ValueError) as e:
        sys.exit(str(e))


if __name__ == "__main__":
    main()
