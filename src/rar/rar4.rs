//! RAR 4.x (a.k.a. RAR 1.5–4.x format) header walker.
//!
//! Block layout: crc u16 | type u8 | flags u16 | size u16 (all LE), followed
//! by type-specific fields within `size`, followed by `add_size` data bytes
//! when flags & 0x8000 (for FILE blocks the data area is `pack_size`).

use crate::error::AppResult;

use super::{malformed, read_exact_at, FileEntry, ReadAt};

pub const MARKER: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];

const BLOCK_MAIN: u8 = 0x73;
const BLOCK_FILE: u8 = 0x74;
const BLOCK_ENDARC: u8 = 0x7B;

const MHD_PASSWORD: u16 = 0x0080; // encrypted headers
const FHD_SPLIT_BEFORE: u16 = 0x0001;
const FHD_SPLIT_AFTER: u16 = 0x0002;
const FHD_PASSWORD: u16 = 0x0004;
const FHD_LARGE: u16 = 0x0100; // 64-bit sizes present
const FHD_UNICODE: u16 = 0x0200; // name is ascii\0unicode
const FHD_WINDOW_MASK: u16 = 0x00E0; // 0xE0 == directory
const LONG_BLOCK: u16 = 0x8000; // add_size field present

const METHOD_STORE: u8 = 0x30;

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Walk all block headers of one RAR4 volume and return its file entries.
pub async fn parse_volume_headers(r: &dyn ReadAt) -> AppResult<Vec<FileEntry>> {
    let marker = read_exact_at(r, 0, MARKER.len()).await?;
    if marker[..] != MARKER {
        return Err(malformed("missing RAR4 marker"));
    }

    let mut entries = Vec::new();
    let mut offset = MARKER.len() as u64;

    while offset + 7 <= r.len() {
        let base = read_exact_at(r, offset, 7).await?;
        let block_type = base[2];
        let flags = u16le(&base[3..5]);
        let header_size = u16le(&base[5..7]) as u64;
        if header_size < 7 {
            return Err(malformed(format!(
                "block header size {header_size} < 7 at offset {offset}"
            )));
        }
        let header = read_exact_at(r, offset, header_size as usize).await?;

        let mut add_size = 0u64;
        match block_type {
            BLOCK_MAIN => {
                if flags & MHD_PASSWORD != 0 {
                    return Err(crate::error::AppError::EncryptedRarUnsupported);
                }
            }
            BLOCK_FILE => {
                let entry = parse_file_header(&header, flags, offset, header_size)?;
                add_size = entry.packed_size;
                entries.push(entry);
            }
            BLOCK_ENDARC => break,
            _ => {}
        }
        if block_type != BLOCK_FILE && flags & LONG_BLOCK != 0 {
            if header.len() < 11 {
                return Err(malformed("LONG_BLOCK header too short for add_size"));
            }
            add_size = u32le(&header[7..11]) as u64;
        }

        offset += header_size + add_size;
    }

    Ok(entries)
}

fn parse_file_header(
    header: &[u8],
    flags: u16,
    block_offset: u64,
    header_size: u64,
) -> AppResult<FileEntry> {
    if flags & FHD_PASSWORD != 0 {
        return Err(crate::error::AppError::EncryptedRarUnsupported);
    }
    if header.len() < 32 {
        return Err(malformed("FILE header shorter than 32 bytes"));
    }
    let mut pack_size = u32le(&header[7..11]) as u64;
    let mut unp_size = u32le(&header[11..15]) as u64;
    // host_os u8 @15, file_crc u32 @16, ftime u32 @20, unp_ver u8 @24
    let method = header[25];
    let name_size = u16le(&header[26..28]) as usize;
    // attr u32 @28
    let mut name_offset = 32usize;
    if flags & FHD_LARGE != 0 {
        if header.len() < 40 {
            return Err(malformed("FILE header with LARGE flag shorter than 40"));
        }
        pack_size |= (u32le(&header[32..36]) as u64) << 32;
        unp_size |= (u32le(&header[36..40]) as u64) << 32;
        name_offset = 40;
    }
    if header.len() < name_offset + name_size {
        return Err(malformed("FILE header name out of bounds"));
    }
    let raw_name = &header[name_offset..name_offset + name_size];
    // With the UNICODE flag the name field is "ascii NUL unicode-extra"; keep
    // the ASCII prefix.
    let raw_name = if flags & FHD_UNICODE != 0 {
        raw_name.split(|&b| b == 0).next().unwrap_or(raw_name)
    } else {
        raw_name
    };
    let name = String::from_utf8_lossy(raw_name).into_owned();

    Ok(FileEntry {
        name,
        unpacked_size: unp_size,
        packed_size: pack_size,
        data_offset: block_offset + header_size,
        method_store: method == METHOD_STORE,
        split_before: flags & FHD_SPLIT_BEFORE != 0,
        split_after: flags & FHD_SPLIT_AFTER != 0,
        is_directory: flags & FHD_WINDOW_MASK == FHD_WINDOW_MASK,
    })
}
