//! Writing and reading: types, nulls, projections, time ranges, blocks.

use trosna::{
    Batch, Codec, Column, DataType, DeviceSchema, EncodingPolicy, Error, ReadOptions, Reader,
    TimeUnit, Value, WriteOptions, Writer,
};

fn path(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(name)
}

fn all_types_device(name: &str) -> DeviceSchema {
    DeviceSchema::new(name, TimeUnit::Nanosecond)
        .with_column("b", DataType::Bool)
        .with_column("i32", DataType::Int32)
        .with_column("i64", DataType::Int64)
        .with_column("f32", DataType::Float32)
        .with_column("f64", DataType::Float64)
        .with_column("s", DataType::Utf8)
}

fn all_types_batch(n: usize, offset: i64) -> Batch {
    let time: Vec<i64> = (0..n as i64).map(|i| offset + i * 1_000_000_007).collect();
    let b: Vec<Option<bool>> = (0..n).map(|i| (i % 7 != 0).then_some(i % 3 == 0)).collect();
    let i32s: Vec<Option<i32>> = (0..n)
        .map(|i| (i % 5 != 0).then_some(i as i32 * -3))
        .collect();
    let i64s: Vec<i64> = (0..n as i64).map(|i| i64::MAX - i * i).collect();
    let f32s: Vec<Option<f32>> = (0..n)
        .map(|i| (i % 4 != 1).then_some(i as f32 / 8.0))
        .collect();
    let f64s: Vec<f64> = (0..n)
        .map(|i| match i % 9 {
            0 => f64::NAN,
            1 => -0.0,
            _ => 20.0 + (i as f64).sin(),
        })
        .collect();
    let s: Vec<Option<String>> = (0..n)
        .map(|i| (i % 6 != 2).then(|| ["норма", "warn", "авария"][i % 3].to_owned()))
        .collect();
    Batch::new(
        time,
        vec![
            ("b", Column::from(b)),
            ("i32", Column::from(i32s)),
            ("i64", Column::from(i64s)),
            ("f32", Column::from(f32s)),
            ("f64", Column::from(f64s)),
            ("s", Column::from(s)),
        ],
    )
    .unwrap()
}

#[test]
fn round_trip_all_types_codecs_and_policies() {
    let dir = tempfile::tempdir().unwrap();
    for codec in [Codec::None, Codec::Lz4, Codec::Zstd] {
        for policy in [
            EncodingPolicy::Adaptive,
            EncodingPolicy::Classic,
            EncodingPolicy::Plain,
        ] {
            let file = path(&dir, &format!("{codec}-{policy:?}.trosna"));
            let options = WriteOptions {
                codec,
                encoding: policy,
                rows_per_block: 97,
                ..WriteOptions::default()
            };
            let batch = all_types_batch(1_000, -5_000_000_000);
            let mut w = Writer::create_with(&file, options).unwrap();
            w.create_device(all_types_device("устройство")).unwrap();
            w.write("устройство", &batch).unwrap();
            w.close().unwrap();

            let r = Reader::open(&file).unwrap();
            assert!(r.is_finalized());
            assert_eq!(r.read("устройство").unwrap(), batch, "{codec} {policy:?}");
            assert_eq!(r.blocks("устройство").unwrap().len(), 11);
        }
    }
}

#[test]
fn projection_and_time_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "p.trosna");
    let batch = all_types_batch(500, 0);
    let mut w = Writer::create_with(
        &file,
        WriteOptions {
            rows_per_block: 64,
            ..Default::default()
        },
    )
    .unwrap();
    w.create_device(all_types_device("d")).unwrap();
    w.write("d", &batch).unwrap();
    w.close().unwrap();
    let r = Reader::open(&file).unwrap();
    let t = batch.time();

    let got = r
        .query("d")
        .unwrap()
        .columns(["s", "f64"])
        .time_range(t[10]..=t[300])
        .collect()
        .unwrap();
    assert_eq!(got.names(), ["s", "f64"]);
    assert_eq!(got.time(), &t[10..=300]);
    assert_eq!(
        got.column("s").unwrap(),
        &batch.column("s").unwrap().slice(10..301)
    );

    let half_open = r
        .query("d")
        .unwrap()
        .time_range(t[10]..t[20])
        .collect()
        .unwrap();
    assert_eq!(half_open.time(), &t[10..20]);
    let from = r.query("d").unwrap().time_range(t[490]..).count().unwrap();
    assert_eq!(from, 10);
    let extreme = r
        .query("d")
        .unwrap()
        .time_range(i64::MIN..=i64::MAX)
        .count()
        .unwrap();
    assert_eq!(extreme, 500);
    let empty = r
        .query("d")
        .unwrap()
        .time_range(t[5]..t[5])
        .collect()
        .unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.names().len(), 6);
    #[allow(clippy::reversed_empty_ranges)]
    let reversed = r
        .query("d")
        .unwrap()
        .time_range(t[20]..=t[10])
        .count()
        .unwrap();
    assert_eq!(reversed, 0);
    let between = r
        .query("d")
        .unwrap()
        .time_range(t[3] + 1..t[4])
        .count()
        .unwrap();
    assert_eq!(between, 0);
    let streamed: usize = r
        .query("d")
        .unwrap()
        .batches()
        .unwrap()
        .map(|b| b.unwrap().len())
        .sum();
    assert_eq!(streamed, 500);

    assert!(matches!(
        r.query("d").unwrap().columns(["nope"]).collect(),
        Err(Error::UnknownColumn { .. })
    ));
    assert!(matches!(r.query("nope"), Err(Error::UnknownDevice(_))));
}

#[test]
fn unsorted_input_and_duplicate_time_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "u.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(DeviceSchema::new("d", TimeUnit::Second).with_column("v", DataType::Int64))
        .unwrap();
    let batch = Batch::new(
        vec![5, 1, 3, 1, 5],
        vec![("v", Column::from(vec![50i64, 10, 30, 11, 51]))],
    )
    .unwrap();
    w.write("d", &batch).unwrap();
    w.close().unwrap();
    let got = Reader::open(&file).unwrap().read("d").unwrap();
    assert_eq!(got.time(), &[1, 3, 5]);
    assert_eq!(got.column("v").unwrap(), &Column::from(vec![11i64, 30, 51]));
}

#[test]
fn missing_columns_are_null_and_types_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "m.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(
        DeviceSchema::new("d", TimeUnit::Millisecond)
            .with_column("a", DataType::Float64)
            .with_column("b", DataType::Utf8),
    )
    .unwrap();
    let only_a = Batch::new(vec![1, 2], vec![("a", Column::from(vec![1.0, 2.0]))]).unwrap();
    w.write("d", &only_a).unwrap();
    let wrong = Batch::new(vec![3], vec![("a", Column::from(vec![1i64]))]).unwrap();
    assert!(matches!(
        w.write("d", &wrong),
        Err(Error::TypeMismatch { .. })
    ));
    let unknown = Batch::new(vec![3], vec![("zzz", Column::from(vec![1i64]))]).unwrap();
    assert!(matches!(
        w.write("d", &unknown),
        Err(Error::UnknownColumn { .. })
    ));
    w.write_row("d", 3, &[("a", Value::Int64(3))]).unwrap(); // lossless cast to float64
    assert!(w.write_row("d", 4, &[("b", Value::Float64(1.0))]).is_err());
    w.close().unwrap();

    let got = Reader::open(&file).unwrap().read("d").unwrap();
    assert_eq!(got.time(), &[1, 2, 3]);
    assert_eq!(got.column("a").unwrap(), &Column::from(vec![1.0, 2.0, 3.0]));
    assert_eq!(got.column("b").unwrap().null_count(), 3);
}

#[test]
fn several_devices_metadata_and_empty_devices() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "multi.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.set_metadata("source", "kvm-collector").unwrap();
    w.create_device(
        DeviceSchema::new("vm01", TimeUnit::Second)
            .with_timezone("UTC")
            .with_metadata("host", "node-1")
            .with_column_schema(
                trosna::ColumnSchema::new("ram", DataType::Int64).with_metadata("unit", "MB"),
            ),
    )
    .unwrap();
    w.create_device(DeviceSchema::new("empty", TimeUnit::Second).with_column("x", DataType::Bool))
        .unwrap();
    assert!(matches!(
        w.create_device(
            DeviceSchema::new("vm01", TimeUnit::Second).with_column("x", DataType::Bool)
        ),
        Err(Error::DeviceExists(_))
    ));
    w.write_row("vm01", 10, &[("ram", Value::Int64(2048))])
        .unwrap();
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    assert_eq!(
        r.metadata().get("source").map(String::as_str),
        Some("kvm-collector")
    );
    let names: Vec<&str> = r.devices().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["vm01", "empty"]);
    let vm = r.device("vm01").unwrap();
    assert_eq!(vm.timezone.as_deref(), Some("UTC"));
    assert_eq!(
        vm.columns[0].metadata.get("unit").map(String::as_str),
        Some("MB")
    );
    let empty = r.read("empty").unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.columns()[0].data_type(), DataType::Bool);
}

#[test]
fn appending_in_several_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "a.trosna");
    let schema = DeviceSchema::new("d", TimeUnit::Millisecond).with_column("v", DataType::Float64);
    for session in 0..5i64 {
        let mut w = if session == 0 {
            Writer::create(&file).unwrap()
        } else {
            Writer::open(&file).unwrap()
        };
        assert!(w.recovery().is_none());
        w.ensure_device(&schema).unwrap();
        let time: Vec<i64> = (0..100).map(|i| session * 100 + i).collect();
        let values: Vec<f64> = time.iter().map(|&t| t as f64 * 0.5).collect();
        w.write(
            "d",
            &Batch::new(time, vec![("v", Column::from(values))]).unwrap(),
        )
        .unwrap();
        w.close().unwrap();
    }
    let r = Reader::open(&file).unwrap();
    assert_eq!(r.commits().len(), 5);
    let all = r.read("d").unwrap();
    assert_eq!(all.len(), 500);
    assert!(all.time().windows(2).all(|w| w[0] < w[1]));
    let mismatch = DeviceSchema::new("d", TimeUnit::Second).with_column("v", DataType::Float64);
    let mut w = Writer::open(&file).unwrap();
    assert!(matches!(w.ensure_device(&mismatch), Err(Error::Schema(_))));
}

#[test]
fn strict_mode_and_non_trosna_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "x.csv");
    std::fs::write(&file, "time,value\n1,2\n").unwrap();
    assert!(matches!(Reader::open(&file), Err(Error::NotTrosna)));
    std::fs::write(&file, "TRO").unwrap();
    assert!(matches!(Reader::open(&file), Err(Error::NotTrosna)));

    let file = path(&dir, "unfinished.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(DeviceSchema::new("d", TimeUnit::Second).with_column("v", DataType::Int32))
        .unwrap();
    w.write_row("d", 1, &[("v", Value::Int32(1))]).unwrap();
    w.commit().unwrap();
    // Simulate a crash: copy the file before the writer finalizes it.
    let copy = path(&dir, "copy.trosna");
    std::fs::copy(&file, &copy).unwrap();
    w.close().unwrap();
    let strict = ReadOptions {
        strict: true,
        ..Default::default()
    };
    assert!(matches!(
        Reader::open_with(&copy, strict),
        Err(Error::NotFinalized)
    ));
    let lenient = Reader::open(&copy).unwrap();
    assert!(!lenient.is_finalized());
    assert_eq!(lenient.read("d").unwrap().len(), 1);
}

#[test]
fn writer_lock_prevents_a_second_writer() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "l.trosna");
    let w = Writer::create(&file).unwrap();
    assert!(matches!(Writer::open(&file), Err(Error::Locked)));
    w.close().unwrap();
    Writer::open(&file).unwrap().close().unwrap();
    assert!(
        Writer::create(&file).is_err(),
        "create must not overwrite by default"
    );
    let options = WriteOptions {
        overwrite: true,
        ..Default::default()
    };
    Writer::create_with(&file, options)
        .unwrap()
        .close()
        .unwrap();
}

#[test]
fn a_file_without_commits_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "e.trosna");
    Writer::create(&file).unwrap().close().unwrap();
    let r = Reader::open(&file).unwrap();
    assert!(r.is_finalized());
    assert_eq!(r.devices().len(), 0);
    assert!(r.head().is_none());
    assert!(trosna::verify(&file).unwrap().is_ok());
}

#[test]
fn compression_of_a_regular_series() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "c.trosna");
    let n = 100_000i64;
    let time: Vec<i64> = (0..n).map(|i| 1_700_000_000_000 + i * 1_000).collect();
    let cpu: Vec<f64> = (0..n).map(|i| ((i / 60) % 100) as f64 / 100.0).collect();
    let ram: Vec<i64> = (0..n).map(|i| 2048 + (i / 300) % 16).collect();
    let mut w = Writer::create(&file).unwrap();
    w.create_device(
        DeviceSchema::new("vm", TimeUnit::Millisecond)
            .with_column("cpu", DataType::Float64)
            .with_column("ram", DataType::Int64),
    )
    .unwrap();
    let batch = Batch::new(
        time,
        vec![("cpu", Column::from(cpu)), ("ram", Column::from(ram))],
    )
    .unwrap();
    w.write("vm", &batch).unwrap();
    w.close().unwrap();
    let size = std::fs::metadata(&file).unwrap().len();
    let raw = n as u64 * 24;
    assert!(size * 50 < raw, "{size} bytes for {raw} raw bytes");
    assert_eq!(Reader::open(&file).unwrap().read("vm").unwrap(), batch);
}

#[test]
fn shared_queries_outlive_the_reader_handle_and_cross_threads() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(&dir, "shared.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(all_types_device("d")).unwrap();
    w.write("d", &all_types_batch(50, 0)).unwrap();
    w.close().unwrap();

    let reader = std::sync::Arc::new(Reader::open(&file).unwrap());
    let expected = reader
        .query("d")
        .unwrap()
        .columns(["s"])
        .time_range(..=10_000_000_070)
        .collect()
        .unwrap();
    let batches = Reader::query_shared(&reader, "d", |q| {
        q.columns(["s"]).time_range(..=10_000_000_070)
    })
    .unwrap();
    drop(reader);
    let got = std::thread::spawn(move || batches.collect::<Result<Vec<_>, _>>().unwrap())
        .join()
        .unwrap();
    assert_eq!(Batch::concat(&got).unwrap(), expected);
    assert_eq!(expected.len(), 11); // i * 1_000_000_007 for i in 0..=10
    assert!(
        Reader::query_shared(
            &std::sync::Arc::new(Reader::open(&file).unwrap()),
            "x",
            |q| q
        )
        .is_err()
    );
}
