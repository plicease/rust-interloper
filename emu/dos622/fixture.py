#!/usr/bin/env python3
"""Write the fixture floppy image for testing on real hardware.

    fixture.py OUT [--md5sum EXE]

Writes OUT/fixture.img, the 1.44MB FAT12 image the VMs share as B:, and
OUT/FIXTURE.SHA256 with the SHA-256 of each file in it. With --md5sum, the
image's root also gets EXE as MD5SUM.EXE (a DOS md5sum, such as md5sum16)
and MD5SUM, the MD5 of each root file, so that after COPY X:\\*.* the
copies can be checked on the DOS machine with MD5SUM -c MD5SUM.
"""

import argparse
import hashlib
import sys
from pathlib import Path

import build
import fatimg


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("out", type=Path, help="directory to write into")
    p.add_argument("--md5sum", type=Path, metavar="EXE",
                   help="DOS md5sum to add, with an MD5SUM check file")
    args = p.parse_args()

    image = args.out / "fixture.img"
    if image.exists():
        sys.exit(f"{image} exists; choose a new directory")
    args.out.mkdir(parents=True, exist_ok=True)
    files = {name: data for name, data in build.fixture_files().items() if data is not None}
    build.fat12_floppy(image, build.fixture_files(), label="FIXTURE",
                       readonly=["READONLY.TXT"])
    added = {}
    if args.md5sum:
        added["MD5SUM.EXE"] = args.md5sum.read_bytes()
        root = {name: data for name, data in files.items() if "/" not in name}
        root.update(added)
        added["MD5SUM"] = "".join(f"{hashlib.md5(data).hexdigest()}  {name}\r\n"
                                  for name, data in sorted(root.items())).encode()
        for name, data in added.items():
            fatimg.put(image, name, data)
    (args.out / "FIXTURE.SHA256").write_text("".join(
        f"{hashlib.sha256(data).hexdigest()}  {name}\n"
        for name, data in sorted(files.items()) + list(added.items())))


if __name__ == "__main__":
    main()
