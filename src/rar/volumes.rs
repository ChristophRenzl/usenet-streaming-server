//! Multi-volume handling: format detection, natural volume ordering and
//! building the [`ArchiveMap`] for the largest inner file across a set.

use std::cmp::Ordering;

use crate::error::{AppError, AppResult};

use super::{malformed, rar4, rar5, ArchiveMap, Extent, FileEntry, RarFormat, ReadAt};

/// Detect RAR4 vs RAR5 from the marker block.
pub async fn detect_format(r: &dyn ReadAt) -> AppResult<RarFormat> {
    let head = r.read_at(0, rar5::MARKER.len()).await?;
    if head.len() >= rar5::MARKER.len() && head[..8] == rar5::MARKER {
        return Ok(RarFormat::Rar5);
    }
    if head.len() >= rar4::MARKER.len() && head[..7] == rar4::MARKER {
        return Ok(RarFormat::Rar4);
    }
    Err(malformed("not a RAR archive (marker not found)"))
}

/// Parse one volume's headers, dispatching on the detected format.
pub async fn parse_volume(r: &dyn ReadAt) -> AppResult<Vec<FileEntry>> {
    match detect_format(r).await? {
        RarFormat::Rar4 => rar4::parse_volume_headers(r).await,
        RarFormat::Rar5 => rar5::parse_volume_headers(r).await,
    }
}

/// Natural sort key for RAR volume file names:
/// `x.rar` < `x.r00` < `x.r01` (old style) and
/// `x.part1.rar` < `x.part2.rar` < `x.part10.rar` (new style).
pub fn volume_sort_key(file_name: &str) -> (u64, String) {
    let lower = file_name.to_ascii_lowercase();
    let number = volume_number(&lower);
    (number, lower)
}

fn volume_number(lower: &str) -> u64 {
    if let Some(stripped) = lower.strip_suffix(".rar") {
        if let Some((_, part)) = stripped.rsplit_once(".part") {
            if let Ok(n) = part.parse::<u64>() {
                return n;
            }
        }
        return 0; // plain .rar: first volume of an old-style set
    }
    if let Some((_, ext)) = lower.rsplit_once('.') {
        if let Some(digits) = ext.strip_prefix('r') {
            if let Ok(n) = digits.parse::<u64>() {
                return n + 1; // .r00 follows .rar
            }
        }
    }
    u64::MAX // unknown: sort last, by name
}

/// Compare two volume names naturally (convenience for sorting name lists).
pub fn compare_volume_names(a: &str, b: &str) -> Ordering {
    volume_sort_key(a).cmp(&volume_sort_key(b))
}

/// Parse every volume of an ordered RAR set and build the byte mapping for
/// the largest inner file. Volumes must already be in natural order.
pub async fn build_archive_map(volumes: &[&dyn ReadAt]) -> AppResult<ArchiveMap> {
    if volumes.is_empty() {
        return Err(malformed("empty RAR volume set"));
    }

    let mut per_volume: Vec<Vec<FileEntry>> = Vec::with_capacity(volumes.len());
    for volume in volumes {
        per_volume.push(parse_volume(*volume).await?);
    }

    // Target: the largest unpacked non-directory file anywhere in the set.
    let target = per_volume
        .iter()
        .flatten()
        .filter(|e| !e.is_directory)
        .max_by_key(|e| e.unpacked_size)
        .ok_or_else(|| malformed("RAR set contains no files"))?;
    let inner_file_name = target.name.clone();
    let unpacked_size = target.unpacked_size;

    // Collect this file's parts across volumes, in volume order.
    let mut extents = Vec::new();
    let mut covered = 0u64;
    let mut parts: Vec<(usize, &FileEntry)> = Vec::new();
    for (volume_index, entries) in per_volume.iter().enumerate() {
        for entry in entries.iter().filter(|e| e.name == inner_file_name) {
            parts.push((volume_index, entry));
        }
    }
    if parts.is_empty() {
        return Err(malformed("target file has no parts"));
    }

    for (i, (volume_index, entry)) in parts.iter().enumerate() {
        if !entry.method_store {
            return Err(AppError::CompressedRarUnsupported);
        }
        let first = i == 0;
        let last = i == parts.len() - 1;
        if entry.split_before == first {
            return Err(malformed(format!(
                "volume {volume_index}: split-before flag breaks the volume chain \
                 (are the volumes ordered and complete?)"
            )));
        }
        if entry.split_after == last {
            return Err(malformed(format!(
                "volume {volume_index}: split-after flag breaks the volume chain \
                 (are the volumes ordered and complete?)"
            )));
        }
        let end = entry
            .data_offset
            .checked_add(entry.packed_size)
            .ok_or_else(|| malformed("data area overflows"))?;
        if end > volumes[*volume_index].len() {
            return Err(malformed(format!(
                "volume {volume_index}: data area extends past end of volume"
            )));
        }
        extents.push(Extent {
            unpacked_start: covered,
            len: entry.packed_size,
            volume_index: *volume_index,
            volume_offset: entry.data_offset,
        });
        covered += entry.packed_size;
    }

    if covered != unpacked_size {
        return Err(malformed(format!(
            "store-mode size mismatch: parts cover {covered} bytes but unpacked \
             size is {unpacked_size}"
        )));
    }

    Ok(ArchiveMap {
        extents,
        inner_file_name,
        unpacked_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_new_style() {
        let mut names = vec!["x.part10.rar", "x.part2.rar", "x.part1.rar"];
        names.sort_by(|a, b| compare_volume_names(a, b));
        assert_eq!(names, ["x.part1.rar", "x.part2.rar", "x.part10.rar"]);
    }

    #[test]
    fn natural_order_old_style() {
        let mut names = vec!["x.r01", "x.rar", "x.r00", "x.r10"];
        names.sort_by(|a, b| compare_volume_names(a, b));
        assert_eq!(names, ["x.rar", "x.r00", "x.r01", "x.r10"]);
    }
}
