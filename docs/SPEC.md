# Trosna file format specification, version 1.0

Status: draft for the first public release.

The key words MUST, MUST NOT, SHOULD and MAY are to be interpreted as described
in RFC 2119. The rationale for the design is given in [DESIGN.md](DESIGN.md).

## 1. Conventions

* All fixed-width integers are **little-endian**.
* `u8`, `u16`, `u32`, `u64`, `i64` denote fixed-width integers.
* `uvarint` is an unsigned LEB128 integer: 7 bits per byte, least significant
  group first, high bit set on all bytes but the last. A `uvarint` MUST NOT be
  longer than 10 bytes and MUST NOT encode a value greater than `2^64 − 1`.
  Writers MUST produce the shortest encoding. Readers decoding a `uvarint`
  into a narrower type MUST reject values that do not fit.
* `zigzag(n) = (n << 1) XOR (n >> 63)` maps `i64` to `u64` (arithmetic shift);
  `svarint` is `uvarint(zigzag(n))`.
* `string` is `uvarint` byte length followed by that many bytes of UTF-8.
  Readers MUST reject invalid UTF-8.
* `opt_string` is a `u8` flag (`0` = absent, `1` = present) followed, if
  present, by a `string`. Other flag values are invalid.
* `kv_map` is `uvarint` count followed by `count` pairs of `string` key and
  `string` value. Keys MUST be unique.
* **Bit streams** are packed least-significant bit first: the first bit
  written is bit 0 of byte 0. A multi-bit field of width `w` is written with
  its least significant bit first. The final byte is padded with zero bits.
* `ceil8(n) = (n + 7) / 8` in integer arithmetic.
* `CRC32C` is the CRC-32 with the Castagnoli polynomial (as used by iSCSI and
  ext4). `SHA-256` is defined in FIPS 180-4.

## 2. File structure

```
file   = header frame*
header = "TROSNA" major:u8 minor:u8
```

The header is the six ASCII bytes `54 52 4F 53 4E 41` followed by the major
version `1` and the minor version `0`. Readers MUST reject files with a
different major version. Minor versions are backward compatible: a reader of
version `1.x` MAY read a file of version `1.y` with `y > x` and MUST then fail
only when it encounters a critical frame kind, encoding or codec it does not
know.

The recommended file name extension is `.trosna`.

## 3. Frames

```
offset  size  field
0       4     sync     = 54 42 4C 4B ("TBLK")
4       1     kind     u8
5       1     flags    u8, MUST be 0 in version 1.0
6       2     reserved u16, MUST be 0
8       4     length   u32, payload length in bytes
12      len   payload
12+len  4     crc      u32 = CRC32C(bytes [4, 12 + length))
```

A frame occupies `16 + length` bytes. `length` MUST NOT exceed `2^30`.

| kind   | name         | critical |
|--------|--------------|----------|
| `0x01` | `Meta`       | yes      |
| `0x02` | `Device`     | yes      |
| `0x03` | `Data`       | yes      |
| `0x04` | `Tombstone`  | yes      |
| `0x05` | `Annotation` | yes      |
| `0x06` | `Commit`     | yes      |
| `0x07` | `Index`      | yes      |
| `0x08` | `Footer`     | yes      |

Kinds with the high bit set (`0x80`–`0xFF`) are **ancillary**: readers that do
not understand them MUST ignore them. Any other unknown kind is critical and
MUST cause an error.

`Index` and `Footer` frames are *derived*: they summarise the frames before
them and are not part of any commit. All other frames are *log frames*.

## 4. Commits

The log frames between the end of the previous `Commit` frame (or the end of
the header) and a `Commit` frame, excluding `Index` and `Footer` frames, are
the frames **sealed** by that commit. Log frames that are not followed by a
`Commit` frame form an incomplete transaction and are not part of the file
state.

`Commit` payload:

```
number          u64   1 for the first commit of the file, then +1
time            i64   wall-clock time of the commit, nanoseconds since the Unix epoch (UTC)
first_offset    u64   offset of the first sealed frame (the Commit frame's own offset if none)
frame_count     u32   number of sealed frames
prev_hash       [32]  commit_hash of the previous commit; for the first commit of the
                      file either 32 zero bytes or the head hash of the file it was
                      compacted from
content_hash    [32]  SHA-256 over the complete bytes (sync … crc) of the sealed
                      frames, concatenated in file order
author          opt_string
message         opt_string
commit_hash     [32]  SHA-256( "trosna/commit/v1" 0x00 ‖ all preceding payload bytes )
```

Readers that verify history MUST check `content_hash`, `commit_hash`,
`frame_count`, `first_offset`, that `number` increases by one, and that each
`prev_hash` equals the `commit_hash` of the previous commit.

## 5. Log frames

Devices are identified by `device_id`, assigned sequentially from 0 in order of
their `Device` frames. Every frame that references a device MUST come after its
`Device` frame.

### 5.1 `Meta` (0x01)

```
metadata  kv_map   complete file metadata; replaces earlier Meta frames
```

### 5.2 `Device` (0x02)

```
device_id     uvarint   MUST equal the number of earlier Device frames
name          string    non-empty, unique within the file
time_unit     u8        0 = seconds, 1 = milliseconds, 2 = microseconds, 3 = nanoseconds
timezone      opt_string  passed through to Arrow (IANA name or "+HH:MM")
time_name     string    name of the time column, e.g. "time"
column_count  uvarint   1 … 65535
columns       column_count × { name: string, type: u8, metadata: kv_map }
metadata      kv_map
```

Column types: `0` bool, `1` int32, `2` int64, `3` float32, `4` float64,
`5` string. Column names MUST be unique within the device and differ from
`time_name`.

### 5.3 `Data` (0x03)

```
header_length  u32
header         header_length bytes:
    device_id      uvarint
    row_count      uvarint   1 … 2^24
    t_min          i64       first time stamp
    t_max          i64       last time stamp
    segment_count  uvarint   MUST equal 1 + column_count of the device
    segments       segment_count × segment_header
segment data    the stored bytes of all segments, concatenated in order
```

The first segment is the time column, followed by the value columns in schema
order. The time stamps of a `Data` frame MUST be strictly increasing.

```
segment_header =
    encoding     u8        see §6
    codec        u8        see §7
    flags        u8        bit 0: validity bitmap present; bit 1: statistics present;
                           other bits MUST be 0
    null_count   uvarint
    stored_len   uvarint   bytes of this segment in the file
    raw_len      uvarint   bytes after decompression, ≤ 2^30
    crc          u32       CRC32C of the stored bytes
    [min         8 bytes]  if bit 1 of flags
    [max         8 bytes]  if bit 1 of flags
```

The time segment MUST have `null_count = 0` and no validity bitmap.
`stored_len` of all segments MUST add up to the rest of the payload.

Statistics are stored as `i64` for time, bool (`0`/`1`), int32 and int64
columns and as the IEEE 754 binary64 bit pattern for float32 (widened) and
float64 columns. Floating-point statistics ignore NaN values; if all non-null
values are NaN, statistics are omitted. String columns have no statistics.

**Raw segment layout** (after decompression):

```
[validity  ceil8(row_count) bytes]   if bit 0 of flags: bit i = 1 ⇔ row i is not null
values                               the (row_count − null_count) non-null values,
                                     in row order, encoded with `encoding`
```

The number of set bits in the validity bitmap MUST equal
`row_count − null_count`; padding bits MUST be zero. If `null_count = 0` the
bitmap MUST be omitted; if all rows are null, the values part is empty and the
encoding MUST be `Plain`.

### 5.4 `Tombstone` (0x04)

```
device_id    uvarint
range_count  uvarint   ≥ 1
ranges       range_count × { start: i64, end: i64 }   start ≤ end, closed intervals
```

### 5.5 `Annotation` (0x05)

```
op_count   uvarint ≥ 1
ops        op_count × op
op = tag:u8 = 1, id:uvarint, device_id:uvarint, start:i64, end:i64,
             label:string, note:opt_string                  -- upsert
   | tag:u8 = 2, id:uvarint                                 -- remove
```

Identifiers are positive and unique within the file; a writer allocates a new
identifier as one more than the largest identifier used so far. `start ≤ end`;
times are in the unit of the device. Removing an identifier that is not present
is an error.

### 5.6 `Commit` (0x06)

See §4.

## 6. Encodings

`n` denotes the number of values to encode (the non-null values of a segment).

| id | name           | allowed for                       |
|----|----------------|-----------------------------------|
| 0  | `Plain`        | all types and the time column     |
| 1  | `DeltaBitPack` | time, int32, int64                |
| 2  | `DeltaOfDelta` | time                              |
| 3  | `Rle`          | bool, int32, int64                |
| 4  | `Xor`          | float32, float64                  |
| 5  | `Dictionary`   | string                            |

Decoders MUST consume the values part exactly (trailing bytes are an error)
and MUST produce exactly `n` values. Int32 values are processed as `i64`; a
decoded value outside the `i32` range is an error. All delta arithmetic is
performed modulo `2^64` (wrapping), so every sequence of 64-bit values is
representable.

### 6.1 `Plain`

* bool: bit stream of `n` bits, `ceil8(n)` bytes.
* int32, float32: 4 bytes per value; int64, float64, time: 8 bytes per value.
  Floating-point values are stored as their IEEE 754 bit patterns.
* string: `n` × `string`.

### 6.2 `DeltaBitPack`

Empty if `n = 0`. Otherwise:

```
first   svarint   v₀
then, for the deltas dᵢ = vᵢ − vᵢ₋₁ (i = 1 … n−1) in groups of 128 (the last group may be shorter):
    min     svarint   minimum of the group's deltas (signed)
    width   u8        0 … 64
    packed  bit stream of (dᵢ − min) as u64 (wrapping), width bits each,
            ceil8(group_size × width) bytes
```

`width` MUST be the number of bits needed for the largest `dᵢ − min`.

### 6.3 `DeltaOfDelta`

Empty if `n = 0`. Otherwise `svarint v₀`; if `n ≥ 2`, `svarint Δ₁` where
`Δ₁ = v₁ − v₀`; then one bit stream holding, for `i = 2 … n−1`,
`z = zigzag(Δᵢ − Δᵢ₋₁)` coded as:

| bits written (in order) | payload                |
|-------------------------|------------------------|
| `0`                     | none, `z = 0`          |
| `1 0`                   | 8 bits of `z`          |
| `1 1 0`                 | 16 bits of `z`         |
| `1 1 1 0`               | 32 bits of `z`         |
| `1 1 1 1`               | 64 bits of `z`         |

The shortest applicable class MUST be used. The bit stream is omitted when
`n ≤ 2`.

### 6.4 `Rle`

A sequence of runs `run_length: uvarint (≥ 1)` followed by the value: an
`svarint` for integers, a `u8` (`0` or `1`) for booleans. Runs MUST add up to
exactly `n` values. Writers MUST merge adjacent runs with equal values.

### 6.5 `Xor`

Gorilla XOR encoding over the IEEE 754 bit patterns, with word width `W = 64`
and field width `F = 6` for float64, and `W = 32`, `F = 5` for float32. Empty
if `n = 0`. Otherwise one bit stream:

```
first value: W bits
for each next value v (prev = previous value's bits, x = bits(v) XOR prev):
    x = 0:  bit 0
    x ≠ 0:  bit 1, then, with lead = leading zeros of x, trail = trailing zeros of x:
        if a window (L, T) exists and lead ≥ L and trail ≥ T:
            bit 0, then (W − L − T) bits of (x >> T)
        else:
            bit 1, lead in F bits, (W − lead − trail − 1) in F bits,
            (W − lead − trail) bits of (x >> trail); the window becomes (lead, trail)
```

Initially no window exists. Decoders MUST reject `lead + length > W`.

### 6.6 `Dictionary`

```
dict_size  uvarint, ≥ 1 when n > 0, ≤ n
entries    dict_size × string (writers SHOULD use first-occurrence order; entries MUST be unique)
indices    bit stream of n indices, each ceil(log2(dict_size)) bits (0 bits if dict_size = 1)
```

Every index MUST be less than `dict_size`.

## 7. Codecs

| id | name   | stored bytes                                                     |
|----|--------|------------------------------------------------------------------|
| 0  | `None` | the raw segment                                                  |
| 1  | `Lz4`  | one LZ4 block (no frame, no size prefix) decompressing to `raw_len` bytes |
| 2  | `Zstd` | one Zstandard frame decompressing to `raw_len` bytes             |

Decompression MUST produce exactly `raw_len` bytes; readers MUST stop
decompressing once `raw_len` bytes have been produced.

## 8. `Index` (0x07) and `Footer` (0x08)

```
Index payload:
    head_commit   u64        number of the last commit covered (0 if none)
    head_hash     [32]       its commit_hash (zeros if none)
    entry_count   uvarint
    entries       entry_count × { kind: u8, offset: u64, body: uvarint length + bytes }

Footer payload:
    index_offset  u64        offset of the Index frame
```

The index lists, in file order, every log frame that belongs to a committed
transaction before the `Index` frame. For `Data` frames `body` is the
frame's `header` (§5.3, without `header_length`); for all other kinds it is
the complete payload. The index MUST be equivalent to what a scan of the file
up to the `Index` frame would produce.

A file is **finalized** if its last 24 bytes are a valid `Footer` frame whose
`index_offset` points to a valid `Index` frame before it. Writers MUST end
every writing session that changed the file with an `Index` and a `Footer`.

## 9. Reading and recovery

A reader opening a file MUST check the header and then:

1. If the file is finalized, it MAY rely on the index instead of reading the
   log frames.
2. Otherwise it MUST reconstruct the state by scanning frames from offset 8.
   The scan stops at end of file or at the first invalid frame (bad sync,
   reserved fields, length beyond the end of the file or beyond the limit, or
   CRC mismatch when checked). Frames after the last valid `Commit` frame are
   discarded.

A writer appending to a file that is not finalized MUST truncate it to the end
of the last valid `Commit` frame (or the header) before writing. It MUST NOT do
so if the scan stopped at an invalid frame that is followed by a valid frame,
unless explicitly instructed: that indicates corruption in the middle of the
file rather than an interrupted write.

## 10. State of the file

For version `v` (the prefix ending with the `v`-th `Commit` frame), the value
of device `d` at time `t` is the row at `t` in the last `Data` frame of `d`
containing `t`, unless a later `Tombstone` frame of `d` covers `t`, in which
case `t` is absent. The state of annotation `id` is given by the last
operation on `id`. "Later" refers to the position in the file.
