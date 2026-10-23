//! FAT12 and FAT16 volumes: their layout, building one from a tree of files,
//! and reading the directory tree of one.
//!
//! Only short (8.3) names are written or shown. Long file name entries in an
//! existing volume are skipped.

use std::collections::HashSet;
use std::fmt;

use anyhow::{Context, Result, bail, ensure};

use crate::image::BlockDevice;
use crate::proto::SECTOR;

/// The layout of a FAT12/FAT16 volume, from its BPB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub sectors_per_cluster: u32,
    pub fat_start: u32,
    pub fat_sectors: u32,
    pub fats: u32,
    pub root_start: u32,
    pub root_entries: u32,
    pub root_sectors: u32,
    pub data_start: u32,
    /// Sectors in the volume.
    pub sectors: u32,
    /// Data clusters, numbered from 2.
    pub clusters: u32,
    pub fat16: bool,
}

impl Layout {
    /// The layout given by a BPB: boot sector bytes 11 to 35.
    pub fn from_bpb(b: &[u8; 25]) -> Result<Layout> {
        let bps = u16_at(b, 0);
        let spc = b[2];
        let reserved = u16_at(b, 3);
        let fats = b[5];
        let root_entries = u16_at(b, 6);
        let media = b[10];
        let fat_sectors = u16_at(b, 11);
        let sectors = match u16_at(b, 8) {
            0 => u32_at(b, 21),
            n => u32::from(n),
        };
        ensure!(
            bps as usize == SECTOR,
            "{bps} bytes per sector, only 512 is supported"
        );
        ensure!(spc.is_power_of_two(), "{spc} sectors per cluster");
        ensure!(reserved >= 1, "no reserved sectors");
        ensure!((1..=2).contains(&fats), "{fats} FATs");
        ensure!(media >= 0xF0, "media descriptor {media:#04x}");
        if fat_sectors == 0 || root_entries == 0 {
            bail!("FAT32 is not supported");
        }
        let root_sectors = (u32::from(root_entries) * 32).div_ceil(SECTOR as u32);
        let root_start = u32::from(reserved) + u32::from(fats) * u32::from(fat_sectors);
        let data_start = root_start + root_sectors;
        ensure!(sectors > data_start, "{sectors} sectors is too small");
        let clusters = (sectors - data_start) / u32::from(spc);
        ensure!(clusters < 65525, "{clusters} clusters is FAT32");
        Ok(Layout {
            sectors_per_cluster: u32::from(spc),
            fat_start: u32::from(reserved),
            fat_sectors: u32::from(fat_sectors),
            fats: u32::from(fats),
            root_start,
            root_entries: u32::from(root_entries),
            root_sectors,
            data_start,
            sectors,
            clusters,
            fat16: clusters >= 4085,
        })
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR
    }

    fn cluster_sector(&self, cluster: u16) -> u32 {
        self.data_start + (u32::from(cluster) - 2) * self.sectors_per_cluster
    }

    /// True if the volume is the standard 1.44MB floppy layout.
    pub fn is_floppy(&self) -> bool {
        self.sectors == FLOPPY_SECTORS && self.root_entries == FLOPPY_ROOT_ENTRIES
    }
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

/// A FAT date and time, in local time with 2-second steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DosTime {
    pub date: u16,
    pub time: u16,
}

impl DosTime {
    /// The earliest time FAT can hold.
    pub const MIN: DosTime = DosTime {
        date: 1 << 5 | 1,
        time: 0,
    };
    /// The latest time FAT can hold: 2107-12-31 23:59:58.
    pub const MAX: DosTime = DosTime {
        date: 127 << 9 | 12 << 5 | 31,
        time: 23 << 11 | 59 << 5 | 29,
    };

    /// A time from its parts. `year` must be 1980 to 2107; odd seconds are
    /// rounded down.
    pub fn new(year: u16, month: u16, day: u16, hour: u16, minute: u16, second: u16) -> DosTime {
        debug_assert!((1980..=2107).contains(&year));
        DosTime {
            date: (year - 1980) << 9 | month << 5 | day,
            time: hour << 11 | minute << 5 | (second / 2),
        }
    }
}

impl fmt::Display for DosTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (d, t) = (self.date, self.time);
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            1980 + (d >> 9),
            d >> 5 & 0x0F,
            d & 0x1F,
            t >> 11,
            t >> 5 & 0x3F,
            (t & 0x1F) * 2
        )
    }
}

const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_LABEL: u8 = 0x08;
const ATTR_DIR: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = 0x0F;

// --- building --------------------------------------------------------------

/// A file or directory to put in a new volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// The 8.3 name as stored: 8 name and 3 extension bytes, blank padded.
    pub name: [u8; 11],
    pub time: DosTime,
    pub kind: NodeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    File(Vec<u8>),
    Dir(Vec<Node>),
}

/// The largest volume built: the 16-bit sector count of the BPB, just under
/// 32MB, which is as large as Interloper has served.
pub const MAX_SECTORS: u32 = 65535;
const FLOPPY_SECTORS: u32 = 2880;
const FLOPPY_ROOT_ENTRIES: u32 = 224;
const DISK_ROOT_ENTRIES: u32 = 512;
/// FAT12 below this many clusters and FAT16 above, well clear of 4085,
/// where some FAT drivers disagree about the type.
const FAT12_MAX_CLUSTERS: u32 = 4000;
const FAT16_MIN_CLUSTERS: u32 = 4200;

/// Build a volume holding `root`: the standard 1.44MB floppy if it fits,
/// otherwise the smallest fixed-disk volume that holds it. The volume has
/// no label, and a serial number derived from its contents, so the same
/// tree always gives the same image.
pub fn build(root: &[Node]) -> Result<Vec<u8>> {
    let boot = choose_boot_sector(root)?;
    let layout = Layout::from_bpb(boot[11..36].try_into().unwrap())?;
    let mut w = Writer {
        img: vec![0u8; layout.sectors as usize * SECTOR],
        fat: vec![0u16; layout.clusters as usize + 2],
        next: 2,
        layout,
    };
    w.img[..SECTOR].copy_from_slice(&boot);
    let root_bytes = w.dir_bytes(root, None);
    let at = w.layout.root_start as usize * SECTOR;
    w.img[at..at + root_bytes.len()].copy_from_slice(&root_bytes);
    w.write_fats(boot[21]);
    let serial = fnv1a(&w.img);
    w.img[39..43].copy_from_slice(&serial.to_le_bytes());
    Ok(w.img)
}

/// Clusters that the files and subdirectories in `nodes` need.
fn clusters_needed(nodes: &[Node], cluster_bytes: usize) -> u64 {
    nodes
        .iter()
        .map(|n| match &n.kind {
            NodeKind::File(data) => data.len().div_ceil(cluster_bytes) as u64,
            NodeKind::Dir(kids) => {
                dir_clusters(kids.len(), cluster_bytes) + clusters_needed(kids, cluster_bytes)
            }
        })
        .sum()
}

/// Clusters a subdirectory with `entries` entries needs, with `.` and `..`.
fn dir_clusters(entries: usize, cluster_bytes: usize) -> u64 {
    ((entries + 2) * 32).div_ceil(cluster_bytes) as u64
}

fn choose_boot_sector(root: &[Node]) -> Result<[u8; SECTOR]> {
    let floppy_clusters = FLOPPY_SECTORS - 33;
    if root.len() <= FLOPPY_ROOT_ENTRIES as usize
        && clusters_needed(root, SECTOR) <= u64::from(floppy_clusters)
    {
        return Ok(boot_sector(&Format {
            sectors_per_cluster: 1,
            root_entries: FLOPPY_ROOT_ENTRIES,
            sectors: FLOPPY_SECTORS,
            media: 0xF0,
            fat_sectors: 9,
            sectors_per_track: 18,
            heads: 2,
            drive: 0x00,
            fat16: false,
        }));
    }
    ensure!(
        root.len() <= DISK_ROOT_ENTRIES as usize,
        "{} entries in the top directory, at most {DISK_ROOT_ENTRIES} fit",
        root.len()
    );
    let root_sectors = DISK_ROOT_ENTRIES * 32 / SECTOR as u32;
    let mut best: Option<Format> = None;
    let mut smallest_need = u64::MAX;
    // Cluster sizes DOS 6.22's FORMAT uses: up to 4KB for FAT12, and 2KB for
    // FAT16 below 128MB.
    for spc in [1u32, 2, 4, 8] {
        let need = clusters_needed(root, spc as usize * SECTOR).max(1);
        let (clusters, fat16) = if need <= u64::from(FAT12_MAX_CLUSTERS) {
            (need as u32, false)
        } else if spc <= 4 && need < 65525 {
            ((need as u32).max(FAT16_MIN_CLUSTERS), true)
        } else {
            continue;
        };
        let fat_bytes = if fat16 {
            (clusters + 2) * 2
        } else {
            ((clusters + 2) * 3).div_ceil(2)
        };
        let fat_sectors = fat_bytes.div_ceil(SECTOR as u32);
        let sectors = 1 + 2 * fat_sectors + root_sectors + clusters * spc;
        smallest_need = smallest_need.min(u64::from(sectors));
        if sectors <= MAX_SECTORS && best.as_ref().is_none_or(|b| sectors < b.sectors) {
            best = Some(Format {
                sectors_per_cluster: spc,
                root_entries: DISK_ROOT_ENTRIES,
                sectors,
                media: 0xF8,
                fat_sectors,
                sectors_per_track: 63,
                heads: 16,
                drive: 0x80,
                fat16,
            });
        }
    }
    match best {
        Some(f) => Ok(boot_sector(&f)),
        None => bail!(
            "too large: needs a volume of about {} KB, at most {} KB is supported",
            smallest_need * SECTOR as u64 / 1024,
            u64::from(MAX_SECTORS) * SECTOR as u64 / 1024
        ),
    }
}

struct Format {
    sectors_per_cluster: u32,
    root_entries: u32,
    sectors: u32,
    media: u8,
    fat_sectors: u32,
    sectors_per_track: u16,
    heads: u16,
    drive: u8,
    fat16: bool,
}

fn boot_sector(f: &Format) -> [u8; SECTOR] {
    let mut b = [0u8; SECTOR];
    b[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    b[3..11].copy_from_slice(b"MSDOS5.0");
    b[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    b[13] = f.sectors_per_cluster as u8;
    b[14..16].copy_from_slice(&1u16.to_le_bytes());
    b[16] = 2;
    b[17..19].copy_from_slice(&(f.root_entries as u16).to_le_bytes());
    b[19..21].copy_from_slice(&(f.sectors as u16).to_le_bytes());
    b[21] = f.media;
    b[22..24].copy_from_slice(&(f.fat_sectors as u16).to_le_bytes());
    b[24..26].copy_from_slice(&f.sectors_per_track.to_le_bytes());
    b[26..28].copy_from_slice(&f.heads.to_le_bytes());
    b[36] = f.drive;
    b[38] = 0x29;
    b[43..54].copy_from_slice(b"NO NAME    ");
    b[54..62].copy_from_slice(if f.fat16 { b"FAT16   " } else { b"FAT12   " });
    // Not bootable: halt if started.
    b[62..65].copy_from_slice(&[0xF4, 0xEB, 0xFD]);
    b[510..512].copy_from_slice(&[0x55, 0xAA]);
    b
}

struct Writer {
    img: Vec<u8>,
    fat: Vec<u16>,
    next: u32,
    layout: Layout,
}

impl Writer {
    /// Allocate `n` contiguous clusters as a chain; 0 for none.
    fn alloc(&mut self, n: u64) -> u16 {
        if n == 0 {
            return 0;
        }
        let first = self.next;
        let last = first + n as u32 - 1;
        assert!(last <= self.layout.clusters + 1, "volume too small");
        for c in first..last {
            self.fat[c as usize] = (c + 1) as u16;
        }
        self.fat[last as usize] = 0xFFFF;
        self.next = last + 1;
        first as u16
    }

    fn write_at_cluster(&mut self, cluster: u16, data: &[u8]) {
        let at = self.layout.cluster_sector(cluster) as usize * SECTOR;
        self.img[at..at + data.len()].copy_from_slice(data);
    }

    /// The entries of a directory holding `nodes`, writing the files and
    /// subdirectories they name. `this` is the directory's own cluster and
    /// its parent's (0 for the root), or `None` for the root.
    fn dir_bytes(&mut self, nodes: &[Node], this: Option<(u16, u16, DosTime)>) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some((own, parent, time)) = this {
            out.extend(dir_entry(b".          ", ATTR_DIR, time, own, 0));
            out.extend(dir_entry(b"..         ", ATTR_DIR, time, parent, 0));
        }
        let own = this.map_or(0, |(own, _, _)| own);
        let cluster_bytes = self.layout.cluster_bytes();
        for node in nodes {
            match &node.kind {
                NodeKind::File(data) => {
                    let first = self.alloc(data.len().div_ceil(cluster_bytes) as u64);
                    if first != 0 {
                        self.write_at_cluster(first, data);
                    }
                    out.extend(dir_entry(
                        &node.name,
                        ATTR_ARCHIVE,
                        node.time,
                        first,
                        data.len() as u32,
                    ));
                }
                NodeKind::Dir(kids) => {
                    let first = self.alloc(dir_clusters(kids.len(), cluster_bytes));
                    let bytes = self.dir_bytes(kids, Some((first, own, node.time)));
                    self.write_at_cluster(first, &bytes);
                    out.extend(dir_entry(&node.name, ATTR_DIR, node.time, first, 0));
                }
            }
        }
        out
    }

    fn write_fats(&mut self, media: u8) {
        let mut fat = vec![0u8; self.layout.fat_sectors as usize * SECTOR];
        self.fat[0] = 0xFF00 | u16::from(media);
        self.fat[1] = 0xFFFF;
        for (i, &v) in self.fat.iter().enumerate() {
            if self.layout.fat16 {
                fat[2 * i..2 * i + 2].copy_from_slice(&v.to_le_bytes());
            } else {
                let v = v & 0x0FFF;
                let at = i * 3 / 2;
                if i % 2 == 0 {
                    fat[at] = v as u8;
                    fat[at + 1] = fat[at + 1] & 0xF0 | (v >> 8) as u8;
                } else {
                    fat[at] = fat[at] & 0x0F | (v << 4) as u8;
                    fat[at + 1] = (v >> 4) as u8;
                }
            }
        }
        for n in 0..self.layout.fats {
            let at = (self.layout.fat_start + n * self.layout.fat_sectors) as usize * SECTOR;
            self.img[at..at + fat.len()].copy_from_slice(&fat);
        }
    }
}

fn dir_entry(name: &[u8; 11], attr: u8, time: DosTime, cluster: u16, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    e[22..24].copy_from_slice(&time.time.to_le_bytes());
    e[24..26].copy_from_slice(&time.date.to_le_bytes());
    e[26..28].copy_from_slice(&cluster.to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

fn fnv1a(data: &[u8]) -> u32 {
    data.iter().fold(0x811C_9DC5, |h, &b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

// --- reading ---------------------------------------------------------------

/// A file or directory found in a volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Short names from the root, as DOS shows them ("README.TXT").
    pub path: Vec<String>,
    pub dir: bool,
    pub size: u32,
    pub time: DosTime,
    pub attr: u8,
    cluster: u16,
}

impl Entry {
    /// The path with `/` separators.
    pub fn path_string(&self) -> String {
        self.path.join("/")
    }

    pub fn read_only(&self) -> bool {
        self.attr & ATTR_READ_ONLY != 0
    }
}

/// A FAT volume read through a [`BlockDevice`], such as an image the server
/// would serve.
pub struct Volume<'a> {
    dev: &'a dyn BlockDevice,
    layout: Layout,
    /// The first FAT, with FAT12 values widened so that end-of-chain and
    /// bad-cluster values match FAT16's.
    fat: Vec<u16>,
}

impl<'a> Volume<'a> {
    pub fn open(dev: &'a dyn BlockDevice) -> Result<Volume<'a>> {
        let layout = Layout::from_bpb(&dev.media_info().bpb)?;
        let raw = read(dev, layout.fat_start, layout.fat_sectors).context("reading the FAT")?;
        let n = layout.clusters as usize + 2;
        let fat = if layout.fat16 {
            ensure!(raw.len() >= 2 * n, "the FAT is too small for {n} clusters");
            (0..n).map(|i| u16_at(&raw, 2 * i)).collect()
        } else {
            ensure!(
                raw.len() > n * 3 / 2,
                "the FAT is too small for {n} clusters"
            );
            (0..n)
                .map(|i| {
                    let v = u16_at(&raw, i * 3 / 2);
                    let v = if i % 2 == 0 { v & 0x0FFF } else { v >> 4 };
                    if v >= 0x0FF7 { v | 0xF000 } else { v }
                })
                .collect()
        };
        Ok(Volume { dev, layout, fat })
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Every file and directory, depth first in directory order.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let root = read(self.dev, self.layout.root_start, self.layout.root_sectors)
            .context("reading the root directory")?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.walk(&root, &[], &mut seen, &mut out)?;
        Ok(out)
    }

    fn walk(
        &self,
        dir: &[u8],
        path: &[String],
        seen: &mut HashSet<u16>,
        out: &mut Vec<Entry>,
    ) -> Result<()> {
        for e in dir.as_chunks::<32>().0 {
            match e[0] {
                0x00 => break,
                0xE5 => continue,
                b'.' => continue, // `.` and `..`
                _ => {}
            }
            let attr = e[11];
            if attr == ATTR_LONG_NAME || attr & ATTR_LABEL != 0 {
                continue;
            }
            let mut entry_path = path.to_vec();
            entry_path.push(short_name_string(e[..11].try_into().unwrap()));
            let entry = Entry {
                path: entry_path,
                dir: attr & ATTR_DIR != 0,
                size: u32_at(e, 28),
                time: DosTime {
                    time: u16_at(e, 22),
                    date: u16_at(e, 24),
                },
                attr,
                cluster: u16_at(e, 26),
            };
            let shown = entry.path_string();
            if entry.dir {
                ensure!(entry.cluster != 0, "{shown}: directory with no clusters");
                ensure!(
                    seen.insert(entry.cluster),
                    "{shown}: directory loop (cluster {} is used twice)",
                    entry.cluster
                );
                let data = self
                    .read_chain(entry.cluster, None)
                    .with_context(|| shown.clone())?;
                let kids = entry.path.clone();
                out.push(entry);
                self.walk(&data, &kids, seen, out)?;
            } else {
                out.push(entry);
            }
        }
        Ok(())
    }

    /// The contents of a file.
    pub fn read_file(&self, entry: &Entry) -> Result<Vec<u8>> {
        ensure!(!entry.dir, "{}: a directory", entry.path_string());
        if entry.size == 0 {
            return Ok(Vec::new());
        }
        self.read_chain(entry.cluster, Some(entry.size as usize))
            .with_context(|| entry.path_string())
    }

    /// The clusters of a chain, or the first `size` bytes of them.
    fn read_chain(&self, first: u16, size: Option<usize>) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut c = first;
        let mut seen = HashSet::new();
        loop {
            ensure!(
                (2..=self.layout.clusters + 1).contains(&u32::from(c)),
                "cluster {c} is outside the volume"
            );
            ensure!(seen.insert(c), "cluster chain loops at cluster {c}");
            out.extend(read(
                self.dev,
                self.layout.cluster_sector(c),
                self.layout.sectors_per_cluster,
            )?);
            if size.is_some_and(|s| out.len() >= s) {
                break;
            }
            match self.fat[c as usize] {
                0xFFF8..=0xFFFF => break,
                next => c = next,
            }
        }
        if let Some(size) = size {
            ensure!(
                out.len() >= size,
                "the file has {size} bytes but its clusters hold {}",
                out.len()
            );
            out.truncate(size);
        }
        Ok(out)
    }
}

fn read(dev: &dyn BlockDevice, start: u32, count: u32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; count as usize * SECTOR];
    dev.read_sectors(start, &mut buf)
        .map_err(|e| anyhow::anyhow!("reading sectors {start}+{count}: {e:?}"))?;
    Ok(buf)
}

/// An 8.3 directory entry name as DOS shows it ("README.TXT"). Bytes
/// outside printable ASCII are shown as `\xNN`.
fn short_name_string(raw: &[u8; 11]) -> String {
    let show = |b: &[u8]| -> String {
        let mut s = String::new();
        for (i, &c) in b.iter().enumerate() {
            let c = if i == 0 && c == 0x05 { 0xE5 } else { c };
            if (0x20..0x7F).contains(&c) {
                s.push(char::from(c));
            } else {
                s.push_str(&format!("\\x{c:02x}"));
            }
        }
        s.trim_end_matches(' ').to_string()
    };
    let (name, ext) = (show(&raw[..8]), show(&raw[8..]));
    if ext.is_empty() {
        name
    } else {
        format!("{name}.{ext}")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::image::ImageDevice;
    use crate::image::tests::temp_image;

    pub(crate) fn name(s: &str) -> [u8; 11] {
        let (base, ext) = s.split_once('.').unwrap_or((s, ""));
        let mut n = [b' '; 11];
        n[..base.len()].copy_from_slice(base.as_bytes());
        n[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
        n
    }

    fn file(n: &str, data: Vec<u8>) -> Node {
        Node {
            name: name(n),
            time: DosTime::new(2026, 10, 23, 9, 7, 3),
            kind: NodeKind::File(data),
        }
    }

    fn dir(n: &str, kids: Vec<Node>) -> Node {
        Node {
            name: name(n),
            time: DosTime::new(1999, 12, 31, 23, 59, 58),
            kind: NodeKind::Dir(kids),
        }
    }

    /// Open `img` as the server would, and read back every file.
    fn read_back(img: &[u8]) -> (Layout, Vec<(Entry, Vec<u8>)>) {
        let path = temp_image(img);
        let dev = ImageDevice::open(&path).unwrap();
        let vol = Volume::open(&dev).unwrap();
        let files = vol
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| {
                let data = if e.dir {
                    Vec::new()
                } else {
                    vol.read_file(&e).unwrap()
                };
                (e, data)
            })
            .collect();
        let layout = vol.layout().clone();
        std::fs::remove_file(path).unwrap();
        (layout, files)
    }

    fn binary(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn round_trip_on_a_floppy() {
        let tree = vec![
            file("README.TXT", b"hello\r\n".to_vec()),
            file("EMPTY.DAT", Vec::new()),
            file("BIG.BIN", binary(66_536)),
            dir(
                "SUB",
                vec![
                    file("NESTED.TXT", b"nested".to_vec()),
                    dir("DEEP", vec![file("X", binary(513))]),
                    dir("EMPTYDIR", vec![]),
                ],
            ),
        ];
        let img = build(&tree).unwrap();
        assert_eq!(img.len(), 1_474_560);
        assert_eq!(build(&tree).unwrap(), img, "not reproducible");
        let (layout, files) = read_back(&img);
        assert!(layout.is_floppy() && !layout.fat16);
        let got: Vec<(String, bool, Vec<u8>)> = files
            .iter()
            .map(|(e, d)| (e.path_string(), e.dir, d.clone()))
            .collect();
        let want = [
            ("README.TXT", false, b"hello\r\n".to_vec()),
            ("EMPTY.DAT", false, Vec::new()),
            ("BIG.BIN", false, binary(66_536)),
            ("SUB", true, Vec::new()),
            ("SUB/NESTED.TXT", false, b"nested".to_vec()),
            ("SUB/DEEP", true, Vec::new()),
            ("SUB/DEEP/X", false, binary(513)),
            ("SUB/EMPTYDIR", true, Vec::new()),
        ];
        let want: Vec<(String, bool, Vec<u8>)> = want
            .into_iter()
            .map(|(p, d, v)| (p.to_string(), d, v))
            .collect();
        assert_eq!(got, want);
        let (readme, sub) = (&files[0].0, &files[3].0);
        assert_eq!(readme.time.to_string(), "2026-10-23 09:07:02");
        assert_eq!(sub.time.to_string(), "1999-12-31 23:59:58");
        assert_eq!(readme.attr, ATTR_ARCHIVE);
    }

    #[test]
    fn layouts() {
        // Too much for a floppy: the smallest fixed-disk FAT12 volume.
        let (l, files) = read_back(&build(&[file("A.BIN", binary(2_000_000))]).unwrap());
        assert!(!l.is_floppy() && !l.fat16);
        assert!(u64::from(l.sectors) * 512 < 2_100_000, "{l:?}");
        assert_eq!(files[0].1, binary(2_000_000));
        // More top-level entries than a floppy's root holds.
        let many: Vec<Node> = (0..300).map(|i| file(&format!("F{i}"), vec![])).collect();
        let (l, files) = read_back(&build(&many).unwrap());
        assert!(!l.is_floppy() && l.root_entries == 512);
        assert_eq!(files.len(), 300);
        // Large files: FAT12 with clusters no larger than 4KB.
        let tree: Vec<Node> = (0..8)
            .map(|i| file(&format!("F{i}"), binary(1_000_000)))
            .collect();
        let (l, files) = read_back(&build(&tree).unwrap());
        assert!(!l.fat16 && l.sectors_per_cluster <= 8, "{l:?}");
        assert!(files.iter().all(|(_, d)| *d == binary(1_000_000)));
        // Too many clusters for FAT12: FAT16.
        let small: Vec<Node> = (0..4500)
            .map(|i| file(&format!("F{i}"), vec![i as u8]))
            .collect();
        let (l, files) = read_back(&build(&[dir("MANY", small)]).unwrap());
        assert!(l.fat16 && l.clusters >= FAT16_MIN_CLUSTERS, "{l:?}");
        assert_eq!(files.len(), 4501);
        assert_eq!(files[4500].0.path_string(), "MANY/F4499");
        assert_eq!(files[4500].1, [4499u32 as u8]);
    }

    #[test]
    fn limits() {
        let tree: Vec<Node> = (0..34)
            .map(|i| file(&format!("F{i}"), binary(1_000_000)))
            .collect();
        let err = build(&tree).unwrap_err().to_string();
        assert!(err.contains("too large"), "{err}");
        let many: Vec<Node> = (0..513).map(|i| file(&format!("F{i}"), vec![])).collect();
        let err = build(&many).unwrap_err().to_string();
        assert!(err.contains("513 entries"), "{err}");
    }

    #[test]
    fn corrupt_volumes() {
        let tree = vec![dir("SUB", vec![file("A.TXT", b"a".to_vec())])];
        let img = build(&tree).unwrap();
        let root = 19 * SECTOR;
        let sub_cluster = u16_at(&img, root + 26);
        // A subdirectory of SUB at SUB's own cluster, in place of A.TXT.
        let mut looped = img.clone();
        let sub = (33 + usize::from(sub_cluster) - 2) * SECTOR;
        looped[sub + 64..sub + 96].copy_from_slice(&dir_entry(
            &name("LOOP"),
            ATTR_DIR,
            DosTime::MIN,
            sub_cluster,
            0,
        ));
        // A.TXT's cluster outside the volume.
        let mut outside = img.clone();
        outside[sub + 64 + 26..sub + 64 + 28].copy_from_slice(&4000u16.to_le_bytes());
        for (img, want) in [(looped, "directory loop"), (outside, "outside the volume")] {
            let path = temp_image(&img);
            let dev = ImageDevice::open(&path).unwrap();
            let vol = Volume::open(&dev).unwrap();
            let read_all = vol.entries().and_then(|entries| {
                entries
                    .iter()
                    .filter(|e| !e.dir)
                    .try_for_each(|e| vol.read_file(e).map(drop))
            });
            let err = format!("{:#}", read_all.unwrap_err());
            assert!(err.contains(want), "{err}");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn short_names() {
        assert_eq!(short_name_string(&name("README.TXT")), "README.TXT");
        assert_eq!(short_name_string(&name("NOEXT")), "NOEXT");
        let mut raw = name("XBC.TXT");
        raw[0] = 0x05;
        raw[1] = 0x81;
        assert_eq!(short_name_string(&raw), "\\xe5\\x81C.TXT");
    }

    #[test]
    fn dos_time_limits() {
        assert_eq!(DosTime::MIN.to_string(), "1980-01-01 00:00:00");
        assert_eq!(DosTime::MAX.to_string(), "2107-12-31 23:59:58");
        assert_eq!(
            DosTime::new(2107, 12, 31, 23, 59, 59),
            DosTime::MAX,
            "odd seconds round down"
        );
    }
}
