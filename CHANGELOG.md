# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-10-08

First public version. File format version 1.0 (draft).

### Added

- `trosna`: the file format library — append-only log of CRC32C-checked
  frames; devices with typed columns (bool, int32, int64, float32, float64,
  string), time units s/ms/us/ns and time zones; adaptive per-segment encodings
  (plain, delta-bitpack, delta-of-delta, RLE, Gorilla XOR, dictionary) and
  LZ4/Zstandard compression; buffered writing, appending and recovery after
  crashes; point updates and deletions, time-range deletions and interval
  annotations as atomic, SHA-256 hash-chained commits; reading any earlier
  version (`as_of`), diffs between versions; `verify`, `recover` and `compact`
  with provenance; `Reader::query_shared` for streams that own their reader.
- `trosna-arrow`: conversion to and from Apache Arrow record batches and
  `RecordBatchReader` streams.
- `trosna-cli`: the `trosna` command — `convert` (CSV ↔ Trosna, streamed),
  `info`, `cat`, `insert`, `update`, `delete`, `annotate`, `annotations`,
  `unannotate`, `log`, `diff`, `verify`, `recover`, `compact`.
- Python package `trosna`: `read_arrow`, `read_batches`, `read_pandas`,
  `read_polars`, `write`, `open` (editing transactions, history, annotations,
  diffs), `verify`, `recover`, `compact`.
- Specification (`docs/SPEC.md`), design document (`docs/DESIGN.md`),
  step-by-step guides (`docs/GUIDE.md`, `docs/GUIDE.ru.md`), golden test file,
  examples and CI.
- `trosna info` reports, per column, the stored size, bits per point, the
  compression ratio and the encodings used.
- Storage benchmark against CSV, Parquet, Arrow IPC, HDF5 and Apache TsFile
  (`benchmarks/`).

[Unreleased]: https://github.com/89605502155/trosna-file/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/89605502155/trosna-file/releases/tag/v0.1.0
