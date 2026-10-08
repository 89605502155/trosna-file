# Storage benchmark

`storage_benchmark.py` writes one time series (time stamps with a 1 s step in
milliseconds and one `float64` value column) in seven formats, calls `fsync`
after every write (Trosna does so on commit, so every format pays for
durability), reads the file back, checks that the values are identical and
records the file size and the median write and read times.

| Format | Library | Settings |
|---|---|---|
| CSV, CSV.gz | pyarrow.csv | gzip for CSV.gz |
| Parquet | pyarrow.parquet | zstd |
| Arrow IPC | pyarrow.feather (v2) | zstd |
| HDF5 | h5py | gzip + byte shuffle |
| TsFile | tsfile (Apache) | defaults (table model) |
| Trosna | trosna | defaults (adaptive encodings, zstd at its fastest level) |

Datasets: `sensor` — a random walk rounded to 0.01 (slowly varying sensor
readings); `noise` — independent normal values at full precision (the worst
case for compression).

```sh
pip install maturin pyarrow pandas h5py tsfile matplotlib
maturin develop --release -m python/Cargo.toml
python benchmarks/storage_benchmark.py --out results --sizes 10 100 1000 10000 100000 1000000
python benchmarks/plot_benchmark.py results/results.csv --out figures
```

On machines with little memory run large sizes one format at a time
(`--formats Trosna`); files are written next to the results (not to `/tmp`,
which may be RAM-backed) and deleted after each measurement.

## Results

[`results/`](results) holds the measurements used in the paper (Intel Core
i3-6006U, 3.2 GB RAM, Linux, Python 3.14, PyArrow 23.0, h5py 3.16, tsfile 2.4).
Bytes per point for the `sensor` dataset:

| Format | N = 10 | N = 10³ | N = 10⁵ | N = 10⁷ |
|---|---|---|---|---|
| Trosna | 69.6 | 4.55 | 2.94 | **2.86** |
| TsFile | 47.0 | 4.75 | 3.88 | 3.84 |
| Parquet | 95.4 | 5.75 | 5.12 | 3.69 |
| Arrow IPC | 82.6 | 4.85 | 3.31 | 3.33 |
| HDF5 | 791.2 | 12.40 | 4.99 | 4.42 |
| CSV.gz | **11.3** | **3.96** | 3.88 | 3.90 |
| CSV | 32.1 | 30.88 | 30.56 | 31.45 |

Short series are dominated by fixed per-file overhead, so compressed CSV is the
smallest up to about a thousand points; from 3·10³ points Trosna is the
smallest. For incompressible full-precision noise HDF5 with byte shuffling is
slightly smaller than Trosna beyond 3·10⁵ points. Arrow IPC writes and reads
large series fastest; Trosna reads short series fastest and pays for two
`fsync` calls per commit when writing.
