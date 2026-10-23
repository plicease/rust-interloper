//! Interloper: a Linux server for MS-DOS INTERLNK clients.
//!
//! The protocol is described in `docs/protocol.md`.

pub mod crc;
pub mod fat;
pub mod image;
pub mod link;
pub mod mkimage;
pub mod proto;
pub mod server;
pub mod transport;
