//! Golden files pin the byte format of version 1: a change to the writer that
//! alters the bytes it produces must be deliberate.
//!
//! Regenerate with `TROSNA_BLESS=1 cargo test -p trosna --test golden`.

use std::path::{Path, PathBuf};

use trosna::{
    Batch, Column, CommitOptions, DataType, DeviceSchema, Reader, TimeUnit, Value, WriteOptions,
    Writer,
};

const HEAD_HASH: &str = "2764d75ada9d460758cb9d1cdea72a0039bb93f3dc7b0e5e6ef9fe681b55cdd4";

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/v1/basic.trosna")
}

/// Builds the golden file deterministically (fixed commit times, no fsync).
fn build(path: &Path) {
    let options = WriteOptions {
        sync: false,
        rows_per_block: 8,
        overwrite: true,
        ..WriteOptions::default()
    };
    let mut w = Writer::create_with(path, options).unwrap();
    w.set_metadata("title", "Trosna golden file").unwrap();
    w.create_device(
        DeviceSchema::new("vm01", TimeUnit::Millisecond)
            .with_timezone("UTC")
            .with_column("cpu", DataType::Float64)
            .with_column("ram_mb", DataType::Int64)
            .with_column("swap", DataType::Int32)
            .with_column("ok", DataType::Bool)
            .with_column("state", DataType::Utf8)
            .with_column("temp", DataType::Float32),
    )
    .unwrap();
    let n = 20i64;
    let time: Vec<i64> = (0..n).map(|i| 1_700_000_000_000 + i * 1_000).collect();
    let batch = Batch::new(
        time,
        vec![
            (
                "cpu",
                Column::from((0..n).map(|i| 0.25 + i as f64 / 100.0).collect::<Vec<_>>()),
            ),
            (
                "ram_mb",
                Column::from((0..n).map(|i| 2048 + i / 4).collect::<Vec<i64>>()),
            ),
            (
                "swap",
                Column::from(
                    (0..n)
                        .map(|i| (i % 3 != 0).then_some(0))
                        .collect::<Vec<Option<i32>>>(),
                ),
            ),
            (
                "ok",
                Column::from((0..n).map(|i| i % 7 != 6).collect::<Vec<bool>>()),
            ),
            (
                "state",
                Column::from(
                    (0..n)
                        .map(|i| {
                            if i < 15 {
                                "работает"
                            } else {
                                "перегрузка"
                            }
                        })
                        .collect::<Vec<_>>(),
                ),
            ),
            (
                "temp",
                Column::from((0..n).map(|i| 40.0 + i as f32 * 0.5).collect::<Vec<f32>>()),
            ),
        ],
    )
    .unwrap();
    w.write("vm01", &batch).unwrap();
    w.commit_with(
        CommitOptions::message("initial import")
            .with_author("golden")
            .with_time(1_700_000_100_000_000_000),
    )
    .unwrap();
    w.update("vm01", 1_700_000_003_000, &[("cpu", Value::Float64(0.99))])
        .unwrap();
    w.delete_range("vm01", 1_700_000_010_000..=1_700_000_012_000)
        .unwrap();
    w.annotate(
        "vm01",
        1_700_000_015_000,
        1_700_000_019_000,
        "overload",
        Some("перегрузка памяти"),
    )
    .unwrap();
    w.commit_with(CommitOptions::message("corrections").with_time(1_700_000_200_000_000_000))
        .unwrap();
    w.close().unwrap();
}

#[test]
fn golden_file_is_byte_stable() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh.trosna");
    build(&fresh);
    if std::env::var_os("TROSNA_BLESS").is_some() {
        std::fs::create_dir_all(golden_path().parent().unwrap()).unwrap();
        std::fs::copy(&fresh, golden_path()).unwrap();
        println!(
            "head: {}",
            Reader::open(&fresh).unwrap().head().unwrap().hash_hex()
        );
        return;
    }
    let expected =
        std::fs::read(golden_path()).expect("golden file missing; run with TROSNA_BLESS=1");
    let actual = std::fs::read(&fresh).unwrap();
    assert!(
        actual == expected,
        "the writer no longer produces the golden bytes"
    );
}

#[test]
fn golden_file_reads() {
    let r = Reader::open(golden_path()).unwrap();
    assert!(r.is_finalized());
    assert_eq!(r.format_version(), (1, 0));
    assert_eq!(r.head().unwrap().hash_hex(), HEAD_HASH);
    let commits = r.commits();
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].message.as_deref(), Some("initial import"));
    let now = r.read("vm01").unwrap();
    assert_eq!(now.len(), 17);
    assert_eq!(now.column("cpu").unwrap().value(3), Value::Float64(0.99));
    assert_eq!(
        now.column("state").unwrap().value(16),
        Value::from("перегрузка")
    );
    let first = r.query("vm01").unwrap().as_of(1).collect().unwrap();
    assert_eq!(first.len(), 20);
    let annotations = r.annotations(None, None).unwrap();
    assert_eq!(annotations[0].note.as_deref(), Some("перегрузка памяти"));
    assert!(trosna::verify(golden_path()).unwrap().is_ok());
}
