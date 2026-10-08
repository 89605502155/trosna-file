# Trosna

**Trosna** is a file format for time series (`.trosna`) with a memory-safe
Rust implementation, a command-line tool and a Python package. It aims to be
as easy to create and read as CSV while storing data the way time-series
databases do: column by column, with encodings chosen for time stamps and
measurements.

[Русская версия](README.ru.md) · [**Step-by-step guide**](docs/GUIDE.md) ·
[Format specification](docs/SPEC.md) · [Design and rationale](docs/DESIGN.md) ·
[Examples](examples/) · [Demo repository](https://github.com/89605502155/trosna-file-demo)

* **Columnar and compact.** Each block stores the time stamps and every column
  separately; each segment picks the smallest of delta-of-delta, TS_2DIFF-style
  bit packing, Gorilla XOR, run-length and dictionary encodings, followed by
  optional LZ4 or Zstandard compression.
* **One file, many devices.** A file holds any number of *devices* (tables of
  time-stamped rows with their own columns, time unit and time zone).
* **Editable, with history.** Points can be inserted, changed and deleted, and
  time intervals can be *annotated* (labelled). Every change is an atomic
  commit; any earlier version stays readable (`as_of`) until the file is
  compacted. Commits form a SHA-256 hash chain, so the history is
  tamper-evident.
* **Crash-safe appends.** The file is an append-only log of checksummed frames.
  A crash leaves a readable file containing every completed commit; the next
  writer (or `trosna recover`) removes the unfinished tail.
* **Safe by construction.** No `unsafe` code and no C dependencies in the
  library; every length read from a file is checked against limits and the
  file size before memory is allocated, and malformed input yields an error,
  never a panic.
* **Works with the Python data stack.** pandas, Polars and PyArrow read and
  write Trosna files through Apache Arrow without copying values; reads and
  writes are streamed batch by batch.

> Status: version 0.1, format version 1.0 (draft). The format may still change
> before 1.0 of the library.

## Why

The review *"Review of data structures and file formats for storage and
transfer of time series with regard to their support in the Python and Rust
ecosystems"* (A. O. Ferubko, O. D. Kazakov, Bryansk State Engineering
Technological University, 2026) found that columnar storage with
separate encoding of time stamps and values — best realised by Apache TsFile —
suits time series most, but TsFile has maintained implementations only in Java
and C++. General-purpose columnar formats (Parquet, Arrow) ignore time-series
specifics and make appends expensive, and scientific formats (HDF5, netCDF)
reach Rust only through C libraries. Trosna develops the ideas of TsFile in
safe Rust and adds what measurement data often needs: corrections that keep
the original values, labelled intervals and verifiable provenance. See
[docs/DESIGN.md](docs/DESIGN.md) for the details and the research contributions.

## Command line

```sh
cargo install --git https://github.com/89605502155/trosna-file trosna-cli   # the `trosna` command

trosna convert sensors.csv sensors.trosna --device room1   # CSV → Trosna
trosna info sensors.trosna
trosna cat sensors.trosna --from "2026-10-08 10:00" --columns temperature
trosna update sensors.trosna --time "2026-10-08 10:00:20" temperature=21.8 -m "sensor glitch"
trosna annotate sensors.trosna --start "2026-10-08 10:00:15" --end "2026-10-08 10:00:25" --label "door open"
trosna log sensors.trosna
trosna diff sensors.trosna --from 1
trosna cat sensors.trosna --as-of 1 --format table
trosna convert sensors.trosna back.csv                       # Trosna → CSV
trosna verify sensors.trosna
```

CSV import detects the column types, reads ISO 8601 times (the first UTC
offset becomes the device's time zone) or integer epochs (the unit is guessed
from their size or given with `--unit`), and processes the file in batches,
so its size is not limited by memory. Run `trosna help <command>` for all
options.

The same time rules hold in the CLI and in Python: a time without a UTC offset
is a wall-clock time of the device's time zone (in a repeated hour the earlier
moment is taken; a time skipped by a daylight-saving change is an error); the
time of a point must be representable in the device's unit exactly, while range
bounds that fall between two time stamps are rounded inwards. Conversions that
replace a file write a temporary file first, so a failure never damages
existing data.

## Python

```sh
pip install "trosna @ git+https://github.com/89605502155/trosna-file#subdirectory=python"
# or, in a clone of the repository: pip install maturin && maturin develop -m python/Cargo.toml
```

```python
import pandas as pd
import trosna

df = pd.read_csv("examples/sensors.csv", parse_dates=["time"])
trosna.write("sensors.trosna", df, device="room1")

trosna.read_pandas("sensors.trosna")
trosna.read_polars("sensors.trosna", start="2026-10-08 10:00:10", columns=["temperature"])
for batch in trosna.read_batches("sensors.trosna"):   # pyarrow.RecordBatch, streamed
    ...

f = trosna.open("sensors.trosna")
with f.edit(message="door was open", author="analyst") as tx:   # one atomic commit
    tx.delete("room1", "2026-10-08 10:00:20")
    tx.annotate("room1", "2026-10-08 10:00:15", "2026-10-08 10:00:25", "door open")
f.read_pandas(as_of=1)     # the data as imported
f.diff(1)                  # what changed since commit 1
f.commits()                # the hash-chained history
```

## Rust

```rust
use trosna::{Batch, Column, CommitOptions, DataType, DeviceSchema, Reader, TimeUnit, Writer};

let mut writer = Writer::create("metrics.trosna")?;
writer.create_device(
    DeviceSchema::new("vm01", TimeUnit::Millisecond).with_column("cpu", DataType::Float64),
)?;
writer.write("vm01", &Batch::new(vec![1_000, 2_000], vec![("cpu", Column::from(vec![0.31, 0.35]))])?)?;
writer.commit_with(CommitOptions::message("first measurements"))?;
writer.close()?;

let reader = Reader::open("metrics.trosna")?;
let cpu = reader.query("vm01")?.columns(["cpu"]).time_range(1_000..=1_500).collect()?;
```

The Arrow adapter `trosna-arrow` turns queries into Arrow `RecordBatchReader`s
for DataFusion and other Arrow-based tools. A complete example is in
[crates/trosna/examples/quickstart.rs](crates/trosna/examples/quickstart.rs).

## How it compares

On a slowly varying sensor series (values rounded to 0.01, 1 s step) a
`.trosna` file takes **2.86 bytes per point** for 10 million points — 14 % less
than Arrow IPC, 22 % less than Parquet and 25 % less than Apache TsFile (all
with Zstandard). Very short series are dominated by fixed per-file overhead,
where compressed CSV is the smallest. See [benchmarks/](benchmarks) for the
method, all numbers and the scripts.

## Repository

```
crates/trosna        core library: format, encodings, writer, reader, history
crates/trosna-arrow  conversion to and from Apache Arrow
crates/trosna-cli    the `trosna` command-line tool
python/              Python package (PyO3 extension + pure-Python layer)
docs/                format specification and design document
testdata/            golden files that pin the byte format
benchmarks/          storage benchmark against CSV, Parquet, Arrow, HDF5, TsFile
examples/            CSV, Python and command-line examples
```

Tests include property-based round trips of every encoding, a model-based
test of the editing semantics against a reference model, truncation of files
at every byte offset, random byte flips, golden files, and integration tests
of the CLI and the Python package:

```sh
cargo test
maturin develop -m python/Cargo.toml && pytest python/tests
```

The minimum supported Rust version is 1.89; Python 3.11 or newer is required.

## Citation

If you use Trosna in research, please cite the review that motivated it:

> Ferubko A. O., Kazakov O. D. Review of data structures and file formats for
> storage and transfer of time series with regard to their support in the
> Python and Rust ecosystems. 2026.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in this
project by you, as defined in the Apache-2.0 license, shall be dual licensed
as above, without any additional terms or conditions.
