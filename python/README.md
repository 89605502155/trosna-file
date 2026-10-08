# trosna for Python

Read and write [Trosna](https://github.com/89605502155/trosna-file) time-series files (`.trosna`) as
easily as CSV, with pandas, Polars and PyArrow. The format and its Rust
implementation are described in the main repository.

```python
import pandas as pd
import trosna

df = pd.DataFrame({
    "time": pd.date_range("2026-10-08 10:00", periods=3, freq="s", tz="Europe/Moscow"),
    "cpu": [0.31, 0.35, 0.33],
})
trosna.write("metrics.trosna", df, device="vm01")

trosna.read_pandas("metrics.trosna")                        # whole device
trosna.read_polars("metrics.trosna", start="2026-10-08 10:00:01")
for batch in trosna.read_batches("metrics.trosna"):         # streaming
    ...

f = trosna.open("metrics.trosna")
with f.edit(message="fix a spike", author="me") as tx:
    tx.update("vm01", "2026-10-08 10:00:01", cpu=0.30)
    tx.annotate("vm01", "2026-10-08 10:00:00", "2026-10-08 10:00:02", "backup")
f.read_pandas(as_of=1)          # the data before the fix
f.diff(1)                       # what changed since commit 1
f.commits()                     # hash-chained history
```

Every change is a commit; old versions stay readable until the file is
compacted (`trosna.compact`). `trosna.verify` checks all checksums and the
hash chain, `trosna.recover` finalizes a file whose writer was interrupted.
