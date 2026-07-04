//! RAR 5.0 format header walker.
//!
//! Block layout: crc32 u32 | header_size vint | header bytes (type vint,
//! header_flags vint, [extra_size vint], [data_size vint], type-specific
//! fields, extra area), then `data_size` data bytes. vints are LSB-first
//! 7-bit groups with the high bit as continuation.

use crate::error::{AppError, AppResult};

use super::{malformed, read_exact_at, FileEntry, ReadAt};

pub const MARKER: [u8; 8] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];

const BLOCK_MAIN: u64 = 1;
const BLOCK_FILE: u64 = 2;
const BLOCK_SERVICE: u64 = 3;
const BLOCK_CRYPT: u64 = 4;
const BLOCK_ENDARC: u64 = 5;

const HFL_EXTRA: u64 = 0x01;
const HFL_DATA: u64 = 0x02;
const HFL_SPLIT_BEFORE: u64 = 0x08;
const HFL_SPLIT_AFTER: u64 = 0x10;

const FFL_DIRECTORY: u64 = 0x01;
const FFL_MTIME: u64 = 0x02;
const FFL_CRC32: u64 = 0x04;

/// Extra-area record: file encryption (`-p` without `-hp`).
const EXTRA_FILE_ENCRYPTION: u64 = 0x01;

/// Read a variable-length integer; advances `pos`.
pub(crate) fn read_vint(buf: &[u8], pos: &mut usize) -> AppResult<u64> {
    let mut value = 0u64;
    for i in 0..10 {
        let &byte = buf
            .get(*pos)
            .ok_or_else(|| malformed("vint runs past end of header"))?;
        *pos += 1;
        value |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(malformed("vint longer than 10 bytes"))
}

/// Walk all block headers of one RAR5 volume and return its file entries.
pub async fn parse_volume_headers(r: &dyn ReadAt) -> AppResult<Vec<FileEntry>> {
    let marker = read_exact_at(r, 0, MARKER.len()).await?;
    if marker[..] != MARKER {
        return Err(malformed("missing RAR5 marker"));
    }

    let mut entries = Vec::new();
    let mut offset = MARKER.len() as u64;

    // Smallest possible block: 4 (crc) + 1 (size vint) + 1 (header).
    while offset + 6 <= r.len() {
        // Read a prefix chunk large enough for crc32 + the header_size vint.
        let prefix_len = 16.min((r.len() - offset) as usize);
        let prefix = read_exact_at(r, offset, prefix_len).await?;
        let mut pos = 4; // skip header crc32
        let header_size = read_vint(&prefix, &mut pos)?;
        if header_size == 0 {
            return Err(malformed("zero-size RAR5 header"));
        }
        let header_start = offset + pos as u64;
        let header = read_exact_at(r, header_start, header_size as usize).await?;

        let mut q = 0usize;
        let block_type = read_vint(&header, &mut q)?;
        let header_flags = read_vint(&header, &mut q)?;
        let extra_size = if header_flags & HFL_EXTRA != 0 {
            read_vint(&header, &mut q)?
        } else {
            0
        };
        let data_size = if header_flags & HFL_DATA != 0 {
            read_vint(&header, &mut q)?
        } else {
            0
        };
        if extra_size > header_size {
            return Err(malformed("extra area larger than header"));
        }

        match block_type {
            BLOCK_CRYPT => return Err(AppError::EncryptedRarUnsupported),
            BLOCK_FILE | BLOCK_SERVICE => {
                let entry = parse_file_header(
                    &header,
                    &mut q,
                    header_flags,
                    extra_size,
                    data_size,
                    header_start + header_size,
                )?;
                if block_type == BLOCK_FILE {
                    entries.push(entry);
                }
            }
            BLOCK_MAIN => {}
            BLOCK_ENDARC => break,
            _ => {}
        }

        offset = header_start + header_size + data_size;
    }

    Ok(entries)
}

fn parse_file_header(
    header: &[u8],
    q: &mut usize,
    header_flags: u64,
    extra_size: u64,
    data_size: u64,
    data_offset: u64,
) -> AppResult<FileEntry> {
    let file_flags = read_vint(header, q)?;
    let unpacked_size = read_vint(header, q)?;
    let _attributes = read_vint(header, q)?;
    if file_flags & FFL_MTIME != 0 {
        *q += 4;
    }
    if file_flags & FFL_CRC32 != 0 {
        *q += 4;
    }
    let compression_info = read_vint(header, q)?;
    let _host_os = read_vint(header, q)?;
    let name_len = read_vint(header, q)? as usize;
    let name_end = q
        .checked_add(name_len)
        .filter(|&e| e <= header.len())
        .ok_or_else(|| malformed("RAR5 file name out of bounds"))?;
    let name = String::from_utf8_lossy(&header[*q..name_end]).into_owned();

    // The extra area sits at the end of the header; scan its records for
    // per-file encryption.
    if extra_size > 0 {
        let extra_start = header.len() - extra_size as usize;
        let extra = &header[extra_start..];
        let mut p = 0usize;
        while p < extra.len() {
            let record_size = read_vint(extra, &mut p)?;
            let record_start = p;
            let record_type = read_vint(extra, &mut p)?;
            if record_type == EXTRA_FILE_ENCRYPTION {
                return Err(AppError::EncryptedRarUnsupported);
            }
            p = record_start
                .checked_add(record_size as usize)
                .filter(|&e| e <= extra.len())
                .ok_or_else(|| malformed("RAR5 extra record out of bounds"))?;
        }
    }

    // compression_info: bits 0..5 = version, bit 6 = solid, bits 7..9 = method.
    let method = (compression_info >> 7) & 0x07;

    Ok(FileEntry {
        name,
        unpacked_size,
        packed_size: data_size,
        data_offset,
        method_store: method == 0,
        split_before: header_flags & HFL_SPLIT_BEFORE != 0,
        split_after: header_flags & HFL_SPLIT_AFTER != 0,
        is_directory: file_flags & FFL_DIRECTORY != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_single_and_multi_byte() {
        let mut pos = 0;
        assert_eq!(read_vint(&[0x2A], &mut pos).unwrap(), 42);
        assert_eq!(pos, 1);

        // 12000 = 0x2EE0 -> e0 dd 00 (as emitted by rar, non-minimal allowed)
        let mut pos = 0;
        assert_eq!(read_vint(&[0xE0, 0xDD, 0x00], &mut pos).unwrap(), 12000);
        assert_eq!(pos, 3);

        // Non-minimal zero: 80 00
        let mut pos = 0;
        assert_eq!(read_vint(&[0x80, 0x00], &mut pos).unwrap(), 0);
        assert_eq!(pos, 2);
    }

    #[test]
    fn vint_error_on_truncation() {
        let mut pos = 0;
        assert!(read_vint(&[0x80], &mut pos).is_err());
    }
}
