//! Editing, versions, history, annotations and diffs.

use trosna::{
    Batch, ChangeKind, Column, CommitOptions, DataType, DeviceSchema, Error, Reader, TimeUnit,
    Value, Writer,
};

fn new_file(dir: &tempfile::TempDir) -> (std::path::PathBuf, Writer) {
    let file = dir.path().join("edit.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(
        DeviceSchema::new("vm01", TimeUnit::Second)
            .with_column("cpu", DataType::Float64)
            .with_column("state", DataType::Utf8),
    )
    .unwrap();
    let batch = Batch::new(
        vec![10, 20, 30, 40],
        vec![
            ("cpu", Column::from(vec![0.1, 0.2, 0.3, 0.4])),
            ("state", Column::from(vec!["ok", "ok", "ok", "ok"])),
        ],
    )
    .unwrap();
    w.write("vm01", &batch).unwrap();
    w.commit_with(
        CommitOptions::message("initial")
            .with_author("collector")
            .with_time(1_000),
    )
    .unwrap()
    .unwrap();
    (file, w)
}

fn cpu(batch: &Batch) -> Vec<Value> {
    (0..batch.len())
        .map(|i| batch.column("cpu").unwrap().value(i))
        .collect()
}

#[test]
fn insert_update_delete_and_time_travel() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut w) = new_file(&dir);

    // commit 2: insert a point between existing ones and change another
    w.write_row(
        "vm01",
        25,
        &[("cpu", Value::from(0.25)), ("state", Value::from("new"))],
    )
    .unwrap();
    w.update("vm01", 30, &[("cpu", Value::from(0.99))]).unwrap();
    w.commit().unwrap().unwrap();
    // commit 3: delete one point and a range
    w.delete("vm01", 10).unwrap();
    w.delete_range("vm01", 35..=45).unwrap();
    w.commit().unwrap().unwrap();
    // commit 4: re-insert a deleted point
    w.write_row("vm01", 40, &[("cpu", Value::from(4.0))])
        .unwrap();
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    assert_eq!(r.commits().len(), 4);
    let v1 = r.query("vm01").unwrap().as_of(1).collect().unwrap();
    assert_eq!(v1.time(), &[10, 20, 30, 40]);
    let v2 = r.query("vm01").unwrap().as_of(2).collect().unwrap();
    assert_eq!(v2.time(), &[10, 20, 25, 30, 40]);
    assert_eq!(cpu(&v2)[3], Value::Float64(0.99));
    assert_eq!(
        v2.column("state").unwrap().value(3),
        Value::from("ok"),
        "update keeps other columns"
    );
    let v3 = r.query("vm01").unwrap().as_of(3).collect().unwrap();
    assert_eq!(v3.time(), &[20, 25, 30]);
    let v4 = r.read("vm01").unwrap();
    assert_eq!(v4.time(), &[20, 25, 30, 40]);
    assert_eq!(cpu(&v4)[3], Value::Float64(4.0));
    assert_eq!(v4.column("state").unwrap().value(3), Value::Null);
    let v0 = r.query("vm01").unwrap().as_of(0).collect().unwrap();
    assert!(v0.is_empty());
    assert!(matches!(
        r.query("vm01").unwrap().as_of(9).collect(),
        Err(Error::UnknownCommit(9))
    ));
    assert_eq!(r.commit_at(999), 0);
    assert_eq!(r.commit_at(1_000), 1);
}

#[test]
fn update_requires_an_existing_point() {
    let dir = tempfile::tempdir().unwrap();
    let (_, mut w) = new_file(&dir);
    assert!(matches!(
        w.update("vm01", 15, &[("cpu", Value::from(1.0))]),
        Err(Error::PointNotFound { time: 15, .. })
    ));
    // a point inserted in the same transaction can be updated
    w.write_row("vm01", 15, &[("cpu", Value::from(1.0))])
        .unwrap();
    w.update("vm01", 15, &[("cpu", Value::from(2.0))]).unwrap();
    assert_eq!(w.get("vm01", 15).unwrap().unwrap()[0], Value::Float64(2.0));
    // a point deleted in the same transaction cannot
    w.delete("vm01", 20).unwrap();
    assert!(w.update("vm01", 20, &[("cpu", Value::from(1.0))]).is_err());
    assert!(w.get("vm01", 20).unwrap().is_none());
    w.close().unwrap();
}

#[test]
fn rollback_discards_the_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut w) = new_file(&dir);
    w.write_row("vm01", 50, &[("cpu", Value::from(0.5))])
        .unwrap();
    w.delete("vm01", 10).unwrap();
    w.create_device(DeviceSchema::new("tmp", TimeUnit::Second).with_column("x", DataType::Bool))
        .unwrap();
    let id = w.annotate("vm01", 10, 20, "x", None).unwrap();
    w.set_metadata("k", "v").unwrap();
    w.rollback().unwrap();
    assert!(w.device("tmp").is_err());
    assert!(w.commit().unwrap().is_none(), "nothing left to commit");
    // identifiers of rolled back annotations are reused
    assert_eq!(w.annotate("vm01", 1, 2, "y", None).unwrap(), id);
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    assert_eq!(r.read("vm01").unwrap().time(), &[10, 20, 30, 40]);
    assert!(r.metadata().is_empty());
    assert_eq!(r.annotations(None, None).unwrap().len(), 1);
    assert!(trosna::verify(&file).unwrap().is_ok());
}

#[test]
fn annotations_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut w) = new_file(&dir);
    let a = w
        .annotate("vm01", 15, 25, "аномалия", Some("скачок CPU"))
        .unwrap();
    let b = w.annotate("vm01", 30, 30, "reboot", None).unwrap();
    assert_eq!((a, b), (1, 2));
    assert!(w.annotate("vm01", 5, 1, "bad", None).is_err());
    assert!(w.annotate("nope", 1, 2, "bad", None).is_err());
    w.commit().unwrap();
    w.update_annotation(a, 14, 26, "anomaly", None).unwrap();
    w.remove_annotation(b).unwrap();
    assert!(matches!(
        w.remove_annotation(b),
        Err(Error::UnknownAnnotation(2))
    ));
    assert!(matches!(
        w.update_annotation(77, 1, 2, "x", None),
        Err(Error::UnknownAnnotation(77))
    ));
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    let v2 = r.annotations(Some("vm01"), Some(2)).unwrap();
    assert_eq!(v2.len(), 2);
    assert_eq!(v2[0].label, "аномалия");
    assert_eq!(v2[0].note.as_deref(), Some("скачок CPU"));
    let now = r.annotations(None, None).unwrap();
    assert_eq!(now.len(), 1);
    assert_eq!(
        (now[0].start, now[0].end, now[0].label.as_str()),
        (14, 26, "anomaly")
    );
    assert_eq!(now[0].device, "vm01");

    let diff = r.diff("vm01", 2, None).unwrap();
    assert!(diff.points.is_empty());
    let kinds: Vec<_> = diff.annotations.iter().map(|c| (c.id, c.kind)).collect();
    assert_eq!(kinds, [(1, ChangeKind::Changed), (2, ChangeKind::Removed)]);
}

#[test]
fn diff_between_versions() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut w) = new_file(&dir);
    w.write_row("vm01", 25, &[("cpu", Value::from(0.25))])
        .unwrap();
    w.update("vm01", 30, &[("state", Value::from("warn"))])
        .unwrap();
    w.delete("vm01", 40).unwrap();
    // rewriting an identical row is not a change
    w.write_row(
        "vm01",
        20,
        &[("cpu", Value::from(0.2)), ("state", Value::from("ok"))],
    )
    .unwrap();
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    let diff = r.diff("vm01", 1, Some(2)).unwrap();
    assert_eq!(diff.columns, ["cpu", "state"]);
    let summary: Vec<(i64, ChangeKind)> = diff.points.iter().map(|p| (p.time, p.kind)).collect();
    assert_eq!(
        summary,
        [
            (25, ChangeKind::Added),
            (30, ChangeKind::Changed),
            (40, ChangeKind::Removed)
        ]
    );
    let changed = &diff.points[1];
    assert_eq!(changed.before.as_ref().unwrap()[1], Value::from("ok"));
    assert_eq!(changed.after.as_ref().unwrap()[1], Value::from("warn"));
    let from_empty = r.diff("vm01", 0, Some(1)).unwrap();
    assert_eq!(from_empty.points.len(), 4);
    assert!(
        from_empty
            .points
            .iter()
            .all(|p| p.kind == ChangeKind::Added)
    );
    assert!(r.diff("vm01", 2, Some(1)).is_err());
}

#[test]
fn commit_history_is_hash_chained() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut w) = new_file(&dir);
    w.delete("vm01", 10).unwrap();
    let second = w
        .commit_with(CommitOptions::message("remove outlier"))
        .unwrap()
        .unwrap();
    w.set_metadata("calibrated", "yes").unwrap();
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    let log = r.commits();
    assert_eq!(log.len(), 3);
    assert_eq!(log[0].message.as_deref(), Some("initial"));
    assert_eq!(log[0].author.as_deref(), Some("collector"));
    assert_eq!(log[0].time, 1_000);
    assert_eq!(log[0].changes.devices_created, ["vm01"]);
    assert_eq!(log[0].changes.rows_written, 4);
    assert_eq!(log[0].prev_hash, [0; 32]);
    assert_eq!(log[1].prev_hash, log[0].hash);
    assert_eq!(log[1], second);
    assert_eq!(log[1].changes.ranges_deleted, 1);
    assert_eq!(log[2].prev_hash, log[1].hash);
    assert!(log[2].changes.metadata_changed);
    assert_eq!(r.head().unwrap().hash, log[2].hash);
    assert_eq!(log[2].short_hash().len(), 12);
}

#[test]
fn compaction_preserves_state_and_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut w) = new_file(&dir);
    for i in 0..20 {
        w.write_row("vm01", 100 + i, &[("cpu", Value::from(i as f64))])
            .unwrap();
        w.commit().unwrap();
    }
    w.delete_range("vm01", 105..=110).unwrap();
    w.annotate("vm01", 100, 120, "load test", None).unwrap();
    w.set_metadata("owner", "lab").unwrap();
    w.close().unwrap();

    let target = dir.path().join("compact.trosna");
    let report = trosna::compact(&file, &target, None, Default::default()).unwrap();
    let source = Reader::open(&file).unwrap();
    let compacted = Reader::open(&target).unwrap();
    assert_eq!(
        compacted.read("vm01").unwrap(),
        source.read("vm01").unwrap()
    );
    assert_eq!(
        compacted.annotations(None, None).unwrap(),
        source.annotations(None, None).unwrap()
    );
    assert_eq!(compacted.commits().len(), 1);
    assert_eq!(
        compacted.commits()[0].prev_hash,
        source.head().unwrap().hash
    );
    assert_eq!(report.origin, source.head().unwrap().hash);
    assert_eq!(
        compacted.metadata().get("owner").map(String::as_str),
        Some("lab")
    );
    assert_eq!(
        compacted.metadata().get("trosna.compacted_from").cloned(),
        Some(source.head().unwrap().hash_hex())
    );
    assert!(compacted.blocks("vm01").unwrap().len() < source.blocks("vm01").unwrap().len());
    let verified = trosna::verify(&target).unwrap();
    assert!(verified.is_ok(), "{verified:?}");
    assert_eq!(verified.origin, Some(source.head().unwrap().hash));

    // compacting an older version
    let old = dir.path().join("old.trosna");
    trosna::compact(&file, &old, Some(1), Default::default()).unwrap();
    assert_eq!(
        Reader::open(&old).unwrap().read("vm01").unwrap().time(),
        &[10, 20, 30, 40]
    );
}
