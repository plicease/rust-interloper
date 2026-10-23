//! Byte transports: a serial port or pseudo-terminal, or any other file
//! descriptor stream (a socket in tests).
//!
//! Transports carry bytes only. Packet framing belongs to [`crate::link`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::termios::{self, BaudRate, SetArg};

/// A bidirectional byte stream with read timeouts.
pub trait Transport {
    /// Read one byte, waiting at most `timeout`. `Ok(None)` means the
    /// timeout passed.
    fn read_byte(&mut self, timeout: Duration) -> io::Result<Option<u8>>;

    /// Read one byte like [`Transport::read_byte`], and whether it arrived
    /// with a line error: a framing or parity error, or a break. Only a
    /// serial port can tell; other transports never report one.
    fn read_byte_checked(&mut self, timeout: Duration) -> io::Result<Option<(u8, bool)>> {
        Ok(self.read_byte(timeout)?.map(|b| (b, false)))
    }

    /// Write all of `data`.
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;

    /// Wait until written bytes have left the transport.
    fn drain(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Change the line speed. Transports without one ignore it.
    fn set_baud(&mut self, _baud: u32) -> io::Result<()> {
        Ok(())
    }
}

/// Logs every byte of a server's transport in the format of
/// `emu/dos622/sniff.py`, so `decode.py` can read it: reads are `C>S`,
/// writes `S>C`.
pub struct Trace<T, W> {
    inner: T,
    out: W,
    start: Instant,
}

impl<T: Transport, W: Write> Trace<T, W> {
    pub fn new(inner: T, out: W) -> Self {
        Self::with_start(inner, out, Instant::now())
    }

    /// Log times relative to `start`, so traces of successive connections
    /// share one clock.
    pub fn with_start(inner: T, out: W, start: Instant) -> Self {
        Trace { inner, out, start }
    }

    fn log(&mut self, direction: &str, data: &[u8]) -> io::Result<()> {
        let hex: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
        writeln!(
            self.out,
            "{:12.6} {direction} {:4} {}",
            self.start.elapsed().as_secs_f64(),
            data.len(),
            hex.join(" ")
        )?;
        self.out.flush()
    }
}

impl<T: Transport, W: Write> Transport for Trace<T, W> {
    fn read_byte(&mut self, timeout: Duration) -> io::Result<Option<u8>> {
        Ok(self.read_byte_checked(timeout)?.map(|(b, _)| b))
    }

    fn read_byte_checked(&mut self, timeout: Duration) -> io::Result<Option<(u8, bool)>> {
        let read = self.inner.read_byte_checked(timeout)?;
        if let Some((b, line_error)) = read {
            self.log("C>S", &[b])?;
            if line_error {
                let at = self.start.elapsed().as_secs_f64();
                writeln!(self.out, "{at:12.6} MARK HOST line error")?;
            }
        }
        Ok(read)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.log("S>C", data)?;
        self.inner.write_all(data)
    }

    fn drain(&mut self) -> io::Result<()> {
        self.inner.drain()
    }

    fn set_baud(&mut self, baud: u32) -> io::Result<()> {
        let at = self.start.elapsed().as_secs_f64();
        writeln!(self.out, "{at:12.6} MARK HOST baud {baud}")?;
        self.inner.set_baud(baud)
    }
}

/// A transport over a file descriptor stream, buffering reads.
pub struct FdTransport<S> {
    stream: S,
    buf: Vec<u8>,
    pos: usize,
}

impl<S: AsFd + Read + Write> FdTransport<S> {
    pub fn new(stream: S) -> Self {
        FdTransport {
            stream,
            buf: Vec::new(),
            pos: 0,
        }
    }

    pub fn get_ref(&self) -> &S {
        &self.stream
    }
}

impl<S: AsFd + Read + Write> Transport for FdTransport<S> {
    fn read_byte(&mut self, timeout: Duration) -> io::Result<Option<u8>> {
        if self.pos < self.buf.len() {
            self.pos += 1;
            return Ok(Some(self.buf[self.pos - 1]));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let wait = PollTimeout::try_from(left.as_millis().min(i32::MAX as u128) as i32)
                .unwrap_or(PollTimeout::MAX);
            let mut fds = [PollFd::new(self.stream.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, wait) {
                Ok(0) => return Ok(None),
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    self.buf.clear();
                    self.buf.extend_from_slice(&chunk[..n]);
                    self.pos = 1;
                    return Ok(Some(self.buf[0]));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.stream.write_all(data)
    }
}

/// A serial port or pseudo-terminal in raw mode.
pub struct Serial {
    inner: FdTransport<File>,
}

/// Identifies which port a path led to: for a symbolic link (such as the
/// `com1.pty` link 86Box replaces each time it starts) the link itself, so
/// a replaced link differs even if it points at a reused pty number. `None`
/// for any other path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortLink(Option<(u64, i64, i64, PathBuf)>);

impl PortLink {
    pub fn of(path: &Path) -> PortLink {
        use std::os::unix::fs::MetadataExt;
        let link = std::fs::symlink_metadata(path)
            .ok()
            .filter(|m| m.file_type().is_symlink())
            .and_then(|m| {
                let target = std::fs::read_link(path).ok()?;
                Some((m.ino(), m.ctime(), m.ctime_nsec(), target))
            });
        PortLink(link)
    }

    /// True if `path` may be reopened after the port opened through this
    /// link failed: a symbolic link must have been replaced.
    pub fn replaced(&self, path: &Path) -> bool {
        self.0.is_none() || PortLink::of(path) != *self
    }
}

impl Serial {
    /// Open a tty (such as `/dev/ttyS0`, or the `com1.pty` link 86Box
    /// creates) in raw 8N1 mode at `baud`, marking bytes received with line
    /// errors (`PARMRK`).
    pub fn open(path: &Path, baud: u32) -> io::Result<Serial> {
        use termios::InputFlags;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags((OFlag::O_NOCTTY | OFlag::O_CLOEXEC).bits())
            .open(path)?;
        let mut tio = termios::tcgetattr(&file)?;
        termios::cfmakeraw(&mut tio);
        tio.control_flags |= termios::ControlFlags::CLOCAL | termios::ControlFlags::CREAD;
        tio.control_flags &= !termios::ControlFlags::CRTSCTS;
        tio.input_flags &= !(InputFlags::IXON | InputFlags::IXOFF | InputFlags::IGNPAR);
        // Without INPCK, Linux serial drivers pass framing errors through as
        // ordinary bytes.
        tio.input_flags |= InputFlags::INPCK | InputFlags::PARMRK;
        let speed = baud_rate(baud)?;
        termios::cfsetispeed(&mut tio, speed)?;
        termios::cfsetospeed(&mut tio, speed)?;
        termios::tcsetattr(&file, SetArg::TCSANOW, &tio)?;
        termios::tcflush(&file, termios::FlushArg::TCIOFLUSH)?;
        Ok(Serial {
            inner: FdTransport::new(file),
        })
    }
}

impl Transport for Serial {
    fn read_byte(&mut self, timeout: Duration) -> io::Result<Option<u8>> {
        Ok(self.read_byte_checked(timeout)?.map(|(b, _)| b))
    }

    fn read_byte_checked(&mut self, timeout: Duration) -> io::Result<Option<(u8, bool)>> {
        read_marked(&mut self.inner, timeout)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner.write_all(data)
    }

    fn drain(&mut self) -> io::Result<()> {
        Ok(termios::tcdrain(self.inner.get_ref())?)
    }

    fn set_baud(&mut self, baud: u32) -> io::Result<()> {
        self.drain()?;
        let file = self.inner.get_ref();
        let mut tio = termios::tcgetattr(file)?;
        let speed = baud_rate(baud)?;
        termios::cfsetispeed(&mut tio, speed)?;
        termios::cfsetospeed(&mut tio, speed)?;
        Ok(termios::tcsetattr(file, SetArg::TCSANOW, &tio)?)
    }
}

/// How long to wait for the rest of a `PARMRK` mark. The tty queues a
/// mark in one piece, so it is there at once.
const MARK_TIME: Duration = Duration::from_millis(100);

/// Read a byte from a tty with `PARMRK` set. The tty sends a byte received
/// with a line error as `ff 00 b` (a break as `ff 00 00`), and a received
/// `ff` as `ff ff`.
fn read_marked<T: Transport>(t: &mut T, timeout: Duration) -> io::Result<Option<(u8, bool)>> {
    let bad_mark = || io::Error::new(io::ErrorKind::InvalidData, "bad PARMRK mark");
    match t.read_byte(timeout)? {
        Some(0xFF) => match t.read_byte(MARK_TIME)? {
            Some(0xFF) => Ok(Some((0xFF, false))),
            Some(0x00) => Ok(Some((t.read_byte(MARK_TIME)?.ok_or_else(bad_mark)?, true))),
            _ => Err(bad_mark()),
        },
        b => Ok(b.map(|b| (b, false))),
    }
}

fn baud_rate(baud: u32) -> io::Result<BaudRate> {
    Ok(match baud {
        1200 => BaudRate::B1200,
        2400 => BaudRate::B2400,
        4800 => BaudRate::B4800,
        9600 => BaudRate::B9600,
        19200 => BaudRate::B19200,
        38400 => BaudRate::B38400,
        57600 => BaudRate::B57600,
        115200 => BaudRate::B115200,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported baud rate {baud}"),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::pty::openpty;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn socket_read_and_timeout() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut a, mut b) = (FdTransport::new(a), FdTransport::new(b));
        a.write_all(&[1, 2, 3]).unwrap();
        let t = Duration::from_millis(200);
        assert_eq!(b.read_byte(t).unwrap(), Some(1));
        assert_eq!(b.read_byte(t).unwrap(), Some(2));
        assert_eq!(b.read_byte(t).unwrap(), Some(3));
        assert_eq!(b.read_byte(Duration::from_millis(20)).unwrap(), None);
    }

    #[test]
    fn closed_peer_is_eof() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        let mut b = FdTransport::new(b);
        let err = b.read_byte(Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn line_error_marks() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut a, mut b) = (FdTransport::new(a), FdTransport::new(b));
        a.write_all(&[0x41, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x98, 0x00])
            .unwrap();
        let t = Duration::from_millis(200);
        for want in [
            (0x41, false),
            (0xFF, false),
            (0x00, true),
            (0x98, true),
            (0x00, false),
        ] {
            assert_eq!(read_marked(&mut b, t).unwrap(), Some(want));
        }
        assert_eq!(
            read_marked(&mut b, Duration::from_millis(20)).unwrap(),
            None
        );
    }

    #[test]
    fn trace_marks_line_errors() {
        struct Errors(Vec<u8>);
        impl Transport for Errors {
            fn read_byte(&mut self, _timeout: Duration) -> io::Result<Option<u8>> {
                Ok(self.0.pop())
            }
            fn read_byte_checked(&mut self, t: Duration) -> io::Result<Option<(u8, bool)>> {
                Ok(self.read_byte(t)?.map(|b| (b, true)))
            }
            fn write_all(&mut self, _data: &[u8]) -> io::Result<()> {
                Ok(())
            }
        }
        let mut out = Vec::new();
        let mut trace = Trace::new(Errors(vec![0x00]), &mut out);
        assert_eq!(trace.read_byte(Duration::ZERO).unwrap(), Some(0x00));
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().map(|l| l.trim_start()).collect();
        assert!(lines[0].ends_with(" C>S    1 00"), "{lines:?}");
        assert!(lines[1].ends_with(" MARK HOST line error"), "{lines:?}");
    }

    #[test]
    fn pty_is_raw() {
        let pty = openpty(None, None).unwrap();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd())).unwrap();
        let mut serial = Serial::open(&path, 9600).unwrap();
        let mut master = File::from(pty.master);
        // Bytes that a cooked tty would translate or treat as control
        // characters must pass through unchanged in both directions.
        let bytes = [0x00, 0x03, 0x0A, 0x0D, 0x11, 0x13, 0x7F, 0xFF];
        master.write_all(&bytes).unwrap();
        for &b in &bytes {
            assert_eq!(serial.read_byte(Duration::from_secs(1)).unwrap(), Some(b));
        }
        serial.write_all(&bytes).unwrap();
        serial.set_baud(115200).unwrap();
        let mut got = [0u8; 8];
        master.read_exact(&mut got).unwrap();
        assert_eq!(got, bytes);
    }
}
