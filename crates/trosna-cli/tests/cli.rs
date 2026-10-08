//! End-to-end tests of the `trosna` command-line tool.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_trosna"))
        .args(args)
        .output()
        .expect("the trosna binary runs")
}

/// Runs a command that must succeed and returns its standard output.
fn ok(args: &[&str]) -> String {
    let out = run(args);
    assert!(
        out.status.success(),
        "trosna {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("output is UTF-8")
}

/// Runs a command that must fail and returns its standard error.
fn fails(args: &[&str]) -> String {
    let out = run(args);
    assert!(
        !out.status.success(),
        "trosna {args:?} unexpectedly succeeded:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stderr).expect("output is UTF-8")
}

fn path(p: &Path) -> &str {
    p.to_str().expect("temporary paths are UTF-8")
}

const SENSOR_CSV: &str = "\
время,cpu,ram_mb,состояние,ok
2026-10-08T10:00:00+03:00,0.5,2048,норма,true
2026-10-08T10:00:01+03:00,,2049,\"с запятой, да\",false
2026-10-08T10:00:02.5+03:00,0.75,,,true
";

const SENSOR_EXPORT: &str = "\
время,cpu,ram_mb,состояние,ok
2026-10-08T10:00:00+03:00,0.5,2048,норма,true
2026-10-08T10:00:01+03:00,,2049,\"с запятой, да\",false
2026-10-08T10:00:02.500+03:00,0.75,,,true
";

/// Converts `SENSOR_CSV` into `<dir>/data.trosna` with the device "датчик".
fn sensor_file(dir: &Path) -> PathBuf {
    let csv = dir.join("data.csv");
    fs::write(&csv, SENSOR_CSV).unwrap();
    let file = dir.join("data.trosna");
    ok(&["convert", path(&csv), path(&file), "--device", "датчик"]);
    file
}

#[test]
fn csv_round_trip_keeps_times_types_nulls_and_cyrillic() {
    let dir = tempfile::tempdir().unwrap();
    let file = sensor_file(dir.path());

    let info = ok(&["info", path(&file)]);
    for expected in [
        "датчик",
        "3 points",
        "bit/point",
        "ms",
        "+03:00",
        "float64",
        "int64",
        "string",
        "bool",
    ] {
        assert!(info.contains(expected), "info lacks {expected:?}:\n{info}");
    }

    let back = dir.path().join("back.csv");
    ok(&["convert", path(&file), path(&back)]);
    assert_eq!(fs::read_to_string(&back).unwrap(), SENSOR_EXPORT);
    assert_eq!(ok(&["cat", path(&file)]), SENSOR_EXPORT);

    let table = ok(&["cat", path(&file), "--format", "table"]);
    assert!(
        table.contains("норма") && table.contains("с запятой, да"),
        "{table}"
    );
}

#[test]
fn integer_times_get_a_guessed_or_explicit_unit() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("epoch.csv");
    fs::write(&csv, "ts,v\n1696760000000,1\n1696760001000,2\n").unwrap();
    let file = dir.path().join("epoch.trosna");
    ok(&["convert", path(&csv), path(&file)]);
    assert!(ok(&["info", path(&file)]).contains("ms"));
    let raw = dir.path().join("raw.csv");
    ok(&["convert", path(&file), path(&raw), "--raw-time"]);
    assert_eq!(
        fs::read_to_string(&raw).unwrap(),
        "ts,v\n1696760000000,1\n1696760001000,2\n"
    );

    let csv = dir.path().join("counter.csv");
    fs::write(&csv, "t,v\n0,1\n1,2\n").unwrap();
    let file = dir.path().join("counter.trosna");
    ok(&["convert", path(&csv), path(&file), "--unit", "s"]);
    assert_eq!(
        ok(&["cat", path(&file)]),
        "t,v\n1970-01-01T00:00:00,1\n1970-01-01T00:00:01,2\n"
    );
}

#[test]
fn selecting_columns_and_time_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let file = sensor_file(dir.path());
    let selected = ok(&[
        "cat",
        path(&file),
        "--columns",
        "cpu,ok",
        "--from",
        "2026-10-08T10:00:01+03:00",
        "--to",
        "2026-10-08 10:00:02.5",
    ]);
    assert_eq!(
        selected,
        "время,cpu,ok\n2026-10-08T10:00:01+03:00,,false\n2026-10-08T10:00:02.500+03:00,0.75,true\n"
    );
}

#[test]
fn editing_commands_change_points_and_keep_history() {
    let dir = tempfile::tempdir().unwrap();
    let file = sensor_file(dir.path());
    let f = path(&file);
    ok(&[
        "insert",
        f,
        "--time",
        "2026-10-08T10:00:03+03:00",
        "cpu=0.9",
        "ram_mb=4096",
        "состояние=пик",
        "ok=true",
        "-m",
        "добавлена точка",
    ]);
    ok(&[
        "update",
        f,
        "--time",
        "2026-10-08T10:00:03+03:00",
        "cpu=0.95",
        "-m",
        "исправлено значение",
        "--author",
        "Ферубко",
    ]);
    // a time without an offset is read in the device's time zone
    ok(&["delete", f, "--time", "2026-10-08 10:00:00"]);

    assert_eq!(
        ok(&["cat", f]),
        "время,cpu,ram_mb,состояние,ok\n\
         2026-10-08T10:00:01+03:00,,2049,\"с запятой, да\",false\n\
         2026-10-08T10:00:02.500+03:00,0.75,,,true\n\
         2026-10-08T10:00:03+03:00,0.95,4096,пик,true\n"
    );
    assert_eq!(ok(&["cat", f, "--as-of", "1"]), SENSOR_EXPORT);

    let log = ok(&["log", f]);
    assert_eq!(log.matches("commit ").count(), 4, "{log}");
    for expected in ["добавлена точка", "исправлено значение", "Ферубко"]
    {
        assert!(log.contains(expected), "log lacks {expected:?}:\n{log}");
    }

    let diff = ok(&["diff", f, "--from", "1"]);
    assert!(diff.contains("+ 2026-10-08T10:00:03+03:00"), "{diff}");
    assert!(diff.contains("- 2026-10-08T10:00:00+03:00"), "{diff}");
    let diff = ok(&["diff", f, "--from", "2", "--to", "3"]);
    assert!(diff.contains("~ 2026-10-08T10:00:03+03:00"), "{diff}");
    assert!(diff.contains("cpu: 0.9 → 0.95"), "{diff}");

    let missing = fails(&["update", f, "--time", "2026-10-08T11:00:00+03:00", "cpu=1"]);
    assert!(missing.contains("no point"), "{missing}");
    let bad = fails(&["insert", f, "--time", "5", "cpu=abc"]);
    assert!(bad.contains("cpu"), "{bad}");
}

#[test]
fn annotations_can_be_added_changed_listed_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let file = sensor_file(dir.path());
    let f = path(&file);
    let id = ok(&[
        "annotate",
        f,
        "--start",
        "2026-10-08T10:00:01+03:00",
        "--end",
        "2026-10-08T10:00:02+03:00",
        "--label",
        "spike",
        "--note",
        "резервное копирование",
    ]);
    assert_eq!(id.trim(), "1");
    let listed = ok(&["annotations", f]);
    for expected in [
        "spike",
        "резервное копирование",
        "2026-10-08T10:00:01+03:00",
    ] {
        assert!(
            listed.contains(expected),
            "annotations lack {expected:?}:\n{listed}"
        );
    }

    ok(&[
        "annotate",
        f,
        "--id",
        "1",
        "--start",
        "2026-10-08T10:00:01+03:00",
        "--label",
        "anomaly",
    ]);
    let listed = ok(&["annotations", f]);
    assert!(
        listed.contains("anomaly") && !listed.contains("spike"),
        "{listed}"
    );

    ok(&["unannotate", f, "1"]);
    assert!(!ok(&["annotations", f]).contains("anomaly"));
    assert!(ok(&["annotations", f, "--as-of", "2"]).contains("spike"));
    assert!(fails(&["unannotate", f, "1"]).contains("unknown annotation"));
}

#[test]
fn verify_detects_damage_and_recover_finalizes_interrupted_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = sensor_file(dir.path());
    let f = path(&file);

    let report = ok(&["verify", f]);
    assert!(report.contains("OK"), "{report}");
    let hash_line = report
        .lines()
        .find(|l| l.contains("head"))
        .expect("head hash is shown");
    assert!(
        hash_line
            .split_whitespace()
            .any(|w| w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit())),
        "{report}"
    );

    // a crash before the index was written: the end of the file is missing
    let bytes = fs::read(&file).unwrap();
    let interrupted = dir.path().join("interrupted.trosna");
    fs::write(&interrupted, &bytes[..bytes.len() - 10]).unwrap();
    let i = path(&interrupted);
    assert_eq!(ok(&["cat", i]), SENSOR_EXPORT);
    assert!(ok(&["recover", i]).contains("removed"));
    assert!(ok(&["verify", i]).contains("finalized"));

    // damage in the middle of the file
    let mut damaged = bytes.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 0xff;
    let broken = dir.path().join("broken.trosna");
    fs::write(&broken, &damaged).unwrap();
    let out = run(&["verify", path(&broken)]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("FAILED"));
}

#[test]
fn compact_removes_history_but_links_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = sensor_file(dir.path());
    let f = path(&file);
    ok(&["delete", f, "--from", "2026-10-08T10:00:01+03:00"]);
    let compacted = dir.path().join("compacted.trosna");
    let c = path(&compacted);
    assert!(ok(&["compact", f, c]).contains("commit 2"));
    assert_eq!(ok(&["cat", c]), ok(&["cat", f]));
    let log = ok(&["log", c]);
    assert_eq!(log.matches("commit ").count(), 1, "{log}");
    assert!(log.contains("compacted from"), "{log}");
    assert!(ok(&["verify", c]).contains("compacted from"));
}

#[test]
fn several_devices_and_error_messages() {
    let dir = tempfile::tempdir().unwrap();
    let file = sensor_file(dir.path());
    let f = path(&file);
    let second = dir.path().join("second.csv");
    fs::write(&second, "time,level\n2026-10-08 10:00:00,1\n").unwrap();
    ok(&[
        "convert",
        path(&second),
        f,
        "--append",
        "--device",
        "второй",
    ]);

    assert!(fails(&["cat", f]).contains("--device"));
    assert_eq!(
        ok(&["cat", f, "--device", "второй"]),
        "time,level\n2026-10-08T10:00:00,1\n"
    );
    assert!(fails(&["cat", f, "--device", "нет"]).contains("unknown device"));
    assert!(fails(&["convert", path(&second), f]).contains("exists"));
    assert!(fails(&["info", path(&second)]).contains("not a Trosna file"));
    assert!(fails(&["info", path(&dir.path().join("missing.trosna"))]).contains("error"));
}

#[test]
fn large_csv_is_imported_in_batches_and_last_duplicate_wins() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("big.csv");
    // more rows than one import batch; the first time stamp is repeated at the end
    let rows = 70_000;
    let mut text = String::from("t,v\n");
    for i in 0..rows {
        text.push_str(&format!("{i},{i}\n"));
    }
    text.push_str("0,-1\n");
    fs::write(&csv, text).unwrap();
    let file = dir.path().join("big.trosna");
    ok(&["convert", path(&csv), path(&file), "--unit", "s"]);
    assert!(ok(&["info", path(&file)]).contains(&format!("{rows} points")));
    let out = ok(&["cat", path(&file), "--raw-time", "--to", "1"]);
    assert_eq!(out, "t,v\n0,-1\n1,1\n");
}

#[test]
fn a_failed_import_leaves_no_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("bad.csv");
    fs::write(&csv, "t,v\n1,1\nnot a time,2\n").unwrap();
    let file = dir.path().join("bad.trosna");
    let err = fails(&["convert", path(&csv), path(&file)]);
    assert!(err.contains("row 3"), "{err}");
    assert!(!file.exists());
}

#[test]
fn failed_conversions_never_damage_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = sensor_file(dir.path());
    let f = path(&file);
    let bad = dir.path().join("bad.csv");
    // fails in the second pass, after the new file has been started
    fs::write(
        &bad,
        "время,cpu\n2026-10-08T10:00:00+03:00,1\n2026-10-08T10:00:00.5+03:00,2\n",
    )
    .unwrap();
    let err = fails(&["convert", path(&bad), f, "--force", "--unit", "s"]);
    assert!(
        err.contains("row 3") && err.contains("more precise"),
        "{err}"
    );
    assert_eq!(ok(&["cat", f]), SENSOR_EXPORT);

    let out = dir.path().join("out.csv");
    fs::write(&out, "keep me").unwrap();
    assert!(fails(&["convert", f, path(&out), "--force", "--as-of", "99"]).contains("99"));
    assert_eq!(fs::read_to_string(&out).unwrap(), "keep me");
    let fresh = dir.path().join("fresh.csv");
    assert!(fails(&["convert", f, path(&fresh), "--as-of", "99"]).contains("99"));
    assert!(!fresh.exists());

    assert!(fails(&["convert", f, f, "--force"]).contains("same file"));
    assert_eq!(ok(&["cat", f]), SENSOR_EXPORT);
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn times_finer_than_the_unit_are_refused_for_points_and_rounded_inward_for_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("fine.csv");
    fs::write(
        &csv,
        "t,v\n2026-10-08T10:00:00.2Z,1\n2026-10-08T10:00:00.7Z,2\n",
    )
    .unwrap();
    let file = dir.path().join("fine.trosna");
    let err = fails(&["convert", path(&csv), path(&file), "--unit", "s"]);
    assert!(err.contains("more precise"), "{err}");
    assert!(!file.exists());

    let csv = dir.path().join("secs.csv");
    fs::write(&csv, "t,v\n0,1\n1,2\n").unwrap();
    let f = path(&file);
    ok(&["convert", path(&csv), f, "--unit", "s"]);
    let from = ok(&["cat", f, "--raw-time", "--from", "1970-01-01 00:00:00.5"]);
    assert_eq!(from, "t,v\n1,2\n");
    let to = ok(&["cat", f, "--raw-time", "--to", "1970-01-01 00:00:00.5"]);
    assert_eq!(to, "t,v\n0,1\n");
    assert!(
        fails(&["insert", f, "--time", "1970-01-01 00:00:02.5", "v=3"]).contains("more precise")
    );
    ok(&["delete", f, "--from", "1970-01-01 00:00:00.5"]);
    assert_eq!(ok(&["cat", f, "--raw-time"]), "t,v\n0,1\n");
}

#[test]
fn dates_outside_the_nanosecond_range_work_for_coarser_units() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("old.csv");
    fs::write(
        &csv,
        "t,v\n1600-01-01T00:00:00Z,1\n2300-06-01T12:00:00.5Z,2\n",
    )
    .unwrap();
    let file = dir.path().join("old.trosna");
    let f = path(&file);
    ok(&["convert", path(&csv), f]);
    assert_eq!(
        ok(&["cat", f]),
        "t,v\n1600-01-01T00:00:00Z,1\n2300-06-01T12:00:00.500Z,2\n"
    );
    assert_eq!(ok(&["cat", f, "--from", "2000-01-01"]).lines().count(), 2);
    assert!(ok(&["info", f]).contains("1600-01-01T00:00:00Z"));
}

#[test]
fn closing_the_output_pipe_early_is_not_an_error() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("big.csv");
    let mut text = String::from("t,v\n");
    for i in 0..200_000 {
        text.push_str(&format!("{i},{i}\n"));
    }
    fs::write(&csv, text).unwrap();
    let file = dir.path().join("big.trosna");
    ok(&["convert", path(&csv), path(&file), "--unit", "s"]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_trosna"))
        .args(["cat", path(&file)])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert_eq!(first, "t,v\n");
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
