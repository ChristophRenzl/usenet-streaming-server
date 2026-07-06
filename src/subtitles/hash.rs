//! OpenSubtitles / OSDb "moviehash".
//!
//! OpenSubtitles matches a subtitle to an exact video release by a cheap
//! 64-bit hash computed from the file size and the first and last 64 KiB of
//! the file (rather than hashing the whole thing). A hash match means the
//! subtitle was timed against *this* release, so its cues line up without any
//! fps/offset correction — the accuracy win we rank first.
//!
//! Algorithm (matching the reference implementations):
//!
//! ```text
//! hash = (filesize
//!         + sum_of_u64_words(first 64 KiB)
//!         + sum_of_u64_words(last 64 KiB)) mod 2^64
//! ```
//!
//! where each 64 KiB chunk is read as 8192 consecutive little-endian `u64`
//! words and summed with wrapping (mod 2^64) arithmetic. The result is
//! rendered as a zero-padded 16-character lowercase hex string.
//!
//! Files smaller than 128 KiB (two chunks) are skipped — there is no reliable
//! hash for them and OpenSubtitles would not match anyway.

use std::sync::Arc;

use crate::error::AppResult;
use crate::vfs::VirtualFile;

/// Size of each hashed chunk: 64 KiB.
pub const CHUNK_SIZE: u64 = 64 * 1024;

/// Minimum media size to hash: two full chunks (128 KiB).
pub const MIN_HASHABLE_SIZE: u64 = CHUNK_SIZE * 2;

/// Sum a byte slice as consecutive little-endian `u64` words with wrapping
/// (mod 2^64) arithmetic. Bytes past the last whole 8-byte word are ignored
/// (a full 64 KiB chunk is always a whole number of words).
fn sum_u64_words(bytes: &[u8]) -> u64 {
    let mut sum: u64 = 0;
    for word in bytes.chunks_exact(8) {
        // `chunks_exact(8)` guarantees an 8-byte slice.
        let value = u64::from_le_bytes(word.try_into().expect("8-byte word"));
        sum = sum.wrapping_add(value);
    }
    sum
}

/// Read exactly `len` bytes at `offset` from `file`, looping until the buffer
/// is full or EOF is hit. Returns fewer bytes only at EOF.
async fn read_exact_at(file: &Arc<dyn VirtualFile>, offset: u64, len: usize) -> AppResult<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut pos = offset;
    while out.len() < len {
        let want = len - out.len();
        let bytes = file.read_at(pos, want).await?;
        if bytes.is_empty() {
            break; // EOF
        }
        pos += bytes.len() as u64;
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

/// Compute the OpenSubtitles/OSDb moviehash for `file`, reading only its first
/// and last 64 KiB plus using its length. Returns `None` when the file is
/// smaller than 128 KiB (unhashable) or a chunk could not be fully read.
///
/// The output is a zero-padded 16-character lowercase hex string, ready to
/// pass to the OpenSubtitles search as `moviehash=<hash>`.
pub async fn osdb_hash(file: &Arc<dyn VirtualFile>) -> AppResult<Option<String>> {
    let filesize = file.len();
    if filesize < MIN_HASHABLE_SIZE {
        return Ok(None);
    }

    let chunk = CHUNK_SIZE as usize;
    let head = read_exact_at(file, 0, chunk).await?;
    let tail = read_exact_at(file, filesize - CHUNK_SIZE, chunk).await?;
    // A short read on either chunk means we cannot form a valid hash.
    if head.len() < chunk || tail.len() < chunk {
        return Ok(None);
    }

    let hash = filesize
        .wrapping_add(sum_u64_words(&head))
        .wrapping_add(sum_u64_words(&tail));
    Ok(Some(format!("{hash:016x}")))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use bytes::Bytes;

    use super::*;

    /// An in-memory `VirtualFile` over a fixed byte buffer, for hashing tests.
    struct MemFile(Vec<u8>);

    #[async_trait]
    impl VirtualFile for MemFile {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        async fn read_at(&self, offset: u64, buf_len: usize) -> AppResult<Bytes> {
            let start = (offset as usize).min(self.0.len());
            let end = (start + buf_len).min(self.0.len());
            Ok(Bytes::copy_from_slice(&self.0[start..end]))
        }
    }

    fn mem(bytes: Vec<u8>) -> Arc<dyn VirtualFile> {
        Arc::new(MemFile(bytes))
    }

    #[test]
    fn sums_little_endian_words_with_wrapping() {
        // Two words: 0x0000000000000001 and 0x0000000000000002 (LE bytes).
        let mut buf = vec![0u8; 16];
        buf[0] = 0x01; // first word = 1
        buf[8] = 0x02; // second word = 2
        assert_eq!(sum_u64_words(&buf), 3);

        // Wrapping: u64::MAX (all 0xFF) + 1 = 0.
        let mut buf = vec![0xFFu8; 8];
        buf.extend_from_slice(&1u64.to_le_bytes());
        assert_eq!(sum_u64_words(&buf), 0);
    }

    #[tokio::test]
    async fn hash_matches_hand_computed_value() {
        // Craft a 128 KiB file (exactly two chunks) where every byte is 0x01.
        // Each 64 KiB chunk is 8192 little-endian u64 words. With every byte
        // 0x01, each word is 0x0101010101010101. Per chunk the word-sum is
        // 8192 * 0x0101010101010101 (wrapping); the two identical chunks give
        // 2 * that, plus the filesize 131072.
        let size = (MIN_HASHABLE_SIZE) as usize; // 131072
        let file = mem(vec![0x01u8; size]);

        let word = 0x0101_0101_0101_0101u64;
        let words_per_chunk = CHUNK_SIZE / 8; // 8192
        let per_chunk = word.wrapping_mul(words_per_chunk);
        let expected = (size as u64)
            .wrapping_add(per_chunk)
            .wrapping_add(per_chunk);
        let expected_hex = format!("{expected:016x}");

        let got = osdb_hash(&file).await.expect("hash").expect("hashable");
        assert_eq!(got, expected_hex);
        assert_eq!(got.len(), 16, "zero-padded 16-char hex");
        assert!(
            got.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only: {got}"
        );
    }

    #[tokio::test]
    async fn hash_is_deterministic_and_size_sensitive() {
        // Distinct head and tail so the two chunks differ.
        let mut bytes = vec![0u8; MIN_HASHABLE_SIZE as usize];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let a = osdb_hash(&mem(bytes.clone())).await.unwrap().unwrap();
        let b = osdb_hash(&mem(bytes.clone())).await.unwrap().unwrap();
        assert_eq!(a, b, "same bytes -> same hash");

        // Appending a byte (which changes both the size and the tail chunk)
        // changes the hash.
        let mut larger = bytes.clone();
        larger.push(0x42);
        let c = osdb_hash(&mem(larger)).await.unwrap().unwrap();
        assert_ne!(a, c, "different size/tail -> different hash");
    }

    #[tokio::test]
    async fn small_files_are_not_hashable() {
        let file = mem(vec![0u8; (MIN_HASHABLE_SIZE - 1) as usize]);
        assert_eq!(osdb_hash(&file).await.unwrap(), None);
    }
}
