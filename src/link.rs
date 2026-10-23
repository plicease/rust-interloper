//! The INTERLNK link layer: handshake, speed change, packets, turnarounds
//! and idle bytes. See `docs/protocol.md` for the rules implemented here.
//!
//! A *block* is one application message. It travels as one or more packets
//! of at most [`MAX_BLOCK`] bytes, all but the last with the "more" flag.
//! The side holding the turn sends blocks, then hands the turn over with a
//! turnaround byte.
//!
//! Packet and turnaround functions work for either side of the link;
//! [`Link::wait`] is the server's loop while the client holds the turn.

use std::fmt;
use std::io;
use std::time::Duration;

use crate::crc::{crc16, frame_ok};
use crate::transport::Transport;

/// Largest packet payload; both programs announce it in SERVER-INFO.
pub const MAX_BLOCK: usize = 8192;

/// The handshake, as (client byte, server reply).
pub const HELLO: [(u8, u8); 3] = [(0xAA, 0x00), (0x55, 0xFF), (0x5A, 0x11)];

/// Line speed for each speed index of a speed change.
pub const BAUD: [u32; 8] = [1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200];

/// The speed change test pattern, as (client byte, server reply).
pub const SPEED_TEST: [(u8, u8); 15] = [
    (0xFF, 0x00),
    (0x00, 0xFF),
    (0x5A, 0xA5),
    (0x55, 0xAA),
    (0xAA, 0x55),
    (0xF0, 0xF0),
    (0x0F, 0x0F),
    (0xE7, 0xFF),
    (0x7E, 0x00),
    (0xC3, 0x18),
    (0x3C, 0x81),
    (0x81, 0x3C),
    (0x18, 0xC3),
    (0x00, 0x7E),
    (0xFF, 0xE7),
];

const BYTE_LENGTH: u8 = 0x80;
const MORE: u8 = 0x40;
const ECHO: u8 = 0x04;
const SEQ: u8 = 0x03;

/// The acknowledgement of flags `f`.
pub fn ack_of(f: u8) -> u8 {
    !f ^ ECHO
}

/// True if `b` can be a flags byte: bits 3 to 5 are always clear.
pub fn is_flags(b: u8) -> bool {
    b & 0x38 == 0
}

/// True if `b` is an idle byte.
pub fn is_idle(b: u8) -> bool {
    b & 0xFC == 0xFC
}

/// A frame: length (1 byte if `byte_length`, else 2), data, CRC high byte
/// first.
pub fn frame(data: &[u8], byte_length: bool) -> Vec<u8> {
    let mut f = Vec::with_capacity(data.len() + 4);
    if byte_length {
        f.push(data.len() as u8);
    } else {
        f.extend_from_slice(&(data.len() as u16).to_le_bytes());
    }
    f.extend_from_slice(data);
    let crc = crc16(&f);
    f.extend_from_slice(&crc.to_be_bytes());
    f
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Nothing arrived in time; the text says what was expected.
    Timeout(&'static str),
    /// The peer broke the link rules.
    Protocol(String),
    /// A packet was not acknowledged after every retry.
    Retries,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Timeout(what) => write!(f, "timed out waiting for {what}"),
            Error::Protocol(text) => write!(f, "protocol error: {text}"),
            Error::Retries => write!(f, "packet not acknowledged after retries"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub struct Timeouts {
    /// For a single reply byte: an ack, an echo, a handshake reply.
    pub reply: Duration,
    /// Between bytes of a frame, and for the flags echo after a frame.
    pub byte: Duration,
    /// Quiet time that ends discarding after a damaged frame.
    pub settle: Duration,
    /// For the next packet or turnaround within a turn.
    pub turn: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            reply: Duration::from_millis(570),
            byte: Duration::from_millis(500),
            settle: Duration::from_millis(100),
            turn: Duration::from_secs(5),
        }
    }
}

/// What the client did while holding the turn.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// A handshake: a new session starts.
    Connected,
    /// The first block of a turn.
    Block(Vec<u8>),
}

/// One end of the link.
pub struct Link<T> {
    t: T,
    timeouts: Timeouts,
    /// Sequence number of the next packet or turnaround, from either side.
    seq: u8,
    /// Send every packet in echo mode (after one of ours was lost).
    echo_mode: bool,
    /// Line speed after a handshake, and the highest we accept.
    base_baud: u32,
    max_baud: u32,
    baud: u32,
    /// Between the handshake and the first packet, `ff` starts a speed
    /// change rather than idling.
    speed_change_allowed: bool,
    retries: u32,
}

impl<T: Transport> Link<T> {
    pub fn new(t: T, base_baud: u32, max_baud: u32) -> Self {
        Link {
            t,
            timeouts: Timeouts::default(),
            seq: 0,
            echo_mode: false,
            base_baud,
            max_baud,
            baud: base_baud,
            speed_change_allowed: false,
            retries: 4,
        }
    }

    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub fn baud(&self) -> u32 {
        self.baud
    }

    pub fn transport(&mut self) -> &mut T {
        &mut self.t
    }

    fn reset(&mut self) {
        self.seq = 0;
        self.echo_mode = false;
        self.speed_change_allowed = true;
    }

    fn read(&mut self, timeout: Duration, what: &'static str) -> Result<u8> {
        self.t.read_byte(timeout)?.ok_or(Error::Timeout(what))
    }

    /// Discard input until the line has been quiet for a while.
    fn settle(&mut self) -> Result<()> {
        while self.t.read_byte(self.timeouts.settle)?.is_some() {}
        Ok(())
    }

    // --- server side while the client holds the turn ----------------------

    /// Wait for the client: answer handshakes and speed changes, echo idle
    /// bytes, and return at a handshake or at the first block of a turn.
    /// Waits indefinitely.
    pub fn wait(&mut self) -> Result<Incoming> {
        loop {
            let b = match self.t.read_byte(Duration::from_secs(3600))? {
                Some(b) => b,
                None => continue,
            };
            if b == HELLO[0].0 {
                if self.server_hello()? {
                    return Ok(Incoming::Connected);
                }
                continue;
            }
            if b == 0xFF && self.speed_change_allowed {
                self.server_speed_change()?;
                continue;
            }
            if is_idle(b) {
                self.t.write_all(&[b])?;
                continue;
            }
            if is_flags(b) {
                if let Some(block) = self.recv_block_from(b)? {
                    return Ok(Incoming::Block(block));
                }
                continue;
            }
            // Noise, perhaps a client restarting at the base speed while we
            // still run faster. Go back to the base speed (a guess: when
            // INTERSVR does this is not known).
            if self.baud != self.base_baud {
                self.baud = self.base_baud;
                self.t.set_baud(self.baud)?;
            }
        }
    }

    /// The rest of a handshake after `aa`. A lone `aa` (INTERLNK's probe
    /// at load) is answered too; false if the rest does not follow.
    fn server_hello(&mut self) -> Result<bool> {
        if self.baud != self.base_baud {
            // INTERLNK always starts at the base speed.
            self.baud = self.base_baud;
            self.t.set_baud(self.baud)?;
        }
        self.t.write_all(&[HELLO[0].1])?;
        for &(want, reply) in &HELLO[1..] {
            match self.t.read_byte(self.timeouts.reply)? {
                Some(b) if b == want => self.t.write_all(&[reply])?,
                _ => return Ok(false),
            }
        }
        self.reset();
        Ok(true)
    }

    /// The rest of a speed change after `ff`.
    fn server_speed_change(&mut self) -> Result<()> {
        self.t.write_all(&[0x00])?;
        let k = self.read(self.timeouts.reply, "speed index")?;
        let baud = *BAUD
            .get(usize::from(k))
            .ok_or_else(|| Error::Protocol(format!("speed index {k:#04x}")))?;
        if baud > self.max_baud {
            // What INTERSVR answers to a speed above its /BAUD limit is not
            // known yet; staying silent makes the client time out.
            return Err(Error::Protocol(format!(
                "speed {baud} above {}",
                self.max_baud
            )));
        }
        self.t.write_all(&[k << 4])?;
        // Assumed: both sides switch after the index pair, and the rest of
        // the exchange checks the new speed. Not confirmed by a capture.
        self.t.drain()?;
        self.t.set_baud(baud)?;
        self.baud = baud;
        let mut pairs = vec![(k << 4, !(k << 4))];
        pairs.extend_from_slice(&SPEED_TEST);
        for (want, reply) in pairs {
            let b = self.read(self.timeouts.reply, "speed test byte")?;
            if b != want {
                self.baud = self.base_baud;
                self.t.set_baud(self.baud)?;
                return Err(Error::Protocol(format!(
                    "speed test at {baud}: got {b:#04x}, expected {want:#04x}"
                )));
            }
            self.t.write_all(&[reply])?;
        }
        Ok(())
    }

    // --- receiving -----------------------------------------------------------

    /// Receive the next block of the peer's turn.
    pub fn recv_block(&mut self) -> Result<Vec<u8>> {
        loop {
            let f = self.read(self.timeouts.turn, "packet flags")?;
            if !is_flags(f) {
                return Err(Error::Protocol(format!("expected flags, got {f:#04x}")));
            }
            if let Some(block) = self.recv_block_from(f)? {
                return Ok(block);
            }
        }
    }

    /// Receive a block whose first flags byte `f` has been read. `None`
    /// if that packet was damaged or a duplicate; the caller waits for the
    /// next flags.
    fn recv_block_from(&mut self, mut f: u8) -> Result<Option<Vec<u8>>> {
        let mut block = Vec::new();
        loop {
            match self.recv_packet(f)? {
                Some(data) => {
                    block.extend_from_slice(&data);
                    if f & MORE == 0 {
                        self.speed_change_allowed = false;
                        return Ok(Some(block));
                    }
                }
                None if block.is_empty() => return Ok(None),
                None => {}
            }
            f = self.read(self.timeouts.turn, "packet flags")?;
            if !is_flags(f) {
                return Err(Error::Protocol(format!("expected flags, got {f:#04x}")));
            }
        }
    }

    /// Receive one packet after its flags `f`. Returns its data, or `None`
    /// if it was damaged (not echoed, so the sender resends it) or a
    /// duplicate of the last one (echoed and dropped).
    fn recv_packet(&mut self, f: u8) -> Result<Option<Vec<u8>>> {
        let seq = f & SEQ;
        let duplicate = seq == self.seq.wrapping_sub(1) & SEQ;
        if seq != self.seq && !duplicate {
            return Err(Error::Protocol(format!(
                "packet sequence {seq}, expected {}",
                self.seq
            )));
        }
        self.t.write_all(&[ack_of(f)])?;
        let echo = f & ECHO != 0;
        let length_bytes = if f & BYTE_LENGTH != 0 { 1 } else { 2 };
        let mut frame = Vec::new();
        let mut total = None;
        loop {
            let b = match self.t.read_byte(self.timeouts.byte)? {
                Some(b) => b,
                None => return Ok(None), // the sender will resend
            };
            frame.push(b);
            if frame.len() == length_bytes {
                let len = if length_bytes == 1 {
                    usize::from(frame[0])
                } else {
                    usize::from(u16::from_le_bytes([frame[0], frame[1]]))
                };
                if len > MAX_BLOCK {
                    self.settle()?;
                    return Ok(None);
                }
                total = Some(length_bytes + len + 2);
            }
            if Some(frame.len()) == total {
                break;
            }
            if echo {
                self.t.write_all(&[b])?;
            }
        }
        if !frame_ok(&frame) {
            self.settle()?;
            return Ok(None);
        }
        self.t.write_all(&[f])?;
        if duplicate {
            return Ok(None);
        }
        self.seq = (self.seq + 1) & SEQ;
        Ok(Some(frame[length_bytes..frame.len() - 2].to_vec()))
    }

    /// Receive the peer's turnaround, which hands us the turn. It carries
    /// the sequence number of the next packet, without using it up.
    pub fn recv_turnaround(&mut self) -> Result<()> {
        let b = self.read(self.timeouts.turn, "turnaround")?;
        if b != self.seq {
            return Err(Error::Protocol(format!(
                "turnaround {b:#04x}, expected {}",
                self.seq
            )));
        }
        Ok(())
    }

    // --- sending -------------------------------------------------------------

    /// Send a block as packets of at most [`MAX_BLOCK`] bytes.
    pub fn send_block(&mut self, data: &[u8]) -> Result<()> {
        let mut chunks = data.chunks(MAX_BLOCK).peekable();
        if chunks.peek().is_none() {
            return self.send_packet(&[], false);
        }
        while let Some(chunk) = chunks.next() {
            self.send_packet(chunk, chunks.peek().is_some())?;
        }
        Ok(())
    }

    /// Hand the turn to the peer.
    pub fn send_turnaround(&mut self) -> Result<()> {
        self.t.write_all(&[self.seq])?;
        Ok(())
    }

    fn send_packet(&mut self, data: &[u8], more: bool) -> Result<()> {
        let byte_length = data.len() <= 0xFF;
        let frame = frame(data, byte_length);
        for _ in 0..self.retries {
            let mut f = self.seq;
            if byte_length {
                f |= BYTE_LENGTH;
            }
            if more {
                f |= MORE;
            }
            if self.echo_mode {
                f |= ECHO;
            }
            if self.try_packet(f, &frame)? {
                self.seq = (self.seq + 1) & SEQ;
                return Ok(());
            }
            // Lost: like INTERLNK and INTERSVR, resend in echo mode and
            // stay in it for the rest of the session.
            self.echo_mode = true;
            self.settle()?;
        }
        Err(Error::Retries)
    }

    fn try_packet(&mut self, f: u8, frame: &[u8]) -> Result<bool> {
        let reply = self.timeouts.reply;
        self.t.write_all(&[f])?;
        self.t.drain()?;
        if self.t.read_byte(reply)? != Some(ack_of(f)) {
            return Ok(false);
        }
        if f & ECHO != 0 {
            let (last, body) = frame.split_last().expect("frame is never empty");
            for &b in body {
                self.t.write_all(&[b])?;
                if self.t.read_byte(reply)? != Some(b) {
                    return Ok(false);
                }
            }
            self.t.write_all(&[*last])?;
            self.t.drain()?;
            return Ok(self.t.read_byte(reply)? == Some(f));
        }
        self.t.write_all(frame)?;
        self.t.drain()?;
        // A pseudo-terminal does not wait for the bytes to reach the peer,
        // so allow for the frame's time on the line at the current speed.
        let line_time = Duration::from_secs_f64(frame.len() as f64 * 10.0 / f64::from(self.baud));
        Ok(self.t.read_byte(reply + line_time)? == Some(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FdTransport;
    use std::os::unix::net::UnixStream;
    use std::thread;

    type Sock = FdTransport<UnixStream>;

    fn fast() -> Timeouts {
        Timeouts {
            reply: Duration::from_millis(100),
            byte: Duration::from_millis(100),
            settle: Duration::from_millis(20),
            turn: Duration::from_secs(2),
        }
    }

    fn pair() -> (Link<Sock>, Link<Sock>) {
        let (a, b) = UnixStream::pair().unwrap();
        let client = Link::new(FdTransport::new(a), 9600, 115200).with_timeouts(fast());
        let server = Link::new(FdTransport::new(b), 9600, 115200).with_timeouts(fast());
        (client, server)
    }

    /// The client's side of the handshake.
    fn client_hello(c: &mut Link<Sock>) {
        for (send, want) in HELLO {
            c.t.write_all(&[send]).unwrap();
            assert_eq!(c.read(c.timeouts.reply, "hello").unwrap(), want);
        }
        c.reset();
    }

    fn client_speed_change(c: &mut Link<Sock>, k: u8) {
        let mut pairs = vec![(0xFF, 0x00), (k, k << 4), (k << 4, !(k << 4))];
        pairs.extend_from_slice(&SPEED_TEST);
        for (send, want) in pairs {
            c.t.write_all(&[send]).unwrap();
            assert_eq!(c.read(c.timeouts.reply, "speed").unwrap(), want);
        }
    }

    #[test]
    fn frames_match_capture() {
        assert_eq!(frame(&[0, 0], true), [0x02, 0x00, 0x00, 0x5D, 0x03]);
        assert!(frame_ok(&frame(&[7; 600], false)));
        assert_eq!(&frame(&[7; 600], false)[..2], &[0x58, 0x02]);
    }

    #[test]
    fn acks_match_capture() {
        // Normal mode: 81 -> 7a, 82 -> 79. Echo mode: 84 -> 7f, 85 -> 7e.
        assert_eq!(ack_of(0x81), 0x7A);
        assert_eq!(ack_of(0x82), 0x79);
        assert_eq!(ack_of(0x84), 0x7F);
        assert_eq!(ack_of(0x85), 0x7E);
        assert_eq!(ack_of(0x80), 0x7B);
    }

    #[test]
    fn request_and_reply() {
        let (mut c, mut s) = pair();
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            let Incoming::Block(first) = s.wait().unwrap() else {
                panic!("expected a block")
            };
            let second = s.recv_block().unwrap();
            s.recv_turnaround().unwrap();
            s.send_block(&[0x00, 0x01, 0x01, 0x00]).unwrap();
            s.send_block(&[0x5A; 20000]).unwrap();
            s.send_turnaround().unwrap();
            // The client idles; the server echoes, then gets the next block.
            let Incoming::Block(third) = s.wait().unwrap() else {
                panic!("expected a block")
            };
            (first, second, third)
        });
        client_hello(&mut c);
        c.send_block(&[0x00, 0x00]).unwrap();
        c.send_block(&[0x04, 0x02, 0xF8, 0x01, 0x00]).unwrap();
        c.send_turnaround().unwrap();
        assert_eq!(c.recv_block().unwrap(), [0x00, 0x01, 0x01, 0x00]);
        assert_eq!(c.recv_block().unwrap(), vec![0x5A; 20000]);
        c.recv_turnaround().unwrap();
        // Six packets so far, so the server turned around with 2, and
        // the idle byte for turnaround 2 is fc | 3.
        assert_eq!(c.seq, 2);
        c.t.write_all(&[0xFF]).unwrap();
        assert_eq!(c.read(c.timeouts.reply, "idle echo").unwrap(), 0xFF);
        c.send_block(&[0x02, 0x02, 0x00]).unwrap();
        let (first, second, third) = server.join().unwrap();
        assert_eq!(first, [0x00, 0x00]);
        assert_eq!(second, [0x04, 0x02, 0xF8, 0x01, 0x00]);
        assert_eq!(third, [0x02, 0x02, 0x00]);
    }

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    #[test]
    fn wire_bytes_match_capture() {
        // The session start of write-9600.log, replayed from the client's
        // side: the server must answer byte for byte as INTERSVR did.
        let request = hex(
            "00 00 00 00 00 00 00 01 01 00 00 01 00 20 00 00 00 00 00 00 00 00 00 00 00 00 \
             d9 ff 10 b0 00 00 00 00",
        );
        let reply = hex(
            "00 00 00 00 00 00 0a 01 01 00 01 00 20 00 00 00 00 00 00 00 00 00 00 00 00 \
             00 00 00 00 34 94 3a 77",
        );
        let (a, b) = UnixStream::pair().unwrap();
        let mut c = FdTransport::new(a);
        let mut s = Link::new(FdTransport::new(b), 9600, 9600).with_timeouts(fast());
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            let announce = s.wait().unwrap();
            let request = s.recv_block().unwrap();
            s.recv_turnaround().unwrap();
            s.send_block(&reply).unwrap();
            s.send_turnaround().unwrap();
            (announce, request)
        });
        let t = Duration::from_millis(500);
        let mut exchange = |send: &[u8], want: &[u8]| {
            c.write_all(send).unwrap();
            for &w in want {
                assert_eq!(c.read_byte(t).unwrap(), Some(w));
            }
        };
        exchange(&[0xAA], &[0x00]);
        exchange(&[0x55], &[0xFF]);
        exchange(&[0x5A], &[0x11]);
        exchange(&[0x80], &[0x7B]);
        exchange(&hex("02 00 00 5d 03"), &[0x80]);
        exchange(&[0x81], &[0x7A]);
        let mut frame = vec![0x22];
        frame.extend(&request);
        frame.extend(hex("a5 ea"));
        exchange(&frame, &[0x81]);
        // Turnaround 2: the server's reply is packet 2, then turnaround 3.
        let mut want = vec![0x82];
        exchange(&[0x02], &want);
        want = vec![0x21];
        want.extend(hex(
            "00 00 00 00 00 00 0a 01 01 00 01 00 20 00 00 00 00 00 00 00 00 00 00 00 00 \
             00 00 00 00 34 94 3a 77 ca c6",
        ));
        exchange(&[0x79], &want);
        exchange(&[0x82], &[0x03]);
        let (announce, got) = server.join().unwrap();
        assert_eq!(announce, Incoming::Block(vec![0, 0]));
        assert_eq!(got, request);
    }

    #[test]
    fn speed_change() {
        let (mut c, mut s) = pair();
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            let block = s.wait().unwrap();
            (block, s.baud())
        });
        client_hello(&mut c);
        for k in 4..=7 {
            client_speed_change(&mut c, k);
        }
        c.send_block(&[0, 0]).unwrap();
        let (block, baud) = server.join().unwrap();
        assert_eq!(block, Incoming::Block(vec![0, 0]));
        assert_eq!(baud, 115200);
    }

    #[test]
    fn speed_above_limit_is_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut c = Link::new(FdTransport::new(a), 9600, 115200).with_timeouts(fast());
        let mut s = Link::new(FdTransport::new(b), 9600, 9600).with_timeouts(fast());
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            s.wait().unwrap_err()
        });
        client_hello(&mut c);
        c.t.write_all(&[0xFF, 0x04]).unwrap();
        assert!(matches!(server.join().unwrap(), Error::Protocol(_)));
    }

    #[test]
    fn echo_mode_packets() {
        let (mut c, mut s) = pair();
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            s.echo_mode = true;
            let block = s.wait().unwrap();
            s.recv_turnaround().unwrap();
            s.send_block(&[9; 300]).unwrap();
            s.send_turnaround().unwrap();
            block
        });
        client_hello(&mut c);
        c.echo_mode = true;
        c.send_block(&[1, 2, 3]).unwrap();
        c.send_turnaround().unwrap();
        assert_eq!(c.recv_block().unwrap(), vec![9; 300]);
        c.recv_turnaround().unwrap();
        assert_eq!(server.join().unwrap(), Incoming::Block(vec![1, 2, 3]));
    }

    #[test]
    fn echo_mode_wire_bytes() {
        // The capture's echo-mode resend of `00 00`: the server echoes
        // each byte except the last, which gets the flags echo.
        let (a, b) = UnixStream::pair().unwrap();
        let mut c = FdTransport::new(a);
        let mut s = Link::new(FdTransport::new(b), 9600, 9600).with_timeouts(fast());
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            s.wait().unwrap()
        });
        let t = Duration::from_millis(500);
        let mut exchange = |send: u8, want: Option<u8>| {
            c.write_all(&[send]).unwrap();
            let got = c
                .read_byte(if want.is_some() { t } else { fast().byte })
                .unwrap();
            assert_eq!(got, want);
        };
        for (send, want) in HELLO {
            exchange(send, Some(want));
        }
        for (send, want) in [
            (0x84, 0x7F),
            (0x02, 0x02),
            (0x00, 0x00),
            (0x00, 0x00),
            (0x5D, 0x5D),
        ] {
            exchange(send, Some(want));
        }
        exchange(0x03, Some(0x84));
        assert_eq!(server.join().unwrap(), Incoming::Block(vec![0, 0]));
    }

    #[test]
    fn damaged_frame_is_resent_in_echo_mode() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut c = FdTransport::new(a);
        let mut s = Link::new(FdTransport::new(b), 9600, 9600).with_timeouts(fast());
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            s.wait().unwrap()
        });
        let t = Duration::from_millis(500);
        for (send, want) in HELLO {
            c.write_all(&[send]).unwrap();
            assert_eq!(c.read_byte(t).unwrap(), Some(want));
        }
        c.write_all(&[0x80]).unwrap();
        assert_eq!(c.read_byte(t).unwrap(), Some(0x7B));
        // A bad CRC: no echo.
        c.write_all(&[0x02, 0x00, 0x00, 0x5D, 0x04]).unwrap();
        assert_eq!(c.read_byte(Duration::from_millis(200)).unwrap(), None);
        // Resend in echo mode, as INTERLNK does.
        for (send, want) in [
            (0x84, 0x7F),
            (0x02, 0x02),
            (0x00, 0x00),
            (0x00, 0x00),
            (0x5D, 0x5D),
            (0x03, 0x84),
        ] {
            c.write_all(&[send]).unwrap();
            assert_eq!(c.read_byte(t).unwrap(), Some(want));
        }
        assert_eq!(server.join().unwrap(), Incoming::Block(vec![0, 0]));
    }

    #[test]
    fn lost_echo_switches_sender_to_echo_mode() {
        // A receiver that ignores the first frame: the sender resends with
        // the echo bit and the same sequence number.
        let (a, b) = UnixStream::pair().unwrap();
        let mut peer = FdTransport::new(a);
        let mut s = Link::new(FdTransport::new(b), 9600, 9600).with_timeouts(fast());
        let sender = thread::spawn(move || {
            s.send_block(&[0x11, 0x22]).unwrap();
            s.echo_mode
        });
        let t = Duration::from_millis(500);
        assert_eq!(peer.read_byte(t).unwrap(), Some(0x80));
        peer.write_all(&[0x7B]).unwrap();
        for _ in 0..5 {
            peer.read_byte(t).unwrap().unwrap();
        }
        // No echo. The resend arrives in echo mode.
        assert_eq!(peer.read_byte(Duration::from_secs(1)).unwrap(), Some(0x84));
        peer.write_all(&[0x7F]).unwrap();
        let f = frame(&[0x11, 0x22], true);
        for (i, &b) in f.iter().enumerate() {
            assert_eq!(peer.read_byte(t).unwrap(), Some(b));
            peer.write_all(&[if i + 1 == f.len() { 0x84 } else { b }])
                .unwrap();
        }
        assert!(sender.join().unwrap());
    }

    #[test]
    fn duplicate_packet_is_dropped() {
        // The client resends a packet whose echo it missed; the server
        // echoes it again but delivers it once.
        let (mut c, mut s) = pair();
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            let first = s.wait().unwrap();
            let second = s.recv_block().unwrap();
            (first, second)
        });
        client_hello(&mut c);
        c.send_block(&[1]).unwrap();
        c.seq = 0; // pretend the echo was lost
        c.send_block(&[1]).unwrap();
        c.send_block(&[2]).unwrap();
        let (first, second) = server.join().unwrap();
        assert_eq!(first, Incoming::Block(vec![1]));
        assert_eq!(second, vec![2]);
    }

    #[test]
    fn handshake_restarts_session() {
        let (mut c, mut s) = pair();
        let server = thread::spawn(move || {
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            assert!(matches!(s.wait().unwrap(), Incoming::Block(_)));
            // The client restarts: a lone probe, then a full handshake.
            assert_eq!(s.wait().unwrap(), Incoming::Connected);
            s.wait().unwrap()
        });
        client_hello(&mut c);
        c.send_block(&[1]).unwrap();
        c.t.write_all(&[0xAA]).unwrap();
        assert_eq!(c.read(c.timeouts.reply, "probe").unwrap(), 0x00);
        thread::sleep(Duration::from_millis(150));
        client_hello(&mut c);
        c.send_block(&[2]).unwrap();
        assert_eq!(server.join().unwrap(), Incoming::Block(vec![2]));
    }
}
