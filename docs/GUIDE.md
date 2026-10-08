# Guide: creating and using `.trosna` files

[Русская версия](GUIDE.ru.md) · [README](../README.md) ·
[Demo repository](https://github.com/89605502155/trosna-file-demo)

A `.trosna` file stores one or more time series. Each series belongs to a
*device*: a table whose rows have a time stamp and values of named columns
(say `temperature`, `humidity`). You can create such a file from the command
line (for example from CSV), from Python, or from a Rust program.

## 1. Installation

Rust 1.89 or newer is required (<https://rustup.rs>); Python 3.11 or newer for
the Python package.

| What | Command |
|---|---|
| The `trosna` tool | `cargo install --git https://github.com/89605502155/trosna-file trosna-cli` |
| Rust library | in `Cargo.toml`: `trosna = { git = "https://github.com/89605502155/trosna-file" }` |
| Python package | `pip install "trosna @ git+https://github.com/89605502155/trosna-file#subdirectory=python"` |
| pandas and Polars (optional) | `pip install pandas polars` |

The Python package is built from source, so Rust must be installed as well.
Check: `trosna --version`, `python -c "import trosna; print(trosna.EXTENSION)"`.

## 2. Your first file

### From CSV (command line)

```csv
time,temperature,humidity
2026-10-08T10:00:00+03:00,21.5,40
2026-10-08T10:00:10+03:00,21.6,41
2026-10-08T10:00:20+03:00,21.7,41
```

```sh
trosna convert room.csv room.trosna --device room1
trosna info room.trosna          # devices, columns, number of points, size
trosna cat room.trosna           # the data back as CSV
```

Column types are detected (integers → `int64`, decimals → `float64`,
`true/false` → `bool`, anything else → `string`; an empty cell is a null). The
first column is the time unless `--time NAME` says otherwise. ISO 8601 times
with an offset set the device's time zone; integers are Unix epoch values
whose unit (`s`, `ms`, `us`, `ns`) is guessed from their size or given with
`--unit`.

### From Python

```python
import pandas as pd
import trosna

df = pd.DataFrame({
    "time": pd.date_range("2026-10-08 10:00", periods=5, freq="10s", tz="Europe/Moscow"),
    "temperature": [21.5, 21.6, 21.7, 21.6, 21.8],
})
trosna.write("room.trosna", df, device="room1")   # creates (or replaces) the file
print(trosna.read_pandas("room.trosna"))
```

A `polars.DataFrame` or `pyarrow.Table` works as well. The time column is
found automatically (the first timestamp column, or one named `time`,
`timestamp`, …) or given with `time_column="ts"`; integer times need
`unit="ms"` (or `s`, `us`, `ns`).

### From Rust

```rust
use trosna::{Batch, Column, CommitOptions, DataType, DeviceSchema, TimeUnit, Writer};

fn main() -> trosna::Result<()> {
    let mut writer = Writer::create("room.trosna")?;
    writer.create_device(
        DeviceSchema::new("room1", TimeUnit::Second)
            .with_timezone("Europe/Moscow")
            .with_column("temperature", DataType::Float64),
    )?;
    let t0 = 1_791_442_800; // 2026-10-08 10:00:00 Moscow time, Unix seconds
    let time: Vec<i64> = (0..5).map(|i| t0 + 10 * i).collect();
    let values = vec![21.5, 21.6, 21.7, 21.6, 21.8];
    writer.write("room1", &Batch::new(time, vec![("temperature", Column::from(values))])?)?;
    writer.commit_with(CommitOptions::message("first measurements"))?;
    writer.close()
}
```

In Rust, a time stamp is an integer number of `TimeUnit`s since 1970-01-01 UTC.

## 3. Rules worth knowing

* **Commits.** Every change is an atomic commit: each `trosna.write` call and
  each `with f.edit(...)` block in Python, each `commit_with` in Rust (`close`
  commits what is left). After a crash the file reads as of its last commit;
  the next writer or `trosna recover` removes the unfinished tail.
* **A repeated time stamp** in a device replaces the point (last write wins);
  points are always read in time order.
* **Times without a UTC offset** are wall-clock times of the device's time
  zone; in a repeated hour the earlier moment is taken, a skipped hour is an
  error.
* **Precision.** A point's time must be exact in the device's unit; range
  bounds between two time stamps are rounded inwards.
* **Several devices.** Choose one when reading: `--device room2` or
  `trosna.read_pandas("f.trosna", "room2")`.

## 4. Appending and editing

```sh
trosna convert more.csv room.trosna --append --device room1
trosna insert room.trosna --time "2026-10-08 10:01:00" temperature=22.0 -m "manual entry"
trosna update room.trosna --time "2026-10-08 10:00:20" temperature=21.65 -m "sensor check"
trosna delete room.trosna --time "2026-10-08 10:00:10"
trosna delete room.trosna --from "2026-10-08 10:00:30" --to "2026-10-08 10:00:40"
```

```python
f = trosna.open("room.trosna")
f.write(more_df, "room1")                               # append (one commit)
with f.edit(message="sensor check", author="me") as tx:  # one atomic commit
    tx.update("room1", "2026-10-08 10:00:20", temperature=21.65)
    tx.delete("room1", "2026-10-08 10:00:10")
    tx.insert("room1", "2026-10-08 10:01:00", temperature=22.0)
```

If the `with` block raises, none of its changes is written.

## 5. Interval annotations

```sh
trosna annotate room.trosna --start "2026-10-08 10:00:15" --end "2026-10-08 10:00:25" \
    --label "door open" --note "ventilation"
trosna annotations room.trosna
trosna annotate room.trosna --id 1 --label "window open"
trosna unannotate room.trosna 1
```

```python
with f.edit() as tx:
    tx.annotate("room1", "2026-10-08 10:00:15", "2026-10-08 10:00:25", "door open")
print(f.annotations())
```

## 6. History and time travel

```sh
trosna log room.trosna
trosna cat room.trosna --as-of 1
trosna diff room.trosna --from 1
```

```python
f.commits(); f.read_pandas(as_of=1); f.diff(1).points
```

Each commit stores the hash of the previous one; `trosna verify` checks every
frame checksum and the whole chain.

## 7. Reading parts of the data

```sh
trosna cat room.trosna --columns temperature --from "2026-10-08 10:00:10" --to "2026-10-08 10:00:30"
trosna cat room.trosna --format table --limit 20
trosna convert room.trosna room.csv
```

```python
trosna.read_arrow("room.trosna", columns=["temperature"], start="2026-10-08 10:00:10")
for batch in trosna.read_batches("room.trosna"):   # streamed block by block
    process(batch)
```

## 8. Maintenance

| Task | Command line | Python |
|---|---|---|
| Check integrity | `trosna verify f.trosna` | `trosna.verify("f.trosna")` |
| Recover after a crash | `trosna recover f.trosna` | `trosna.recover("f.trosna")` |
| Compact (drop history) | `trosna compact f.trosna new.trosna` | `trosna.compact("f.trosna", "new.trosna")` |

The format itself is specified in [SPEC.md](SPEC.md); the design rationale is
in [DESIGN.md](DESIGN.md).
