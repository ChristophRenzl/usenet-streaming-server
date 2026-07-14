//! Dolby Vision profile 5 init-segment surgery.
//!
//! ffmpeg 5.x's HLS muxer drops the DOVI configuration record (the child mp4
//! muxer never sees the stream side data), and its mp4 muxer can only tag
//! HEVC as `hvc1`/`hev1`. AVPlayer engages its Dolby Vision pipeline solely
//! for a `dvh1` sample entry carrying a `dvcC` box — without both, a P5
//! stream (no HDR10/SDR-compatible base layer) decodes to garbage colors.
//!
//! The init segment is a single small atomic write, so the fix is byte-level:
//! rename the sample-entry fourcc and splice a spec-built `dvcC` box into it,
//! bumping every ancestor box size on the way down.

/// Byte length of the dvcC box: 8 header + 24 payload.
const DVCC_BOX_LEN: u32 = 32;

/// Build the 24-byte DOVIDecoderConfigurationRecord for single-layer profile
/// 5: RPU present, no enhancement layer, base layer present, compatibility
/// id 0 (P5's IPTPQc2 base layer is compatible with nothing).
fn dvcc_box(dv_level: u8) -> [u8; DVCC_BOX_LEN as usize] {
    let mut b = [0u8; DVCC_BOX_LEN as usize];
    b[0..4].copy_from_slice(&DVCC_BOX_LEN.to_be_bytes());
    b[4..8].copy_from_slice(b"dvcC");
    b[8] = 1; // dv_version_major
    b[9] = 0; // dv_version_minor
    // dv_profile (7 bits) | dv_level (6 bits) | rpu(1) | el(1) | bl(1)
    let profile: u16 = 5;
    let bits: u16 = (profile << 9) | ((dv_level as u16 & 0x3F) << 3) | 0b101;
    b[10..12].copy_from_slice(&bits.to_be_bytes());
    b[12] = 0; // dv_bl_signal_compatibility_id (4 bits) << 4
    b
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|s| u32::from_be_bytes(s.try_into().expect("4 bytes")))
}

/// Offset of the first child box named `name` inside the box starting at
/// `start` (whose header is 8 bytes), scanning up to `end`.
fn find_child(bytes: &[u8], start: usize, end: usize, name: &[u8; 4]) -> Option<usize> {
    let mut at = start + 8;
    while at + 8 <= end {
        let size = read_u32(bytes, at)? as usize;
        if size < 8 || at + size > end {
            return None;
        }
        if &bytes[at + 4..at + 8] == name {
            return Some(at);
        }
        at += size;
    }
    None
}

/// Rewrite a DV profile 5 fMP4 init segment in place: `hvc1`/`hev1` sample
/// entry becomes `dvh1` and gains a `dvcC` box. Returns the input unchanged
/// when the box tree does not look like the expected single-video init (the
/// stream then plays as before the patch — mislabeled, but not corrupted).
pub fn patch_init_for_dv_p5(bytes: Vec<u8>, dv_level: u8) -> Vec<u8> {
    let patched = try_patch(&bytes, dv_level);
    patched.unwrap_or(bytes)
}

fn try_patch(bytes: &[u8], dv_level: u8) -> Option<Vec<u8>> {
    // Top level: find moov.
    let mut moov = None;
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let size = read_u32(bytes, at)? as usize;
        if size < 8 || at + size > bytes.len() {
            return None;
        }
        if &bytes[at + 4..at + 8] == b"moov" {
            moov = Some(at);
            break;
        }
        at += size;
    }
    let moov = moov?;
    let moov_end = moov + read_u32(bytes, moov)? as usize;

    // moov > trak > mdia > minf > stbl > stsd. The init segment carries the
    // video track first; a non-video first trak simply fails the fourcc
    // check below and the input is returned unchanged.
    let trak = find_child(bytes, moov, moov_end, b"trak")?;
    let trak_end = trak + read_u32(bytes, trak)? as usize;
    let mdia = find_child(bytes, trak, trak_end, b"mdia")?;
    let mdia_end = mdia + read_u32(bytes, mdia)? as usize;
    let minf = find_child(bytes, mdia, mdia_end, b"minf")?;
    let minf_end = minf + read_u32(bytes, minf)? as usize;
    let stbl = find_child(bytes, minf, minf_end, b"stbl")?;
    let stbl_end = stbl + read_u32(bytes, stbl)? as usize;
    let stsd = find_child(bytes, stbl, stbl_end, b"stsd")?;

    // stsd: 8 header + 1 version + 3 flags + 4 entry_count, then entries.
    let entry = stsd + 16;
    let entry_size = read_u32(bytes, entry)? as usize;
    if entry + entry_size > stbl_end || entry_size < 8 {
        return None;
    }
    let fourcc = &bytes[entry + 4..entry + 8];
    if fourcc != b"hvc1" && fourcc != b"hev1" {
        return None;
    }

    // Dolby's ISOBMFF spec: the DOVIConfigurationBox must DIRECTLY follow the
    // HEVCConfigurationBox — Apple's parser will not engage the DV pipeline
    // with another box (e.g. `pasp`) in between. Sub-boxes start after the
    // 78-byte VisualSampleEntry fields.
    let mut sub = entry + 86;
    let entry_end = entry + entry_size;
    let mut insert_at = None;
    while sub + 8 <= entry_end {
        let size = read_u32(bytes, sub)? as usize;
        if size < 8 || sub + size > entry_end {
            return None;
        }
        if &bytes[sub + 4..sub + 8] == b"hvcC" {
            insert_at = Some(sub + size);
            break;
        }
        sub += size;
    }
    let insert_at = insert_at?;

    let mut out = Vec::with_capacity(bytes.len() + DVCC_BOX_LEN as usize);
    out.extend_from_slice(bytes);
    out[entry + 4..entry + 8].copy_from_slice(b"dvh1");
    let dvcc = dvcc_box(dv_level);
    out.splice(insert_at..insert_at, dvcc.iter().copied());
    // Every ancestor grows by the spliced box.
    for header in [moov, trak, mdia, minf, stbl, stsd, entry] {
        let size = read_u32(&out, header)?;
        out[header..header + 4].copy_from_slice(&(size + DVCC_BOX_LEN).to_be_bytes());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box with children.
    fn container(name: &[u8; 4], children: Vec<u8>) -> Vec<u8> {
        let mut b = ((children.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(name);
        b.extend(children);
        b
    }

    fn synthetic_init() -> Vec<u8> {
        // hvc1 sample entry: 78-byte visual header, then an hvcC box followed
        // by a pasp box (mirrors ffmpeg output; dvcC must land between them).
        let mut entry_payload = vec![0u8; 78];
        entry_payload.extend(container(b"hvcC", vec![0xBB; 8]));
        entry_payload.extend(container(b"pasp", vec![0u8; 8]));
        let entry = container(b"hvc1", entry_payload);
        let mut stsd_payload = vec![0, 0, 0, 0, 0, 0, 0, 1]; // version/flags + count
        stsd_payload.extend(entry);
        let stsd = container(b"stsd", stsd_payload);
        let stbl = container(b"stbl", stsd);
        let minf = container(b"minf", stbl);
        let mdia = container(b"mdia", minf);
        let trak = container(b"trak", mdia);
        let moov = container(b"moov", trak);
        let mut init = container(b"ftyp", b"iso5iso6mp41".to_vec());
        init.extend(moov);
        init
    }

    #[test]
    fn renames_fourcc_and_inserts_dvcc() {
        let init = synthetic_init();
        let before = init.len();
        let patched = patch_init_for_dv_p5(init, 6);
        assert_eq!(patched.len(), before + DVCC_BOX_LEN as usize);
        assert!(patched.windows(4).any(|w| w == b"dvh1"));
        assert!(patched.windows(4).any(|w| w == b"dvcC"));
        assert!(!patched.windows(4).any(|w| w == b"hvc1"));
        // Profile/level bitfield: 5<<9 | 6<<3 | 0b101 = 0x0A35.
        let at = patched
            .windows(4)
            .position(|w| w == b"dvcC")
            .expect("dvcC");
        assert_eq!(&patched[at + 4..at + 9], &[1, 0, 0x0A, 0x35, 0]);
        // dvcC must directly follow hvcC (before pasp), per Dolby's spec.
        let hvcc = patched.windows(4).position(|w| w == b"hvcC").expect("hvcC");
        let pasp = patched.windows(4).position(|w| w == b"pasp").expect("pasp");
        assert!(hvcc < at && at < pasp, "dvcC must sit between hvcC and pasp");
        // moov size grew by exactly one box.
        let moov_at = patched
            .windows(4)
            .position(|w| w == b"moov")
            .expect("moov")
            - 4;
        let orig = synthetic_init();
        let moov_orig = orig.windows(4).position(|w| w == b"moov").expect("moov") - 4;
        let old = u32::from_be_bytes(orig[moov_orig..moov_orig + 4].try_into().unwrap());
        let new = u32::from_be_bytes(patched[moov_at..moov_at + 4].try_into().unwrap());
        assert_eq!(new, old + DVCC_BOX_LEN);
    }

    #[test]
    fn non_hevc_entry_is_left_untouched() {
        let mut init = synthetic_init();
        let at = init.windows(4).position(|w| w == b"hvc1").expect("hvc1");
        init[at..at + 4].copy_from_slice(b"avc1");
        let before = init.clone();
        assert_eq!(patch_init_for_dv_p5(init, 6), before);
    }

    #[test]
    fn garbage_input_is_returned_unchanged() {
        let junk = vec![0u8; 64];
        assert_eq!(patch_init_for_dv_p5(junk.clone(), 6), junk);
    }
}
