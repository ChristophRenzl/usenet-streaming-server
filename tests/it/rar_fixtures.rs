//! RAR store-mode mapping validated against the committed fixtures
//! (generated + unrar-verified by scripts/gen-rar-fixtures.sh).

use std::sync::Arc;

use usenet_streaming_server::error::AppError;
use usenet_streaming_server::rar::{build_archive_map, volume_sort_key, ArchiveMap, ReadAt};
use usenet_streaming_server::vfs::{DiskFile, RarInnerFile, VirtualFile};

use crate::support::{payload_3mib, TestRng, PAYLOAD_CRC32};

const PAYLOAD_LEN: u64 = 3 * 1024 * 1024;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/rar/{name}", env!("CARGO_MANIFEST_DIR"))
}

async fn open_volumes(names: &[&str]) -> Vec<Arc<DiskFile>> {
    let mut sorted: Vec<&str> = names.to_vec();
    sorted.sort_by_key(|name| volume_sort_key(name));
    let mut volumes = Vec::with_capacity(sorted.len());
    for name in sorted {
        volumes.push(Arc::new(
            DiskFile::open(fixture(name)).await.expect("open fixture"),
        ));
    }
    volumes
}

async fn map_for(volumes: &[Arc<DiskFile>]) -> Result<ArchiveMap, AppError> {
    let refs: Vec<&dyn ReadAt> = volumes.iter().map(|v| v.as_ref() as &dyn ReadAt).collect();
    build_archive_map(&refs).await
}

/// Full extraction through RarInnerFile must be byte-identical to the
/// deterministic payload.
async fn assert_extracts_payload(volumes: Vec<Arc<DiskFile>>) {
    let map = map_for(&volumes).await.expect("archive map");
    assert_eq!(map.inner_file_name, "payload.bin");
    assert_eq!(map.unpacked_size, PAYLOAD_LEN);

    let vfs_volumes: Vec<Arc<dyn VirtualFile>> = volumes
        .into_iter()
        .map(|v| v as Arc<dyn VirtualFile>)
        .collect();
    let inner = RarInnerFile::new(map, vfs_volumes).expect("inner file");
    assert_eq!(inner.len(), PAYLOAD_LEN);

    let mut extracted = Vec::with_capacity(PAYLOAD_LEN as usize);
    let mut offset = 0u64;
    while offset < inner.len() {
        let chunk = inner.read_at(offset, 512 * 1024).await.expect("read");
        assert!(!chunk.is_empty());
        extracted.extend_from_slice(&chunk);
        offset += chunk.len() as u64;
    }
    assert_eq!(extracted.len() as u64, PAYLOAD_LEN);
    assert_eq!(crc32fast::hash(&extracted), PAYLOAD_CRC32);
    assert_eq!(extracted, payload_3mib());
}

#[tokio::test]
async fn rar4_store_single_extracts_byte_identical() {
    assert_extracts_payload(open_volumes(&["rar4-store.rar"]).await).await;
}

#[tokio::test]
async fn rar4_store_multivolume_extracts_byte_identical() {
    let volumes = open_volumes(&[
        // deliberately shuffled: volume_sort_key must order them
        "rar4-store-multi.r01",
        "rar4-store-multi.rar",
        "rar4-store-multi.r02",
        "rar4-store-multi.r00",
    ])
    .await;
    let map = map_for(&volumes).await.expect("map");
    assert!(map.extents.len() > 1, "split file must span extents");
    assert_extracts_payload(volumes).await;
}

#[tokio::test]
async fn rar5_store_single_extracts_byte_identical() {
    assert_extracts_payload(open_volumes(&["rar5-store.rar"]).await).await;
}

#[tokio::test]
async fn rar5_store_multivolume_extracts_byte_identical() {
    let volumes = open_volumes(&[
        "rar5-store-multi.part3.rar",
        "rar5-store-multi.part1.rar",
        "rar5-store-multi.part4.rar",
        "rar5-store-multi.part2.rar",
    ])
    .await;
    let map = map_for(&volumes).await.expect("map");
    assert_eq!(map.extents.len(), 4);
    // Extents are contiguous and ordered.
    let mut covered = 0u64;
    for extent in &map.extents {
        assert_eq!(extent.unpacked_start, covered);
        covered += extent.len;
    }
    assert_eq!(covered, PAYLOAD_LEN);
    assert_extracts_payload(volumes).await;
}

#[tokio::test]
async fn reads_across_extent_boundaries_are_correct() {
    let volumes = open_volumes(&[
        "rar5-store-multi.part1.rar",
        "rar5-store-multi.part2.rar",
        "rar5-store-multi.part3.rar",
        "rar5-store-multi.part4.rar",
    ])
    .await;
    let map = map_for(&volumes).await.expect("map");
    let boundaries: Vec<u64> = map
        .extents
        .iter()
        .skip(1)
        .map(|e| e.unpacked_start)
        .collect();
    let inner = RarInnerFile::new(
        map,
        volumes
            .into_iter()
            .map(|v| v as Arc<dyn VirtualFile>)
            .collect(),
    )
    .expect("inner");

    let payload = payload_3mib();
    for boundary in boundaries {
        let start = (boundary - 1000) as usize;
        let end = (start + 2000).min(payload.len()); // last boundary sits near EOF
        let chunk = inner
            .read_at(start as u64, 2000)
            .await
            .expect("boundary read");
        assert_eq!(
            &chunk[..],
            &payload[start..end],
            "mismatch across boundary at {boundary}"
        );
    }

    // Fuzz random reads, including suffix reads near EOF.
    let mut rng = TestRng::new(0xFEED);
    for _ in 0..50 {
        let offset = rng.next_u64() % (PAYLOAD_LEN + 512);
        let len = (rng.next_u64() % 200_000) as usize;
        let chunk = inner.read_at(offset, len).await.expect("fuzz read");
        let expect_start = (offset as usize).min(payload.len());
        let expect_end = (expect_start + len).min(payload.len());
        assert_eq!(&chunk[..], &payload[expect_start..expect_end]);
    }
}

#[tokio::test]
async fn compressed_archives_are_rejected() {
    for name in ["rar5-compressed.rar", "rar4-compressed.rar"] {
        let volumes = open_volumes(&[name]).await;
        match map_for(&volumes).await {
            Err(AppError::CompressedRarUnsupported) => {}
            other => panic!("{name}: expected CompressedRarUnsupported, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn encrypted_archives_are_rejected() {
    for name in ["rar5-encrypted.rar", "rar4-encrypted.rar"] {
        let volumes = open_volumes(&[name]).await;
        match map_for(&volumes).await {
            Err(AppError::EncryptedRarUnsupported) => {}
            other => panic!("{name}: expected EncryptedRarUnsupported, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn garbage_input_is_not_a_rar() {
    use bytes::Bytes;
    use usenet_streaming_server::rar::SliceReadAt;

    let junk = SliceReadAt(Bytes::from_static(b"definitely not a rar archive"));
    let refs: Vec<&dyn ReadAt> = vec![&junk];
    assert!(matches!(
        build_archive_map(&refs).await,
        Err(AppError::InvalidRarArchive(_))
    ));
}
