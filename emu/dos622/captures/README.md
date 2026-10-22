# Protocol captures

Serial traffic between MS-DOS 6.22 `INTERLNK.EXE` (client) and
`INTERSVR.EXE` (server), recorded by `sniff.py` between the two 86Box VMs at
9600 baud. The server shares its C: drive (FAT16, 32MB) as client D: and its
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

`--image` checks the data of every READ against the server's disk images.
C: is the partition starting at sector 63 of `dos622.img`; B: is
`fixture.img`. Both logs here decode with no anomalies, and all their reads
matched the images of the VMs that produced them. Those VMs are not kept, and
a fresh build has different volume serial numbers, so its boot sectors will
not match the old logs.

## Files

| File | Notes |
|------|-------|
| `dir-copy-9600-marked.log` | `capture.py` run of `CAPTURE.BAT`, with `MARK` lines for each step. |
| `dir-copy-9600.log` | First capture of the same steps, without markers. The client ran them from AUTOEXEC.BAT with 5-second pauses that do not show up as quiet gaps. |

Both sessions boot the client with the server already running, so they start
with INTERLNK's connection handshake, then run:

1. `DIR D:\FIXTURE`
2. `COPY D:\FIXTURE\README.TXT C:\CAP` (81 bytes)
3. `COPY D:\FIXTURE\BIG.BIN C:\CAP` (66,536 bytes)
4. `DIR E:\`
5. `COPY E:\SECTOR1.BIN C:\CAP` (513 bytes)

The copied files were checked against `FIXTURE.SHA256` after each run and
matched.

## Recording a new capture

```sh
emu/dos622/build.py --capture --floppies DIR --out /tmp/dos622-capture
emu/dos622/capture.py --vms /tmp/dos622-capture --log new-capture.log
```

Run `capture.py` under `xvfb-run` to keep the 86Box windows off screen.
