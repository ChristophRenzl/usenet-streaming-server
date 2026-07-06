//! End-to-end virtual file tests: NZB -> mock NNTP -> yEnc decode ->
//! (optionally RAR store mapping) -> random-access reads.

use std::sync::Arc;
use std::time::Duration;

use usenet_streaming_server::error::AppError;
use usenet_streaming_server::nntp::{NntpPool, NntpTimeouts, PoolOptions};
use usenet_streaming_server::nzb::{
    health_check, main_content_segments, parse_nzb, select_main, MainContent, Nzb,
};
use usenet_streaming_server::rar::{build_archive_map, ReadAt};
use usenet_streaming_server::vfs::{NzbBackedFile, RarInnerFile, SegmentCache, VirtualFile};

use crate::support::{
    add_yenc_file, build_nzb_xml, payload_3mib, xorshift_bytes, MockNntp, TestRng,
};

fn fast_options() -> PoolOptions {
    PoolOptions {
        timeouts: NntpTimeouts {
            connect: Duration::from_secs(2),
            read: Duration::from_secs(5),
            write: Duration::from_secs(5),
        },
        ..PoolOptions::default()
    }
}

fn test_cache() -> SegmentCache {
    SegmentCache::new(64 * 1024 * 1024)
}

/// Fuzz random (offset, len) reads, including reads past EOF, against the
/// reference bytes.
async fn fuzz_reads(file: &dyn VirtualFile, reference: &[u8], seed: u64) {
    let len = reference.len() as u64;
    assert_eq!(file.len(), len);
    let mut rng = TestRng::new(seed);
    for i in 0..50 {
        let offset = rng.next_u64() % (len + 1024);
        let buf_len = (rng.next_u64() % 200_000) as usize;
        let chunk = file.read_at(offset, buf_len).await.expect("fuzz read");
        let start = (offset as usize).min(reference.len());
        let end = (start + buf_len).min(reference.len());
        assert_eq!(
            &chunk[..],
            &reference[start..end],
            "read #{i} at {offset}+{buf_len}"
        );
    }
    // Explicit suffix reads near EOF.
    let tail = file.read_at(len - 10, 100).await.expect("suffix read");
    assert_eq!(&tail[..], &reference[reference.len() - 10..]);
    assert!(file.read_at(len, 16).await.expect("read at EOF").is_empty());
    assert!(file
        .read_at(len + 5, 16)
        .await
        .expect("read past EOF")
        .is_empty());
}

#[tokio::test]
async fn plain_media_file_reads_byte_identical() {
    let payload = xorshift_bytes(300_000);
    let server = MockNntp::start(Some(("user", "pass"))).await;
    let segments = add_yenc_file(&server, "plain", &payload, 20_000, "movie.mkv");
    let nzb = parse_nzb(&build_nzb_xml(&[(
        r#"Movie [1/1] - "movie.mkv" yEnc (1/15)"#.to_string(),
        segments,
    )]))
    .expect("parse nzb");

    let main = select_main(&nzb).expect("select");
    let MainContent::Plain(file_ref) = &main else {
        panic!("expected plain media, got {main:?}");
    };
    assert_eq!(file_ref.file_name, "movie.mkv");

    let pool = NntpPool::with_options(vec![server.provider("p", 0, 4)], fast_options());
    let file = NzbBackedFile::open(&nzb.files[file_ref.index], pool, test_cache(), 4)
        .await
        .expect("open");
    assert_eq!(file.file_name(), "movie.mkv");
    assert_eq!(file.segment_count(), 15);

    // Sequential read (drives the readahead path) then fuzz.
    let mut sequential = Vec::new();
    let mut offset = 0u64;
    while offset < VirtualFile::len(&file) {
        let chunk = VirtualFile::read_at(&file, offset, 33_000)
            .await
            .expect("sequential read");
        sequential.extend_from_slice(&chunk);
        offset += chunk.len() as u64;
    }
    assert_eq!(sequential, payload);

    fuzz_reads(&file, &payload, 0xABCD).await;
}

/// Build the RAR-set scenario: fixture volumes served as yEnc articles.
async fn rar_scenario(server: &MockNntp) -> (Nzb, MainContent) {
    let mut files = Vec::new();
    for part in 1..=4 {
        let name = format!("rar5-store-multi.part{part}.rar");
        let bytes = std::fs::read(format!(
            "{}/tests/fixtures/rar/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("read fixture");
        let inner_name = format!("movie.part{part}.rar");
        let segments = add_yenc_file(server, &format!("vol{part}"), &bytes, 50_000, &inner_name);
        files.push((
            format!(r#"Movie [{part}/5] - "{inner_name}" yEnc"#),
            segments,
        ));
    }
    // Classification noise: par2 + nfo entries that must be ignored.
    files.push((
        r#"Movie [5/5] - "movie.par2" yEnc"#.to_string(),
        vec![("par2-seg@mock".to_string(), 5_000)],
    ));
    files.push((
        r#"Movie [0/5] - "movie.nfo" yEnc"#.to_string(),
        vec![("nfo-seg@mock".to_string(), 400)],
    ));

    let nzb = parse_nzb(&build_nzb_xml(&files)).expect("parse nzb");
    let main = select_main(&nzb).expect("select");
    (nzb, main)
}

#[tokio::test]
async fn rar_set_end_to_end_reads_byte_identical() {
    let server = MockNntp::start(None).await;
    let (nzb, main) = rar_scenario(&server).await;

    let MainContent::RarSet(set) = &main else {
        panic!("expected RAR set, got {main:?}");
    };
    let names: Vec<_> = set.iter().map(|f| f.file_name.as_str()).collect();
    assert_eq!(
        names,
        [
            "movie.part1.rar",
            "movie.part2.rar",
            "movie.part3.rar",
            "movie.part4.rar"
        ]
    );

    let pool = NntpPool::with_options(vec![server.provider("p", 0, 8)], fast_options());
    let cache = test_cache();
    let mut volumes: Vec<Arc<NzbBackedFile>> = Vec::new();
    for file_ref in set {
        volumes.push(Arc::new(
            NzbBackedFile::open(&nzb.files[file_ref.index], pool.clone(), cache.clone(), 4)
                .await
                .expect("open volume"),
        ));
    }

    // Parse RAR headers straight over Usenet-backed volumes.
    let refs: Vec<&dyn ReadAt> = volumes.iter().map(|v| v.as_ref() as &dyn ReadAt).collect();
    let map = build_archive_map(&refs).await.expect("archive map");
    assert_eq!(map.inner_file_name, "payload.bin");

    let payload = payload_3mib();
    let inner = RarInnerFile::new(
        map,
        volumes
            .iter()
            .map(|v| v.clone() as Arc<dyn VirtualFile>)
            .collect(),
    )
    .expect("inner file");
    assert_eq!(inner.len(), payload.len() as u64);

    fuzz_reads(&inner, &payload, 0x5EED).await;
}

/// Same as [`rar_set_end_to_end_reads_byte_identical`] but over the
/// real-release-shaped fixtures: a companion file precedes the media file in
/// volume 1, the media is a real MKV, and each volume is split into many yEnc
/// segments (as a real 700+-segment volume would be). Exercises the full
/// NzbBackedFile -> RAR store map -> RarInnerFile read path on the packing
/// that actually ships in the wild.
#[tokio::test]
async fn real_shaped_rar_set_reads_byte_identical_over_usenet() {
    let server = MockNntp::start(None).await;
    let fixture = |name: &str| format!("{}/tests/fixtures/rar/{name}", env!("CARGO_MANIFEST_DIR"));

    let mut files = Vec::new();
    for part in 1..=4 {
        let name = format!("rar5-store-multi-real.part{part}.rar");
        let bytes = std::fs::read(fixture(&name)).expect("read fixture");
        let inner_name = format!("release.part{part}.rar");
        // 64 KiB parts -> up to ~16 segments per 1 MiB volume: multi-segment
        // volumes like the real set (which has ~730 segments per volume).
        let segments = add_yenc_file(
            &server,
            &format!("rv{part}"),
            &bytes,
            64 * 1024,
            &inner_name,
        );
        files.push((format!(r#"Rel [{part}/5] - "{inner_name}" yEnc"#), segments));
    }
    files.push((
        r#"Rel [5/5] - "release.par2" yEnc"#.to_string(),
        vec![("par2@mock".to_string(), 5_000)],
    ));

    let nzb = parse_nzb(&build_nzb_xml(&files)).expect("parse nzb");
    let main = select_main(&nzb).expect("select");
    let MainContent::RarSet(set) = &main else {
        panic!("expected RAR set, got {main:?}");
    };

    let pool = NntpPool::with_options(vec![server.provider("p", 0, 8)], fast_options());
    let cache = test_cache();
    let mut volumes: Vec<Arc<NzbBackedFile>> = Vec::new();
    for file_ref in set {
        volumes.push(Arc::new(
            NzbBackedFile::open(&nzb.files[file_ref.index], pool.clone(), cache.clone(), 4)
                .await
                .expect("open volume"),
        ));
    }

    let refs: Vec<&dyn ReadAt> = volumes.iter().map(|v| v.as_ref() as &dyn ReadAt).collect();
    let map = build_archive_map(&refs).await.expect("archive map");
    assert_eq!(
        map.inner_file_name, "feature.mkv",
        "must target the MKV, not the companion"
    );

    let reference =
        std::fs::read(fixture("rar5-store-multi-real.mkv")).expect("read reference mkv");
    let inner = RarInnerFile::new(
        map,
        volumes
            .iter()
            .map(|v| v.clone() as Arc<dyn VirtualFile>)
            .collect(),
    )
    .expect("inner file");
    assert_eq!(inner.len(), reference.len() as u64);

    // The head must be the EBML/Matroska magic (offset alignment across the
    // companion block + main/archive headers).
    let head = inner.read_at(0, 4).await.expect("head read");
    assert_eq!(&head[..], &[0x1A, 0x45, 0xDF, 0xA3]);

    fuzz_reads(&inner, &reference, 0xC0FFEE).await;
}

#[tokio::test]
async fn health_check_reports_green_and_red() {
    let server = MockNntp::start(None).await;
    let (nzb, main) = rar_scenario(&server).await;
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 4)], fast_options());

    let segments = main_content_segments(&nzb, &main);
    assert!(segments.len() > 20);

    let report = health_check(&segments, &pool, 10).await.expect("health");
    assert_eq!(report.checked, 10);
    assert_eq!(report.missing, 0);
    assert!(report.ok);

    // Remove the very first segment: first/last rule must trip.
    server.remove_article(&segments[0].message_id);
    let report = health_check(&segments, &pool, 10).await.expect("health");
    assert_eq!(report.missing, 1);
    assert!(!report.ok);

    // Remove a middle sampled segment too: ratio drops below 95%.
    server.remove_article(&segments[segments.len() / 2].message_id);
    let report = health_check(&segments, &pool, 10).await.expect("health");
    assert!(report.missing >= 1);
    assert!(!report.ok);
}

#[tokio::test]
async fn missing_segment_read_is_typed_error() {
    let payload = xorshift_bytes(120_000);
    let server = MockNntp::start(None).await;
    let segments = add_yenc_file(&server, "gap", &payload, 20_000, "movie.mkv");
    let nzb = parse_nzb(&build_nzb_xml(&[(
        r#"Movie - "movie.mkv" yEnc"#.to_string(),
        segments.clone(),
    )]))
    .expect("parse");

    let pool = NntpPool::with_options(vec![server.provider("p", 0, 4)], fast_options());
    let file = NzbBackedFile::open(&nzb.files[0], pool, test_cache(), 0)
        .await
        .expect("open");

    // Nuke a middle segment after open (open already fetched segment 1).
    let victim = &segments[3].0;
    server.remove_article(victim);

    // Reads before the gap still work.
    let ok = VirtualFile::read_at(&file, 0, 10_000)
        .await
        .expect("early read");
    assert_eq!(&ok[..], &payload[..10_000]);

    // A read inside the missing segment surfaces MissingSegment.
    match VirtualFile::read_at(&file, 65_000, 4_096).await {
        Err(AppError::MissingSegment(id)) => assert_eq!(&id, victim),
        other => panic!("expected MissingSegment, got {other:?}"),
    }
}

#[tokio::test]
async fn opening_a_file_with_missing_first_segment_fails() {
    let server = MockNntp::start(None).await;
    let nzb = parse_nzb(&build_nzb_xml(&[(
        r#"X - "gone.mkv" yEnc"#.to_string(),
        vec![("never-posted@mock".to_string(), 1000)],
    )]))
    .expect("parse");
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 2)], fast_options());
    match NzbBackedFile::open(&nzb.files[0], pool, test_cache(), 0).await {
        Err(AppError::MissingSegment(id)) => assert_eq!(id, "never-posted@mock"),
        other => panic!("expected MissingSegment, got {other:?}"),
    }
}

#[tokio::test]
async fn corrupt_segment_crc_is_an_upstream_error() {
    let payload = xorshift_bytes(30_000);
    let server = MockNntp::start(None).await;
    let segments = add_yenc_file(&server, "bad", &payload, 20_000, "movie.mkv");

    // Corrupt segment 2: flip a data byte but keep the yEnc trailer intact.
    let mut corrupted = crate::support::yenc_encode_part(
        &payload[20_000..],
        "movie.mkv",
        2,
        2,
        20_000,
        payload.len() as u64,
    );
    let flip = corrupted.len() / 2;
    corrupted[flip] = corrupted[flip].wrapping_add(1);
    server.add_article(&segments[1].0, corrupted);

    let nzb = parse_nzb(&build_nzb_xml(&[(
        r#"X - "movie.mkv" yEnc"#.to_string(),
        segments,
    )]))
    .expect("parse");
    let pool = NntpPool::with_options(vec![server.provider("p", 0, 2)], fast_options());
    let file = NzbBackedFile::open(&nzb.files[0], pool, test_cache(), 0)
        .await
        .expect("open");

    match VirtualFile::read_at(&file, 25_000, 1_000).await {
        Err(AppError::Upstream(msg)) => {
            assert!(msg.contains("yEnc"), "unexpected message: {msg}")
        }
        other => panic!("expected Upstream yEnc error, got {other:?}"),
    }
}
