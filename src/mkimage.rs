//! Building a FAT image from a directory on the host, for `interloper image
//! create`.
//!
//! Every name must already be a valid DOS 8.3 name. Nothing is truncated or
//! given a generated alias; names are only uppercased.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use nix::libc;

use crate::fat::{self, DosTime, Layout, Node, NodeKind};
use crate::proto::SECTOR;

/// Characters DOS allows in a short name besides letters and digits.
const NAME_PUNCTUATION: &[u8] = b"!#$%&'()-@^_`{}~";

/// Names DOS opens as devices, whatever their extension. COM5 to COM9 and
/// LPT4 to LPT9 are only devices on later systems, but are refused too.
const DEVICE_NAMES: [&str; 23] = [
    "CON", "PRN", "AUX", "NUL", "CLOCK$", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// DOS keeps a current directory of at most 63 characters after `X:\`, so
/// deeper directories cannot be entered.
const MAX_DIR_PATH: usize = 63;

/// The 8.3 name for a host file name, uppercased as stored, or why it has
/// none.
pub fn short_name(host: &OsStr) -> std::result::Result<[u8; 11], String> {
    let s = host.to_str().ok_or("the name is not valid UTF-8")?;
    let upper = s.to_ascii_uppercase();
    let (base, ext) = upper.split_once('.').unwrap_or((&upper, ""));
    if ext.contains('.') {
        return Err("more than one dot".into());
    }
    if base.is_empty() {
        return Err("nothing before the dot".into());
    }
    if upper.ends_with('.') {
        return Err("ends with a dot".into());
    }
    if base.len() > 8 {
        return Err(format!("{base:?} is longer than 8 characters"));
    }
    if ext.len() > 3 {
        return Err(format!("extension {ext:?} is longer than 3 characters"));
    }
    if let Some(c) = base.chars().chain(ext.chars()).find(|&c| {
        !c.is_ascii_alphanumeric() && !(c.is_ascii() && NAME_PUNCTUATION.contains(&(c as u8)))
    }) {
        return Err(format!("{c:?} is not allowed in a DOS name"));
    }
    if DEVICE_NAMES.contains(&base) {
        return Err(format!("{base} is a DOS device name"));
    }
    let mut name = [b' '; 11];
    name[..base.len()].copy_from_slice(base.as_bytes());
    name[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    Ok(name)
}

/// A host time as a FAT time: local time, with odd seconds rounded down.
/// Times FAT cannot hold are clamped to its range, with a note saying so.
pub fn dos_time(t: SystemTime) -> (DosTime, Option<String>) {
    let before = || {
        let note = format!("before 1980, stored as {}", DosTime::MIN);
        (DosTime::MIN, Some(note))
    };
    let after = || {
        let note = format!("after 2107, stored as {}", DosTime::MAX);
        (DosTime::MAX, Some(note))
    };
    let Ok(since) = t.duration_since(UNIX_EPOCH) else {
        return before();
    };
    let Ok(secs) = libc::time_t::try_from(since.as_secs()) else {
        return (
            DosTime::MAX,
            Some(format!(
                "too late for this system's clock, stored as {}",
                DosTime::MAX
            )),
        );
    };
    // SAFETY: `tm` is plain data that localtime_r fills in, and both
    // pointers are valid for the call.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return after();
        }
        tm
    };
    let year = tm.tm_year + 1900;
    if year < 1980 {
        return before();
    }
    if year > 2107 {
        return after();
    }
    let part = |v: libc::c_int| v as u16;
    let time = DosTime::new(
        part(year),
        part(tm.tm_mon + 1),
        part(tm.tm_mday),
        part(tm.tm_hour),
        part(tm.tm_min),
        part(tm.tm_sec.min(59)),
    );
    (time, None)
}

/// What [`create`] wrote.
#[derive(Debug)]
pub struct Summary {
    pub layout: Layout,
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    /// Paths whose times had to be clamped, and how.
    pub notes: Vec<String>,
}

/// Build a FAT image of the files under `dir` and write it to `image`,
/// which must not exist. Hidden entries (names starting with `.`) are left
/// out. The whole tree is checked first: if any name cannot be stored, or
/// the tree does not fit, nothing is written and the error lists every
/// problem.
pub fn create(image: &Path, dir: &Path) -> Result<Summary> {
    ensure!(
        fs::symlink_metadata(image).is_err(),
        "{} already exists; not overwriting it",
        image.display()
    );
    let meta = fs::metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    ensure!(meta.is_dir(), "{} is not a directory", dir.display());

    let mut scan = Scan::default();
    let planned = scan.dir(dir, Path::new(""));
    if !scan.errors.is_empty() {
        bail!(
            "{} cannot be put in an image:\n  {}",
            dir.display(),
            scan.errors.join("\n  ")
        );
    }
    let limit = u64::from(fat::MAX_SECTORS) * SECTOR as u64;
    ensure!(
        scan.bytes <= limit,
        "{}: {} bytes of files is too large; an image holds at most {} KB",
        dir.display(),
        scan.bytes,
        limit / 1024
    );
    let root = read_files(dir, planned)?;
    let img = fat::build(&root)?;
    let layout = Layout::from_bpb(img[11..36].try_into().unwrap())?;
    write_new(image, &img)?;
    Ok(Summary {
        layout,
        files: scan.files,
        dirs: scan.dirs,
        bytes: scan.bytes,
        notes: scan.notes,
    })
}

/// A file or directory found by the scan, before its contents are read.
struct Planned {
    name: [u8; 11],
    /// Path relative to the source directory.
    rel: PathBuf,
    time: DosTime,
    dir: Option<Vec<Planned>>,
}

#[derive(Default)]
struct Scan {
    errors: Vec<String>,
    notes: Vec<String>,
    files: usize,
    dirs: usize,
    bytes: u64,
}

impl Scan {
    fn error(&mut self, rel: &Path, why: impl std::fmt::Display) {
        self.errors.push(format!("{}: {why}", rel.display()));
    }

    /// The entries of the directory at `path`, sorted by DOS name.
    fn dir(&mut self, path: &Path, rel: &Path) -> Vec<Planned> {
        let mut host: Vec<_> = match fs::read_dir(path).and_then(|d| d.collect()) {
            Ok(entries) => entries,
            Err(e) => {
                self.error(rel, format!("cannot read the directory: {e}"));
                return Vec::new();
            }
        };
        host.sort_by_key(|e| e.file_name());
        let mut names: HashMap<[u8; 11], PathBuf> = HashMap::new();
        let mut out = Vec::new();
        for entry in host {
            let file_name = entry.file_name();
            if file_name.as_bytes().starts_with(b".") {
                continue;
            }
            let rel = rel.join(&file_name);
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(e) => {
                    self.error(&rel, e);
                    continue;
                }
            };
            let kind = meta.file_type();
            if kind.is_symlink() {
                self.error(&rel, "symbolic links are not supported");
                continue;
            }
            if !kind.is_file() && !kind.is_dir() {
                self.error(&rel, "not a regular file or directory");
                continue;
            }
            let name = match short_name(&file_name) {
                Ok(name) => name,
                Err(why) => {
                    self.error(&rel, why);
                    continue;
                }
            };
            if let Some(other) = names.get(&name) {
                let why = format!("same DOS name as {}", other.display());
                self.error(&rel, why);
                continue;
            }
            names.insert(name, rel.clone());
            let time = match meta.modified() {
                Ok(t) => {
                    let (time, note) = dos_time(t);
                    if let Some(note) = note {
                        self.notes
                            .push(format!("{}: modification time {note}", rel.display()));
                    }
                    time
                }
                Err(_) => DosTime::MIN,
            };
            let dir = if kind.is_dir() {
                let dos_path = rel.to_string_lossy().replace('/', "\\");
                if dos_path.len() > MAX_DIR_PATH {
                    let why = format!(
                        "directory path {} is longer than {MAX_DIR_PATH} characters",
                        dos_path.to_ascii_uppercase()
                    );
                    self.error(&rel, why);
                    continue;
                }
                self.dirs += 1;
                Some(self.dir(&entry.path(), &rel))
            } else {
                self.files += 1;
                self.bytes += meta.len();
                None
            };
            out.push(Planned {
                name,
                rel,
                time,
                dir,
            });
        }
        out.sort_by_key(|p| p.name);
        out
    }
}

fn read_files(base: &Path, planned: Vec<Planned>) -> Result<Vec<Node>> {
    planned
        .into_iter()
        .map(|p| {
            let kind = match p.dir {
                Some(kids) => NodeKind::Dir(read_files(base, kids)?),
                None => NodeKind::File(
                    fs::read(base.join(&p.rel))
                        .with_context(|| format!("reading {}", p.rel.display()))?,
                ),
            };
            Ok(Node {
                name: p.name,
                time: p.time,
                kind,
            })
        })
        .collect()
}

/// Write `data` to a new file at `path`, failing if it exists. On a failed
/// write the partial file is removed.
fn write_new(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    if let Err(e) = file.write_all(data).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fat::Volume;
    use crate::image::{BlockDevice, ImageDevice};
    use std::fs::File;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// A directory removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "interloper-mkimage-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        /// Create files ("a/b.txt") and directories ("a/c/") under the
        /// source directory `src`.
        fn tree(&self, entries: &[(&str, &[u8])]) -> PathBuf {
            let src = self.0.join("src");
            fs::create_dir_all(&src).unwrap();
            for &(path, data) in entries {
                let path = src.join(path);
                if path.to_str().unwrap().ends_with('/') {
                    fs::create_dir_all(path).unwrap();
                } else {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(path, data).unwrap();
                }
            }
            src
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        File::open(path).unwrap().set_modified(t).unwrap();
    }

    /// Every entry of an image as (path, is a directory, contents).
    fn contents(image: &Path) -> Vec<(String, bool, Vec<u8>)> {
        let dev = ImageDevice::open(image).unwrap();
        let vol = Volume::open(&dev).unwrap();
        let mut all: Vec<_> = vol
            .entries()
            .unwrap()
            .iter()
            .map(|e| {
                let data = if e.dir {
                    Vec::new()
                } else {
                    vol.read_file(e).unwrap()
                };
                (e.path_string(), e.dir, data)
            })
            .collect();
        all.sort();
        all
    }

    fn binary(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 13 + i / 509) as u8).collect()
    }

    #[test]
    fn short_names() {
        let ok =
            |s: &str| short_name(OsStr::new(s)).map(|n| String::from_utf8(n.to_vec()).unwrap());
        assert_eq!(ok("readme.txt").unwrap(), "README  TXT");
        assert_eq!(ok("Foo").unwrap(), "FOO        ");
        assert_eq!(ok("A-1_$~!.{}@").unwrap(), "A-1_$~! {}@");
        assert_eq!(ok("12345678.123").unwrap(), "12345678123");
        for (bad, why) in [
            ("toolongname.txt", "longer than 8"),
            ("a.text", "longer than 3"),
            ("a.b.c", "more than one dot"),
            ("readme.", "ends with a dot"),
            ("con.txt", "device name"),
            ("Lpt1", "device name"),
            ("sp ace", "' '"),
            ("a+b", "'+'"),
            ("na\u{ef}ve", "'ï'"),
        ] {
            let err = ok(bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn round_trip() {
        let tmp = TempDir::new();
        let src = tmp.tree(&[
            ("ReadMe.txt", b"hello\r\n"),
            ("empty.dat", b""),
            ("BIG.BIN", &binary(70_000)),
            ("sub/nested.TXT", b"nested"),
            ("sub/deep/", b""),
            ("sub/emptydir/", b""),
            ("sub/README.TXT", b"same name, other directory"),
            (".hidden", b"x"),
            (".git/config", b"x"),
            ("sub/.cache/", b""),
        ]);
        let t = UNIX_EPOCH + Duration::from_secs(1_761_210_423); // 2025-10-23
        for p in ["ReadMe.txt", "sub/nested.TXT", "sub/deep", "sub"] {
            set_mtime(&src.join(p), t);
        }
        let image = tmp.0.join("out.img");
        let summary = create(&image, &src).unwrap();
        assert!(summary.layout.is_floppy());
        assert_eq!((summary.files, summary.dirs), (5, 3));
        let want = [
            ("BIG.BIN", false, binary(70_000)),
            ("EMPTY.DAT", false, vec![]),
            ("README.TXT", false, b"hello\r\n".to_vec()),
            ("SUB", true, vec![]),
            ("SUB/DEEP", true, vec![]),
            ("SUB/EMPTYDIR", true, vec![]),
            ("SUB/NESTED.TXT", false, b"nested".to_vec()),
            (
                "SUB/README.TXT",
                false,
                b"same name, other directory".to_vec(),
            ),
        ];
        let want: Vec<_> = want
            .into_iter()
            .map(|(p, d, v)| (p.to_string(), d, v))
            .collect();
        assert_eq!(contents(&image), want);

        let dev = ImageDevice::open(&image).unwrap();
        let entries = Volume::open(&dev).unwrap().entries().unwrap();
        let (dos, _) = dos_time(t);
        for name in ["README.TXT", "SUB", "SUB/NESTED.TXT", "SUB/DEEP"] {
            let e = entries.iter().find(|e| e.path_string() == name).unwrap();
            assert_eq!(e.time, dos, "{name}");
        }
    }

    #[test]
    fn bad_names_are_all_reported_and_nothing_is_written() {
        let tmp = TempDir::new();
        let src = tmp.tree(&[
            ("FOO.TXT", b""),
            ("foo.txt", b""),
            ("Dir/", b""),
            ("DIR", b""),
            ("sub/much_too_long.txt", b""),
            ("ok.txt", b""),
        ]);
        std::os::unix::fs::symlink("ok.txt", src.join("link")).unwrap();
        nix::unistd::mkfifo(&src.join("fifo"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let image = tmp.0.join("out.img");
        let err = create(&image, &src).unwrap_err().to_string();
        for want in [
            "foo.txt: same DOS name as FOO.TXT",
            "Dir: same DOS name as DIR",
            "sub/much_too_long.txt: \"MUCH_TOO_LONG\" is longer than 8",
            "link: symbolic links are not supported",
            "fifo: not a regular file or directory",
        ] {
            assert!(err.contains(want), "{want:?} not in {err}");
        }
        assert!(!image.exists());
    }

    #[test]
    fn deep_directories_are_refused() {
        let tmp = TempDir::new();
        // AAAAAAAA\AAAAAAAA\...: 7 levels are 62 characters, 8 are 71.
        let deep = ["AAAAAAAA"; 8].join("/");
        let src = tmp.tree(&[(&format!("{deep}/"), b"")]);
        let err = create(&tmp.0.join("out.img"), &src)
            .unwrap_err()
            .to_string();
        assert!(err.contains("longer than 63 characters"), "{err}");
    }

    #[test]
    fn existing_image_is_kept() {
        let tmp = TempDir::new();
        let src = tmp.tree(&[("a.txt", b"a")]);
        let image = tmp.0.join("out.img");
        fs::write(&image, b"keep").unwrap();
        let err = create(&image, &src).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(fs::read(&image).unwrap(), b"keep");
        // A dangling symbolic link counts as existing too.
        let link = tmp.0.join("link.img");
        std::os::unix::fs::symlink("nowhere", &link).unwrap();
        assert!(create(&link, &src).is_err());
        assert!(!tmp.0.join("nowhere").exists());
    }

    #[test]
    fn too_large() {
        let tmp = TempDir::new();
        let src = tmp.tree(&[("a.bin", b"")]);
        File::options()
            .write(true)
            .open(src.join("a.bin"))
            .unwrap()
            .set_len(40 << 20)
            .unwrap();
        let image = tmp.0.join("out.img");
        let err = create(&image, &src).unwrap_err().to_string();
        assert!(err.contains("too large"), "{err}");
        assert!(!image.exists());
    }

    #[test]
    fn times_outside_fat_are_clamped() {
        let (t, note) = dos_time(UNIX_EPOCH + Duration::from_secs(86_400));
        assert_eq!(t, DosTime::MIN);
        assert!(note.unwrap().contains("before 1980"));
        let (t, note) = dos_time(UNIX_EPOCH - Duration::from_secs(1));
        assert_eq!(t, DosTime::MIN);
        assert!(note.is_some());
        let (_, note) = dos_time(UNIX_EPOCH + Duration::from_secs(1_761_210_423));
        assert!(note.is_none());

        let tmp = TempDir::new();
        let src = tmp.tree(&[("old.txt", b"")]);
        set_mtime(&src.join("old.txt"), UNIX_EPOCH);
        let summary = create(&tmp.0.join("out.img"), &src).unwrap();
        assert_eq!(summary.notes.len(), 1);
        assert!(summary.notes[0].starts_with("old.txt: modification time before 1980"));
    }

    #[test]
    fn served_by_the_server() {
        // Read the created image back through the server's own requests:
        // BUILD-BPB for the layout, then READs, as INTERLNK would.
        use crate::image::MediaInfo;
        use crate::proto::{DeviceError, Reply, Request, Sectors, Status};
        use crate::server::Server;
        use std::cell::RefCell;

        struct Served {
            server: RefCell<Server>,
            info: MediaInfo,
        }

        impl BlockDevice for Served {
            fn media_info(&self) -> &MediaInfo {
                &self.info
            }

            fn read_sectors(&self, start: u32, dst: &mut [u8]) -> Result<(), DeviceError> {
                let request = Request::Read(Sectors {
                    unit: 2,
                    media: self.info.media,
                    count: (dst.len() / SECTOR) as u16,
                    start,
                });
                match self.server.borrow_mut().handle(&request) {
                    Some(Reply::Read { status, data }) if status == Status::DONE => {
                        dst.copy_from_slice(&data);
                        Ok(())
                    }
                    other => panic!("{other:?}"),
                }
            }
        }

        let tmp = TempDir::new();
        let src = tmp.tree(&[("big.bin", &binary(3_000_000)), ("a/b/c.txt", b"deep")]);
        let image = tmp.0.join("out.img");
        let summary = create(&image, &src).unwrap();
        assert!(!summary.layout.is_floppy());
        let dev = ImageDevice::open(&image).unwrap();
        let mut info = dev.media_info().clone();
        let mut server = Server::new(vec![Box::new(dev) as Box<dyn BlockDevice>]);
        let Some(Reply::BuildBpb { bpb, .. }) =
            server.handle(&Request::BuildBpb { unit: 2, media: 0 })
        else {
            panic!()
        };
        info.bpb = bpb;
        let served = Served {
            server: RefCell::new(server),
            info,
        };
        let vol = Volume::open(&served).unwrap();
        let entries = vol.entries().unwrap();
        let read = |name: &str| {
            let e = entries.iter().find(|e| e.path_string() == name).unwrap();
            vol.read_file(e).unwrap()
        };
        assert_eq!(read("BIG.BIN"), binary(3_000_000));
        assert_eq!(read("A/B/C.TXT"), b"deep");
    }
}
