# Protocol captures

Serial traffic between MS-DOS 6.22 `INTERLNK.EXE` (client) and
`INTERSVR.EXE` (server), recorded by `sniff.py` between the two 86Box VMs.
The file name ends with the line speed. The server shares its C: drive (FAT16, 32MB) as client D: and its
B: fixture floppy (FAT12, 1.44MB) as client E:.

Line format:

```
   17.156942 C>S    1 aa          seconds, direction, byte count, hex bytes
   67.336720 MARK COPY D:\...     step marker sent by the client on COM2
```

`C>S` is client to server and `S>C` is server to client. Chunk boundaries are
only where the PTY read happened to split the stream, not protocol packet
boundaries. While the link is idle both sides exchange `fc` or `fd` bytes.

## Decoding

`../decode.py` turns a log into protocol events (see `docs/protocol.md`):

```sh
emu/dos622/decode.py emu/dos622/captures/dir-copy-9600-marked.log
emu/dos622/decode.py LOG --link     # also show flags and turnarounds
emu/dos622/decode.py LOG --hex      # also show packet bytes
emu/dos622/decode.py LOG --image 2=SERVER/dos622.img@63 --image 1=SERVER/fixture.img
```

`--image` checks the data of every READ, and the last WRITE to each sector,
against the server's disk images. C: is the partition starting at sector 63
of `dos622.img`; B: is `fixture.img`. Every log here was checked against the
images of the VMs that produced them, and all READs matched, except in
`write-9600.log`, where two sectors were read before being rewritten. All
written sectors matched. Those VMs are not kept, and a fresh build has
different volume serial numbers, so its boot sectors will not match these
logs.

Every log decodes with no anomalies except `reconnect-9600.log`, where 12
idle bytes from the client go unanswered while the server is down.

## Files

| File | Notes |
|------|-------|
| `dir-copy-9600-marked.log` | `capture.py` run of `CAPTURE.BAT`, with `MARK` lines for each step. |
| `dir-copy-9600.log` | First capture of the same steps, without markers. The client ran them from AUTOEXEC.BAT with 5-second pauses that do not show up as quiet gaps. |
| `write-9600.log` | `write` scenario: WRITE requests, and a write refused with status `8100` (write protect). |
| `reconnect-9600.log` | `reconnect` scenario: client restart, connection at load with DRIVE-INFO, then server restart. `MARK HOST` lines show when `capture.py` stopped and started each VM. |
| `baud-default-115200.log` | `baud-default` scenario: no `/BAUD`, so the client steps the speed up to 115200; lost packets and echo mode. |

The two `dir-copy` sessions boot the client with the server already running,
so they start with INTERLNK's connection handshake, then run:

1. `DIR D:\FIXTURE`
2. `COPY D:\FIXTURE\README.TXT C:\CAP` (81 bytes)
3. `COPY D:\FIXTURE\BIG.BIN C:\CAP` (66,536 bytes)
4. `DIR E:\`
5. `COPY E:\SECTOR1.BIN C:\CAP` (513 bytes)

The copied files were checked against `FIXTURE.SHA256` after each run and
matched.

## Recording a new capture

```sh
emu/dos622/build.py --floppies DIR --out /tmp/dos622-vms
emu/dos622/capture.py --vms /tmp/dos622-vms --scenario write \
    --work /tmp/run-write --log new-capture.log
```

The scenarios are in `../scenarios/` (see `../README.md`). Run `capture.py`
under `xvfb-run` to keep the 86Box windows off screen. The server images to
check READ data against are in `--work`/`dos622-server`.
