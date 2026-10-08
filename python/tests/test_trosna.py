"""Tests of the trosna Python package."""

import datetime as dt
import pathlib
import shutil

import pandas as pd
import polars as pl
import pyarrow as pa
import pytest

import trosna

GOLDEN = pathlib.Path(__file__).resolve().parents[2] / "testdata" / "v1" / "basic.trosna"
MSK = "+03:00"


def sensor_table():
    times = pd.to_datetime(
        ["2026-10-08T10:00:00+03:00", "2026-10-08T10:00:01+03:00", "2026-10-08T10:00:02.5+03:00"],
        format="ISO8601",
    ).as_unit("ms")
    return pa.table(
        {
            "время": pa.array(times, pa.timestamp("ms", MSK)),
            "cpu": pa.array([0.5, None, 0.75], pa.float64()),
            "ram_mb": pa.array([2048, 2049, None], pa.int64()),
            "состояние": pa.array(["норма", "с запятой, да", None], pa.string()),
            "ok": pa.array([True, False, True]),
        }
    )


@pytest.fixture
def sensor(tmp_path):
    path = tmp_path / "data.trosna"
    trosna.write(path, sensor_table(), "датчик")
    return path


def test_arrow_round_trip_keeps_types_nulls_time_zone_and_cyrillic(sensor):
    table = trosna.read_arrow(sensor)
    assert table.schema.field("время").type == pa.timestamp("ms", MSK)
    assert table.schema.metadata[b"trosna.device"] == "датчик".encode()
    source = sensor_table()
    assert table.column_names == source.column_names
    for name in source.column_names:
        assert table.schema.field(name).type == source.schema.field(name).type
        assert table[name].to_pylist() == source[name].to_pylist()
    f = trosna.open(sensor)
    d = f.device()
    assert (d.name, d.time_unit, d.timezone, d.time_name) == ("датчик", "ms", MSK, "время")
    assert d.columns == {"cpu": "float64", "ram_mb": "int64", "состояние": "string", "ok": "bool"}
    assert f.count() == 3 and f.finalized


def test_pandas_round_trip_and_datetime_index(tmp_path):
    index = pd.date_range("2026-10-08 10:00", periods=4, freq="250ms", tz="Europe/Moscow", name="t")
    df = pd.DataFrame({"v": [1.0, 2.0, None, 4.0], "s": ["a", "б", None, "d"]}, index=index)
    path = tmp_path / "p.trosna"
    trosna.write(path, df, "dev")
    back = trosna.read_pandas(path)
    assert list(back.columns) == ["t", "v", "s"]
    assert (back["t"] == index.to_series().reset_index(drop=True)).all()
    assert back["v"].isna().tolist() == [False, False, True, False]
    assert back["s"].tolist()[:2] == ["a", "б"]
    assert trosna.open(path).device().timezone == "Europe/Moscow"


def test_polars_round_trip_and_fixed_offsets(tmp_path, sensor):
    frame = trosna.read_polars(sensor)
    assert frame.columns == ["время", "cpu", "ram_mb", "состояние", "ok"]
    assert frame["cpu"].to_list() == [0.5, None, 0.75]
    # the instants survive the change to an IANA zone name
    assert frame["время"].dt.timestamp("ms").to_list() == trosna.read_arrow(sensor)["время"].cast(
        pa.int64()
    ).to_pylist()

    df = pl.DataFrame(
        {
            "ts": pl.datetime_range(
                dt.datetime(2026, 1, 1), dt.datetime(2026, 1, 1, 0, 0, 2), "1s", eager=True, time_unit="ns"
            ),
            "x": [1, 2, 3],
        }
    )
    path = tmp_path / "pl.trosna"
    trosna.write(path, df, "p")
    assert trosna.read_polars(path).equals(df)
    assert trosna.open(path).device().time_unit == "ns"


def test_duplicates_and_disorder_in_one_write_keep_the_last_value_sorted(tmp_path):
    table = pa.table({"t": pa.array([3, 1, 2, 1], pa.int64()), "v": pa.array([30, 10, 20, 11])})
    path = tmp_path / "d.trosna"
    trosna.write(path, table, "d", unit="s")
    back = trosna.read_arrow(path)
    assert back["t"].cast(pa.int64()).to_pylist() == [1, 2, 3]
    assert back["v"].to_pylist() == [11, 20, 30]
    with pytest.raises(ValueError, match="unit"):
        trosna.write(tmp_path / "e.trosna", table, "d")


def test_empty_data_and_all_null_columns_keep_the_schema(tmp_path):
    empty = pa.table(
        {"time": pa.array([], pa.timestamp("us")), "a": pa.array([], pa.float32()), "b": pa.array([], pa.string())}
    )
    path = tmp_path / "e.trosna"
    trosna.write(path, empty, "e")
    back = trosna.read_arrow(path)
    assert back.num_rows == 0
    assert back.schema.types == [pa.timestamp("us"), pa.float32(), pa.string()]

    nulls = pa.table({"time": pa.array([1, 2], pa.timestamp("s")), "a": pa.array([None, None], pa.float32())})
    trosna.write(path, nulls, "e", mode="a")
    assert trosna.read_arrow(path)["a"].to_pylist() == [None, None]
    assert trosna.read_arrow(path)["b"].to_pylist() == [None, None]


def test_time_ranges_columns_and_extreme_bounds(sensor):
    f = trosna.open(sensor)
    part = f.read_arrow(columns=["ok"], start="2026-10-08 10:00:01", end=dt.datetime(2026, 10, 8, 7, 0, 2, tzinfo=dt.timezone.utc))
    assert part.column_names == ["время", "ok"]
    assert part["ok"].to_pylist() == [False]
    assert f.read_arrow(start=-(2**63), end=2**63 - 1).num_rows == 3
    assert f.read_arrow(start="2026-10-08 10:00:02", end="2026-10-08 10:00:01").num_rows == 0
    assert f.read_arrow(start=pd.Timestamp("2026-10-08 10:00:02.5", tz=MSK)).num_rows == 1
    assert f.count(end="2026-10-08T07:00:00Z") == 1


def test_streaming_reads_and_writes(tmp_path):
    n = 300_000
    schema = pa.schema([("t", pa.timestamp("ns")), ("v", pa.float64())])

    def batches():
        for start in range(0, n, 50_000):
            yield pa.record_batch(
                [pa.array(range(start, start + 50_000), pa.int64()).cast(pa.timestamp("ns")),
                 pa.array([float(i) for i in range(start, start + 50_000)])],
                schema=schema,
            )

    path = tmp_path / "s.trosna"
    trosna.write(path, pa.RecordBatchReader.from_batches(schema, batches()), "s")
    reader = trosna.read_batches(path)
    sizes = [b.num_rows for b in reader]
    assert sum(sizes) == n and len(sizes) > 1 and max(sizes) < n


def test_editing_history_and_time_travel(sensor):
    f = trosna.open(sensor)
    with f.edit(message="добавлена точка") as tx:
        tx.insert("датчик", "2026-10-08T10:00:03+03:00", cpu=0.9, ram_mb=4096, состояние="пик", ok=True)
    assert tx.commit.number == 2 and tx.commit.message == "добавлена точка"
    with f.edit(message="исправлено", author="Ферубко") as tx:
        tx.update("датчик", "2026-10-08 10:00:03", {"cpu": 0.95})
        tx.delete("датчик", "2026-10-08 10:00:00")
    assert [c.number for c in f.commits()] == [1, 2, 3]
    assert f.head.author == "Ферубко" and f.head.prev_hash == f.commits()[1].hash

    now = f.read_pandas()
    assert now["cpu"].tolist()[-1] == 0.95 and len(now) == 3
    assert f.read_arrow(as_of=1).equals(trosna.read_arrow(sensor, as_of=1))
    assert f.read_arrow(as_of=1).num_rows == 3

    diff = f.diff(2)
    kinds = {(p.kind, f.device().to_datetime(p.time).second) for p in diff.points}
    assert kinds == {("changed", 3), ("removed", 0)}
    changed = next(p for p in diff.points if p.kind == "changed")
    assert (changed.before["cpu"], changed.after["cpu"]) == (0.9, 0.95)


def test_failed_edits_and_writes_change_nothing(sensor, tmp_path):
    f = trosna.open(sensor)
    with pytest.raises(trosna.TrosnaError, match="no point"):
        with f.edit() as tx:
            tx.insert("датчик", 1, cpu=1.0)
            tx.update("датчик", "2030-01-01", cpu=2.0)
    with pytest.raises(TypeError, match="cpu"):
        with f.edit() as tx:
            tx.insert("датчик", 5, cpu="abc")
    with pytest.raises(RuntimeError):
        with f.edit() as tx:
            tx.delete("датчик", "2026-10-08 10:00:00")
            raise RuntimeError("abort")
    assert len(f.commits()) == 1 and f.count() == 3

    bad = pa.table({"время": pa.array([1], pa.timestamp("ms", MSK)), "cpu": pa.array(["x"])})
    with pytest.raises(ValueError):
        f.write(bad, "датчик")
    assert len(f.commits()) == 1 and trosna.verify(sensor).ok

    new = tmp_path / "new.trosna"
    unsupported = pa.table({"t": pa.array([1], pa.timestamp("s")), "x": pa.array([[1, 2]])})
    with pytest.raises(ValueError, match="x"):
        trosna.write(new, unsupported, "x")
    assert not new.exists()
    with pytest.raises(FileExistsError):
        trosna.write(sensor, sensor_table(), "датчик", mode="x")


def test_annotations(sensor):
    f = trosna.open(sensor)
    with f.edit() as tx:
        tx.annotate("датчик", "2026-10-08 10:00:01", "2026-10-08 10:00:02", "spike", "резервное копирование")
    (i,) = tx.annotation_ids
    (a,) = f.annotations()
    assert (a.id, a.label, a.note) == (i, "spike", "резервное копирование")
    assert f.device().to_datetime(a.start) == dt.datetime(2026, 10, 8, 10, 0, 1, tzinfo=dt.timezone(dt.timedelta(hours=3)))
    with f.edit() as tx:
        tx.update_annotation(i, label="anomaly")
    assert f.annotations()[0].label == "anomaly" and f.annotations()[0].note == "резервное копирование"
    with f.edit() as tx:
        tx.remove_annotation(i)
    assert f.annotations() == [] and f.annotations(as_of=2)[0].label == "spike"
    assert [c.kind for c in f.diff(1).annotations] == []
    assert [c.kind for c in f.diff(2, 3).annotations] == ["changed"]


def test_several_devices_metadata_and_errors(sensor, tmp_path):
    f = trosna.open(sensor)
    f.write(pa.table({"time": pa.array([0], pa.timestamp("s")), "level": [1]}), "второй")
    assert set(f.devices) == {"датчик", "второй"}
    with pytest.raises(ValueError, match="device"):
        f.read_arrow()
    with pytest.raises(KeyError):
        f.read_arrow("нет")
    with f.edit() as tx:
        tx.set_metadata("source", "лаборатория")
    assert f.metadata == {"source": "лаборатория"}
    with pytest.raises(trosna.CorruptedError):
        (tmp_path / "x.trosna").write_text("time,v\n1,2\n")
        trosna.open(tmp_path / "x.trosna")
    with pytest.raises(FileNotFoundError):
        trosna.open(tmp_path / "missing.trosna")


def test_interrupted_and_damaged_files(sensor, tmp_path):
    data = sensor.read_bytes()
    cut = tmp_path / "cut.trosna"
    cut.write_bytes(data[:-10])
    f = trosna.open(cut)
    assert not f.finalized and f.count() == 3
    report = trosna.recover(cut)
    assert not report.was_finalized and report.removed_bytes > 0
    assert trosna.verify(cut).finalized and trosna.recover(cut).was_finalized

    damaged = tmp_path / "damaged.trosna"
    broken = bytearray(data)
    broken[len(broken) // 2] ^= 0xFF
    damaged.write_bytes(bytes(broken))
    try:
        assert not trosna.verify(damaged).ok
    except trosna.CorruptedError:
        pass


def test_compaction_keeps_data_and_links_history(sensor, tmp_path):
    f = trosna.open(sensor)
    with f.edit() as tx:
        tx.delete_range("датчик", start="2026-10-08 10:00:01")
    compacted = f.compact(tmp_path / "c.trosna")
    assert compacted.read_arrow().equals(f.read_arrow())
    (c,) = compacted.commits()
    assert c.prev_hash == f.head.hash
    assert trosna.verify(compacted.path).origin == f.head.hash


def test_golden_file_is_readable():
    f = trosna.open(GOLDEN)
    assert f.devices and trosna.verify(GOLDEN).ok
    for name in f.devices:
        f.read_arrow(name)


def test_time_conversions():
    d = trosna.Device("d", "ns", MSK, "t", {})
    assert d.to_raw("1970-01-01T03:00:00.000000001+03:00") == 1
    assert d.to_raw("1970-01-01 03:00:00") == 0
    assert d.to_raw(pd.Timestamp(5, unit="ns", tz="UTC")) == 5
    assert d.to_raw(pd.Timestamp("1970-01-01 03:00:00.000000007").to_datetime64()) == 7
    assert trosna.Device("d", "s", None, "t", {}).to_raw("1969-12-31T23:59:59.5Z") == -1
    with pytest.raises(ValueError):
        d.to_raw("yesterday")
    with pytest.raises(TypeError):
        d.to_raw(True)


def test_cli_and_python_share_the_format(tmp_path, sensor):
    exe = shutil.which("trosna") or str(pathlib.Path(__file__).resolve().parents[2] / "target" / "debug" / "trosna")
    if not pathlib.Path(exe).exists():
        pytest.skip("the trosna CLI is not built")
    import subprocess

    out = subprocess.run([exe, "cat", str(sensor)], capture_output=True, text=True, check=True).stdout
    assert out.splitlines()[0] == "время,cpu,ram_mb,состояние,ok"
    assert "с запятой, да" in out


def test_a_failed_overwrite_keeps_the_existing_file(sensor, tmp_path):
    before = sensor.read_bytes()
    unsupported = pa.table({"t": pa.array([1], pa.timestamp("s")), "x": pa.array([[1, 2]])})
    with pytest.raises(ValueError):
        trosna.write(sensor, unsupported, "x")  # mode "w" by default
    assert sensor.read_bytes() == before
    assert not [p for p in tmp_path.iterdir() if p.name.endswith(".tmp")]
    trosna.write(sensor, pa.table({"t": pa.array([5], pa.timestamp("s")), "v": [1]}), "new")
    assert list(trosna.open(sensor).devices) == ["new"]


def test_naive_times_are_wall_clock_times_of_the_device_everywhere(tmp_path):
    path = tmp_path / "tz.trosna"
    zoned = pd.DataFrame({"time": pd.to_datetime(["2026-10-08 09:00"]).tz_localize("Europe/Moscow"), "v": [1]})
    trosna.write(path, zoned, "d")
    naive = pd.DataFrame({"time": pd.to_datetime(["2026-10-08 10:00"]), "v": [2]})
    trosna.write(path, naive, "d", mode="a")
    f = trosna.open(path)
    with f.edit() as tx:
        tx.insert("d", dt.datetime(2026, 10, 8, 11, 0), v=3)
        tx.insert("d", "2026-10-08 12:00", v=4)
        tx.insert("d", pd.Timestamp("2026-10-08 13:00"), v=5)
    hours = [t.hour for t in f.read_pandas()["time"]]
    assert hours == [9, 10, 11, 12, 13]
    with pytest.raises(ValueError, match="time zone"):
        _raise_naive_through_native(path)


def _raise_naive_through_native(path):
    # bypassing the Python layer, the native writer refuses naive times for a zoned device
    from trosna import _trosna

    table = pa.table({"time": pa.array([0], pa.timestamp("s")), "v": [1]})
    _trosna.write(str(path), table, "d", "time", None, "a", None, None)


def test_daylight_saving_transitions_and_rounding():
    d = trosna.Device("d", "s", "Europe/Berlin", "t", {})
    with pytest.raises(ValueError, match="does not exist"):
        d.to_raw("2026-03-29 02:30")
    with pytest.raises(ValueError, match="does not exist"):
        d.to_raw(pd.Timestamp("2026-03-29 02:30"))
    earlier = d.to_raw("2026-10-25T00:30:00Z")
    assert d.to_raw("2026-10-25 02:30") == earlier
    assert d.to_raw(dt.datetime(2026, 10, 25, 2, 30)) == earlier
    assert d.to_raw(pd.Timestamp("2026-10-25 02:30")) == earlier
    u = trosna.Device("u", "s", "+03", "t", {})
    assert u.to_raw("1970-01-01 03:00:01") == 1
    assert u.to_raw("1970-01-01 03:00:00.5", "ceil") == 1
    assert u.to_raw("1970-01-01 03:00:00.5") == 0
    with pytest.raises(ValueError, match="more precise"):
        u.to_raw("1970-01-01 03:00:00.5", "exact")


def test_range_bounds_round_inward_and_points_must_be_exact(tmp_path):
    path = tmp_path / "r.trosna"
    trosna.write(path, pa.table({"t": pa.array([0, 1], pa.timestamp("s")), "v": [1, 2]}), "r")
    f = trosna.open(path)
    assert f.read_arrow(start="1970-01-01 00:00:00.5")["v"].to_pylist() == [2]
    assert f.read_arrow(end="1970-01-01 00:00:00.5")["v"].to_pylist() == [1]
    assert f.count(start="1970-01-01 00:00:00.5") == 1
    with pytest.raises(ValueError, match="more precise"):
        with f.edit() as tx:
            tx.insert("r", "1970-01-01 00:00:02.5", v=3)
    with f.edit() as tx:
        tx.delete_range("r", start="1970-01-01 00:00:00.5")
    assert f.read_arrow()["v"].to_pylist() == [1]


def test_polars_zone_names_for_any_offset():
    table = pa.table(
        {
            "a": pa.array([0], pa.timestamp("s", "+03")),
            "b": pa.array([0], pa.timestamp("s", "-13:00")),
            "c": pa.array([0], pa.timestamp("s", "+05:30")),
        }
    )
    types = trosna._polars_compatible(table).schema.types
    assert [t.tz for t in types] == ["Etc/GMT-3", "UTC", "UTC"]


def test_damage_found_while_reading_is_a_corrupted_error(sensor, tmp_path):
    data = sensor.read_bytes()
    damaged = tmp_path / "d.trosna"
    found = 0
    for i in range(8, len(data)):
        broken = bytearray(data)
        broken[i] ^= 0x55
        damaged.write_bytes(bytes(broken))
        try:
            f = trosna.open(damaged)
            for name in f.devices:
                f.read_arrow(name)
        except trosna.CorruptedError:
            found += 1
        except (trosna.TrosnaError, KeyError, ValueError, UnicodeDecodeError):
            pass
    assert found > 0
