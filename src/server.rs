//! The server: answers INTERLNK requests from a set of block devices.
//!
//! Drives are given server unit numbers from 2 up, so INTERLNK's drive table
//! shows the first as "C:", as for an INTERSVR sharing a hard disk.

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::image::BlockDevice;
use crate::link::{self, Incoming, Link};
use crate::proto::{
    DeviceError, InitReply, MediaChange, Reply, Request, SECTOR, ServerInfoReply, Status, padded,
    request_blocks,
};
use crate::transport::{PortLink, Serial, Trace, Transport};

/// INTERSVR's version, which INTERLNK 1.00 is known to accept.
pub const VERSION: (u8, u8) = (1, 10);
const FIRST_UNIT: u8 = 2;
/// Attribute word INTERSVR gives each of its drives (see the INIT reply
/// in docs/protocol.md).
const DRIVE_ATTRIBUTES: u16 = 0x08C2;

/// Kinds of line [`Server::serve`] logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Log {
    /// A client connected.
    Session,
    /// A request and its outcome.
    Request,
    /// A link error, a malformed request or an unknown command.
    Problem,
}

pub struct Server {
    drives: Vec<Box<dyn BlockDevice>>,
    server_id: u32,
    /// Client ID of the previous session, reported in SERVER-INFO.
    last_client_id: u32,
}

impl Server {
    pub fn new(drives: Vec<Box<dyn BlockDevice>>) -> Server {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let server_id = (now as u32) ^ (std::process::id() << 16) | 1;
        Server {
            drives,
            server_id,
            last_client_id: 0,
        }
    }

    fn drive(&self, unit: u8) -> Option<&dyn BlockDevice> {
        let i = usize::from(unit.checked_sub(FIRST_UNIT)?);
        self.drives.get(i).map(|d| d.as_ref())
    }

    /// The reply to a request, or `None` for a command whose reply format
    /// is not known.
    pub fn handle(&mut self, request: &Request) -> Option<Reply> {
        let unknown_unit = Status::error(DeviceError::UnknownUnit);
        Some(match request {
            Request::ServerInfo(r) => {
                let reply = Reply::ServerInfo(ServerInfoReply {
                    version: VERSION,
                    max_block: link::MAX_BLOCK as u16,
                    last_client_id: self.last_client_id,
                    server_id: self.server_id,
                });
                self.last_client_id = r.client_id;
                reply
            }
            Request::Init(r) => {
                let mut drive_map = [InitReply::NO_DRIVE; 26];
                let mut attributes = [0; 26];
                let slots = usize::from(r.drives).min(26);
                for (i, slot) in drive_map.iter_mut().take(slots).enumerate() {
                    if i < self.drives.len() {
                        *slot = FIRST_UNIT + i as u8;
                    }
                }
                // The attribute table is indexed by server unit.
                let units = usize::from(FIRST_UNIT)..usize::from(FIRST_UNIT) + self.drives.len();
                attributes[units.start.min(26)..units.end.min(26)].fill(DRIVE_ATTRIBUTES);
                Reply::Init(InitReply {
                    version: VERSION,
                    dos_version: r.dos_version,
                    // The server's drive count: INTERLNK ignores units at
                    // or above it.
                    drives: units.end as u8,
                    printers: [0xFF; 3],
                    drive_map,
                    attributes,
                    multitasking: 0,
                })
            }
            Request::MediaCheck { unit, .. } => match self.drive(*unit) {
                Some(d) => {
                    let info = d.media_info();
                    Reply::MediaCheck {
                        status: Status::DONE,
                        // Images never change while served.
                        change: MediaChange::Unchanged,
                        label: padded(info.label.as_bytes()),
                        serial: info.serial,
                    }
                }
                None => Reply::MediaCheck {
                    status: unknown_unit,
                    change: MediaChange::Unknown,
                    label: [0; 12],
                    serial: 0,
                },
            },
            Request::BuildBpb { unit, .. } => match self.drive(*unit) {
                Some(d) => Reply::BuildBpb {
                    status: Status::DONE,
                    bpb: d.media_info().bpb,
                    extra: bpb_extra(d.media_info().removable),
                },
                None => Reply::BuildBpb {
                    status: unknown_unit,
                    bpb: [0; 25],
                    extra: [0; 6],
                },
            },
            Request::Read(s) => {
                let result = match self.drive(s.unit) {
                    Some(d) => {
                        let mut data = vec![0u8; usize::from(s.count) * SECTOR];
                        d.read_sectors(s.start, &mut data).map(|()| data)
                    }
                    None => Err(DeviceError::UnknownUnit),
                };
                match result {
                    Ok(data) => Reply::Read {
                        status: Status::DONE,
                        data,
                    },
                    Err(e) => Reply::Read {
                        status: Status::error(e),
                        data: Vec::new(),
                    },
                }
            }
            Request::Write(s, data) => {
                let result = match self
                    .drives
                    .get_mut(usize::from(s.unit.wrapping_sub(FIRST_UNIT)))
                {
                    Some(d) => d.write_sectors(s.start, data),
                    None => Err(DeviceError::UnknownUnit),
                };
                match result {
                    Ok(()) => Reply::Write {
                        status: Status::DONE,
                        count: s.count,
                    },
                    Err(e) => Reply::Write {
                        status: Status::error(e),
                        count: 0,
                    },
                }
            }
            Request::DriveInfo { unit } => match self.drive(*unit) {
                // INTERSVR leaves both fields empty for removable drives.
                Some(d) if !d.media_info().removable => {
                    let info = d.media_info();
                    let mb = (u64::from(info.sectors) * SECTOR as u64).div_ceil(1 << 20);
                    Reply::DriveInfo {
                        size: padded(format!("({mb}Mb)").as_bytes()),
                        label: padded(info.label.as_bytes()),
                    }
                }
                _ => Reply::DriveInfo {
                    size: [0; 10],
                    label: [0; 13],
                },
            },
            Request::GenericIoctl {
                unit,
                category: 0x08,
                function: 0x66,
            } => match self.drive(*unit) {
                Some(d) => {
                    // Get Media ID: info level, serial, label, file system.
                    let info = d.media_info();
                    let mut data = vec![0, 0];
                    data.extend_from_slice(&info.serial.to_le_bytes());
                    data.extend_from_slice(&info.boot_label);
                    data.extend_from_slice(&info.fs_type);
                    Reply::GenericIoctl {
                        status: Status::DONE,
                        data,
                    }
                }
                None => Reply::GenericIoctl {
                    status: unknown_unit,
                    data: Vec::new(),
                },
            },
            Request::GenericIoctl { .. } => Reply::GenericIoctl {
                status: Status::error(DeviceError::UnknownCommand),
                data: Vec::new(),
            },
            Request::Unknown { .. } => return None,
        })
    }

    /// Serve the serial port at `port` indefinitely. When the port fails
    /// (a pseudo-terminal whose emulator exited, an unplugged adapter), wait
    /// until it can be opened again, then serve it afresh; a symbolic link
    /// is only reopened once it has been replaced. Fails only if the first
    /// open fails. `trace` gives the trace output for each opening.
    pub fn serve_port<W: Write>(
        &mut self,
        port: &Path,
        max_baud: u32,
        retry: Duration,
        trace: &mut dyn FnMut() -> io::Result<W>,
        log: &mut dyn FnMut(Log, &str),
    ) -> io::Result<()> {
        let start = Instant::now();
        let mut dead: Option<PortLink> = None;
        loop {
            let link_id = PortLink::of(port);
            let serial = match &dead {
                Some(d) if !d.replaced(port) => None,
                _ => match Serial::open(port, 9600) {
                    Ok(s) => Some(s),
                    Err(e) if dead.is_none() => return Err(e),
                    Err(_) => None,
                },
            };
            let Some(serial) = serial else {
                std::thread::sleep(retry);
                continue;
            };
            log(Log::Session, &format!("serving on {}", port.display()));
            let transport = Trace::with_start(serial, trace()?, start);
            let mut link = Link::new(transport, 9600, max_baud);
            if let Err(e) = self.serve(&mut link, log) {
                log(
                    Log::Session,
                    &format!("{}: {e}; waiting to reopen it", port.display()),
                );
            }
            dead = Some(link_id);
        }
    }

    /// Serve a link until its transport fails, reporting to `log`.
    pub fn serve<T: Transport>(
        &mut self,
        link: &mut Link<T>,
        log: &mut dyn FnMut(Log, &str),
    ) -> Result<(), link::Error> {
        loop {
            let first = match link.wait() {
                Ok(Incoming::Connected) => {
                    log(Log::Session, "client connected");
                    continue;
                }
                Ok(Incoming::Block(block)) => block,
                Err(link::Error::Io(e)) => return Err(link::Error::Io(e)),
                Err(e) => {
                    log(Log::Problem, &format!("link: {e}"));
                    continue;
                }
            };
            match self.turn(link, first, log) {
                Err(link::Error::Io(e)) => return Err(link::Error::Io(e)),
                Err(e) => log(Log::Problem, &format!("link: {e}")),
                Ok(()) => {}
            }
        }
    }

    /// Finish receiving a client turn that began with `first`, and answer it.
    fn turn<T: Transport>(
        &mut self,
        link: &mut Link<T>,
        first: Vec<u8>,
        log: &mut dyn FnMut(Log, &str),
    ) -> Result<(), link::Error> {
        let mut blocks = vec![first];
        while blocks.len() < request_blocks(&blocks[0]) {
            blocks.push(link.recv_block()?);
        }
        link.recv_turnaround()?;
        let request = match Request::parse(&blocks) {
            Ok(r) => r,
            Err(e) => {
                log(Log::Problem, &format!("{e}"));
                return link.send_turnaround();
            }
        };
        log(Log::Request, &describe(&request));
        match self.handle(&request) {
            Some(reply) => {
                if let Some(status) = reply_status(&reply).filter(|s| s.is_error()) {
                    log(Log::Request, &format!("  error status {:04x}", status.0));
                }
                for block in reply.blocks() {
                    link.send_block(&block)?;
                }
            }
            None => log(Log::Problem, &format!("no reply to {}", describe(&request))),
        }
        link.send_turnaround()
    }
}

/// The 6 bytes INTERSVR sends after the BPB, copied from the captures. The
/// fourth matches the DOS device type (5 fixed disk, 7 1.44MB floppy); the
/// rest are not understood.
fn bpb_extra(removable: bool) -> [u8; 6] {
    if removable {
        [0x00, 0x00, 0x00, 0x07, 0x22, 0x00]
    } else {
        [0x40, 0x00, 0x00, 0x05, 0x21, 0x00]
    }
}

fn reply_status(reply: &Reply) -> Option<Status> {
    match reply {
        Reply::MediaCheck { status, .. }
        | Reply::BuildBpb { status, .. }
        | Reply::Read { status, .. }
        | Reply::Write { status, .. }
        | Reply::GenericIoctl { status, .. } => Some(*status),
        _ => None,
    }
}

/// A one-line description of a request for logs.
pub fn describe(request: &Request) -> String {
    match request {
        Request::ServerInfo(r) => format!(
            "SERVER-INFO client {}.{:02} id {:08x}",
            r.version.0, r.version.1, r.client_id
        ),
        Request::Init(r) => format!(
            "INIT DOS {}.{:02}, {} drives from unit {}",
            r.dos_version.0, r.dos_version.1, r.drives, r.first_unit
        ),
        Request::MediaCheck { unit, media } => format!("MEDIA-CHECK unit {unit} media {media:02x}"),
        Request::BuildBpb { unit, media } => format!("BUILD-BPB unit {unit} media {media:02x}"),
        Request::Read(s) => format!("READ unit {} sectors {}+{}", s.unit, s.start, s.count),
        Request::Write(s, _) => format!("WRITE unit {} sectors {}+{}", s.unit, s.start, s.count),
        Request::DriveInfo { unit } => format!("DRIVE-INFO unit {unit}"),
        Request::GenericIoctl {
            unit,
            category,
            function,
        } => format!("GENERIC-IOCTL unit {unit} {category:02x}/{function:02x}"),
        Request::Unknown { command, blocks } => {
            format!("command {command:02x} ({} blocks)", blocks.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ImageDevice;
    use crate::image::tests::{disk, floppy, temp_image};
    use crate::proto::{InitRequest, Sectors, ServerInfoRequest};
    use crate::transport::FdTransport;
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::Duration;

    fn server() -> (Server, Vec<std::path::PathBuf>) {
        let paths = vec![
            temp_image(&disk(b"DOS622     ")),
            temp_image(&floppy(b"FIXTURE    ")),
        ];
        let drives = paths
            .iter()
            .map(|p| Box::new(ImageDevice::open(p).unwrap()) as Box<dyn BlockDevice>)
            .collect();
        (Server::new(drives), paths)
    }

    fn init(drives: u8) -> Request {
        Request::Init(InitRequest {
            version: (1, 0),
            dos_version: (6, 22),
            drives,
            first_unit: 3,
            printers: [0xFF; 3],
            drive_map: [0xFE; 26],
        })
    }

    #[test]
    fn session_start() {
        let (mut s, paths) = server();
        let info = |id| {
            Request::ServerInfo(ServerInfoRequest {
                version: (1, 0),
                max_block: 8192,
                client_id: id,
                last_server_id: 0,
            })
        };
        let Some(Reply::ServerInfo(r)) = s.handle(&info(0xB010FFD9)) else {
            panic!()
        };
        assert_eq!((r.version, r.last_client_id), ((1, 10), 0));
        // A reconnecting client sees the previous client's ID.
        let Some(Reply::ServerInfo(r)) = s.handle(&info(0x1814B9DD)) else {
            panic!()
        };
        assert_eq!(r.last_client_id, 0xB010FFD9);

        let Some(Reply::Init(r)) = s.handle(&init(3)) else {
            panic!()
        };
        assert_eq!(&r.drive_map[..3], &[2, 3, 0xFE]);
        assert_eq!(&r.attributes[..5], &[0, 0, 0x08C2, 0x08C2, 0]);
        assert_eq!(r.drives, 4);
        // Fewer letters than drives: only the first is mapped.
        let Some(Reply::Init(r)) = s.handle(&init(1)) else {
            panic!()
        };
        assert_eq!(&r.drive_map[..2], &[2, 0xFE]);
        paths
            .into_iter()
            .for_each(|p| std::fs::remove_file(p).unwrap());
    }

    #[test]
    fn drive_requests() {
        let (mut s, paths) = server();
        assert_eq!(
            s.handle(&Request::MediaCheck { unit: 3, media: 0 }),
            Some(Reply::MediaCheck {
                status: Status::DONE,
                change: MediaChange::Unchanged,
                label: padded(b"FIXTURE"),
                serial: 0x1C0F2216,
            })
        );
        let Some(Reply::BuildBpb { status, bpb, extra }) =
            s.handle(&Request::BuildBpb { unit: 3, media: 0 })
        else {
            panic!()
        };
        assert_eq!(status, Status::DONE);
        assert_eq!(&bpb[..2], &[0x00, 0x02]);
        assert_eq!(extra[3], 7);
        assert_eq!(
            s.handle(&Request::DriveInfo { unit: 2 }),
            Some(Reply::DriveInfo {
                size: padded(b"(2Mb)"),
                label: padded(b"DOS622"),
            })
        );
        let Some(Reply::GenericIoctl { status, data }) = s.handle(&Request::GenericIoctl {
            unit: 2,
            category: 8,
            function: 0x66,
        }) else {
            panic!()
        };
        assert_eq!(status, Status::DONE);
        assert_eq!(data.len(), 25);
        assert_eq!(&data[17..], b"FAT12   ");
        paths
            .into_iter()
            .for_each(|p| std::fs::remove_file(p).unwrap());
    }

    #[test]
    fn reads_writes_and_bad_units() {
        let (mut s, paths) = server();
        let read = |unit, start, count| {
            Request::Read(Sectors {
                unit,
                media: 0xF0,
                count,
                start,
            })
        };
        let Some(Reply::Read { status, data }) = s.handle(&read(2, 40, 3)) else {
            panic!()
        };
        assert_eq!(status, Status::DONE);
        assert_eq!(data.len(), 3 * SECTOR);
        assert_eq!((data[0], data[SECTOR], data[2 * SECTOR]), (40, 41, 42));
        assert_eq!(
            s.handle(&read(2, 2879, 2)),
            Some(Reply::Read {
                status: Status::error(DeviceError::SectorNotFound),
                data: vec![]
            })
        );
        assert_eq!(
            s.handle(&read(4, 0, 1)),
            Some(Reply::Read {
                status: Status::error(DeviceError::UnknownUnit),
                data: vec![]
            })
        );
        let sectors = Sectors {
            unit: 2,
            media: 0xF0,
            count: 1,
            start: 19,
        };
        assert_eq!(
            s.handle(&Request::Write(sectors, vec![0; SECTOR])),
            Some(Reply::Write {
                status: Status(0x8100),
                count: 0
            })
        );
        assert_eq!(
            s.handle(&Request::MediaCheck { unit: 0, media: 0 })
                .and_then(|r| reply_status(&r)),
            Some(Status::error(DeviceError::UnknownUnit))
        );
        paths
            .into_iter()
            .for_each(|p| std::fs::remove_file(p).unwrap());
    }

    #[test]
    fn serves_a_link() {
        // A whole exchange over a socket: SERVER-INFO, INIT and a READ.
        let (mut s, paths) = server();
        let (a, b) = UnixStream::pair().unwrap();
        let mut link = Link::new(FdTransport::new(b), 9600, 9600);
        thread::spawn(move || s.serve(&mut link, &mut |_, _| {}));
        let mut c = FdTransport::new(a);
        use crate::transport::Transport;
        let t = Duration::from_millis(500);
        for (send, want) in link::HELLO {
            c.write_all(&[send]).unwrap();
            assert_eq!(c.read_byte(t).unwrap(), Some(want));
        }
        let mut client = Link::new(c, 9600, 9600);
        let mut turn = |blocks: &[&[u8]], replies: usize| {
            for block in blocks {
                client.send_block(block).unwrap();
            }
            client.send_turnaround().unwrap();
            let got: Vec<Vec<u8>> = (0..replies).map(|_| client.recv_block().unwrap()).collect();
            client.recv_turnaround().unwrap();
            got
        };
        let mut info = vec![0u8; 34];
        info[6..8].copy_from_slice(&[0, 1]);
        let replies = turn(&[&[0, 0], &info], 1);
        assert_eq!(&replies[0][6..8], &[0x0A, 0x01]);
        // 20 sectors from the disk image's volume: more than one packet.
        let read = [0x04, 0x02, 0xF0, 20, 0, 0xFF, 0xFF, 40, 0, 0, 0];
        let replies = turn(&[&read], 2);
        assert_eq!(replies[0], [0x00, 0x01, 20, 0]);
        assert_eq!(replies[1].len(), 20 * SECTOR);
        assert_eq!(replies[1][19 * SECTOR], 59);
        paths
            .into_iter()
            .for_each(|p| std::fs::remove_file(p).unwrap());
    }

    #[test]
    fn reopens_a_replaced_port() {
        // A pty behind a symbolic link, as 86Box makes one: when the
        // emulator exits and a new one replaces the link, the server serves
        // the new pty and remembers the previous client.
        use nix::pty::openpty;
        use std::fs::File;
        use std::os::fd::AsRawFd;
        use std::sync::mpsc;

        let dir = std::env::temp_dir().join(format!("interloper-port-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let port = dir.join("com1.pty");
        let new_pty = || {
            let pty = openpty(None, None).unwrap();
            let slave =
                std::fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd())).unwrap();
            let tmp = dir.join("com1.new");
            std::os::unix::fs::symlink(slave, &tmp).unwrap();
            std::fs::rename(&tmp, &port).unwrap();
            File::from(pty.master)
        };
        let master = new_pty();

        let (mut s, paths) = server();
        let (tx, rx) = mpsc::channel();
        let serve_at = port.clone();
        thread::spawn(move || {
            s.serve_port(
                &serve_at,
                9600,
                Duration::from_millis(20),
                &mut || Ok(io::sink()),
                &mut |_, line| tx.send(line.to_string()).unwrap(),
            )
        });
        let t = Duration::from_secs(2);
        let opened = |rx: &mpsc::Receiver<String>| loop {
            if rx.recv_timeout(t).unwrap().starts_with("serving on") {
                break;
            }
        };
        // Connect as client `id`, returning the server's last client ID.
        let connect = |master: File, id: u32| {
            let mut c = FdTransport::new(master);
            for (send, want) in link::HELLO {
                c.write_all(&[send]).unwrap();
                assert_eq!(c.read_byte(t).unwrap(), Some(want));
            }
            let mut client = Link::new(c, 9600, 9600);
            let mut info = vec![0u8; 34];
            info[6..8].copy_from_slice(&[0, 1]);
            info[26..30].copy_from_slice(&id.to_le_bytes());
            client.send_block(&[0, 0]).unwrap();
            client.send_block(&info).unwrap();
            client.send_turnaround().unwrap();
            let reply = client.recv_block().unwrap();
            client.recv_turnaround().unwrap();
            u32::from_le_bytes(reply[25..29].try_into().unwrap())
        };

        opened(&rx);
        assert_eq!(connect(master, 0xB010FFD9), 0);
        // connect() dropped the master: the server loses the port, and
        // must wait for the link to be replaced.
        loop {
            if rx.recv_timeout(t).unwrap().contains("waiting to reopen") {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
        assert!(rx.try_recv().is_err());
        let master = new_pty();
        opened(&rx);
        assert_eq!(connect(master, 0x1814B9DD), 0xB010FFD9);

        std::fs::remove_dir_all(&dir).unwrap();
        paths
            .into_iter()
            .for_each(|p| std::fs::remove_file(p).unwrap());
    }
}
