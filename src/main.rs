use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use interloper::image::{BlockDevice, ImageDevice};
use interloper::link::{BAUD, Link};
use interloper::server::{Log, Server};
use interloper::transport::{Serial, Trace};

/// INTERLNK server for Linux: serves FAT disk images to MS-DOS INTERLNK.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve images read-only over a serial port.
    Serve {
        /// Serial port or pseudo-terminal, such as /dev/ttyS0.
        #[arg(long)]
        port: PathBuf,
        /// FAT12 or FAT16 image: a bare volume, or a whole disk whose first
        /// FAT partition is served. Repeat for more drives; they appear in
        /// INTERLNK's table as C:, D: and so on.
        #[arg(long = "image", required = true)]
        images: Vec<PathBuf>,
        /// Highest line speed to accept. Connections start at 9600; a
        /// client without /BAUD steps up to this.
        #[arg(long, default_value_t = 9600)]
        baud: u32,
        /// Write every byte to FILE, in the format emu/dos622/decode.py
        /// reads.
        #[arg(long, value_name = "FILE")]
        trace: Option<PathBuf>,
        /// Log each request.
        #[arg(short, long)]
        verbose: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve {
            port,
            images,
            baud,
            trace,
            verbose,
        } => serve(port, images, baud, trace, verbose),
    }
}

fn serve(
    port: PathBuf,
    images: Vec<PathBuf>,
    baud: u32,
    trace: Option<PathBuf>,
    verbose: bool,
) -> Result<()> {
    if !BAUD.contains(&baud) || baud < 9600 {
        bail!("--baud must be one of 9600, 19200, 38400, 57600 or 115200");
    }
    let mut drives: Vec<Box<dyn BlockDevice>> = Vec::new();
    for (i, path) in images.iter().enumerate() {
        let image = ImageDevice::open(path)?;
        let info = image.media_info();
        eprintln!(
            "unit {} ({}:) {}: {} sectors from sector {}, label {:?}",
            i + 2,
            char::from(b'C' + i as u8),
            path.display(),
            info.sectors,
            image.first_sector(),
            info.label
        );
        drives.push(Box::new(image));
    }
    let serial =
        Serial::open(&port, 9600).with_context(|| format!("opening {}", port.display()))?;
    let trace: Box<dyn Write> = match trace {
        Some(path) => {
            Box::new(File::create(&path).with_context(|| format!("creating {}", path.display()))?)
        }
        None => Box::new(std::io::sink()),
    };
    let mut link = Link::new(Trace::new(serial, trace), 9600, baud);
    let mut server = Server::new(drives);
    eprintln!("serving on {}", port.display());
    let mut log = |kind: Log, line: &str| {
        if verbose || kind != Log::Request {
            eprintln!("{line}");
        }
    };
    server
        .serve(&mut link, &mut log)
        .with_context(|| format!("serving {}", port.display()))
}
