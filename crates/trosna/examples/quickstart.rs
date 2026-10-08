//! Write a device, edit a point, annotate an interval and read an old version.
//!
//! Run with `cargo run -p trosna --example quickstart`.

use trosna::{Batch, Column, CommitOptions, DataType, DeviceSchema, Reader, TimeUnit, Writer};

fn main() -> trosna::Result<()> {
    let dir = std::env::temp_dir().join(format!("trosna-quickstart-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("metrics.trosna");

    // Commit 1: a device and three points.
    let mut writer = Writer::create(&path)?;
    writer.create_device(
        DeviceSchema::new("vm01", TimeUnit::Millisecond)
            .with_timezone("Europe/Moscow")
            .with_column("cpu", DataType::Float64)
            .with_column("state", DataType::Utf8),
    )?;
    let t0 = 1_791_442_800_000; // 2026-10-08 10:00:00 +03:00
    writer.write(
        "vm01",
        &Batch::new(
            vec![t0, t0 + 1_000, t0 + 2_000],
            vec![
                ("cpu", Column::from(vec![0.31, 0.97, 0.33])),
                ("state", Column::from(vec!["ok", "spike", "ok"])),
            ],
        )?,
    )?;
    writer.commit_with(CommitOptions::message("initial import"))?;

    // Commit 2: fix the spike and explain it.
    writer.update(
        "vm01",
        t0 + 1_000,
        &[("cpu", 0.35.into()), ("state", "ok".into())],
    )?;
    let id = writer.annotate("vm01", t0 + 500, t0 + 1_500, "backup", Some("nightly job"))?;
    writer.commit_with(CommitOptions::message("remove backup spike").with_author("analyst"))?;
    writer.close()?;

    let reader = Reader::open(&path)?;
    let now = reader.read("vm01")?;
    let before = reader.query("vm01")?.as_of(1).collect()?;
    println!(
        "cpu now:    {:?}",
        now.column("cpu").and_then(Column::as_f64)
    );
    println!(
        "cpu before: {:?}",
        before.column("cpu").and_then(Column::as_f64)
    );
    println!(
        "annotation {id}: {:?}",
        reader.annotations(Some("vm01"), None)?
    );
    for commit in reader.commits() {
        println!(
            "commit {} {} {:?}",
            commit.number,
            commit.short_hash(),
            commit.message
        );
    }
    let diff = reader.diff("vm01", 1, None)?;
    println!("{} point(s) changed since commit 1", diff.points.len());

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
