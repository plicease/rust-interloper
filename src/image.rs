//! Block devices served to INTERLNK, and the read-only FAT image backend.
//!
//! INTERLNK reads raw sectors numbered from the volume's boot sector, so a
//! device is a FAT12 or FAT16 *volume*. An image may be a bare volume, or a
//! whole disk with an MBR, in which case the first FAT partition is served.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::proto::{DeviceError, SECTOR};

/// What INTERLNK needs to know about a volume, taken from its boot sector
/// and root directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaInfo {
    /// The DOS 4 BPB, boot sector bytes 11 to 35.
    pub bpb: [u8; 25],
    pub media: u8,
    /// Sectors in the volume.
    pub sectors: u32,
    /// Volume label from the root directory, as DOS shows it ("DOS622",
    /// "NAME.EXT"), or empty.
    pub label: String,
    /// Volume serial number from the extended BPB, or 0.
    pub serial: u32,
    /// Label and file system type fields of the extended BPB, blank padded.
    pub boot_label: [u8; 11],
    pub fs_type: [u8; 8],
    /// Floppy-like media (descriptor other than `f8`).
    pub removable: bool,
}

/// A device that serves sectors. Writes are refused unless a backend
/// supports them.
pub trait BlockDevice: Send {
    fn media_info(&self) -> &MediaInfo;

    /// Read `dst.len() / 512` sectors from volume sector `start`.
    fn read_sectors(&self, start: u32, dst: &mut [u8]) -> std::result::Result<(), DeviceError>;

    fn write_sectors(&mut self, _start: u32, _data: &[u8]) -> std::result::Result<(), DeviceError> {
        Err(DeviceError::WriteProtect)
    }
}

/// A raw FAT image file, opened read-only.
pub struct ImageDevice {
    path: PathBuf,
    file: File,
    /// Byte offset of the volume within the file.
    offset: u64,
    info: MediaInfo,
}

const FAT_PARTITION_TYPES: [u8; 4] = [0x01, 0x04, 0x06, 0x0E];

impl ImageDevice {
    pub fn open(path: &Path) -> Result<ImageDevice> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len();
        let mut boot = [0u8; SECTOR];
        file.read_exact_at(&mut boot, 0)
            .with_context(|| format!("{}: too short for a boot sector", path.display()))?;

        let (offset, boot) = match parse_bpb(&boot) {
            Ok(_) => (0, boot),
            Err(volume_err) => {
                let start = mbr_fat_partition(&boot).with_context(|| {
                    format!(
                        "{}: not a FAT volume ({volume_err}) or a disk with an MBR",
                        path.display()
                    )
                })?;
                let offset = u64::from(start) * SECTOR as u64;
                let mut pboot = [0u8; SECTOR];
                file.read_exact_at(&mut pboot, offset)
                    .with_context(|| format!("{}: partition outside the image", path.display()))?;
                (offset, pboot)
            }
        };
        let bpb = parse_bpb(&boot).with_context(|| format!("{}", path.display()))?;
        let end = offset + u64::from(bpb.sectors) * SECTOR as u64;
        ensure!(
            end <= len,
            "{}: volume of {} sectors needs {end} bytes, file has {len}",
            path.display(),
            bpb.sectors
        );

        let mut root = vec![0u8; bpb.root_sectors as usize * SECTOR];
        file.read_exact_at(
            &mut root,
            offset + u64::from(bpb.root_start) * SECTOR as u64,
        )?;

        let extended = boot[38] == 0x29;
        let info = MediaInfo {
            bpb: boot[11..36].try_into().unwrap(),
            media: boot[21],
            sectors: bpb.sectors,
            label: root_label(&root),
            serial: if extended { u32_at(&boot, 39) } else { 0 },
            boot_label: if extended {
                boot[43..54].try_into().unwrap()
            } else {
                *b"NO NAME    "
            },
            fs_type: if extended {
                boot[54..62].try_into().unwrap()
            } else if bpb.fat16 {
                *b"FAT16   "
            } else {
                *b"FAT12   "
            },
            removable: boot[21] != 0xF8,
        };
        Ok(ImageDevice {
            path: path.to_path_buf(),
            file,
            offset,
            info,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// First sector of the volume within the image file.
    pub fn first_sector(&self) -> u64 {
        self.offset / SECTOR as u64
    }
}

impl BlockDevice for ImageDevice {
    fn media_info(&self) -> &MediaInfo {
        &self.info
    }

    fn read_sectors(&self, start: u32, dst: &mut [u8]) -> std::result::Result<(), DeviceError> {
        if !dst.len().is_multiple_of(SECTOR) {
            return Err(DeviceError::BadRequest);
        }
        let count = (dst.len() / SECTOR) as u64;
        if u64::from(start) + count > u64::from(self.info.sectors) {
            return Err(DeviceError::SectorNotFound);
        }
        let pos = self.offset + u64::from(start) * SECTOR as u64;
        self.file
            .read_exact_at(dst, pos)
            .map_err(|_| DeviceError::ReadFault)
    }
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

/// The layout facts a FAT12/16 boot sector gives.
#[derive(Debug)]
struct Bpb {
    sectors: u32,
    root_start: u32,
    root_sectors: u32,
    fat16: bool,
}

fn parse_bpb(b: &[u8; SECTOR]) -> Result<Bpb> {
    let bps = u16_at(b, 11);
    let spc = b[13];
    let reserved = u16_at(b, 14);
    let fats = b[16];
    let root_entries = u16_at(b, 17);
    let media = b[21];
    let fat_sectors = u16_at(b, 22);
    let sectors = match u16_at(b, 19) {
        0 => u32_at(b, 32),
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
    Ok(Bpb {
        sectors,
        root_start,
        root_sectors,
        fat16: clusters >= 4085,
    })
}

/// Start sector of the first FAT12/16 partition in an MBR.
fn mbr_fat_partition(b: &[u8; SECTOR]) -> Result<u32> {
    ensure!(b[510..512] == [0x55, 0xAA], "no boot signature");
    for i in 0..4 {
        let entry = &b[446 + 16 * i..462 + 16 * i];
        if FAT_PARTITION_TYPES.contains(&entry[4]) && u32_at(entry, 8) > 0 {
            return Ok(u32_at(entry, 8));
        }
    }
    bail!("no FAT12/FAT16 partition")
}

/// The volume label entry of a root directory, formatted as DOS shows it.
fn root_label(root: &[u8]) -> String {
    for entry in root.as_chunks::<32>().0 {
        match entry[0] {
            0x00 => break,
            0xE5 => continue,
            _ => {}
        }
        if entry[11] & 0x08 != 0 && entry[11] != 0x0F {
            let text = |b: &[u8]| String::from_utf8_lossy(b).trim_end().to_string();
            let (name, ext) = (text(&entry[..8]), text(&entry[8..11]));
            return if ext.is_empty() {
                name
            } else {
                format!("{name}.{ext}")
            };
        }
    }
    String::new()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A blank 1.44MB FAT12 volume: sector *n* of the data area is filled
    /// with the byte *n*, so reads can be checked.
    pub(crate) fn floppy(label: &[u8; 11]) -> Vec<u8> {
        let mut v = vec![0u8; 2880 * SECTOR];
        let b = &mut v[..SECTOR];
        b[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
        b[3..11].copy_from_slice(b"TESTTEST");
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = 1;
        b[14..16].copy_from_slice(&1u16.to_le_bytes());
        b[16] = 2;
        b[17..19].copy_from_slice(&224u16.to_le_bytes());
        b[19..21].copy_from_slice(&2880u16.to_le_bytes());
        b[21] = 0xF0;
        b[22..24].copy_from_slice(&9u16.to_le_bytes());
        b[24..26].copy_from_slice(&18u16.to_le_bytes());
        b[26..28].copy_from_slice(&2u16.to_le_bytes());
        b[38] = 0x29;
        b[39..43].copy_from_slice(&0x1C0F2216u32.to_le_bytes());
        b[43..54].copy_from_slice(label);
        b[54..62].copy_from_slice(b"FAT12   ");
        b[510..512].copy_from_slice(&[0x55, 0xAA]);
        for fat in [1, 10] {
            v[fat * SECTOR..fat * SECTOR + 3].copy_from_slice(&[0xF0, 0xFF, 0xFF]);
        }
        let root = 19 * SECTOR;
        v[root..root + 11].copy_from_slice(label);
        v[root + 11] = 0x08;
        for s in 33..2880 {
            v[s * SECTOR..(s + 1) * SECTOR].fill(s as u8);
        }
        v
    }

    /// The same volume with a fixed-disk media descriptor, as a whole disk
    /// with partition 2 starting at sector 63.
    pub(crate) fn disk(label: &[u8; 11]) -> Vec<u8> {
        let mut volume = floppy(label);
        volume[21] = 0xF8;
        for fat in [1, 10] {
            volume[fat * SECTOR] = 0xF8;
        }
        let mut v = vec![0u8; 63 * SECTOR];
        let entry = 446 + 16;
        v[entry + 4] = 0x01;
        v[entry + 8..entry + 12].copy_from_slice(&63u32.to_le_bytes());
        v[entry + 12..entry + 16].copy_from_slice(&2880u32.to_le_bytes());
        v[510..512].copy_from_slice(&[0x55, 0xAA]);
        v.extend(volume);
        v
    }

    pub(crate) fn temp_image(data: &[u8]) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "interloper-test-{}-{}.img",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn volume_image() {
        let path = temp_image(&floppy(b"FIXTURE    "));
        let dev = ImageDevice::open(&path).unwrap();
        let info = dev.media_info();
        assert_eq!(dev.first_sector(), 0);
        assert_eq!(info.sectors, 2880);
        assert_eq!(info.media, 0xF0);
        assert_eq!(info.label, "FIXTURE");
        assert_eq!(info.serial, 0x1C0F2216);
        assert_eq!(&info.fs_type, b"FAT12   ");
        assert!(info.removable);
        // The BPB as INTERSVR sent it for the fixture floppy.
        assert_eq!(
            info.bpb,
            [
                0x00, 0x02, 0x01, 0x01, 0x00, 0x02, 0xE0, 0x00, 0x40, 0x0B, 0xF0, 0x09, 0x00, 0x12,
                0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
            ]
        );
        let mut buf = vec![0u8; 2 * SECTOR];
        dev.read_sectors(40, &mut buf).unwrap();
        assert!(buf[..SECTOR].iter().all(|&b| b == 40));
        assert!(buf[SECTOR..].iter().all(|&b| b == 41));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disk_image_serves_partition() {
        let path = temp_image(&disk(b"LONGNAMEEXT"));
        let dev = ImageDevice::open(&path).unwrap();
        assert_eq!(dev.first_sector(), 63);
        assert_eq!(dev.media_info().label, "LONGNAME.EXT");
        let mut buf = vec![0u8; SECTOR];
        dev.read_sectors(0, &mut buf).unwrap();
        assert_eq!(&buf[3..11], b"TESTTEST");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn out_of_range_and_writes() {
        let path = temp_image(&floppy(b"NO NAME    "));
        let mut dev = ImageDevice::open(&path).unwrap();
        let mut buf = vec![0u8; 2 * SECTOR];
        assert_eq!(
            dev.read_sectors(2879, &mut buf),
            Err(DeviceError::SectorNotFound)
        );
        assert_eq!(
            dev.read_sectors(u32::MAX, &mut buf),
            Err(DeviceError::SectorNotFound)
        );
        assert_eq!(dev.write_sectors(0, &buf), Err(DeviceError::WriteProtect));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_bad_images() {
        let mut short = floppy(b"NO NAME    ");
        short.truncate(1000 * SECTOR);
        let mut fat32 = floppy(b"NO NAME    ");
        fat32[22] = 0;
        fat32[23] = 0;
        let mut blank = vec![0u8; 4096];
        blank[510] = 0x55;
        blank[511] = 0xAA;
        for (data, want) in [
            (short, "needs"),
            (fat32, "not a FAT volume"),
            (blank, "no FAT12/FAT16 partition"),
            (vec![0u8; 100], "too short"),
        ] {
            let path = temp_image(&data);
            let err = format!("{:#}", ImageDevice::open(&path).err().unwrap());
            assert!(err.contains(want), "{err}");
            std::fs::remove_file(path).unwrap();
        }
    }
}
