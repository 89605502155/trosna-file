"""Trosna time-series files (``.trosna``) for Python.

Read a device into PyArrow, pandas or Polars::

    import trosna
    df = trosna.read_pandas("metrics.trosna")

Write any table with a time column (pandas, Polars, PyArrow or anything that
exports an Arrow stream)::

    trosna.write("metrics.trosna", df, device="vm01")

Edit, annotate and travel in time::

    f = trosna.open("metrics.trosna")
    with f.edit(message="fix a spike") as tx:
        tx.update("vm01", "2026-10-08 10:00:03", cpu=0.95)
        tx.annotate("vm01", "2026-10-08 10:00:00", "2026-10-08 10:05:00", "backup")
    old = f.read_pandas(as_of=1)

Times can be given as ``datetime``, ``pandas.Timestamp``, ``numpy.datetime64``,
ISO 8601 text (without an offset it is read in the device's time zone) or an
integer in the device's time unit. Data is streamed batch by batch in both
directions, so reading with :func:`read_batches` and writing an Arrow stream
use memory proportional to one batch, not to the file.
"""

from __future__ import annotations

import datetime as _dt
import numbers
import os
import re
from dataclasses import dataclass, field
from typing import Any, Iterable, Mapping

import pyarrow as pa

from . import _trosna
from ._trosna import CorruptedError, TrosnaError

__all__ = [
    "Annotation",
    "AnnotationChange",
    "Commit",
    "CorruptedError",
    "Device",
    "Diff",
    "File",
    "PointChange",
    "RecoverReport",
    "Transaction",
    "TrosnaError",
    "VerifyReport",
    "compact",
    "open",
    "read_arrow",
    "read_batches",
    "read_pandas",
    "read_polars",
    "recover",
    "verify",
    "write",
]

EXTENSION = _trosna.EXTENSION

_NS_PER = {"s": 1_000_000_000, "ms": 1_000_000, "us": 1_000, "ns": 1}
_EPOCH = _dt.datetime(1970, 1, 1, tzinfo=_dt.timezone.utc)
_OFFSET = re.compile(r"^([+-])(\d{2})(?::?(\d{2}))?$")
_ISO = re.compile(
    r"^(\d{4})-(\d{2})-(\d{2})"
    r"(?:[T ](\d{2}):(\d{2})(?::(\d{2})(?:[.,](\d{1,9}))?)?)?"
    r"\s*(Z|z|[+-]\d{2}:?\d{2})?$"
)
_TIME_NAMES = ("time", "timestamp", "ts", "datetime", "date", "t", "время")


# ---------------------------------------------------------------- data classes


@dataclass(frozen=True)
class Device:
    """Schema of a device (a table of time-stamped rows)."""

    name: str
    time_unit: str
    timezone: str | None
    time_name: str
    columns: dict[str, str]
    metadata: dict[str, str] = field(default_factory=dict)

    def to_raw(self, value: Any, rounding: str = "floor") -> int:
        """Converts a time to a raw time stamp in this device's unit.

        ``rounding`` decides what happens to a time finer than the unit:
        ``"floor"``, ``"ceil"`` or ``"exact"`` (an error).
        """
        return _to_raw(value, self.time_unit, self.timezone, rounding)

    def to_datetime(self, raw: int) -> _dt.datetime:
        """Converts a raw time stamp to a ``datetime`` (microsecond precision)."""
        return _to_datetime(raw, self.time_unit, self.timezone)


@dataclass(frozen=True)
class Commit:
    """A commit: an atomic, hashed set of changes."""

    number: int
    time: _dt.datetime
    author: str | None
    message: str | None
    hash: str
    prev_hash: str


@dataclass(frozen=True)
class Annotation:
    """A labelled time interval ``[start, end]`` of a device (raw time stamps)."""

    id: int
    device: str
    start: int
    end: int
    label: str
    note: str | None = None


@dataclass(frozen=True)
class PointChange:
    """A point that differs between two versions; ``kind`` is
    ``"added"``, ``"removed"`` or ``"changed"``."""

    time: int
    kind: str
    before: dict[str, Any] | None
    after: dict[str, Any] | None


@dataclass(frozen=True)
class AnnotationChange:
    """An annotation that differs between two versions."""

    id: int
    kind: str
    before: Annotation | None
    after: Annotation | None


@dataclass(frozen=True)
class Diff:
    """Differences of a device between commits ``from_commit`` and ``to_commit``."""

    device: str
    columns: list[str]
    from_commit: int
    to_commit: int
    points: list[PointChange]
    annotations: list[AnnotationChange]


@dataclass(frozen=True)
class VerifyReport:
    """Result of :func:`verify`."""

    ok: bool
    frames: int
    commits: int
    blocks: int
    finalized: bool
    head: str | None
    origin: str | None
    problems: list[str]
    warnings: list[str]


@dataclass(frozen=True)
class RecoverReport:
    """Result of :func:`recover`; ``removed_bytes`` is 0 if the file was finalized."""

    was_finalized: bool
    removed_bytes: int = 0
    removed_frames: int = 0
    reason: str | None = None


# ---------------------------------------------------------------- time


def _tzinfo(tz: str | None) -> _dt.tzinfo | None:
    if tz is None:
        return None
    m = _OFFSET.match(tz)
    if m:
        sign = -1 if m.group(1) == "-" else 1
        return _dt.timezone(sign * _dt.timedelta(hours=int(m.group(2)), minutes=int(m.group(3) or 0)))
    if tz.upper() in ("UTC", "Z"):
        return _dt.timezone.utc
    from zoneinfo import ZoneInfo

    return ZoneInfo(tz)


def _datetime_ns(value: _dt.datetime, tz: str | None) -> int:
    """Exact nanoseconds since the epoch; naive values are in ``tz`` (UTC if None).

    A local time that occurs twice (clocks moved back) is the earlier moment; one
    that does not exist (clocks moved forward) is an error — as in the CLI.
    """
    if value.tzinfo is None:
        zone = _tzinfo(tz) or _dt.timezone.utc
        naive = value.replace(fold=0)
        value = naive.replace(tzinfo=zone)
        back = value.astimezone(_dt.timezone.utc).astimezone(zone).replace(tzinfo=None, fold=0)
        if back != naive:
            raise ValueError(f"{naive} does not exist in time zone {tz} (the clocks were moved forward)")
    delta = value - _EPOCH
    return (delta.days * 86_400 + delta.seconds) * 1_000_000_000 + delta.microseconds * 1_000


def _text_ns(text: str, tz: str | None) -> int:
    m = _ISO.match(text.strip())
    if not m:
        raise ValueError(
            f"cannot read {text!r} as a time (expected e.g. 2026-10-08T10:00:00+03:00 "
            "or 2026-10-08 10:00:00)"
        )
    year, month, day, hour, minute, second, fraction, offset = m.groups()
    base = _dt.datetime(
        int(year), int(month), int(day), int(hour or 0), int(minute or 0), int(second or 0)
    )
    if offset:
        offset = "+00:00" if offset in ("Z", "z") else offset
        base = base.replace(tzinfo=_tzinfo(offset))
    nanos = int((fraction or "").ljust(9, "0"))
    return _datetime_ns(base, tz) + nanos


def _to_raw(value: Any, unit: str, tz: str | None, rounding: str = "floor") -> int:
    """Converts a time given in any supported form to a raw time stamp."""
    if isinstance(value, bool):
        raise TypeError("a bool is not a time")
    if isinstance(value, numbers.Integral):
        return int(value)
    if isinstance(value, str):
        ns = _text_ns(value, tz)
    elif hasattr(value, "tz_localize") and hasattr(value, "value"):  # pandas.Timestamp
        if value.tzinfo is None:
            ns = _datetime_ns(value.to_pydatetime(warn=False), tz) + value.nanosecond
        else:
            ns = int(value.value)
    elif type(value).__name__ == "datetime64":  # numpy, naive by definition
        ns = int(value.astype("datetime64[ns]").astype("int64"))
        if tz is not None:
            local = _EPOCH.replace(tzinfo=None) + _dt.timedelta(microseconds=ns // 1_000)
            ns = _datetime_ns(local, tz) + ns % 1_000
    elif isinstance(value, _dt.datetime):
        ns = _datetime_ns(value, tz)
    elif isinstance(value, _dt.date):
        ns = _datetime_ns(_dt.datetime(value.year, value.month, value.day), tz)
    else:
        raise TypeError(f"cannot use a {type(value).__name__} as a time")
    step = _NS_PER[unit]
    if ns % step == 0 or rounding == "floor":
        return ns // step
    if rounding == "ceil":
        return -(-ns // step)
    if rounding == "exact":
        raise ValueError(f"{value!r} is more precise than the device's time unit ({unit})")
    raise ValueError(f"rounding must be 'floor', 'ceil' or 'exact', not {rounding!r}")


def _to_datetime(raw: int, unit: str, tz: str | None) -> _dt.datetime:
    ns = raw * _NS_PER[unit]
    value = _EPOCH + _dt.timedelta(microseconds=ns // 1_000)
    zone = _tzinfo(tz)
    return value.astimezone(zone) if zone else value.replace(tzinfo=None)


def _opt_raw(value: Any, device: Device, rounding: str) -> int | None:
    return None if value is None else device.to_raw(value, rounding)


# ---------------------------------------------------------------- data frames


def _arrow_stream(data: Any, time_column: str | None) -> tuple[Any, str]:
    """Turns pandas, Polars or Arrow data into an Arrow stream and finds the time column."""
    module = type(data).__module__.split(".")[0]
    if module == "pandas":
        import pandas as pd

        frame = data
        if time_column is None and isinstance(frame.index, pd.DatetimeIndex):
            name = frame.index.name or "time"
            frame = frame.rename_axis(name).reset_index()
            time_column = name
        data = pa.Table.from_pandas(frame, preserve_index=False)
    elif module == "polars":
        data = data.to_arrow()
    if isinstance(data, (pa.Table, pa.RecordBatch)):
        schema = data.schema
        stream = data
    elif hasattr(data, "__arrow_c_stream__"):
        stream = pa.RecordBatchReader.from_stream(data)
        schema = stream.schema
    else:
        raise TypeError(
            f"cannot write a {type(data).__name__}; give a pandas or Polars DataFrame, "
            "a PyArrow Table, RecordBatch or RecordBatchReader"
        )
    if time_column is None:
        time_column = _guess_time_column(schema)
    elif time_column not in schema.names:
        raise KeyError(f"the data has no column {time_column!r}")
    return stream, time_column


def _localize(stream: Any, time_column: str, tz: str) -> Any:
    """Attaches ``tz`` to naive time stamps: like times given as text, they are
    wall-clock readings of the device's zone (the earlier moment when the clocks
    were moved back; an error for times skipped when they were moved forward)."""
    schema = stream.schema
    index = schema.get_field_index(time_column)
    field = schema.field(index)
    if not pa.types.is_timestamp(field.type) or field.type.tz is not None:
        return stream
    import pyarrow.compute as pc

    target = schema.set(index, field.with_type(pa.timestamp(field.type.unit, tz)))

    def fix(batch: pa.RecordBatch) -> pa.RecordBatch:
        columns = list(batch.columns)
        columns[index] = pc.assume_timezone(columns[index], tz, ambiguous="earliest", nonexistent="raise")
        return pa.RecordBatch.from_arrays(columns, schema=target)

    if isinstance(stream, pa.RecordBatch):
        return fix(stream)
    if isinstance(stream, pa.Table):
        return pa.Table.from_batches([fix(b) for b in stream.to_batches()], target)
    return pa.RecordBatchReader.from_batches(target, (fix(b) for b in stream))


def _guess_time_column(schema: pa.Schema) -> str:
    for f in schema:
        if pa.types.is_timestamp(f.type):
            return f.name
    lowered = {name.lower(): name for name in schema.names}
    for candidate in _TIME_NAMES:
        if candidate in lowered:
            return lowered[candidate]
    raise ValueError("cannot tell which column holds the time; pass time_column=...")


# ---------------------------------------------------------------- the file


class File:
    """A Trosna file. Opening it reads only its index; data is read on demand."""

    def __init__(self, path: str | os.PathLike[str]):
        self.path = os.fspath(path)
        self._load()

    def _load(self) -> None:
        info = _trosna.info(self.path)
        self.finalized: bool = info["finalized"]
        self.format_version: tuple[int, int] = tuple(info["format_version"])
        self.size: int = info["size"]
        self.metadata: dict[str, str] = dict(info["metadata"])
        self.devices: dict[str, Device] = {
            d["name"]: Device(
                name=d["name"],
                time_unit=d["time_unit"],
                timezone=d["timezone"],
                time_name=d["time_name"],
                columns=dict(d["columns"]),
                metadata=dict(d["metadata"]),
            )
            for d in info["devices"]
        }

    def __repr__(self) -> str:
        return f"trosna.File({self.path!r}, devices={list(self.devices)})"

    def device(self, name: str | None = None) -> Device:
        """The named device, or the only one in the file."""
        if name is not None:
            try:
                return self.devices[name]
            except KeyError:
                raise KeyError(f"unknown device {name!r}") from None
        if len(self.devices) == 1:
            return next(iter(self.devices.values()))
        if not self.devices:
            raise KeyError("the file has no devices")
        raise ValueError(f"the file has several devices {list(self.devices)}; pass device=...")

    # reading

    def read_batches(
        self,
        device: str | None = None,
        *,
        columns: Iterable[str] | None = None,
        start: Any = None,
        end: Any = None,
        as_of: int | None = None,
    ) -> pa.RecordBatchReader:
        """Streams the points of a device in time order (``start``/``end`` inclusive)."""
        d = self.device(device)
        native = _trosna.read(
            self.path,
            d.name,
            None if columns is None else list(columns),
            _opt_raw(start, d, "ceil"),
            _opt_raw(end, d, "floor"),
            as_of,
        )
        return pa.RecordBatchReader.from_stream(native)

    def read_arrow(self, device: str | None = None, **query: Any) -> pa.Table:
        """Reads a device into a PyArrow Table; see :meth:`read_batches` for the options.

        Damage found while reading raises :class:`CorruptedError` (a stream from
        :meth:`read_batches` reports it as a PyArrow error instead)."""
        stream = self.read_batches(device, **query)
        try:
            return stream.read_all()
        except pa.ArrowException as e:
            if "corrupted data" in str(e):
                raise CorruptedError(str(e)) from e
            raise

    def read_pandas(self, device: str | None = None, **query: Any):
        """Reads a device into a pandas DataFrame with the time as its first column."""
        return self.read_arrow(device, **query).to_pandas()

    def read_polars(self, device: str | None = None, **query: Any):
        """Reads a device into a Polars DataFrame."""
        import polars as pl

        table = self.read_arrow(device, **query)
        return pl.from_arrow(_polars_compatible(table))

    def count(
        self, device: str | None = None, *, start: Any = None, end: Any = None, as_of: int | None = None
    ) -> int:
        """Number of points (reads only the time stamps)."""
        d = self.device(device)
        return _trosna.count(
            self.path, d.name, _opt_raw(start, d, "ceil"), _opt_raw(end, d, "floor"), as_of
        )

    # writing

    def write(self, data: Any, device: str, **options: Any) -> Commit | None:
        """Appends data to a device (created if missing); see :func:`write`."""
        commit = write(self.path, data, device, mode="a", **options)
        self._load()
        return commit

    def edit(self, message: str | None = None, author: str | None = None) -> Transaction:
        """Starts a transaction: all its changes become one commit when the
        ``with`` block ends without an exception, and none otherwise."""
        return Transaction(self, message, author)

    # history

    def commits(self) -> list[Commit]:
        """All commits, oldest first."""
        return [_commit(c) for c in _trosna.commits(self.path)]

    @property
    def head(self) -> Commit | None:
        """The latest commit."""
        commits = self.commits()
        return commits[-1] if commits else None

    def annotations(self, device: str | None = None, *, as_of: int | None = None) -> list[Annotation]:
        """Annotations (of one device, or of all), as of a commit if given."""
        if device is not None:
            self.device(device)
        return [Annotation(*a) for a in _trosna.annotations(self.path, device, as_of)]

    def diff(self, from_commit: int, to_commit: int | None = None, *, device: str | None = None) -> Diff:
        """What changed in a device between two commits (0 is the empty file)."""
        d = _trosna.diff(self.path, self.device(device).name, from_commit, to_commit)
        return Diff(
            device=d["device"],
            columns=list(d["columns"]),
            from_commit=d["from"],
            to_commit=d["to"],
            points=[PointChange(*p) for p in d["points"]],
            annotations=[
                AnnotationChange(
                    i, kind, Annotation(*b) if b else None, Annotation(*a) if a else None
                )
                for i, kind, b, a in d["annotations"]
            ],
        )

    # maintenance

    def verify(self) -> VerifyReport:
        return verify(self.path)

    def compact(self, target: str | os.PathLike[str], *, as_of: int | None = None, overwrite: bool = False) -> File:
        compact(self.path, target, as_of=as_of, overwrite=overwrite)
        return File(target)


class Transaction:
    """Changes collected by :meth:`File.edit`; applied atomically on exit."""

    def __init__(self, file: File, message: str | None, author: str | None):
        self._file = file
        self._message = message
        self._author = author
        self._ops: list[tuple] = []
        self.commit: Commit | None = None
        self.annotation_ids: list[int] = []

    def __enter__(self) -> Transaction:
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        if exc_type is not None or not self._ops:
            return
        number, self.annotation_ids = _trosna.edit(
            self._file.path, self._ops, self._message, self._author
        )
        if number is not None:
            self.commit = next(c for c in self._file.commits() if c.number == number)
        self._file._load()

    def _raw(self, device: str, time: Any) -> int:
        # a point's time must be representable exactly, or points would merge
        return self._file.device(device).to_raw(time, "exact")

    def insert(self, device: str, time: Any, values: Mapping[str, Any] | None = None, **kw: Any) -> None:
        """Adds a point, or replaces the one at the same time; missing columns are null."""
        self._ops.append(("insert", device, self._raw(device, time), {**(values or {}), **kw}))

    def update(self, device: str, time: Any, values: Mapping[str, Any] | None = None, **kw: Any) -> None:
        """Changes some values of an existing point."""
        self._ops.append(("update", device, self._raw(device, time), {**(values or {}), **kw}))

    def delete(self, device: str, time: Any) -> None:
        """Deletes the point at ``time``, if any."""
        self._ops.append(("delete", device, self._raw(device, time)))

    def delete_range(self, device: str, start: Any = None, end: Any = None) -> None:
        """Deletes all points with ``start <= time <= end`` (open ends if None)."""
        d = self._file.device(device)
        self._ops.append(("delete_range", device, _opt_raw(start, d, "ceil"), _opt_raw(end, d, "floor")))

    def annotate(self, device: str, start: Any, end: Any, label: str, note: str | None = None) -> None:
        """Adds an annotation; its identifier is in ``annotation_ids`` after the commit."""
        d = self._file.device(device)
        self._ops.append(("annotate", device, d.to_raw(start), d.to_raw(end), label, note))

    def update_annotation(
        self,
        id: int,
        *,
        start: Any = None,
        end: Any = None,
        label: str | None = None,
        note: str | None = None,
    ) -> None:
        """Changes an annotation; arguments that are not given keep their values."""
        current = next((a for a in self._file.annotations() if a.id == id), None)
        if current is None:
            raise TrosnaError(f"unknown annotation {id}")
        d = self._file.device(current.device)
        self._ops.append(
            (
                "update_annotation",
                id,
                current.start if start is None else d.to_raw(start),
                current.end if end is None else d.to_raw(end),
                current.label if label is None else label,
                current.note if note is None else note,
            )
        )

    def remove_annotation(self, id: int) -> None:
        self._ops.append(("remove_annotation", id))

    def set_metadata(self, key: str, value: str) -> None:
        self._ops.append(("set_metadata", key, value))

    def remove_metadata(self, key: str) -> None:
        self._ops.append(("remove_metadata", key))


def _commit(c: Mapping[str, Any]) -> Commit:
    time = _EPOCH + _dt.timedelta(microseconds=c["time"] // 1_000)
    return Commit(c["number"], time, c["author"], c["message"], c["hash"], c["prev_hash"])


def _polars_compatible(table: pa.Table) -> pa.Table:
    """Polars accepts only IANA time zones; fixed offsets become ``Etc/GMT±H``
    when whole hours, otherwise UTC (the instants are unchanged)."""
    fields = []
    changed = False
    for f in table.schema:
        if pa.types.is_timestamp(f.type) and f.type.tz and _OFFSET.match(f.type.tz):
            m = _OFFSET.match(f.type.tz)
            hours, minutes = int(m.group(2)), int(m.group(3) or 0)
            limit = 14 if m.group(1) == "+" else 12  # Etc/GMT-14 … Etc/GMT+12 exist
            if minutes == 0 and 0 < hours <= limit:
                # POSIX-style names have the opposite sign.
                tz = f"Etc/GMT{'-' if m.group(1) == '+' else '+'}{hours}"
            else:
                tz = "UTC"
            f = f.with_type(pa.timestamp(f.type.unit, tz))
            changed = True
        fields.append(f)
    return table.cast(pa.schema(fields, metadata=table.schema.metadata)) if changed else table


# ---------------------------------------------------------------- module API


def open(path: str | os.PathLike[str]) -> File:  # noqa: A001 - mirrors built-in open
    """Opens a Trosna file for reading, editing and history queries."""
    return File(path)


def read_batches(path: str | os.PathLike[str], device: str | None = None, **query: Any) -> pa.RecordBatchReader:
    """Streams a device as Arrow record batches (memory: one batch at a time)."""
    return File(path).read_batches(device, **query)


def read_arrow(path: str | os.PathLike[str], device: str | None = None, **query: Any) -> pa.Table:
    """Reads a device into a PyArrow Table.

    Options: ``columns``, ``start``, ``end`` (inclusive) and ``as_of`` (a commit number).
    """
    return File(path).read_arrow(device, **query)


def read_pandas(path: str | os.PathLike[str], device: str | None = None, **query: Any):
    """Reads a device into a pandas DataFrame; options as in :func:`read_arrow`."""
    return File(path).read_pandas(device, **query)


def read_polars(path: str | os.PathLike[str], device: str | None = None, **query: Any):
    """Reads a device into a Polars DataFrame; options as in :func:`read_arrow`."""
    return File(path).read_polars(device, **query)


def write(
    path: str | os.PathLike[str],
    data: Any,
    device: str,
    *,
    time_column: str | None = None,
    unit: str | None = None,
    mode: str = "w",
    message: str | None = None,
    author: str | None = None,
) -> Commit | None:
    """Writes a table to a device in one commit.

    ``data`` is a pandas or Polars DataFrame (a pandas ``DatetimeIndex`` is used
    as the time), a PyArrow Table/RecordBatch/RecordBatchReader or any object
    exporting an Arrow stream. ``time_column`` defaults to the first timestamp
    column or a column named like "time"; integer times need ``unit``
    ("s", "ms", "us" or "ns"). ``mode`` is "w" (replace the file), "x" (create a
    new file) or "a" (append; a missing device is created). Rows with the time
    of an existing point replace it. If anything fails, nothing is committed.
    """
    stream, time_column = _arrow_stream(data, time_column)
    if mode == "a" and os.path.exists(path):
        existing = File(path).devices.get(device)
        if existing is not None and existing.timezone is not None:
            stream = _localize(stream, time_column, existing.timezone)
    number = _trosna.write(
        os.fspath(path), stream, device, time_column, unit, mode, message, author
    )
    if number is None:
        return None
    return next(c for c in File(path).commits() if c.number == number)


def verify(path: str | os.PathLike[str]) -> VerifyReport:
    """Checks all checksums, the hash chain and the decoding of all data."""
    return VerifyReport(**_trosna.verify(os.fspath(path)))


def recover(path: str | os.PathLike[str], *, force: bool = False) -> RecoverReport:
    """Finalizes a file whose writer was interrupted (``force`` also cuts off
    everything after a damaged frame in the middle of the file)."""
    r = _trosna.recover(os.fspath(path), force)
    if r is None:
        return RecoverReport(was_finalized=True)
    return RecoverReport(False, *r)


def compact(
    source: str | os.PathLike[str],
    target: str | os.PathLike[str],
    *,
    as_of: int | None = None,
    overwrite: bool = False,
) -> dict[str, Any]:
    """Copies a version (the latest by default) into a new file without history;
    its first commit links to the source version's hash."""
    return _trosna.compact(os.fspath(source), os.fspath(target), as_of, overwrite)
