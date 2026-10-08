# Trosna: design and rationale

This document explains *why* Trosna looks the way it does. The byte-level
format is defined normatively in [SPEC.md](SPEC.md); this document describes
the data model, the semantics of editing and history, the algorithms used by
the reference implementation, and the research contributions of the project.

## 1. Context and motivation

The review by A. O. Ferubko and O. D. Kazakov, *"Review of data structures and
file formats for storage and transfer of time series with regard to their
support in the Python and Rust ecosystems"* (2026), compares text formats,
row-oriented binary formats, Parquet/ORC/Arrow, HDF5/netCDF/Zarr, Apache
TsFile, internal formats of time-series databases and industry measurement
formats. Its conclusions are:

* columnar storage with separate encoding of timestamps and values is the most
  promising organisation for time series, and is most completely realised by
  Apache TsFile;
* TsFile has maintained implementations only in Java and C++; the only Rust
  crate (`tsfile 0.0.2`) has been abandoned since 2021;
* general-purpose columnar formats (Parquet, Arrow) have mature Rust and Python
  implementations but do not account for time-series specifics, and appending
  to a Parquet file is expensive;
* HDF5, netCDF and industry formats are available in Rust only through C
  bindings or early-stage crates, which carry the memory-safety risks of the
  reference C implementations into Rust programs.

The review therefore motivates *a memory-safe Rust implementation of a
time-series file format that develops the ideas of TsFile, with Python
bindings and adapters to Apache Arrow, pandas and Polars*. Trosna is that
implementation.

## 2. Goals and non-goals

Goals:

1. A single, self-contained file (`*.trosna`) that can be created, read and
   edited as easily as a CSV file.
2. Columnar storage with time-series-specific encodings (delta-of-delta for
   timestamps, XOR for floating-point values) followed by a general-purpose
   codec, with per-page statistics and a time index.
3. Streaming writes, cheap appends, and recovery of files whose writer crashed.
4. Point-level editing — inserting, updating and deleting points anywhere on
   the time axis — and interval annotations, both with full, verifiable
   history.
5. A pure, safe Rust core (`#![forbid(unsafe_code)]`, no C dependencies) that
   never panics or exhibits undefined behaviour on malformed input.
6. Zero-copy-friendly adapters to Apache Arrow, and from there to pandas and
   Polars in Python.

Non-goals (for format version 1):

* binary compatibility with Apache TsFile;
* schema evolution of an existing device (adding columns later);
* value-predicate push-down and secondary indexes;
* multi-writer concurrency (one writer per file, any number of readers);
* lossy compression.

## 3. Data model

```
file.trosna
├── file metadata               (string → string)
├── device "vm01"               (metadata: string → string)
│     time      : i64, unit ∈ {s, ms, us, ns}, optional time zone
│     cpu       : float64       (column metadata, e.g. unit=%)
│     ram_mb    : int64
│     state     : string
├── device "vm02" …
└── annotations                 (id, device, [start, end], label, note)
```

* A **device** is a table: one time column shared by all of its value columns.
  This follows TsFile's device/sensor model while avoiding repeated timestamps
  for synchronous sensors.
* Value types: `bool`, `int32`, `int64`, `float32`, `float64`, `string` —
  exactly the set of TsFile primitive types. Every value may be null.
* At any version of the file, the time stamps of a device are unique: a
  device is a function from time to a row of values.
* A device's schema is fixed when the device is created. New devices may be
  added at any time.
* An **annotation** is a labelled closed interval `[start, end]` on the time
  axis of a device (`start = end` marks an instant event). Annotations are
  intended for marking anomalies, regimes or events, e.g. as training labels
  for machine-learning models.
* Mapping to Apache Arrow: the time column becomes `Timestamp(unit, tz)`; the
  other types map one-to-one (`Boolean`, `Int32`, `Int64`, `Float32`,
  `Float64`, `Utf8`). One device corresponds to one Arrow record batch stream
  and one DataFrame.

## 4. The file is a log

A Trosna file is an append-only sequence of self-delimiting, checksummed
*frames* after an 8-byte header. Bytes that have been written are never
modified; the only operation that ever shortens a file is the removal of an
incomplete, uncommitted tail (§7.3).

```
header | Device Data Data Commit₁ | Data Tombstone Annotation Commit₂ | Index Footer
       |←──────── commit 1 ───────→|←───────────── commit 2 ───────────→|
```

Frame kinds:

| Kind         | Role                                                              |
|--------------|-------------------------------------------------------------------|
| `Meta`       | file metadata (the latest one is in effect)                       |
| `Device`     | definition of a device (name, time unit, columns, metadata)        |
| `Data`       | a sorted run of rows of one device, stored column by column       |
| `Tombstone`  | deletion of all points of a device inside time intervals          |
| `Annotation` | creation, modification or removal of annotations                  |
| `Commit`     | seals the preceding frames into an atomic, hashed transaction     |
| `Index`      | snapshot of the catalogue for fast opening (derived data)         |
| `Footer`     | pointer to the latest `Index`; the last frame of a closed file     |

### 4.1 Formal semantics

Let the log `L = f₁ f₂ … f_m` be the sequence of frames, ordered by file
offset, and let `c₁ < c₂ < … < c_k` be the positions of `Commit` frames. The
*version* `v` of the file is the prefix `L_v = f₁ … f_{c_v}`; frames after the
last commit do not belong to any version.

For a device `d`, define for each time stamp `t`:

* `W_v(d, t)` — the set of `Data` frames in `L_v` of device `d` that contain a
  row with time stamp `t`;
* `D_v(d, t)` — the set of `Tombstone` frames in `L_v` of device `d` with an
  interval containing `t`.

Let `w = max W_v(d, t)` (by position) and `x = max D_v(d, t)` (∅ if empty).
Then the state of the file is

```
S_v(d)(t) = row of w at t,  if W_v(d, t) ≠ ∅ and (D_v(d, t) = ∅ or x < w)
          = absent,         otherwise.
```

In words: **the later write wins, where "later" means "later in the log"**.
A deletion removes everything written before it; a point written after a
deletion is visible again. Updates are writes of a full row at an existing
time stamp. Annotations follow the same rule: the state of annotation `id` at
version `v` is given by the last annotation operation on `id` in `L_v`
(upsert ⇒ present with those attributes, remove ⇒ absent).

Consequences that the implementation relies on and the tests check:

1. **Prefix closure.** Every prefix of the log that ends with a `Commit` frame
   is a valid file. Crash consistency therefore needs no separate write-ahead
   log: the file *is* the log.
2. **Time travel is free.** `S_v` is defined for every `v`, so reading the file
   "as of" any past commit costs nothing beyond keeping the history.
3. **No-edit fast path.** If the `Data` frames of a device do not overlap in
   time and no tombstones exist, `S_v(d)` is the concatenation of the frames;
   reading is a plain columnar scan without merging.
4. **Bitemporality.** Each point has a *valid time* (its time stamp) and a
   *transaction time* (the commit that wrote it, which carries a wall-clock
   time). The file answers both "what was measured at t?" and "what did the
   file say about t at commit v / at date T?".

### 4.2 Commits and atomicity

A writer accumulates changes and seals them with a `Commit` frame. The commit
records its sequence number, wall-clock time, optional author and message, the
offset and count of the frames it seals, the SHA-256 hash of those frames
(`content_hash`) and the hash of the previous commit (`prev_hash`). Its own
`commit_hash` is the SHA-256 of all of these fields.

* Frames not followed by a `Commit` belong to an incomplete transaction and
  are ignored by readers; `rollback()` and recovery truncate them.
* With durable commits enabled (the default), the writer issues `fsync` after
  the sealed frames and again after the `Commit` frame, so a commit record can
  never reach the disk before the data it seals.

### 4.3 Verifiable history

Commit hashes form a hash chain, as in Git or Certificate Transparency logs.
Any modification of historical bytes — even one that recomputes the CRC32C
checksums — changes the content hash of its commit and breaks every later
link of the chain. Publishing the head hash (for example in a paper or a lab
journal) therefore makes the whole measurement history tamper-evident:
nobody can silently "correct" or delete a measurement retroactively. CRC32C
detects accidental corruption; SHA-256 detects deliberate modification.

The threat model is integrity, not authenticity: someone who rewrites the
entire file and all subsequent hashes produces a different head hash, which
is detected only by comparing it with an externally anchored value. Digital
signatures of commits are left for future work.

## 5. Encodings

Each column of a `Data` frame is stored as a *segment*: an optional validity
bitmap followed by the non-null values encoded with a column encoding, the
whole compressed with a general-purpose codec. This is the two-stage scheme of
Fig. 1 of the review.

| Encoding       | Applies to             | Idea                                                          |
|----------------|------------------------|---------------------------------------------------------------|
| `Plain`        | all                    | little-endian values, bitmaps for booleans                    |
| `DeltaOfDelta` | time                   | Gorilla-style prefix codes for `Dᵢ = Δᵢ − Δᵢ₋₁` (formula 2)   |
| `DeltaBitPack` | time, int32, int64     | TS_2DIFF-style: deltas, frame of reference, bit packing       |
| `Rle`          | bool, int32, int64     | run-length encoding for constant stretches                    |
| `Xor`          | float32, float64       | Gorilla XOR with leading/trailing-zero windows (formula 3)    |
| `Dictionary`   | string                 | dictionary + bit-packed indices                               |

**Adaptive selection.** Data have different character even within one series
(regular vs. jittery sampling, smooth vs. noisy signals, long constant
stretches). Following the adaptive approaches surveyed in the review (AFC,
ACTSD, WZS, Cocv), the writer encodes each segment with *every* applicable
encoding and keeps the smallest result. The choice is made per segment and
recorded in the segment header, so different parts of one column may use
different encodings.

**Codecs.** The second stage is `None`, `Lz4` (pure-Rust `lz4_flex` in safe
mode) or `Zstd` (pure-Rust `ruzstd`; its encoder currently implements only the
"fastest" level, roughly zstd level 1, while the decoder is complete). If a
codec does not make a segment smaller, the segment is stored uncompressed.

**Statistics.** Each segment header stores the null count and, for numeric
and boolean columns, the minimum and maximum (floating-point statistics ignore
NaN). Each `Data` frame stores its row count and time range. `trosna info`
reports the compression ratio `K = S_raw / S_stored` and the mean number of
bits per point `B = 8·S_stored / n` (formula 4 of the review) per column, where
`S_raw` is the size of the column in `Plain` encoding.

## 6. Reading

1. Read the last 24 bytes. If they are a valid `Footer`, read the `Index` it
   points to. The index contains the headers of all frames except the column
   data, so after opening, the whole catalogue is in memory.
2. Otherwise the file was not closed (the writer crashed or is still
   writing): rebuild the catalogue by scanning frame headers from the
   beginning, keeping only committed frames. By default this happens
   transparently and `Reader::is_finalized()` reports `false`; a strict mode
   turns it into an error.
3. For a query `(device, columns, [start, end], as_of)` select the `Data`
   frames that overlap the interval and belong to the requested version, and
   read **only the segments of the requested columns** with positioned reads.
   This realises formula (5) of the review: the volume read is proportional to
   the widths of the selected columns only. Every segment is verified against
   its CRC32C before decoding.

### 6.1 Merge-on-read

Frames are grouped into *clusters* of transitively time-overlapping frames
(a sweep over frames sorted by `t_min`). For each cluster:

* a single frame without relevant tombstones is decoded and sliced to the
  query interval — the fast path for files without edits;
* otherwise the time columns of all frames of the cluster are decoded, rows are
  ordered by `(t, −position)`, the first row for each distinct `t` is kept, and
  rows covered by a tombstone positioned after their frame are dropped. Only
  then are the requested value columns gathered.

Tombstones relevant to a frame are those positioned after it and intersecting
its time range; they are merged into a sorted list of disjoint intervals and
tested with a moving pointer. Merging a cluster of `N` rows costs
`O(N log N)`; a file without edits is read in `O(N)` with no merging overhead.
The degree of overlap is the *read amplification* caused by edits;
`trosna compact` removes it.

## 7. Writing

### 7.1 Buffering and blocks

Rows are buffered per device. When a buffer reaches `rows_per_block` rows
(65 536 by default) or a byte limit, it is sorted by time (stable, the last
write of a duplicate time stamp wins) and written as a `Data` frame. Buffered
rows are also written before a tombstone of the same device, so the order of
operations within a transaction is preserved in the log, and on `commit`.

### 7.2 Editing operations

* `write` / `insert` — rows with arbitrary time stamps (out-of-order inserts are
  allowed). A row with an existing time stamp replaces the old row (upsert).
* `update(device, t, values)` — reads the current row at `t` (from the
  in-memory buffer or the file, including uncommitted frames of the current
  transaction), replaces the given values and writes the full row; fails if no
  point exists at `t`.
* `delete(device, t)` / `delete_range(device, [a, b])` — writes a tombstone.
* `annotate`, `update_annotation`, `remove_annotation` — annotation operations.
  Annotation identifiers are unique within the file.
* `commit(message, author)` — seals the transaction; `rollback()` discards it.

### 7.3 Appending and recovery

Opening an existing file for writing takes an exclusive advisory lock
(`File::try_lock`) and reads the index. If the file has no footer, the
catalogue is rebuilt by a full scan (with checksum verification), the
uncommitted tail is truncated, and writing continues; a collector restarted
after a crash simply keeps writing. Truncation never removes committed data:
if a damaged frame is followed by further valid frames (corruption in the
middle of the file), the writer refuses to open the file and `trosna recover`
requires `--force`.

`close()` commits pending changes and writes a new `Index` and `Footer`.
Earlier indexes and footers remain in the file as dead frames; this costs a
few kilobytes per writing session and is removed by `trosna compact`.

### 7.4 Compaction and provenance

`compact` writes the state of a chosen version into a new file as large,
non-overlapping blocks, with current annotations, in a single commit. The
history is not copied, but provenance is preserved: the first commit of the
new file uses the head hash of the source file as its `prev_hash`, and the
file metadata records `trosna.compacted_from` and
`trosna.compacted_from_commit`.

## 8. Components

```
crates/trosna        core library: format, encodings, writer, reader, history
crates/trosna-arrow  conversion between Trosna batches and Arrow record batches
crates/trosna-cli    the `trosna` command-line tool
python/              PyO3 extension module and the `trosna` Python package
```

* **Core** depends only on pure-Rust crates (`thiserror`, `lz4_flex`, `ruzstd`,
  `crc32c`, `sha2`) and forbids `unsafe` code. In-memory columns use Arrow's
  physical layout (value buffers, LSB-first validity bitmaps, `i32` string
  offsets), so the Arrow adapter moves buffers instead of copying values.
* **trosna-arrow** maps device schemas to Arrow schemas, converts batches in
  both directions (with lossless widening of narrower Arrow integer types and
  casting of other string representations), and exposes reads as
  `RecordBatchReader`s, consumable by DataFusion and other Arrow tools.
* **CLI**: `convert` (CSV ↔ Trosna), `info`, `cat`, `insert`, `update`,
  `delete`, `annotate`, `annotations`, `unannotate`, `log`, `diff`, `verify`,
  `recover`, `compact`.
* **Python**: `trosna.read_pandas / read_polars / read_arrow / read_batches`,
  `trosna.write`, and `trosna.open(...)` returning a file object with editing,
  annotation and history methods. Data cross the Rust/Python boundary through
  the Arrow PyCapsule interface (`pyo3-arrow`), so pandas, Polars and pyarrow
  all receive Arrow buffers without serialisation. Reads are exposed as an
  Arrow C stream backed by `Reader::query_shared`, whose batches own a shared
  handle to the reader, so Python consumes them one block at a time.

The workspace pins Arrow 59 because `pyo3-arrow 0.19` depends on it.

## 9. Robustness

* `#![forbid(unsafe_code)]` in all crates of the workspace except the Python
  extension (whose binding macros expand to `unsafe` blocks).
* All parsing goes through a bounds-checked cursor; every length, count and
  offset read from the file is validated against limits and against the file
  size before any allocation, which protects against decompression bombs and
  malicious length fields.
* Memory use is bounded by a block or a batch, not by the file: queries,
  exports and Python reads are streams; CSV import makes one pass to infer the
  types and a second pass to convert batches of 65 536 rows; decoders reserve
  memory only for what their input can actually hold, and an LZ4 segment that
  claims to expand by more than the format allows (255:1) is rejected before
  any allocation; the decoded size of a dictionary-encoded string segment is
  computed from its indices and checked against the segment limit (2^30 bytes)
  before the strings are materialised.
* Integer arithmetic on data from the file is checked or explicitly wrapping;
  decoders validate that they consume exactly the declared amount of data.
* Errors are reported through a single `trosna::Error` type with the offset of
  the damaged frame where applicable. Malformed input never panics.

## 10. Testing

* Round-trip property tests for every encoding and type (including NaN, ±0.0,
  ±∞, extreme integers and time stamps).
* **Model-based testing of the semantics of §4.1**: random sequences of writes,
  deletions, annotation operations and commits are applied both to a Trosna
  file and to a simple reference model (ordered maps); the state at every
  version must coincide.
* Crash simulation: the file is truncated at every byte offset; the reader must
  either return the state of the last complete commit or a clean error.
* Corruption tests: random byte flips must yield an error or the correct data,
  never a panic.
* Golden files under `testdata/` pin the byte format of version 1.
* Integration tests for the Arrow adapter, the CLI and the Python package.

## 11. Research contributions

1. A single-file, log-structured, **bitemporal** time-series format: point-level
   edits and annotations with free time travel, where the log position is the
   version and every committed prefix is a consistent file.
2. **Tamper-evident history** of measurements through a hash chain of commits,
   with provenance links preserved across compaction.
3. **Merge-on-read with a no-edit fast path**: files without edits are read as
   plain columnar storage; the cost of edits is confined to overlapping
   clusters and removable by compaction.
4. **Adaptive per-segment encoding selection** among delta-of-delta,
   TS_2DIFF-style bit packing, Gorilla XOR, RLE and dictionary encodings.
5. Versioned **interval annotations** stored alongside the data they label.
6. A **memory-safe implementation** without `unsafe` code or C dependencies,
   with Arrow-based adapters for the Python data ecosystem.

## 12. Future work

* Polars IO plugin and DataFusion `TableProvider` with projection, time-range
  and statistics-based push-down.
* Import from and export to Apache TsFile.
* Signed commits (e.g. Ed25519) for authenticity, not only integrity.
* Schema evolution (adding columns to an existing device).
* Parallel decoding of blocks; more encodings (Chimp, ALP, Elf).
