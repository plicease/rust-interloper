# Interloper — Design Document

**Status:** Draft 0.1  
**Implementation:** Rust on Linux  
**Initial client:** Unmodified Microsoft INTERLNK.EXE from MS-DOS 6.22  
**Initial target:** Phaistos (NuXT, NEC V20) over RS-232

## 1. Purpose

Interloper is an independent Linux implementation of the server side of Microsoft's INTERLNK/INTERSVR protocol. A DOS machine running the original INTERLNK device driver should see a drive backed by a disk image on Linux, and use ordinary DOS commands to list and copy files.

Interloper is **not** part of Sprocket, and does not require installing a new DOS executable.

```text
MS-DOS 6.22 / INTERLNK.EXE       Linux / interloper
         E:                 RS-232       raw FAT image
    DIR / COPY  <-------------------->   read-only
```

The exact drive letter is assigned by DOS and is not guaranteed to be E:.

## 2. MVP scope

### Required

- Interoperate with the **unmodified MS-DOS 6.22 INTERLNK.EXE** over a physical serial null-modem connection.
- Advertise one drive backed by an **existing raw FAT12 or FAT16 image**, provided its layout is compatible with the protocol.
- Support drive discovery, necessary media/BPB information, and reads sufficient for `DIR` and `COPY` from the remote drive.
- Enforce **read-only behavior at both protocol and storage layers**; never open the image writable.
- Detect malformed frames, report useful diagnostics, and allow a fresh client session after a disconnect.
- Support Linux serial ports and a transparent emulator test transport.
- Include repeatable interoperability tests with the original Microsoft programs.

### Not in the MVP

- Writable FAT images or write-through storage.
- Directory-backed synthetic FAT volumes.
- Image creation or formatting.
- Parallel-port transport, multiple clients, non-Linux hosts, a new DOS client, FAT32/LFN support, or direct access to physical disks.

These exclusions must **not** become permanent assumptions in the protocol API or storage abstraction. There is no commitment to implement them later.

## 3. Compatibility and unknowns

The initial target is MS-DOS 6.22 on a NEC V20 machine at 9600 baud, with higher speeds evaluated later. Serial settings and handshake behavior must be established experimentally rather than assumed.

The wire protocol is **not yet fully specified**. In particular, determine whether it exposes DOS block-device requests, how it represents BPBs and media types, what writes look like, and whether a hard-disk-style image can be advertised directly. Avoid committing to an on-disk image layout until these details are verified.

Primary research lead: [sarahemm/ruby-interlnk](https://github.com/sarahemm/ruby-interlnk), a client-side implementation with packet definitions. Treat it as a reference to inspect and verify against captures, not as a normative specification. Also consult official MS-DOS INTERLNK/INTERSVR documentation and capture traffic from original binaries.

## 4. Architecture

```text
CLI and configuration
       |
Session manager  <------> Transport (serial / TCP test adapter)
       |
Protocol codec + request dispatcher
       |
Drive registry and capabilities
       |
BlockDevice abstraction
       |
ReadOnlyImage backend
```

### 4.1 Transport

Transport presents a bidirectional byte stream with timeouts and explicit connection lifecycle. Serial framing/baud configuration belongs here; protocol packet framing does not. A TCP stream is useful for DOSBox-X/86Box tests and must not introduce telnet negotiation or modem commands.

### 4.2 Protocol

Keep byte-level framing, packet integrity, request decoding, session negotiation, and dispatch separate from storage. Use explicit types for request and response variants, including unknown or unsupported operations. A request must be validated before it reaches the backend. Document protocol findings with capture fixtures and tests.

### 4.3 Drive abstraction

Expose protocol-visible drives through an interface representing **capabilities and media metadata**, not just a file path. An illustrative design (subject to protocol findings):

```rust
trait BlockDevice {
    fn media_info(&self) -> Result<MediaInfo, DeviceError>;
    fn capabilities(&self) -> DeviceCapabilities;
    fn read_sectors(&mut self, lba: u64, count: u32, dst: &mut [u8])
        -> Result<(), DeviceError>;
    fn write_sectors(&mut self, lba: u64, data: &[u8])
        -> Result<(), DeviceError> {
        Err(DeviceError::WriteProtected)
    }
    fn flush(&mut self) -> Result<(), DeviceError> { Ok(()) }
}
```

This is a design sketch, **not a fixed API**. In particular, protocol requests may need additional fields or a different addressing model. Do not bake read-only semantics into the trait: the MVP backend is read-only, while capability reporting and unsupported-write responses are explicit.

### 4.4 Read-only image backend

Open raw images read-only; validate length, sector boundaries, advertised geometry/BPB, and all offsets using checked arithmetic. Reject out-of-range requests. Keep FAT interpretation out of the request dispatcher. A small inspection module may parse the boot sector/BPB to validate the image and derive media metadata.

Choose and document the precise supported image form (volume image versus whole-disk image with MBR) based on observed INTERSVR behavior. Do not assume that a valid FAT image is automatically acceptable to INTERLNK.

### 4.5 Future compatibility

Design decisions that preserve optional future backends:

- Keep drive metadata/capabilities separate from the concrete image file.
- Represent writes and flushes in the backend boundary even though the MVP rejects writes.
- Keep protocol operations separate from image layout and host filesystem paths.
- Avoid global single-drive assumptions in the protocol types, even if MVP configuration advertises exactly one drive.
- Make the transport pluggable; avoid hard-coding `/dev/ttyS0` into protocol logic.
- Preserve request ordering and error semantics needed for future writes, rather than optimizing around immutable storage.

A writable backend would later need coherent caching, durable write ordering, and careful media-change semantics. A directory-backed backend would need a virtual FAT allocation and naming model. Neither is part of this project milestone.

## 5. CLI proposal

```sh
interloper serve --port /dev/ttyS0 --baud 9600 --image dos-share.img
interloper serve --port tcp://127.0.0.1:5000 --image dos-share.img
```

Potential diagnostics: `--verbose`, `--trace FILE`, and configurable timeouts. Exact flags are provisional until transport behavior is established. No write-enable flag exists in the MVP.

Do not assume the image path is safe merely because the protocol is read-only: avoid following unexpected paths where practical, and never expose arbitrary host files through client-controlled names.

## 6. Protocol research plan

1. Read `ruby-interlnk` packet definitions, connection code, and documented limitations. Verify its license before reusing any code.
2. Build a controlled original INTERLNK/INTERSVR pair in emulation, with a raw serial-byte capture between them.
3. Capture connection, drive enumeration, BPB/media queries, `DIR`, small and multi-sector `COPY`, a failed write, disconnect, and reconnect.
4. Annotate each trace with request/response boundaries, checksums, addressing, error codes, and timing.
5. Create golden protocol fixtures from captures. Keep Microsoft binaries outside the repository.
6. Implement the smallest compatible read-only server and test against original INTERLNK.

Research questions include baud negotiation, packet retries/duplicates, drive numbering, media changes, geometry, device control requests, write-protect reporting, and session recovery. Record unresolved questions explicitly rather than inventing protocol behavior.

## 7. Testing

**Unit tests:** codec round trips, malformed/truncated frames, checksums, timeouts, request validation, range checks, metadata encoding, write rejection, and session reset.

**Emulator integration:** DOSBox-X first, then 86Box where practical, running genuine MS-DOS 6.22 INTERLNK against Interloper via a transparent serial connection. Compare file bytes and directory listings against known FAT fixtures.

**Real hardware:** Phaistos at 9600 baud first. Verify `DIR`, copy from remote drive to local disk, byte-for-byte integrity, restart behavior, and that attempts to modify the remote image fail without changing its hash. Repeat at faster speeds only after the baseline is stable.

**Safety:** use disposable test images and copies of files. The server image is opened read-only; check its digest before and after each test. Do not run destructive DOS disk utilities against redirected drives.

## 8. MVP acceptance criteria

- A stock MS-DOS 6.22 INTERLNK client discovers the drive served by Interloper.
- `DIR` displays the expected files and directories from a FAT12 or FAT16 fixture.
- `COPY` from the remote drive produces byte-identical files locally, including a multi-sector binary.
- The image cannot be changed through protocol writes; its digest is unchanged after tests.
- The Linux server survives malformed input and supports a fresh session after disconnect.
- The same tests pass in emulation and on Phaistos at 9600 baud.
- Protocol traces and setup instructions are sufficient to reproduce the tests.

## 9. Implementation milestones

1. **Protocol reconnaissance:** packet map, captures, and a short compatibility note.
2. **Codec and test transport:** deterministic fixtures and TCP serial-equivalent test setup.
3. **Drive discovery:** original INTERLNK recognizes a read-only advertised drive.
4. **Read-only image service:** `DIR` and `COPY` succeed in emulation.
5. **Hardware acceptance:** Phaistos succeeds at 9600 baud; document limitations.

## 10. Risks and decisions deferred

- **Undocumented protocol:** mitigated by client-source study and original-binary captures.
- **Block versus filesystem semantics:** settle experimentally before freezing storage interfaces.
- **FAT image layout:** determine accepted BPB, partition, and geometry combinations.
- **Serial compatibility:** test physical UART timing; emulator success alone is insufficient.
- **Read-only signaling:** ensure the original client receives appropriate DOS errors for writes, not merely a silent backend failure.
- **Licensing:** Interloper should contain independently authored implementation code and properly attributed compatible dependencies; do not redistribute Microsoft binaries.

**Non-goal:** Turning Interloper into a general-purpose network filesystem. The initial product is a small, interoperable, read-only INTERSVR replacement for DOS machines.
