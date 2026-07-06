//! RAR store-mode mapping validated against the committed fixtures
//! (generated + unrar-verified by scripts/gen-rar-fixtures.sh).

use std::sync::Arc;

use usenet_streaming_server::error::AppError;
use usenet_streaming_server::rar::{build_archive_map, volume_sort_key, ArchiveMap, ReadAt};
use usenet_streaming_server::vfs::{DiskFile, RarInnerFile, VirtualFile};

use crate::support::{ffmpeg_available, payload_3mib, TestRng, PAYLOAD_CRC32};

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

// ---- Real-release-shaped RAR5 fixtures ---------------------------------------
//
// These mirror the packing of an actual scene release (verified against the
// real "Dune.Part.2.2024...DarQ-HONE" 16-volume set fetched from Usenet):
//
//   * a small companion file (`.jpg`) is stored *first* in volume 1, so the
//     large media file's data does not start at the top of volume 1 — its
//     `data_offset` is past both the archive/main headers *and* the whole
//     companion block. The old fixtures always put the media file first, so
//     this offset arithmetic was never exercised.
//   * the media payload is a *real, playable* MKV, so `ffprobe` can actually
//     validate that the extracted bytes are correctly aligned (the previous
//     fixtures used random bytes, which ffprobe can never accept).
//   * the media file spans four store-mode volumes with a QO (quick-open)
//     service block + end-of-archive trailer after the split data in each
//     volume — exactly like the real set.
//
// See scripts/gen-rar-fixtures.sh for how they are produced.

const REAL_MKV_LEN: u64 = 3_559_360;

fn real_store_volumes() -> [&'static str; 4] {
    [
        "rar5-store-multi-real.part1.rar",
        "rar5-store-multi-real.part2.rar",
        "rar5-store-multi-real.part3.rar",
        "rar5-store-multi-real.part4.rar",
    ]
}

async fn real_reference_mkv() -> Vec<u8> {
    std::fs::read(fixture("rar5-store-multi-real.mkv")).expect("read reference mkv")
}

/// The companion file precedes the media file in volume 1, and the map must
/// still target the *largest* file (the MKV) with a byte-correct offset that
/// accounts for the companion block sitting ahead of it.
#[tokio::test]
async fn rar5_real_companion_first_maps_media_byte_identical() {
    let volumes = open_volumes(&real_store_volumes()).await;
    let map = map_for(&volumes).await.expect("archive map");

    // Largest file wins even though the companion is stored first.
    assert_eq!(map.inner_file_name, "feature.mkv");
    assert_eq!(map.unpacked_size, REAL_MKV_LEN);
    assert!(map.extents.len() > 1, "media must span multiple volumes");

    // The first extent starts *inside* volume 0, past the companion's data —
    // this is the offset arithmetic the clean fixtures never covered.
    assert_eq!(map.extents[0].volume_index, 0);
    assert!(
        map.extents[0].volume_offset > 8,
        "media data must not start at the top of volume 1"
    );

    let reference = real_reference_mkv().await;
    let vfs: Vec<Arc<dyn VirtualFile>> = volumes
        .into_iter()
        .map(|v| v as Arc<dyn VirtualFile>)
        .collect();
    let inner = RarInnerFile::new(map, vfs).expect("inner");
    assert_eq!(inner.len(), REAL_MKV_LEN);

    // Whole-file extraction is byte-identical...
    let mut extracted = Vec::with_capacity(REAL_MKV_LEN as usize);
    let mut offset = 0u64;
    while offset < inner.len() {
        let chunk = inner.read_at(offset, 512 * 1024).await.expect("read");
        assert!(!chunk.is_empty());
        extracted.extend_from_slice(&chunk);
        offset += chunk.len() as u64;
    }
    assert_eq!(extracted, reference);

    // ...and it starts with the EBML/Matroska magic, proving alignment.
    assert_eq!(&extracted[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
}

/// Random and boundary-crossing reads over the real media map must match the
/// reference MKV exactly (guards the per-extent offset math end to end).
#[tokio::test]
async fn rar5_real_reads_across_boundaries_are_correct() {
    let volumes = open_volumes(&real_store_volumes()).await;
    let map = map_for(&volumes).await.expect("map");
    let boundaries: Vec<u64> = map
        .extents
        .iter()
        .skip(1)
        .map(|e| e.unpacked_start)
        .collect();
    let reference = real_reference_mkv().await;
    let inner = RarInnerFile::new(
        map,
        volumes
            .into_iter()
            .map(|v| v as Arc<dyn VirtualFile>)
            .collect(),
    )
    .expect("inner");

    for boundary in boundaries {
        let start = boundary.saturating_sub(1000);
        let len = 2000usize;
        let chunk = inner.read_at(start, len).await.expect("boundary read");
        let s = start as usize;
        let e = (s + len).min(reference.len());
        assert_eq!(
            &chunk[..],
            &reference[s..e],
            "mismatch at boundary {boundary}"
        );
    }

    let mut rng = TestRng::new(0xD00D);
    for _ in 0..50 {
        let offset = rng.next_u64() % (REAL_MKV_LEN + 512);
        let len = (rng.next_u64() % 300_000) as usize;
        let chunk = inner.read_at(offset, len).await.expect("fuzz read");
        let s = (offset as usize).min(reference.len());
        let e = (s + len).min(reference.len());
        assert_eq!(&chunk[..], &reference[s..e]);
    }
}

/// A compressed (`-m3`) multi-volume RAR5 set must be cleanly rejected, not
/// mis-mapped into garbage bytes.
#[tokio::test]
async fn rar5_compressed_multivolume_is_rejected() {
    let volumes = open_volumes(&[
        "rar5-compressed-multi.part1.rar",
        "rar5-compressed-multi.part2.rar",
        "rar5-compressed-multi.part3.rar",
    ])
    .await;
    match map_for(&volumes).await {
        Err(AppError::CompressedRarUnsupported) => {}
        other => panic!("expected CompressedRarUnsupported, got {other:?}"),
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

/// End-to-end proof that the extracted inner file is not just byte-identical
/// but *media-valid*: serve the [`RarInnerFile`] over the same HTTP range
/// endpoint ffprobe uses in production and confirm a real `ffprobe` reports a
/// video stream. This is the check that would have failed if the store-mode
/// offset mapping were wrong (the previous symptom was "ffprobe exited with
/// status 1"). Skipped when ffprobe is unavailable.
#[tokio::test]
async fn rar5_real_media_is_ffprobe_readable_over_http() {
    if !ffmpeg_available() {
        eprintln!("skipping rar5_real_media_is_ffprobe_readable_over_http: ffprobe not found");
        return;
    }

    let volumes = open_volumes(&real_store_volumes()).await;
    let map = map_for(&volumes).await.expect("map");
    let inner: Arc<dyn VirtualFile> = Arc::new(
        RarInnerFile::new(
            map,
            volumes
                .into_iter()
                .map(|v| v as Arc<dyn VirtualFile>)
                .collect(),
        )
        .expect("inner"),
    );

    // Serve the virtual file over a real loopback HTTP server using the exact
    // production range handler.
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::response::Response;
    use axum::routing::get;

    async fn serve(State(file): State<Arc<dyn VirtualFile>>, headers: HeaderMap) -> Response {
        let range = headers
            .get(axum::http::header::RANGE)
            .and_then(|v| v.to_str().ok());
        usenet_streaming_server::stream::range::range_response(file, "feature.mkv", range, |_e| {})
    }

    let app = axum::Router::new()
        .route("/vfs", get(serve))
        .with_state(inner);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let url = format!("http://{addr}/vfs");
    let probe = usenet_streaming_server::stream::ffprobe::probe_url("ffprobe", &url)
        .await
        .expect("ffprobe must succeed on correctly-mapped store-mode media");
    assert!(
        probe.video_codec.is_some(),
        "ffprobe reported no video stream: {probe:?}"
    );
    assert!(
        probe.duration_secs.unwrap_or(0.0) > 1.0,
        "ffprobe reported an implausible duration: {probe:?}"
    );

    server.abort();
}
