//! Answer INTERLNK's session start with no drives, logging each request.
//!
//!     cargo run --example handshake -- PORT [MAX_BAUD [TRACE]]
//!
//! PORT is a serial port or an 86Box `com1.pty` link. This exercises the
//! transport and link layer against the real client; it serves nothing.
//! TRACE, if given, receives every byte in the format `decode.py` reads.

use std::path::PathBuf;

use anyhow::{Context, Result};
use interloper::link::{Incoming, Link};
use interloper::proto::{InitReply, Reply, Request, ServerInfoReply, request_blocks};
use interloper::transport::{Serial, Trace};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let port = PathBuf::from(args.next().context("usage: handshake PORT [MAX_BAUD]")?);
    let max_baud = args.next().map_or(Ok(9600), |b| b.parse())?;
    let trace: Box<dyn std::io::Write> = match args.next() {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(std::io::sink()),
    };
    let serial =
        Serial::open(&port, 9600).with_context(|| format!("opening {}", port.display()))?;
    let mut link = Link::new(Trace::new(serial, trace), 9600, max_baud);
    let mut last_client_id = 0;
    loop {
        let first = match link.wait() {
            Ok(Incoming::Connected) => {
                println!("connected");
                continue;
            }
            Ok(Incoming::Block(block)) => block,
            Err(e) => {
                println!("link: {e}");
                continue;
            }
        };
        let mut blocks = vec![first];
        let result = (|| {
            while blocks.len() < request_blocks(&blocks[0]) {
                blocks.push(link.recv_block()?);
            }
            link.recv_turnaround()
        })();
        if let Err(e) = result {
            println!("link: {e}");
            continue;
        }
        let request = Request::parse(&blocks);
        println!("{request:?} at {} baud", link.baud());
        let reply = match request {
            Ok(Request::ServerInfo(r)) => {
                let reply = Reply::ServerInfo(ServerInfoReply {
                    version: (1, 10),
                    max_block: 8192,
                    last_client_id,
                    server_id: 0x1234_5678,
                });
                last_client_id = r.client_id;
                reply
            }
            Ok(Request::Init(r)) => Reply::Init(InitReply {
                version: (1, 10),
                dos_version: r.dos_version,
                drives: r.drives,
                printers: [0xFF; 3],
                drive_map: [InitReply::NO_DRIVE; 26],
                attributes: [0; 26],
                multitasking: 0,
            }),
            _ => {
                println!("no reply for this request");
                continue;
            }
        };
        let sent = (|| {
            for block in reply.blocks() {
                link.send_block(&block)?;
            }
            link.send_turnaround()
        })();
        if let Err(e) = sent {
            println!("link: {e}");
        }
    }
}
