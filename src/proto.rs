//! INTERLNK requests and replies: the blocks exchanged in each turn.
//! See `docs/protocol.md` for the layouts.
//!
//! A request is every block the client sends in one turn. Most requests are
//! one block; SERVER-INFO is an announcement block `00 00` and then the
//! request, and WRITE is the request and then the sector data. A reply is
//! every block the server sends before turning around.

use std::fmt;

pub const SECTOR: usize = 512;

/// Blocks a request takes, judging by its first block.
pub fn request_blocks(first: &[u8]) -> usize {
    match first {
        [0x00, 0x00] => 2,
        [0x05, ..] => 2,
        _ => 1,
    }
}

/// The status word at the start of most replies: a DOS device driver
/// request header status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub u16);

impl Status {
    pub const DONE: Status = Status(0x0100);

    /// A failure with a DOS device error code.
    pub fn error(code: DeviceError) -> Status {
        Status(0x8100 | code as u16)
    }

    pub fn is_error(self) -> bool {
        self.0 & 0x8000 != 0
    }
}

/// DOS device driver error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceError {
    WriteProtect = 0x00,
    UnknownUnit = 0x01,
    NotReady = 0x02,
    UnknownCommand = 0x03,
    Crc = 0x04,
    BadRequest = 0x05,
    Seek = 0x06,
    UnknownMedia = 0x07,
    SectorNotFound = 0x08,
    WriteFault = 0x0A,
    ReadFault = 0x0B,
    General = 0x0C,
}

/// Version as (major, minor), for example (1, 10) for INTERSVR 1.10.
pub type Version = (u8, u8);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfoRequest {
    pub version: Version,
    pub max_block: u16,
    pub client_id: u32,
    /// The server ID from this client's previous session, or 0.
    pub last_server_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitRequest {
    pub version: Version,
    pub dos_version: Version,
    /// Drive letters INTERLNK reserved (`/DRIVES`).
    pub drives: u8,
    /// The client's unit number of its first redirected drive.
    pub first_unit: u8,
    pub printers: [u8; 3],
    pub drive_map: [u8; 26],
}

/// Addressing shared by READ and WRITE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sectors {
    pub unit: u8,
    pub media: u8,
    pub count: u16,
    /// Volume-relative start sector.
    pub start: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    ServerInfo(ServerInfoRequest),
    Init(InitRequest),
    MediaCheck {
        unit: u8,
        media: u8,
    },
    BuildBpb {
        unit: u8,
        media: u8,
    },
    Read(Sectors),
    Write(Sectors, Vec<u8>),
    DriveInfo {
        unit: u8,
    },
    GenericIoctl {
        unit: u8,
        category: u8,
        function: u8,
    },
    /// A command this codec does not know; the blocks as received.
    Unknown {
        command: u8,
        blocks: Vec<Vec<u8>>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad request: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn need(d: &[u8], len: usize, what: &str) -> Result<(), ParseError> {
    if d.len() == len {
        Ok(())
    } else {
        Err(ParseError(format!(
            "{what} is {} bytes, expected {len}",
            d.len()
        )))
    }
}

impl Request {
    /// Parse the blocks of one client turn.
    pub fn parse(blocks: &[Vec<u8>]) -> Result<Request, ParseError> {
        let first = blocks
            .first()
            .ok_or_else(|| ParseError("no blocks".into()))?;
        let command = *first
            .first()
            .ok_or_else(|| ParseError("empty block".into()))?;
        let expect = request_blocks(first);
        if blocks.len() != expect {
            return Err(ParseError(format!(
                "command {command:02x} has {} blocks, expected {expect}",
                blocks.len()
            )));
        }
        let d = first.as_slice();
        Ok(match command {
            0x00 => {
                let r = &blocks[1];
                need(r, 34, "SERVER-INFO")?;
                Request::ServerInfo(ServerInfoRequest {
                    version: (r[7], r[6]),
                    max_block: u16_at(r, 12),
                    client_id: u32_at(r, 26),
                    last_server_id: u32_at(r, 30),
                })
            }
            0x01 => {
                need(d, 36, "INIT")?;
                Request::Init(InitRequest {
                    version: (d[1], d[2]),
                    dos_version: (d[4], d[3]),
                    drives: d[5],
                    first_unit: d[6],
                    printers: [d[7], d[8], d[9]],
                    drive_map: d[10..36].try_into().unwrap(),
                })
            }
            0x02 | 0x03 => {
                need(d, 3, "MEDIA-CHECK/BUILD-BPB")?;
                let (unit, media) = (d[1], d[2]);
                if command == 0x02 {
                    Request::MediaCheck { unit, media }
                } else {
                    Request::BuildBpb { unit, media }
                }
            }
            0x04 | 0x05 => {
                need(d, 11, "READ/WRITE")?;
                let start = match u16_at(d, 5) {
                    0xFFFF => u32_at(d, 7),
                    start => u32::from(start),
                };
                let s = Sectors {
                    unit: d[1],
                    media: d[2],
                    count: u16_at(d, 3),
                    start,
                };
                if command == 0x04 {
                    Request::Read(s)
                } else {
                    need(&blocks[1], usize::from(s.count) * SECTOR, "WRITE data")?;
                    Request::Write(s, blocks[1].clone())
                }
            }
            0x1C => {
                need(d, 2, "DRIVE-INFO")?;
                Request::DriveInfo { unit: d[1] }
            }
            0x1D => {
                need(d, 4, "GENERIC-IOCTL")?;
                Request::GenericIoctl {
                    unit: d[1],
                    category: d[2],
                    function: d[3],
                }
            }
            _ => Request::Unknown {
                command,
                blocks: blocks.to_vec(),
            },
        })
    }

    /// The server unit a drive request addresses, if any.
    pub fn unit(&self) -> Option<u8> {
        match self {
            Request::MediaCheck { unit, .. }
            | Request::BuildBpb { unit, .. }
            | Request::DriveInfo { unit }
            | Request::GenericIoctl { unit, .. } => Some(*unit),
            Request::Read(s) | Request::Write(s, _) => Some(s.unit),
            _ => None,
        }
    }
}

/// MEDIA-CHECK answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaChange {
    Unknown = 0x00,
    Unchanged = 0x01,
    Changed = 0xFF,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfoReply {
    pub version: Version,
    pub max_block: u16,
    /// The client ID from this server's previous session, or 0.
    pub last_client_id: u32,
    pub server_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitReply {
    pub version: Version,
    pub dos_version: Version,
    pub drives: u8,
    pub printers: [u8; 3],
    /// Entry *i* is the server unit for the client's *i*th redirected
    /// drive, or [`InitReply::NO_DRIVE`].
    pub drive_map: [u8; 26],
    /// DOS device attribute word for each entry.
    pub attributes: [u16; 26],
    pub multitasking: u8,
}

impl InitReply {
    pub const NO_DRIVE: u8 = 0xFE;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    ServerInfo(ServerInfoReply),
    Init(InitReply),
    MediaCheck {
        status: Status,
        change: MediaChange,
        /// Volume label, NUL padded.
        label: [u8; 12],
        serial: u32,
    },
    BuildBpb {
        status: Status,
        /// The DOS 4 BPB as it appears in the boot sector at offset 11.
        bpb: [u8; 25],
        extra: [u8; 6],
    },
    /// READ: the status and sector count, then the data.
    Read {
        status: Status,
        data: Vec<u8>,
    },
    Write {
        status: Status,
        count: u16,
    },
    DriveInfo {
        /// NUL-padded size text such as `(32Mb)`.
        size: [u8; 10],
        label: [u8; 13],
    },
    /// GENERIC-IOCTL: the status, then the function's data, if any.
    GenericIoctl {
        status: Status,
        data: Vec<u8>,
    },
}

impl Reply {
    /// The blocks to send for this reply.
    pub fn blocks(&self) -> Vec<Vec<u8>> {
        match self {
            Reply::ServerInfo(r) => {
                let mut d = vec![0u8; 33];
                d[6] = r.version.1;
                d[7] = r.version.0;
                d[8] = 1; // "device server"
                d[10] = 1; // CRC
                d[11..13].copy_from_slice(&r.max_block.to_le_bytes());
                d[25..29].copy_from_slice(&r.last_client_id.to_le_bytes());
                d[29..33].copy_from_slice(&r.server_id.to_le_bytes());
                vec![d]
            }
            Reply::Init(r) => {
                let mut d = vec![r.version.0, r.version.1, r.dos_version.1, r.dos_version.0];
                d.push(r.drives);
                d.extend_from_slice(&r.printers);
                d.extend_from_slice(&r.drive_map);
                for a in r.attributes {
                    d.extend_from_slice(&a.to_le_bytes());
                }
                d.push(r.multitasking);
                vec![d]
            }
            Reply::MediaCheck {
                status,
                change,
                label,
                serial,
            } => {
                let mut d = status.0.to_le_bytes().to_vec();
                d.push(*change as u8);
                d.extend_from_slice(label);
                d.extend_from_slice(&serial.to_le_bytes());
                vec![d]
            }
            Reply::BuildBpb { status, bpb, extra } => {
                let mut d = status.0.to_le_bytes().to_vec();
                d.extend_from_slice(bpb);
                d.extend_from_slice(extra);
                vec![d]
            }
            Reply::Read { status, data } => {
                let count = (data.len() / SECTOR) as u16;
                let mut head = status.0.to_le_bytes().to_vec();
                head.extend_from_slice(&count.to_le_bytes());
                if data.is_empty() {
                    vec![head]
                } else {
                    vec![head, data.clone()]
                }
            }
            Reply::Write { status, count } => {
                let mut d = status.0.to_le_bytes().to_vec();
                d.extend_from_slice(&count.to_le_bytes());
                vec![d]
            }
            Reply::DriveInfo { size, label } => {
                let mut d = size.to_vec();
                d.extend_from_slice(label);
                vec![d]
            }
            Reply::GenericIoctl { status, data } => {
                let head = status.0.to_le_bytes().to_vec();
                if data.is_empty() {
                    vec![head]
                } else {
                    vec![head, data.clone()]
                }
            }
        }
    }
}

/// Copy `text` into a NUL-padded field, truncating if needed.
pub fn padded<const N: usize>(text: &[u8]) -> [u8; N] {
    let mut f = [0u8; N];
    let n = text.len().min(N);
    f[..n].copy_from_slice(&text[..n]);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    // Bytes below are from emu/dos622/captures/write-9600.log unless noted.

    #[test]
    fn server_info() {
        let announce = hex("00 00");
        let request = hex(
            "00 00 00 00 00 00 00 01 01 00 00 01 00 20 00 00 00 00 00 00 00 00 00 00 00 00 \
             d9 ff 10 b0 00 00 00 00",
        );
        assert_eq!(request_blocks(&announce), 2);
        assert_eq!(
            Request::parse(&[announce, request]).unwrap(),
            Request::ServerInfo(ServerInfoRequest {
                version: (1, 0),
                max_block: 8192,
                client_id: 0xB010FFD9,
                last_server_id: 0,
            })
        );
        let reply = Reply::ServerInfo(ServerInfoReply {
            version: (1, 10),
            max_block: 8192,
            last_client_id: 0,
            server_id: 0x773A9434,
        });
        assert_eq!(
            reply.blocks(),
            [hex(
                "00 00 00 00 00 00 0a 01 01 00 01 00 20 00 00 00 00 00 00 00 00 00 00 00 00 \
                 00 00 00 00 34 94 3a 77"
            )]
        );
    }

    #[test]
    fn init() {
        let mut request = hex("01 01 00 16 06 03 03 ff ff ff");
        request.extend([0xFE; 26]);
        assert_eq!(
            Request::parse(&[request]).unwrap(),
            Request::Init(InitRequest {
                version: (1, 0),
                dos_version: (6, 22),
                drives: 3,
                first_unit: 3,
                printers: [0xFF; 3],
                drive_map: [0xFE; 26],
            })
        );
        let mut drive_map = [InitReply::NO_DRIVE; 26];
        drive_map[..2].copy_from_slice(&[2, 1]);
        let mut attributes = [0; 26];
        attributes[..3].copy_from_slice(&[0x08C2; 3]);
        let reply = Reply::Init(InitReply {
            version: (1, 10),
            dos_version: (6, 22),
            drives: 3,
            printers: [0xFF; 3],
            drive_map,
            attributes,
            multitasking: 0,
        });
        let mut want = hex("01 0a 16 06 03 ff ff ff 02 01");
        want.extend([0xFE; 24]);
        want.extend(hex("c2 08 c2 08 c2 08"));
        want.extend([0; 47]);
        assert_eq!(want.len(), 87);
        assert_eq!(reply.blocks(), [want]);
    }

    #[test]
    fn media_check() {
        assert_eq!(
            Request::parse(&[hex("02 02 f8")]).unwrap(),
            Request::MediaCheck {
                unit: 2,
                media: 0xF8
            }
        );
        let reply = Reply::MediaCheck {
            status: Status(0x0102),
            change: MediaChange::Unchanged,
            label: padded(b"DOS622"),
            serial: 0x5D5610C7,
        };
        assert_eq!(
            reply.blocks(),
            [hex(
                "02 01 01 44 4f 53 36 32 32 00 00 00 00 00 00 c7 10 56 5d"
            )]
        );
    }

    #[test]
    fn build_bpb() {
        assert_eq!(
            Request::parse(&[hex("03 01 00")]).unwrap(),
            Request::BuildBpb { unit: 1, media: 0 }
        );
        let want = hex(
            "09 01 00 02 01 01 00 02 e0 00 40 0b f0 09 00 12 00 02 00 00 00 00 00 00 00 00 00 \
             00 00 00 07 22 00",
        );
        let reply = Reply::BuildBpb {
            status: Status(0x0109),
            bpb: want[2..27].try_into().unwrap(),
            extra: want[27..].try_into().unwrap(),
        };
        assert_eq!(reply.blocks(), [want]);
    }

    #[test]
    fn read() {
        let request = Request::parse(&[hex("04 02 f8 01 00 ff ff 7f 00 00 00")]).unwrap();
        assert_eq!(
            request,
            Request::Read(Sectors {
                unit: 2,
                media: 0xF8,
                count: 1,
                start: 127
            })
        );
        assert_eq!(request.unit(), Some(2));
        // A 16-bit start sector.
        assert_eq!(
            Request::parse(&[hex("04 01 f0 02 00 13 00 00 00 00 00")]).unwrap(),
            Request::Read(Sectors {
                unit: 1,
                media: 0xF0,
                count: 2,
                start: 19
            })
        );
        let reply = Reply::Read {
            status: Status::DONE,
            data: vec![0xAB; 1024],
        };
        let blocks = reply.blocks();
        assert_eq!(blocks[0], hex("00 01 02 00"));
        assert_eq!(blocks[1], vec![0xAB; 1024]);
    }

    #[test]
    fn write_and_refusal() {
        let request = hex("05 01 f0 01 00 ff ff 13 00 00 00");
        assert_eq!(request_blocks(&request), 2);
        let parsed = Request::parse(&[request.clone(), vec![0x41; 512]]).unwrap();
        assert_eq!(
            parsed,
            Request::Write(
                Sectors {
                    unit: 1,
                    media: 0xF0,
                    count: 1,
                    start: 19
                },
                vec![0x41; 512]
            )
        );
        assert!(Request::parse(&[request, vec![0x41; 100]]).is_err());
        let refusal = Reply::Write {
            status: Status::error(DeviceError::WriteProtect),
            count: 0,
        };
        assert_eq!(refusal.blocks(), [hex("00 81 00 00")]);
    }

    #[test]
    fn drive_info() {
        // From reconnect-9600.log.
        assert_eq!(
            Request::parse(&[hex("1c 02")]).unwrap(),
            Request::DriveInfo { unit: 2 }
        );
        let reply = Reply::DriveInfo {
            size: padded(b"(32Mb)"),
            label: padded(b"DOS622"),
        };
        assert_eq!(
            reply.blocks(),
            [hex(
                "28 33 32 4d 62 29 00 00 00 00 44 4f 53 36 32 32 00 00 00 00 00 00 00"
            )]
        );
    }

    #[test]
    fn generic_ioctl() {
        assert_eq!(
            Request::parse(&[hex("1d 02 08 66")]).unwrap(),
            Request::GenericIoctl {
                unit: 2,
                category: 8,
                function: 0x66
            }
        );
        let data =
            hex("1d 60 c7 10 56 5d 20 20 20 20 20 20 20 20 20 20 20 46 41 54 31 36 20 20 20");
        let reply = Reply::GenericIoctl {
            status: Status::DONE,
            data: data.clone(),
        };
        assert_eq!(reply.blocks(), [hex("00 01"), data]);
    }

    #[test]
    fn malformed_requests() {
        assert!(Request::parse(&[]).is_err());
        assert!(Request::parse(&[vec![]]).is_err());
        assert!(Request::parse(&[hex("02 02")]).is_err());
        assert!(Request::parse(&[hex("04 02 f8 01 00")]).is_err());
        assert!(Request::parse(&[hex("00 00")]).is_err());
        assert_eq!(
            Request::parse(&[hex("42 01")]).unwrap(),
            Request::Unknown {
                command: 0x42,
                blocks: vec![hex("42 01")]
            }
        );
    }
}
