"""Trosna from Python: write a pandas DataFrame, edit it, read old versions.

    pip install trosna[pandas,polars]
    python examples/quickstart.py
"""

import pathlib
import tempfile

import pandas as pd

import trosna

path = pathlib.Path(tempfile.mkdtemp()) / "sensors.trosna"

df = pd.read_csv(pathlib.Path(__file__).with_name("sensors.csv"), parse_dates=["time"])
trosna.write(path, df, device="room1", message="import from CSV")

f = trosna.open(path)
print(f.devices["room1"])

with f.edit(message="door was open, drop the reading", author="analyst") as tx:
    tx.delete("room1", "2026-10-08 10:00:20")
    tx.annotate("room1", "2026-10-08 10:00:15", "2026-10-08 10:00:25", "door open")

print(f.read_pandas())               # current data
print(f.read_pandas(as_of=1))        # as imported
print(f.diff(1).points)              # what the edit changed
for commit in f.commits():
    print(commit.number, commit.hash[:12], commit.message)

# Streaming: memory use is one batch, whatever the file size.
for batch in trosna.read_batches(path, columns=["temperature"]):
    print(batch.num_rows, "rows")
