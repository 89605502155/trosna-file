"""File size, write time and read time of one time series in several formats.

For every series length n and every format the script writes the same series
(time stamps with a 1 s step in milliseconds and one float64 value column),
calls fsync (Trosna does so on commit by default, so every format pays for
durability), reads the file back into memory, checks that the values are
identical, and records the file size and the median write and read times.

Datasets:
  sensor  readings of a slowly varying quantity rounded to 0.01 (random walk);
  noise   independent normally distributed values at full float64 precision.

Formats and the in-memory form each one is given and returns:
  CSV, CSV.gz        pyarrow.csv            pyarrow.Table
  Parquet (zstd)     pyarrow.parquet        pyarrow.Table
  Arrow IPC (zstd)   pyarrow.feather v2     pyarrow.Table
  HDF5 (gzip)        h5py                   numpy arrays
  TsFile             tsfile (Apache)        pandas.DataFrame
  Trosna (zstd)      trosna                 pyarrow.Table

Usage:
  python storage_benchmark.py --out results --sizes 10 100 1000 ...
Run it under a memory limit on small machines; files are deleted after each
measurement, so disk use is bounded by the largest series.
"""

from __future__ import annotations

import argparse
import csv
import gc
import json
import os
import platform
import shutil
import statistics
import tempfile
import time
from importlib import metadata
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.csv as pacsv
import pyarrow.feather as feather
import pyarrow.parquet as pq

T0_MS = 1_791_442_800_000  # 2026-10-08 07:00:00 UTC
SEED = 20261008


# ---------------------------------------------------------------- data


def make_series(kind: str, n: int) -> tuple[np.ndarray, np.ndarray]:
    rng = np.random.default_rng(SEED)
    time_ms = T0_MS + np.arange(n, dtype=np.int64) * 1000
    if kind == "sensor":
        value = np.round(20.0 + np.cumsum(rng.normal(0.0, 0.05, n)), 2)
    elif kind == "noise":
        value = rng.normal(0.0, 1.0, n)
    else:
        raise ValueError(kind)
    return time_ms, value


def as_table(time_ms: np.ndarray, value: np.ndarray) -> pa.Table:
    return pa.table(
        {
            "time": pa.array(time_ms, pa.int64()).cast(pa.timestamp("ms", "UTC")),
            "value": pa.array(value, pa.float64()),
        }
    )


# ---------------------------------------------------------------- formats


def fsync(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


CSV_TYPES = pacsv.ConvertOptions(
    column_types={"time": pa.timestamp("ms", "UTC"), "value": pa.float64()}
)


class Csv:
    name, ext = "CSV", ".csv"

    def prepare(self, t, v):
        return as_table(t, v)

    def write(self, data, path):
        pacsv.write_csv(data, path)

    def read(self, path):
        table = pacsv.read_csv(path, convert_options=CSV_TYPES)
        return table["value"].to_numpy()


class CsvGzip(Csv):
    name, ext = "CSV.gz", ".csv.gz"

    def write(self, data, path):
        with pa.CompressedOutputStream(str(path), "gzip") as out:
            pacsv.write_csv(data, out)

    def read(self, path):
        with pa.CompressedInputStream(pa.OSFile(str(path)), "gzip") as src:
            table = pacsv.read_csv(src, convert_options=CSV_TYPES)
        return table["value"].to_numpy()


class Parquet:
    name, ext = "Parquet", ".parquet"

    def prepare(self, t, v):
        return as_table(t, v)

    def write(self, data, path):
        pq.write_table(data, path, compression="zstd")

    def read(self, path):
        return pq.read_table(path)["value"].to_numpy()


class ArrowIpc:
    name, ext = "Arrow IPC", ".arrow"

    def prepare(self, t, v):
        return as_table(t, v)

    def write(self, data, path):
        feather.write_feather(data, path, compression="zstd")

    def read(self, path):
        return feather.read_table(path)["value"].to_numpy()


class Hdf5:
    name, ext = "HDF5", ".h5"

    def prepare(self, t, v):
        return t, v

    def write(self, data, path):
        import h5py

        t, v = data
        with h5py.File(path, "w") as f:
            f.create_dataset("time", data=t, compression="gzip", shuffle=True)
            f.create_dataset("value", data=v, compression="gzip", shuffle=True)

    def read(self, path):
        import h5py

        with h5py.File(path, "r") as f:
            f["time"][:]
            return f["value"][:]


class TsFile:
    name, ext = "TsFile", ".tsfile"

    def prepare(self, t, v):
        import pandas as pd

        return pd.DataFrame({"time": t, "value": v})

    def write(self, data, path):
        from tsfile import ColumnCategory, ColumnSchema, TableSchema, TSDataType, TsFileTableWriter

        schema = TableSchema("sensor", [ColumnSchema("value", TSDataType.DOUBLE, ColumnCategory.FIELD)])
        writer = TsFileTableWriter(str(path), schema)
        writer.write_dataframe(data)
        writer.close()

    def read(self, path):
        from tsfile import to_dataframe

        return to_dataframe(str(path))["value"].to_numpy(dtype=np.float64)


class Trosna:
    name, ext = "Trosna", ".trosna"

    def prepare(self, t, v):
        return as_table(t, v)

    def write(self, data, path):
        import trosna

        trosna.write(path, data, "sensor")

    def read(self, path):
        import trosna

        return trosna.read_arrow(path)["value"].to_numpy()


FORMATS = [Csv(), CsvGzip(), Parquet(), ArrowIpc(), Hdf5(), TsFile(), Trosna()]


# ---------------------------------------------------------------- measurement


def repeats_for(n: int) -> int:
    if n <= 10_000:
        return 9
    if n <= 300_000:
        return 5
    if n <= 3_000_000:
        return 3
    return 2


def measure(fmt, kind: str, n: int, workdir: Path) -> dict:
    t, v = make_series(kind, n)
    data = fmt.prepare(t, v)
    path = workdir / f"series{fmt.ext}"
    writes, reads = [], []
    size = None
    for _ in range(repeats_for(n)):
        if path.exists():
            path.unlink()
        gc.collect()
        start = time.perf_counter()
        fmt.write(data, path)
        fsync(path)
        writes.append(time.perf_counter() - start)
        size = path.stat().st_size
        gc.collect()
        start = time.perf_counter()
        values = fmt.read(path)
        reads.append(time.perf_counter() - start)
        if not np.array_equal(np.asarray(values, dtype=np.float64), v):
            raise AssertionError(f"{fmt.name}: values read back differ (n={n}, {kind})")
        del values
    path.unlink()
    return {
        "dataset": kind,
        "format": fmt.name,
        "n": n,
        "size_bytes": size,
        "bytes_per_point": size / n,
        "write_s": statistics.median(writes),
        "read_s": statistics.median(reads),
        "repeats": len(writes),
    }


def environment() -> dict:
    versions = {}
    for package in ["numpy", "pyarrow", "pandas", "h5py", "tsfile", "trosna"]:
        try:
            versions[package] = metadata.version(package)
        except metadata.PackageNotFoundError:
            versions[package] = None
    cpu = ""
    try:
        for line in open("/proc/cpuinfo"):
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    mem = ""
    try:
        mem = open("/proc/meminfo").readline().split(":", 1)[1].strip()
    except OSError:
        pass
    return {
        "python": platform.python_version(),
        "platform": platform.platform(),
        "cpu": cpu,
        "memory": mem,
        "versions": versions,
        "seed": SEED,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--out", type=Path, required=True, help="folder for results.csv")
    parser.add_argument("--sizes", type=float, nargs="+", required=True)
    parser.add_argument("--datasets", nargs="+", default=["sensor", "noise"])
    parser.add_argument("--formats", nargs="+", default=[f.name for f in FORMATS])
    parser.add_argument("--workdir", type=Path, default=None, help="where the files are written (default: --out)")
    args = parser.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "environment.json").write_text(json.dumps(environment(), indent=2, ensure_ascii=False))
    results = args.out / "results.csv"
    fields = ["dataset", "format", "n", "size_bytes", "bytes_per_point", "write_s", "read_s", "repeats"]
    new = not results.exists()
    # On disk next to the results by default (/tmp may be a RAM-backed tmpfs,
    # which would both use memory and make fsync meaningless).
    workdir = Path(tempfile.mkdtemp(prefix="trosna-bench-", dir=args.workdir or args.out))
    try:
        with results.open("a", newline="") as f:
            out = csv.DictWriter(f, fieldnames=fields)
            if new:
                out.writeheader()
            for kind in args.datasets:
                for n in (int(s) for s in args.sizes):
                    for fmt in FORMATS:
                        if fmt.name not in args.formats:
                            continue
                        row = measure(fmt, kind, n, workdir)
                        out.writerow(row)
                        f.flush()
                        print(
                            f"{kind:6} {fmt.name:9} n={n:>9} size={row['size_bytes']:>11} B "
                            f"({row['bytes_per_point']:7.2f} B/pt) write={row['write_s'] * 1e3:9.2f} ms "
                            f"read={row['read_s'] * 1e3:9.2f} ms",
                            flush=True,
                        )
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    main()
