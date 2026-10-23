use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use interloper::fat::Volume;
use interloper::image::{BlockDevice, ImageDevice};
use interloper::link::BAUD;
use interloper::mkimage;
use interloper::server::{Log, Server};

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
        /// Serial port or pseudo-terminal, such as /dev/ttyS0. If it fails
        /// (an emulator closing its pty), it is reopened when it can be; a
        /// symbolic link once it has been replaced.
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
    /// Create or list FAT images.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Build a FAT image from the files under a directory.
    ///
    /// The image is a 1.44MB FAT12 floppy if the files fit, otherwise the
    /// smallest FAT12 or FAT16 volume that holds them, up to just under 32MB.
    /// It has no free space and no volume label.
    ///
    /// Entries whose names start with "." are left out. Every other name
    /// must be a DOS 8.3 name: up to 8 characters, optionally a dot and up to
    /// 3 more, using letters, digits and ! # $ % & ' ( ) - @ ^ _ ` { } ~.
    /// Names are stored in uppercase, so two names in one directory that
    /// differ only in case are an error. Device names such as CON or LPT1
    /// are refused, as are symbolic links, special files and directories
    /// more than 63 characters deep. Every problem is reported before
    /// anything is written.
    ///
    /// Modification times are stored as local time, rounded down to an even
    /// second. Times before 1980 or after 2107 are clamped, with a warning.
    Create {
        /// The new image file. It must not exist.
        image: PathBuf,
        /// The directory whose contents become the image's root.
        directory: PathBuf,
    },
    /// List the files and directories in a FAT12 or FAT16 image.
    ///
    /// Takes the same images as `serve`. Prints the size (- for a
    /// directory), the modification time and the path of each entry, sorted
    /// by path, with / after directories.
    List {
        /// A FAT12 or FAT16 image: a bare volume, or a whole disk whose first
        /// FAT partition is listed.
        image: PathBuf,
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
        Command::Image {
            command: ImageCommand::Create { image, directory },
        } => image_create(&image, &directory),
        Command::Image {
            command: ImageCommand::List { image },
        } => image_list(&image),
    }
}

fn image_create(image: &Path, directory: &Path) -> Result<()> {
    let summary = mkimage::create(image, directory)?;
    for note in &summary.notes {
        eprintln!("warning: {note}");
    }
    let l = &summary.layout;
    let kind = if l.is_floppy() {
        "1.44MB floppy".to_string()
    } else {
        format!("{} KB volume", u64::from(l.sectors) * 512 / 1024)
    };
    eprintln!(
        "{}: {kind}, {}, {} bytes in {} files and {} directories",
        image.display(),
        if l.fat16 { "FAT16" } else { "FAT12" },
        summary.bytes,
        summary.files,
        summary.dirs
    );
    Ok(())
}

fn image_list(image: &Path) -> Result<()> {
    let dev = ImageDevice::open(image)?;
    let vol = Volume::open(&dev).with_context(|| format!("{}", image.display()))?;
    let mut entries = vol
        .entries()
        .with_context(|| format!("{}", image.display()))?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut out = io::stdout().lock();
    for e in &entries {
        let line = if e.dir {
            format!("{:>10} {}  {}/", "-", e.time, e.path_string())
        } else {
            format!("{:>10} {}  {}", e.size, e.time, e.path_string())
        };
        match writeln!(out, "{line}") {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            r => r?,
        }
    }
    Ok(())
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
    let trace = match trace {
        Some(path) => {
            Some(File::create(&path).with_context(|| format!("creating {}", path.display()))?)
        }
        None => None,
    };
    // Each connection writes to the same trace file.
    let mut trace = || -> io::Result<Box<dyn Write>> {
        Ok(match &trace {
            Some(file) => Box::new(file.try_clone()?),
            None => Box::new(io::sink()),
        })
    };
    let mut server = Server::new(drives);
    let mut log = |kind: Log, line: &str| {
        if verbose || kind != Log::Request {
            eprintln!("{line}");
        }
    };
    server
        .serve_port(
            &port,
            baud,
            Duration::from_millis(200),
            &mut trace,
            &mut log,
        )
        .with_context(|| format!("opening {}", port.display()))
}
