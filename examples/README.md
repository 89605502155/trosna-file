# Examples

* [`sensors.csv`](sensors.csv) — a small CSV for the command-line tool:

  ```sh
  trosna convert examples/sensors.csv sensors.trosna --device room1
  trosna info sensors.trosna
  trosna update sensors.trosna --time "2026-10-08 10:00:20" temperature=21.8 -m "sensor glitch"
  trosna annotate sensors.trosna --start "2026-10-08 10:00:15" --end "2026-10-08 10:00:25" --label "door open"
  trosna log sensors.trosna
  trosna diff sensors.trosna --from 1
  trosna cat sensors.trosna --as-of 1 --format table
  trosna verify sensors.trosna
  ```

* [`quickstart.py`](quickstart.py) — the same from Python with pandas.
* [`../crates/trosna/examples/quickstart.rs`](../crates/trosna/examples/quickstart.rs)
  — the Rust library: `cargo run -p trosna --example quickstart`.
