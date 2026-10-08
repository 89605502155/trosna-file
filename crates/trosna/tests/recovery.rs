//! Crash consistency, recovery and robustness against damaged files.

use std::path::{Path, PathBuf};

use trosna::{
    Batch, Column, CommitOptions, DataType, DeviceSchema, Error, Reader, TimeUnit, Value, Writer,
};

/// Writes a small file with three commits and returns its path and the
/// expected rows of each version.
fn build(dir: &Path) -> (PathBuf, Vec<Vec<i64>>) {
    let file = dir.join("source.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(
        DeviceSchema::new("d", TimeUnit::Millisecond)
            .with_column("v", DataType::Int64)
            .with_column("s", DataType::Utf8),
    )
    .unwrap();
    let batch = Batch::new(
        (0..40).collect(),
        vec![
            (
                "v",
                Column::from((0..40).map(|i| i * i).collect::<Vec<i64>>()),
            ),
            (
                "s",
                Column::from((0..40).map(|i| format!("значение {i}")).collect::<Vec<_>>()),
            ),
        ],
    )
    .unwrap();
    w.write("d", &batch).unwrap();
    w.commit_with(CommitOptions::default().with_time(1))
        .unwrap();
    w.delete_range("d", 10..20).unwrap();
    w.annotate("d", 0, 5, "start", None).unwrap();
    w.commit_with(CommitOptions::default().with_time(2))
        .unwrap();
    w.write_row("d", 15, &[("v", Value::Int64(-1))]).unwrap();
    w.set_metadata("k", "v").unwrap();
    w.commit_with(CommitOptions::default().with_time(3))
        .unwrap();
    w.close().unwrap();
    let r = Reader::open(&file).unwrap();
    let versions = (0..=3)
        .map(|v| {
            r.query("d")
                .unwrap()
                .as_of(v)
                .collect()
                .unwrap()
                .time()
                .to_vec()
        })
        .collect();
    (file, versions)
}

/// The end of each commit frame (commits have no author or message).
fn commit_ends(file: &Path) -> Vec<u64> {
    Reader::open(file)
        .unwrap()
        .commits()
        .iter()
        .map(|c| c.offset + 16 + 126)
        .collect()
}

#[test]
fn every_truncation_reads_the_last_complete_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (file, versions) = build(dir.path());
    let bytes = std::fs::read(&file).unwrap();
    let ends = commit_ends(&file);
    let cut_file = dir.path().join("cut.trosna");
    for cut in 0..=bytes.len() {
        std::fs::write(&cut_file, &bytes[..cut]).unwrap();
        let expected = ends.iter().filter(|&&end| end <= cut as u64).count();
        match Reader::open(&cut_file) {
            Err(Error::NotTrosna) => assert!(cut < 8),
            Err(e) => panic!("cut at {cut}: {e}"),
            Ok(r) => {
                let head = r.head().map_or(0, |c| c.number) as usize;
                assert_eq!(head, expected, "cut at {cut}");
                let rows = if head == 0 && r.devices().len() == 0 {
                    Vec::new()
                } else {
                    r.read("d").unwrap().time().to_vec()
                };
                assert_eq!(rows, versions[head], "cut at {cut}");
            }
        }
    }
}

#[test]
fn writer_recovers_an_interrupted_file_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let (file, versions) = build(dir.path());
    let bytes = std::fs::read(&file).unwrap();
    let ends = commit_ends(&file);
    // Cut inside the third commit's frames.
    let cut = (ends[1] + ends[2]) / 2;
    let damaged = dir.path().join("damaged.trosna");
    std::fs::write(&damaged, &bytes[..cut as usize]).unwrap();

    let mut w = Writer::open(&damaged).unwrap();
    let report = w.recovery().cloned().unwrap();
    assert_eq!(report.truncated_bytes, cut - ends[1]);
    assert_eq!(w.head(), 2);
    w.write_row("d", 100, &[("v", Value::Int64(100))]).unwrap();
    w.close().unwrap();

    assert!(std::fs::metadata(&damaged).unwrap().len() > ends[1]);
    let r = Reader::open(&damaged).unwrap();
    assert!(r.is_finalized());
    let mut expected = versions[2].clone();
    expected.push(100);
    assert_eq!(r.read("d").unwrap().time(), expected);
    assert!(trosna::verify(&damaged).unwrap().is_ok());
}

#[test]
fn recover_finalizes_without_writing_data() {
    let dir = tempfile::tempdir().unwrap();
    let (file, versions) = build(dir.path());
    let bytes = std::fs::read(&file).unwrap();
    let ends = commit_ends(&file);
    let damaged = dir.path().join("r.trosna");
    std::fs::write(&damaged, &bytes[..ends[2] as usize + 5]).unwrap();
    let report = trosna::recover(&damaged, false).unwrap();
    assert!(!report.was_finalized);
    assert_eq!(report.recovery.unwrap().truncated_bytes, 5);
    let r = Reader::open(&damaged).unwrap();
    assert!(r.is_finalized());
    assert_eq!(r.read("d").unwrap().time(), versions[3]);
    let again = trosna::recover(&damaged, false).unwrap();
    assert!(again.was_finalized);
}

#[test]
fn corruption_in_the_middle_is_not_truncated_silently() {
    let dir = tempfile::tempdir().unwrap();
    let (file, _) = build(dir.path());
    let mut bytes = std::fs::read(&file).unwrap();
    let ends = commit_ends(&file);
    // Damage a byte inside the second commit and drop the footer so that a scan is needed.
    let target = (ends[0] + 20) as usize;
    bytes[target] ^= 0xff;
    let damaged = dir.path().join("mid.trosna");
    std::fs::write(&damaged, &bytes[..ends[2] as usize]).unwrap();

    assert!(matches!(
        Writer::open(&damaged),
        Err(Error::Corrupted { .. })
    ));
    assert_eq!(
        std::fs::metadata(&damaged).unwrap().len(),
        ends[2],
        "file untouched"
    );
    let report = trosna::verify(&damaged).unwrap();
    assert!(!report.is_ok());
    let repaired = trosna::recover(&damaged, true).unwrap();
    assert!(repaired.recovery.is_some());
    let r = Reader::open(&damaged).unwrap();
    assert_eq!(r.head().unwrap().number, 1);
}

#[test]
fn verify_detects_tampering_even_with_valid_checksums() {
    let dir = tempfile::tempdir().unwrap();
    let (file, _) = build(dir.path());
    assert!(trosna::verify(&file).unwrap().is_ok());

    // Change a stored value and fix the frame CRC, as a careful forger would.
    let mut bytes = std::fs::read(&file).unwrap();
    let r = Reader::open(&file).unwrap();
    let block = &r.blocks("d").unwrap()[0];
    let frame_start = block.offset as usize;
    let len =
        u32::from_le_bytes(bytes[frame_start + 8..frame_start + 12].try_into().unwrap()) as usize;
    let last_segment = frame_start + 12 + len - 1;
    bytes[last_segment] ^= 0x01;
    // Recompute the segment CRC is not possible without decoding the header,
    // so only the frame CRC is fixed: the segment CRC still catches the change.
    let crc = crc32c_of(&bytes[frame_start + 4..frame_start + 12 + len]);
    bytes[frame_start + 12 + len..frame_start + 16 + len].copy_from_slice(&crc.to_le_bytes());
    let forged = dir.path().join("forged.trosna");
    std::fs::write(&forged, &bytes).unwrap();
    let report = trosna::verify(&forged).unwrap();
    assert!(!report.is_ok());
    assert!(
        report.problems.iter().any(|p| p.contains("content hash")),
        "{report:?}"
    );
}

/// Software CRC32C (Castagnoli), so that the test does not depend on the crate's internals.
fn crc32c_of(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn flipping_any_byte_never_panics() {
    let dir = tempfile::tempdir().unwrap();
    let (file, _) = build(dir.path());
    let bytes = std::fs::read(&file).unwrap();
    let flipped = dir.path().join("flip.trosna");
    for i in 0..bytes.len() {
        let mut copy = bytes.clone();
        copy[i] ^= 0x5a;
        std::fs::write(&flipped, &copy).unwrap();
        if let Ok(r) = Reader::open(&flipped) {
            for d in r.devices().map(|d| d.name.clone()).collect::<Vec<_>>() {
                let _ = r.read(&d);
                let _ = r.diff(&d, 0, None);
            }
            let _ = r.annotations(None, None);
            let _ = r.commits();
        }
        let _ = trosna::verify(&flipped);
    }
}
