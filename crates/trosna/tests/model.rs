//! Model-based testing of the semantics of DESIGN §4.1: random sequences of
//! operations are applied to a file and to a reference model built from
//! ordered maps, and every version of the file must match the model.

use std::collections::BTreeMap;

use proptest::prelude::*;
use trosna::{
    CommitOptions, DataType, DeviceSchema, Reader, TimeUnit, Value, WriteOptions, Writer,
};

#[derive(Debug, Clone)]
enum Op {
    Write(Vec<(i64, Option<i64>, Option<String>)>),
    Update(i64, i64),
    Delete(i64, i64),
    Annotate(i64, i64, String),
    RemoveAnnotation(usize),
    Commit,
    Rollback,
}

fn op() -> impl Strategy<Value = Op> {
    let t = -20i64..60;
    prop_oneof![
        4 => prop::collection::vec((t.clone(), prop::option::of(any::<i64>()), prop::option::of("[a-dё]{0,3}")), 1..12)
            .prop_map(Op::Write),
        2 => (t.clone(), any::<i64>()).prop_map(|(t, v)| Op::Update(t, v)),
        2 => (t.clone(), 0i64..15).prop_map(|(t, len)| Op::Delete(t, t + len)),
        1 => (t.clone(), 0i64..10, "[a-z]{1,4}").prop_map(|(t, len, l)| Op::Annotate(t, t + len, l)),
        1 => (0usize..8).prop_map(Op::RemoveAnnotation),
        2 => Just(Op::Commit),
        1 => Just(Op::Rollback),
    ]
}

type Row = (Option<i64>, Option<String>);
type State = (BTreeMap<i64, Row>, BTreeMap<u64, (i64, i64, String)>);

fn run(ops: &[Op], rows_per_block: usize) -> Result<(), TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("model.trosna");
    let options = WriteOptions {
        rows_per_block,
        sync: false,
        ..WriteOptions::default()
    };
    let mut w = Writer::create_with(&file, options).unwrap();
    w.create_device(
        DeviceSchema::new("d", TimeUnit::Second)
            .with_column("v", DataType::Int64)
            .with_column("s", DataType::Utf8),
    )
    .unwrap();
    // The device is part of the first commit, so a rollback cannot remove it.
    w.commit().unwrap().unwrap();

    // committed[v] is the expected state of version v (0 = the empty file).
    let mut committed: Vec<State> = vec![(BTreeMap::new(), BTreeMap::new()); 2];
    let mut current: State = committed[0].clone();

    for op in ops {
        match op {
            Op::Write(rows) => {
                for (t, v, s) in rows {
                    w.write_row(
                        "d",
                        *t,
                        &[("v", Value::from(*v)), ("s", Value::from(s.clone()))],
                    )
                    .unwrap();
                    current.0.insert(*t, (*v, s.clone()));
                }
            }
            Op::Update(t, v) => {
                let result = w.update("d", *t, &[("v", Value::Int64(*v))]);
                match current.0.get_mut(t) {
                    Some(row) => {
                        result.unwrap();
                        row.0 = Some(*v);
                    }
                    None => prop_assert!(result.is_err()),
                }
            }
            Op::Delete(lo, hi) => {
                w.delete_range("d", *lo..=*hi).unwrap();
                let doomed: Vec<i64> = current.0.range(*lo..=*hi).map(|(t, _)| *t).collect();
                for t in doomed {
                    current.0.remove(&t);
                }
            }
            Op::Annotate(lo, hi, label) => {
                let id = w.annotate("d", *lo, *hi, label, None).unwrap();
                current.1.insert(id, (*lo, *hi, label.clone()));
            }
            Op::RemoveAnnotation(k) => {
                if let Some(&id) = current.1.keys().nth(*k) {
                    w.remove_annotation(id).unwrap();
                    current.1.remove(&id);
                }
            }
            Op::Commit => {
                if w.commit_with(CommitOptions::default()).unwrap().is_some() {
                    committed.push(current.clone());
                }
            }
            Op::Rollback => {
                w.rollback().unwrap();
                current = committed.last().unwrap().clone();
            }
        }
    }
    if w.commit().unwrap().is_some() {
        committed.push(current.clone());
    }
    w.close().unwrap();

    let r = Reader::open(&file).unwrap();
    prop_assert_eq!(r.commits().len(), committed.len() - 1);
    for (version, (rows, annotations)) in committed.iter().enumerate().skip(1) {
        let got = r
            .query("d")
            .unwrap()
            .as_of(version as u64)
            .collect()
            .unwrap();
        let expected_time: Vec<i64> = rows.keys().copied().collect();
        prop_assert_eq!(got.time(), &expected_time[..], "version {}", version);
        for (i, (_, (v, s))) in rows.iter().enumerate() {
            prop_assert_eq!(got.column("v").unwrap().value(i), Value::from(*v));
            prop_assert_eq!(got.column("s").unwrap().value(i), Value::from(s.clone()));
        }
        // a time window and a projection
        let window = r
            .query("d")
            .unwrap()
            .columns(["s"])
            .time_range(0..=30)
            .as_of(version as u64)
            .collect()
            .unwrap();
        let expected_window: Vec<i64> = rows.range(0..=30).map(|(t, _)| *t).collect();
        prop_assert_eq!(window.time(), &expected_window[..]);

        let got_annotations: Vec<(u64, i64, i64, String)> = r
            .annotations(None, Some(version as u64))
            .unwrap()
            .into_iter()
            .map(|a| (a.id, a.start, a.end, a.label))
            .collect();
        let expected_annotations: Vec<(u64, i64, i64, String)> = annotations
            .iter()
            .map(|(id, (s, e, l))| (*id, *s, *e, l.clone()))
            .collect();
        prop_assert_eq!(got_annotations, expected_annotations);
    }
    let report = trosna::verify(&file).unwrap();
    prop_assert!(report.is_ok(), "{:?}", report);
    Ok(())
}

proptest! {
    // 48 cases by default; set PROPTEST_CASES for a longer run.
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(48),
        ..ProptestConfig::default()
    })]

    #[test]
    fn file_matches_reference_model(ops in prop::collection::vec(op(), 1..40)) {
        run(&ops, 7)?;
    }

    #[test]
    fn file_matches_reference_model_with_large_blocks(ops in prop::collection::vec(op(), 1..40)) {
        run(&ops, 65_536)?;
    }
}
