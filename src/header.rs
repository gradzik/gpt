//! GPT-header object and helper functions.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Error, ErrorKind, Read, Result, Seek, SeekFrom, Write};
use std::path::Path;

extern crate crc;
extern crate itertools;

use self::itertools::Itertools;

use self::crc::{crc32, Hasher32};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use disk;
use partition;
use uuid;

#[derive(Debug, Eq, PartialEq)]
pub struct Header {
    /// EFI PART
    pub signature: String, // Offset  0. "EFI PART", 45h 46h 49h 20h 50h 41h 52h 54h
    /// 00 00 01 00
    pub revision: u32, // Offset  8
    /// little endian
    pub header_size_le: u32, // Offset 12
    /// CRC32 of the header with crc32 section zeroed
    pub crc32: u32, // Offset 16
    /// must be 0
    pub reserved: u32, // Offset 20
    /// For main header, 1
    pub current_lba: u64, // Offset 24
    /// LBA for backup header
    pub backup_lba: u64, // Offset 32
    /// First usable LBA for partitions (primary table last LBA + 1)
    pub first_usable: u64, // Offset 40
    /// Last usable LBA (secondary partition table first LBA - 1)
    pub last_usable: u64, // Offset 48
    /// UUID of the disk
    pub disk_guid: uuid::Uuid, // Offset 56
    /// Starting LBA of partition entries
    pub part_start: u64, // Offset 72
    /// Number of partition entries
    pub num_parts: u32, // Offset 80
    /// Size of a partition entry, usually 128
    pub part_size: u32, // Offset 84
    /// CRC32 of the partition table
    pub crc32_parts: u32, // Offset 88
}

impl Header {
    pub(crate) fn compute_new(
        primary: bool,
        pp: &[partition::Partition],
        guid: uuid::Uuid,
        backup_offset: u64,
    ) -> Result<Self> {
        let (cur, bak) = if primary {
            (1, backup_offset)
        } else {
            (backup_offset, 1)
        };
        let first = 34u64;
        let last = backup_offset
            .checked_sub(first)
            .ok_or_else(|| Error::new(ErrorKind::Other, "header underflow - last usable"))?;

        let hdr = Header {
            signature: "EFI PART".to_string(),
            revision: 65536,
            header_size_le: 92,
            crc32: 0,
            reserved: 0,
            current_lba: cur,
            backup_lba: bak,
            first_usable: first,
            last_usable: last,
            disk_guid: guid,
            part_start: 2,
            num_parts: pp.len() as u32,
            part_size: 128,
            crc32_parts: 0,
        };

        Ok(hdr)
    }

    /// Write the header to a location.  With a crc32 set to zero
    /// this will set the crc32 after writing the Header out
    pub fn write_primary(&self, file: &mut File, lb_size: disk::LogicalBlockSize) -> Result<usize> {
        let mut bytes_written: usize = 0;

        // Build up byte array in memory
        let parts_checksum = partentry_checksum(file, self, lb_size)?;
        let bytes = self.as_bytes(None, Some(parts_checksum))?;

        // Calculate the crc32 from the byte array
        let checksum = calculate_crc32(&bytes)?;

        // Write it to disk in 1 shot
        let start = self.current_lba
            .checked_mul(lb_size.into())
            .ok_or_else(|| Error::new(ErrorKind::Other, "primary header overflow - offset"))?;
        let _ = file.seek(SeekFrom::Start(start))?;
        let csum_len = file.write(&self.as_bytes(Some(checksum), Some(parts_checksum))?)?;
        bytes_written = bytes_written
            .checked_add(csum_len)
            .ok_or_else(|| Error::new(ErrorKind::Other, "primary header overflow - checksum"))?;

        Ok(bytes_written)
    }

    // TODO: implement writing backup header too.
    pub fn write_backup(&self, file: &mut File, lb_size: disk::LogicalBlockSize) -> Result<usize> {
        let start = self.backup_lba
            .checked_mul(lb_size.into())
            .ok_or_else(|| Error::new(ErrorKind::Other, "backup header overflow - offset"))?;
        let _ = file.seek(SeekFrom::Start(start))?;
        Ok(0)
    }

    fn as_bytes(&self, checksum: Option<u32>, parts_checksum: Option<u32>) -> Result<Vec<u8>> {
        let mut buff: Vec<u8> = Vec::new();

        buff.write_all(self.signature.as_bytes())?;
        buff.write_u32::<LittleEndian>(self.revision)?;
        buff.write_u32::<LittleEndian>(self.header_size_le)?;
        match checksum {
            Some(c) => buff.write_u32::<LittleEndian>(c)?,
            None => buff.write_u32::<LittleEndian>(0)?,
        };
        buff.write_u32::<LittleEndian>(0)?;
        buff.write_u64::<LittleEndian>(self.current_lba)?;
        buff.write_u64::<LittleEndian>(self.backup_lba)?;
        buff.write_u64::<LittleEndian>(self.first_usable)?;
        buff.write_u64::<LittleEndian>(self.last_usable)?;
        buff.write_all(self.disk_guid.as_bytes())?;
        buff.write_u64::<LittleEndian>(self.part_start)?;
        buff.write_u32::<LittleEndian>(self.num_parts)?;
        buff.write_u32::<LittleEndian>(self.part_size)?;
        match parts_checksum {
            Some(c) => buff.write_u32::<LittleEndian>(c)?,
            None => buff.write_u32::<LittleEndian>(0)?,
        };
        trace!("Buffer: {:02x}", buff.iter().format(","));
        Ok(buff)
    }
}

/// Parses a uuid with first 3 portions in little endian.
pub fn parse_uuid(rdr: &mut Cursor<&[u8]>) -> Result<uuid::Uuid> {
    let d1: u32 = rdr.read_u32::<LittleEndian>()?;
    let d2: u16 = rdr.read_u16::<LittleEndian>()?;
    let d3: u16 = rdr.read_u16::<LittleEndian>()?;
    let uuid = uuid::Uuid::from_fields(
        d1,
        d2,
        d3,
        &rdr.get_ref()[rdr.position() as usize..rdr.position() as usize + 8],
    );
    rdr.seek(SeekFrom::Current(8))?;

    match uuid {
        Ok(uuid) => Ok(uuid),
        Err(_) => Err(Error::new(ErrorKind::Other, "Invalid Disk UUID?")),
    }
}

impl fmt::Display for Header {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Disk:\t\t{}\nCRC32:\t\t{}\nTable CRC:\t{}",
            self.disk_guid, self.crc32, self.crc32_parts
        )
    }
}

/// Read a GPT header from a given path.
///
/// ## Example
///
/// ```rust,no_run
/// use gpt::header::read_header;
///
/// let lb_size = gpt::disk::DEFAULT_SECTOR_SIZE;
/// let diskpath = std::path::Path::new("/dev/sdz");
///
/// let h = read_header(diskpath, lb_size).unwrap();
/// ```
pub fn read_header(path: &Path, sector_size: disk::LogicalBlockSize) -> Result<Header> {
    let mut file = File::open(path)?;
    read_primary_header(&mut file, sector_size)
}

pub(crate) fn read_primary_header(
    file: &mut File,
    sector_size: disk::LogicalBlockSize,
) -> Result<Header> {
    let cur = file.seek(SeekFrom::Current(0)).unwrap_or(0);
    let offset: u64 = sector_size.into();
    let res = file_read_header(file, offset);
    let _ = file.seek(SeekFrom::Start(cur));
    res
}

pub(crate) fn read_backup_header(
    file: &mut File,
    sector_size: disk::LogicalBlockSize,
) -> Result<Header> {
    let cur = file.seek(SeekFrom::Current(0)).unwrap_or(0);
    let h2sect = find_backup_lba(file, sector_size)?;
    let offset = h2sect
        .checked_mul(sector_size.into())
        .ok_or_else(|| Error::new(ErrorKind::Other, "backup header overflow - offset"))?;
    let res = file_read_header(file, offset);
    let _ = file.seek(SeekFrom::Start(cur));
    res
}

pub(crate) fn file_read_header(file: &mut File, offset: u64) -> Result<Header> {
    let _ = file.seek(SeekFrom::Start(offset));
    let mut hdr: [u8; 92] = [0; 92];

    let _ = file.read_exact(&mut hdr);
    let mut reader = Cursor::new(&hdr[..]);

    let sigstr = String::from_utf8_lossy(
        &reader.get_ref()[reader.position() as usize..reader.position() as usize + 8],
    );
    reader.seek(SeekFrom::Current(8))?;

    if sigstr != "EFI PART" {
        return Err(Error::new(ErrorKind::Other, "invalid GPT signature"));
    };

    let h = Header {
        signature: sigstr.to_string(),
        revision: reader.read_u32::<LittleEndian>()?,
        header_size_le: reader.read_u32::<LittleEndian>()?,
        crc32: reader.read_u32::<LittleEndian>()?,
        reserved: reader.read_u32::<LittleEndian>()?,
        current_lba: reader.read_u64::<LittleEndian>()?,
        backup_lba: reader.read_u64::<LittleEndian>()?,
        first_usable: reader.read_u64::<LittleEndian>()?,
        last_usable: reader.read_u64::<LittleEndian>()?,
        disk_guid: parse_uuid(&mut reader)?,
        part_start: reader.read_u64::<LittleEndian>()?,
        num_parts: reader.read_u32::<LittleEndian>()?,
        part_size: reader.read_u32::<LittleEndian>()?,
        crc32_parts: reader.read_u32::<LittleEndian>()?,
    };

    let mut hdr_crc = hdr;
    for crc_byte in hdr_crc.iter_mut().skip(16).take(4) {
        *crc_byte = 0;
    }
    let c = crc32::checksum_ieee(&hdr_crc);
    trace!("hdr_crc: {:?}, h.crc32: {:?}", c, h.crc32);
    if crc32::checksum_ieee(&hdr_crc) == h.crc32 {
        Ok(h)
    } else {
        Err(Error::new(ErrorKind::Other, "invalid CRC32 checksum"))
    }
}

pub(crate) fn find_backup_lba(f: &mut File, sector_size: disk::LogicalBlockSize) -> Result<u64> {
    trace!("Querying file size to find backup header location");
    let lb_size: u64 = sector_size.into();
    let m = f.metadata()?;
    if m.len() <= lb_size {
        return Err(Error::new(
            ErrorKind::Other,
            "disk image too small for backup header",
        ));
    }
    let backup_location = (m.len().saturating_sub(lb_size)) / lb_size;
    trace!("Backup location: {:#x}", backup_location);

    Ok(backup_location)
}

fn calculate_crc32(b: &[u8]) -> Result<u32> {
    let mut digest = crc32::Digest::new(crc32::IEEE);
    trace!("Writing buffer to digest calculator");
    digest.write(b);

    Ok(digest.sum32())
}

pub(crate) fn partentry_checksum(
    file: &mut File,
    hdr: &Header,
    lb_size: disk::LogicalBlockSize,
) -> Result<u32> {
    // Seek to start of partition table.
    let start = hdr.part_start
        .checked_mul(lb_size.into())
        .ok_or_else(|| Error::new(ErrorKind::Other, "header overflow - partition table start"))?;
    let _ = file.seek(SeekFrom::Start(start))?;

    // Read partition table.
    let pt_len = u64::from(hdr.num_parts)
        .checked_mul(hdr.part_size.into())
        .ok_or_else(|| Error::new(ErrorKind::Other, "partition table - size"))?;
    let mut buf = vec![0; pt_len as usize];
    file.read_exact(&mut buf)?;

    // Compute CRC32 over all table bits.
    let mut digest = crc32::Digest::new(crc32::IEEE);
    digest.write(&buf);
    Ok(digest.sum32())
}

/// A helper function to create a new header and write it to disk.
/// If the uuid isn't given a random one will be generated.  Use
/// this in conjunction with Partition::write()
// TODO: Move this to Header::new() and Header::write to write it
// that will match the Partition::write() API
pub fn write_header(
    p: &Path,
    uuid: Option<uuid::Uuid>,
    sector_size: disk::LogicalBlockSize,
) -> Result<uuid::Uuid> {
    debug!("opening {} for writing", p.display());
    let mut file = OpenOptions::new().write(true).read(true).open(p)?;
    let bak = find_backup_lba(&mut file, sector_size)?;
    let guid = match uuid {
        Some(u) => u,
        None => {
            let u = uuid::Uuid::new_v4();
            debug!("Generated random uuid: {}", u);
            u
        }
    };

    let hdr = Header::compute_new(true, &[], guid, bak)?;
    debug!("new header: {:#?}", hdr);
    hdr.write_primary(&mut file, sector_size)?;

    Ok(guid)
}
