use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float64Type, Int32Type, TimestampMillisecondType, TimestampNanosecondType,
};
use arrow_array::{
    Array, ArrayRef, Float64Array, Int8Array, Int64Array, NullArray, RecordBatch,
    RecordBatchReader, StringArray, TimestampNanosecondArray, TimestampSecondArray, UInt16Array,
};
use arrow_schema::{DataType as ArrowType, Field, Schema, TimeUnit as ArrowUnit};
use trosna::{Batch, Column, DataType, DeviceSchema, Reader, TimeUnit, Writer};
use trosna_arrow::{ArrowReader, annotations_to_arrow, write_arrow};

#[test]
fn reading_produces_typed_record_batches() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(
        DeviceSchema::new("vm01", TimeUnit::Millisecond)
            .with_timezone("Europe/Moscow")
            .with_metadata("host", "kvm-1")
            .with_column("cpu", DataType::Float64)
            .with_column("state", DataType::Utf8),
    )
    .unwrap();
    let batch = Batch::new(
        vec![1_000, 2_000, 3_000],
        vec![
            ("cpu", Column::from(vec![Some(0.5), None, Some(0.7)])),
            ("state", Column::from(vec!["ok", "warn", "ok"])),
        ],
    )
    .unwrap();
    w.write("vm01", &batch).unwrap();
    w.annotate("vm01", 1_000, 2_000, "spike", None).unwrap();
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    let query = r.query("vm01").unwrap().columns(["cpu"]);
    let reader = ArrowReader::new(&query).unwrap();
    let schema = reader.schema();
    assert_eq!(
        schema.field(0).data_type(),
        &ArrowType::Timestamp(ArrowUnit::Millisecond, Some("Europe/Moscow".into()))
    );
    assert_eq!(schema.field(1).name(), "cpu");
    assert_eq!(
        schema.metadata().get("trosna.device").map(String::as_str),
        Some("vm01")
    );
    assert_eq!(
        schema.metadata().get("host").map(String::as_str),
        Some("kvm-1")
    );
    let (_, batches) = reader.read_all().unwrap();
    assert_eq!(batches.len(), 1);
    let rb = &batches[0];
    assert_eq!(rb.num_columns(), 2);
    let time = rb.column(0).as_primitive::<TimestampMillisecondType>();
    assert_eq!(time.values().to_vec(), vec![1_000, 2_000, 3_000]);
    let cpu = rb.column(1).as_primitive::<Float64Type>();
    assert!(cpu.is_null(1));
    assert_eq!(cpu.value(2), 0.7);

    let annotations = annotations_to_arrow(&r.annotations(None, None).unwrap()).unwrap();
    assert_eq!(annotations.num_rows(), 1);
    assert_eq!(annotations.column(4).as_string::<i32>().value(0), "spike");

    // an empty result still has a schema
    let empty_query = r.query("vm01").unwrap().time_range(5_000..);
    let empty = ArrowReader::new(&empty_query).unwrap();
    assert_eq!(empty.schema().fields().len(), 3);
    assert_eq!(empty.count(), 0);
}

#[test]
fn writing_record_batches_creates_devices() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("w.trosna");
    let schema = Arc::new(Schema::new(vec![
        Field::new("ts", ArrowType::Int64, false),
        Field::new("port", ArrowType::UInt16, true),
        Field::new("name", ArrowType::Utf8, true),
    ]));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(vec![30, 10, 20])),
        Arc::new(UInt16Array::from(vec![Some(80), None, Some(65_535)])),
        Arc::new(StringArray::from(vec![Some("http"), Some("ssh"), None])),
    ];
    let rb = RecordBatch::try_new(schema, columns).unwrap();
    let mut w = Writer::create(&file).unwrap();
    assert!(
        write_arrow(&mut w, "router", "ts", &rb, None).is_err(),
        "integer time needs a unit"
    );
    write_arrow(&mut w, "router", "ts", &rb, Some(TimeUnit::Second)).unwrap();
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    let device = r.device("router").unwrap();
    assert_eq!(device.time_name, "ts");
    assert_eq!(device.columns[0].data_type, DataType::Int32);
    let batch = r.read("router").unwrap();
    assert_eq!(batch.time(), &[10, 20, 30]);
    assert_eq!(
        batch.column("port").unwrap(),
        &Column::from(vec![None, Some(65_535), Some(80)])
    );
}

#[test]
fn appending_converts_values_to_the_types_of_an_existing_device() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("c.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(
        DeviceSchema::new("датчик", TimeUnit::Millisecond)
            .with_column("счётчик", DataType::Int32)
            .with_column("температура", DataType::Float32)
            .with_column("флаг", DataType::Bool),
    )
    .unwrap();
    let schema = Arc::new(Schema::new(vec![
        Field::new(
            "время",
            ArrowType::Timestamp(ArrowUnit::Second, None),
            false,
        ),
        // pandas stores an integer column with missing values as float64
        Field::new("счётчик", ArrowType::Float64, true),
        Field::new("температура", ArrowType::Float64, true),
        Field::new("флаг", ArrowType::Null, true),
    ]));
    let rows = |time: Vec<i64>, counter: Vec<Option<f64>>, temperature: Vec<Option<f64>>| {
        let n = time.len();
        let columns: Vec<ArrayRef> = vec![
            Arc::new(TimestampSecondArray::from(time)),
            Arc::new(Float64Array::from(counter)),
            Arc::new(Float64Array::from(temperature)),
            Arc::new(NullArray::new(n)),
        ];
        RecordBatch::try_new(schema.clone(), columns).unwrap()
    };
    let ok = rows(
        vec![2, 1],
        vec![Some(7.0), None],
        vec![Some(0.1), Some(-2.5)],
    );
    write_arrow(&mut w, "датчик", "время", &ok, None).unwrap();

    let fractional = rows(vec![3], vec![Some(7.5)], vec![Some(1.0)]);
    assert!(
        write_arrow(&mut w, "датчик", "время", &fractional, None).is_err(),
        "7.5 is not an integer"
    );
    let overflow = rows(vec![4], vec![Some(3e9)], vec![Some(1.0)]);
    assert!(
        write_arrow(&mut w, "датчик", "время", &overflow, None).is_err(),
        "3e9 overflows int32"
    );
    let text_schema = Arc::new(Schema::new(vec![
        Field::new(
            "время",
            ArrowType::Timestamp(ArrowUnit::Second, None),
            false,
        ),
        Field::new("счётчик", ArrowType::Utf8, true),
    ]));
    let text_columns: Vec<ArrayRef> = vec![
        Arc::new(TimestampSecondArray::from(vec![5])),
        Arc::new(StringArray::from(vec!["12"])),
    ];
    let text = RecordBatch::try_new(text_schema, text_columns).unwrap();
    assert!(
        write_arrow(&mut w, "датчик", "время", &text, None).is_err(),
        "strings are not numbers"
    );
    w.close().unwrap();

    let batch = Reader::open(&file).unwrap().read("датчик").unwrap();
    assert_eq!(batch.time(), &[1_000, 2_000]);
    assert_eq!(
        batch.column("счётчик").unwrap(),
        &Column::from(vec![None, Some(7i32)])
    );
    assert_eq!(
        batch.column("температура").unwrap(),
        &Column::from(vec![-2.5f32, 0.1f32])
    );
    assert_eq!(
        batch.column("флаг").unwrap(),
        &Column::nulls(DataType::Bool, 2)
    );
}

#[test]
fn empty_devices_and_all_null_columns_keep_their_schema() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("e.trosna");
    let mut w = Writer::create(&file).unwrap();
    w.create_device(DeviceSchema::new("empty", TimeUnit::Second).with_column("v", DataType::Int64))
        .unwrap();
    w.create_device(
        DeviceSchema::new("sparse", TimeUnit::Second)
            .with_column("a", DataType::Float64)
            .with_column("s", DataType::Utf8),
    )
    .unwrap();
    let batch = Batch::new(vec![1, 2], vec![("a", Column::from(vec![1.0, 2.0]))]).unwrap();
    w.write("sparse", &batch).unwrap();
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    let empty_query = r.query("empty").unwrap();
    let empty = ArrowReader::new(&empty_query).unwrap();
    let schema = empty.schema();
    assert_eq!(schema.fields().len(), 2);
    assert_eq!(schema.field(1).data_type(), &ArrowType::Int64);
    assert_eq!(empty.count(), 0);

    let sparse_query = r.query("sparse").unwrap();
    let (_, batches) = ArrowReader::new(&sparse_query).unwrap().read_all().unwrap();
    assert_eq!(batches.len(), 1);
    let strings = batches[0].column(2);
    assert_eq!(strings.data_type(), &ArrowType::Utf8);
    assert_eq!(strings.null_count(), 2);
}

#[test]
fn nanosecond_time_stamps_and_time_zones_survive_the_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ns.trosna");
    let times = vec![i64::MIN, -1, 0, i64::MAX];
    let zone = ArrowType::Timestamp(ArrowUnit::Nanosecond, Some("+03:00".into()));
    let schema = Arc::new(Schema::new(vec![
        Field::new("t", zone.clone(), false),
        Field::new("v", ArrowType::Int8, true),
    ]));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(TimestampNanosecondArray::from(times.clone()).with_timezone("+03:00")),
        Arc::new(Int8Array::from(vec![Some(-128), None, Some(0), Some(127)])),
    ];
    let rb = RecordBatch::try_new(schema, columns).unwrap();
    let mut w = Writer::create(&file).unwrap();
    write_arrow(&mut w, "d", "t", &rb, None).unwrap();
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    assert_eq!(r.device("d").unwrap().timezone.as_deref(), Some("+03:00"));
    let query = r.query("d").unwrap();
    let (schema, batches) = ArrowReader::new(&query).unwrap().read_all().unwrap();
    assert_eq!(schema.field(0).data_type(), &zone);
    assert_eq!(batches.len(), 1);
    let time = batches[0]
        .column(0)
        .as_primitive::<TimestampNanosecondType>();
    assert_eq!(time.values().to_vec(), times);
    let values = batches[0].column(1).as_primitive::<Int32Type>();
    assert_eq!(
        values.iter().collect::<Vec<_>>(),
        vec![Some(-128), None, Some(0), Some(127)]
    );
}

#[test]
fn shared_readers_are_static_and_send() {
    fn assert_send_static<T: Send + 'static>(_: &T) {}
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.trosna");
    let mut w = Writer::create(&path).unwrap();
    w.create_device(DeviceSchema::new("d", TimeUnit::Second).with_column("v", DataType::Int64))
        .unwrap();
    w.write(
        "d",
        &Batch::new(vec![1, 2], vec![("v", Column::from(vec![10i64, 20]))]).unwrap(),
    )
    .unwrap();
    w.close().unwrap();
    let reader = Arc::new(Reader::open(&path).unwrap());
    let arrow =
        ArrowReader::from_batches(Reader::query_shared(&reader, "d", |q| q).unwrap()).unwrap();
    assert_send_static(&arrow);
    let rows: usize = std::thread::spawn(move || arrow.map(|b| b.unwrap().num_rows()).sum())
        .join()
        .unwrap();
    assert_eq!(rows, 2);
}

#[test]
fn naive_time_stamps_are_refused_for_devices_with_a_time_zone() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::create(dir.path().join("z.trosna")).unwrap();
    w.create_device(
        DeviceSchema::new("d", TimeUnit::Second)
            .with_timezone("+03:00")
            .with_column("v", DataType::Int64),
    )
    .unwrap();
    let naive = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("t", ArrowType::Timestamp(ArrowUnit::Second, None), false),
            Field::new("v", ArrowType::Int64, true),
        ])),
        vec![
            Arc::new(TimestampSecondArray::from(vec![0])),
            Arc::new(Int64Array::from(vec![1])),
        ],
    )
    .unwrap();
    let err = write_arrow(&mut w, "d", "t", &naive, None).unwrap_err();
    assert!(err.to_string().contains("no time zone"), "{err}");
    let raw = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("t", ArrowType::Int64, false),
            Field::new("v", ArrowType::Int64, true),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![0])),
            Arc::new(Int64Array::from(vec![1])),
        ],
    )
    .unwrap();
    write_arrow(&mut w, "d", "t", &raw, None).unwrap();
}
