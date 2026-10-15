<!--
  ~ Licensed to the Apache Software Foundation (ASF) under one
  ~ or more contributor license agreements.  See the NOTICE file
  ~ distributed with this work for additional information
  ~ regarding copyright ownership.  The ASF licenses this file
  ~ to you under the Apache License, Version 2.0 (the
  ~ "License"); you may not use this file except in compliance
  ~ with the License.  You may obtain a copy of the License at
  ~
  ~   http://www.apache.org/licenses/LICENSE-2.0
  ~
  ~ Unless required by applicable law or agreed to in writing,
  ~ software distributed under the License is distributed on an
  ~ "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
  ~ KIND, either express or implied.  See the License for the
  ~ specific language governing permissions and limitations
  ~ under the License.
-->

# Writer memory diagnostics

This patch builds on main's integer buffer/encoding optimization. Dictionary
hash tables are now built from buffered values only when a bucket is encoded,
then released before the next bucket. Appending and resetting retain no dictionary
tables. The schema, dictionary limits/first-occurrence ordering, encoding selection,
row-group thresholds and data ownership stay the same. The FFI now releases the
consumed ArrowSchema export wrappers after importing their owned metadata, before
core construction/append. Imported ArrowArray owners remain alive through buffering
and any automatic flush. A new optional C checkpoint function covers caller code;
existing call signatures and opaque-handle ABI remain unchanged.
Diagnostics are default-on; no application logger registration or memory profiler
is required. Moving dictionary work into flush can change flush latency; compare
whole-write CPU time as well as append and close time.

## Collect one useful run

1. Build and deploy main plus this PR through the application's usual build. Do
   not stack the old optimization or diagnostic PRs. Diagnostics are **on by
   default**; remove any existing `MOSAIC_MEMORY_DIAGNOSTICS=0` override.
2. Before starting the normal workload, collect Android INFO records:
   ```sh
   adb logcat -v threadtime -T 1 'mosaic-memory:I' '*:S' | tee mosaic-memory.log
   ```
   Check that NEW lines contain `[mosaic-memory] v=4`, `stage=create_begin`, and
   the application's PID. Device log filtering still applies. On Linux, capture
   the application's stderr instead, for example `./app 2>mosaic-memory.log`.
3. Keep the same data, 30-second append interval, large row groups, and normal
   memory reclamation. Collect at least two complete file-writing cycles (about
   20 minutes for a 10-minute rotation). Include flush and close records.
4. Return the complete log, the build commit, and the existing peak-memory
   monitor's result/unit and approximate peak time. No heap profiling is needed. For the current export/write investigation, add the single pre-export
   checkpoint below in the same build. Start with a fresh application process when
   practical so the process high-water mark covers this run.

The tag-filtered command above excludes application business logs. If a business
log from the same run exists, collect it too and align by PID/timestamp before
asking for another run. It can identify conversion, cleanup, rotation or other
concurrent activity, but does not measure allocation ownership by itself. For a
future run requiring both kinds of logcat records, capture the application's
current PID without a tag filter, for example:

```sh
APP_PID=12345  # Replace with the PID of this run, before starting the workload.
adb logcat -v threadtime -T 1 --pid="$APP_PID" '*:V' | tee mosaic-with-business.log
```

Restarting the process changes the PID; update it and restart collection. Business
logs written to a separate file still need to be collected separately. RSS/HWM
in Mosaic records already include business memory even when business log lines
were filtered out.

Android writes directly to NDK liblog at INFO with tag `mosaic-memory`; it does
not automatically enter a separate business log file. Other platforms use
stderr. No device properties or process-wide logging configuration are changed.

`MOSAIC_MEMORY_DIAGNOSTICS` is read once when each writer is constructed:

| Value | Behavior |
|---|---|
| Unset or `1` | Both boundaries of every append, plus creation and every flush/close |
| Positive `N` | Both boundaries of the first and every Nth append; all creation/flush/close records |
| `0`, invalid, or non-Unicode | Disabled: no metadata walks, RSS reads, or log output |

Use the default for the diagnostic run. Logging is synchronous and has a cost;
keep an equivalent diagnostics-disabled run for precise performance comparisons.

## What the log can answer

| Investigation order | Evidence | Result/action supported |
|---|---|---|
| P0: memory before appending | `create_begin` → `create_end` → first `append_begin`, process RSS/HWM | Separate core writer construction from the interval before the first append. The input schema and caller-side FFI/Arrow setup can already exist at `create_begin`; this is not a pure business baseline. |
| P1: dictionary lifetime | Zero resident dictionary capacities at append/reset; `dictionary_flush` reports the largest prepared bucket | Verify that dictionary allocations are limited to one bucket rather than every bucket concurrently. Actual DICT column/byte shares still appear in `flush`. |
| P2: flush | `flush_begin` → `flush_encoded` → `flush_reset`, RSS/HWM and raw/compressed temporary counters | Locate the remaining peak interval. Bulk context sizes and synchronous output-callback RSS changes narrow the interval; allocator retention and concurrent work still require care. |
| P3: application input | Arrow buffers at entry and `append_begin` RSS | Measure the batch coexisting with writer caches; unexplained process memory is not automatically application memory. |
| P4: constant-column cache | `component=columns state=constant` and `all_null` | Rank these by measured bytes rather than historical field proportions. |

Compare these byte totals at the same stage near the peak. This is a prioritization
order, not a predetermined ranking of savings. If the measured components stay
small while process RSS grows, the log establishes that the current counters do
not explain the growth; it must not label all remaining RSS as business memory.

## Caller and binding boundaries

Replacing Mosaic alone automatically enables FFI and output-callback records. It
cannot observe application startup or conversion before a Mosaic call. When a
skip-write comparison already brackets the problem at `EnsureWriterConstructed`,
start with **one call**, immediately after it succeeds and before
`arrow::ExportRecordBatch`:

```cpp
mosaic_memory_checkpoint(4);  // caller_before_export; keep normal writing enabled.
```

Use the newly generated `mosaic.h` (also included by `mosaic.hpp`). Compare this
checkpoint with `ffi_append_begin` to bracket export/caller-wrapper allocations,
then follow the automatic FFI/core/output stages. These are process measurements,
not ownership attribution. The comparison must use unchanged data, duration and
upstream success handling; an early error return can change application behavior.

If the remaining pre-export baseline needs investigation, these **four optional
calls in application code** provide earlier boundaries:

```cpp
mosaic_memory_checkpoint(0);  // Startup, before collection/cache initialization.
// Existing application startup and data collection...
mosaic_memory_checkpoint(1);  // Immediately before Arrow conversion.
// Existing BucketToArrowConverter::Convert / BuildRecordBatchDirect...
mosaic_memory_checkpoint(2);  // After conversion, before Append.
// Existing Append, followed by normal destruction/release of this batch's temporaries...
mosaic_memory_checkpoint(3);  // After those temporaries leave scope.
```

Place stage 0 once and stages 1–3 in the normal per-batch path. These are insertion
points, not a request to change existing object lifetimes or force collection.
Stage 3 must be after caller-held Arrow batches/temporary owners leave scope; an
FFI array release alone does not free buffers still referenced by the caller.
Checkpoint stages map to `caller_startup`, `caller_before_convert`,
`caller_after_convert`, and `caller_after_release`; stage 4 is
`caller_before_export`. They report whole-process
RSS/HWM, not business-owned allocation totals. No business values/names/addresses
are logged. Each call reads the diagnostic setting; any positive value logs all
caller checkpoints (no per-writer sampling applies).

`component=boundary` records have `binding`, `boundary_sample`, `writer`, `append`,
RSS/HWM and a stage. `writer=0` before core construction means unknown. Join a
binding to core logs using its successful `ffi_open_end` record; do not confuse
`boundary_sample` with the core `sample` counter. Failed open keeps its binding ID.
FFI append counts are attempted calls, so rejected input can differ from core
append counts. First/every-N append sampling follows the writer setting.

| FFI stage | Observation |
|---|---|
| `ffi_open_begin` | Entry before foreign schema import; caller conversion/export may already have happened. |
| `ffi_schema_imported` / `ffi_schema_released` | Owned metadata imported, then foreign schema wrappers released after option strings are copied. |
| `ffi_open_end` | Core constructed, temporary imported schema dropped, handle ready. |
| `ffi_append_begin` | Before Arrow C Data import. |
| `ffi_input_imported` | Imported batch ready, consumed foreign schema already released. |
| `ffi_append_written` | Core append/automatic flush returned; imported batch still alive. |
| `ffi_input_released` | Imported batch and FFI array owner dropped; caller references may remain. |
| `ffi_close_begin` / `ffi_close_end` | Around core close. |
| `ffi_free_begin` / `ffi_free_end` | Around handle destruction, including implicit close if needed. |
| `ffi_open_error` / `ffi_append_error` / `ffi_close_error` | Failed call, including caught panic; not a success boundary. |

`component=output` records summarize **cumulative** write/flush callbacks per
binding, emitted at open, sampled append and close. Counters include calls,
successfully written bytes, failures, RSS observations, summed positive/negative
RSS changes, largest single-call increase, and HWM increases across callbacks.
Subtract consecutive counter values to examine one interval. `rss_observations=0`
means measurements unavailable, not zero memory cost. `hwm_observations` serves
the same purpose for HWM. Byte counts mean synchronous callback acceptance, not
flash persistence. Concurrent threads and allocator behavior can affect these
whole-process deltas; they locate a boundary but do not prove ownership.
Asynchronous allocations after callback return are outside this measurement.

`component=compression` records at `flush_encoded` report the number of measured
bulk contexts, their largest `ZSTD_sizeof_CCtx`, and maximum simultaneous known
raw/retained-slot + compressed + context capacity at compression completion.
These counters overlap existing flush capacities and must **not** be added to
them. They exclude allocator overhead, transient resizing, dictionary/plan memory,
streaming contexts, schema-footer compression and caller output buffers. The
compressor is released before output as before; context pooling is not enabled.

## Record identity and stages

Every core snapshot line carries `v=4`, `unix_ms`, `pid`, `writer`, `sample`, `stage`, `component`,
`append`, `total_rows`, `row_group`, and `row_group_rows`. Join components by
process-run + PID + writer + sample, not timestamp alone. Writers share process
RSS; never sum their RSS values. Sample numbers also expose missing records.

| Stage | Exact observation point |
|---|---|
| `create_begin` | Entry to the core writer constructor, before schema conversion/validation and bucket allocation. Buffer counters are zero because the writer does not exist yet. |
| `create_end` | Successful construction, before returning the empty writer. Same writer ID as `create_begin`. |
| `create_error` | Construction failed; no writer is returned. Buffer counters are unavailable/zero; process RSS remains measured. |
| `append_begin` | Schema/nullability validation succeeded; input is alive, before bucket/statistics mutation. `append` identifies the incoming call; row totals exclude it. |
| `append_buffered` | Bucket caches and statistics updated, before any automatic row-group flush. Row totals now include this batch. |
| `flush_begin` | Nonempty row group is about to encode/write; logged even for unsampled appends. |
| `flush_encoded` | All bucket output written, before cache reset. Includes a summary of capacities observed during this flush; those temporaries may already have been freed. |
| `flush_reset` | Bucket caches reset. Still labels the completed row group/row count; column states are `empty`, exposing retained capacity. |
| `close_begin` / `close_end` | Around final row-group flush, footer, and sink flush. Close can retain some cleared cache capacity until writer destruction. |
| `append_error` / `close_error` | Operation failed after entering mutation; do not treat preceding samples as a completed write/close. |

Validation failures do not advance append counters or emit records. Empty flushes
and repeated successful close calls do not emit flush/close records. Core snapshots do not instrument writer destruction; FFI boundaries do. Ordinary snapshots have six lines; append
snapshots have seven; `flush_encoded` has nine (including `dictionary_flush` and `compression`). Each line is below 2 KiB, including its
terminator; the diagnostic state retains no history in production.

## Counter definitions and limits

All byte counters use bytes (divide by 1,048,576 for MiB). Counts concern physical
columns, including nested children/length columns; they need not equal the number
of top-level business signals.

- **`buffers`**: integer logical bytes are the schema-width equivalent of buffered
  non-null values, not a measured old-build allocation. Payload bytes are used
  compact block bytes. Capacity is reserved block storage; metadata includes the
  boxed integer buffer and allocated block descriptors. Other value capacity and
  null bitmap capacity are separate. `width1/2/4/8_values` verify compaction.
- **`columns`**: partitions the same buffer totals by `all_null`, `constant`
  (all non-null values equal so far), `varying`, or `empty`. Payload excludes
  bitmaps/metadata. Capacity fields expose allocation slack and retained empty
  buffers. These are subdivisions of `buffers`, not additional memory. A column
  can change from constant to varying within a row group; no value scan is added.
- **`dictionary`**: resident tables in writer caches. With deferred construction,
  entries/capacities/key bytes are zero at these snapshots; `pending_columns`
  counts columns whose dictionaries are not prepared. It does **not** mean that
  the output uses PLAIN or that dictionary encoding is disabled.
- **`dictionary_flush`**: `prepared_buckets` counts buckets prepared during this
  flush; `peak_bucket_lower_bound_bytes` is the maximum per-bucket sum of table
  payload capacity and allocated variable key bytes. Entry/capacity/state fields
  all describe that same bucket, not the sum over buckets or independent maxima.
  Table payload is HashMap capacity × key/index pair size, excluding extra hash
  buckets/control bytes, allocator overhead and rounding. Counts exclude tables
  discarded after exceeding the dictionary limit, temporary reallocation copies,
  view metadata and ordering vectors. This is a lower bound at the prepared
  boundary, **not** a complete transient heap peak. Tables have already been
  released when the flush summary is logged. Compare this against the old
  all-bucket resident dictionary count, without equating the difference to RSS.
- **`input`**: recursively sums the lengths of buffers exposed by the current
  RecordBatch and separately sums Arrow-reported capacities. Shared buffers may
  be counted more than once; sliced arrays can retain larger backing allocations
  (especially variable-length values). This is not unique allocation size or all
  application memory. Integer (`Int16/32/64`) and Float64 subtotals classify
  top-level columns only. Metadata conversion can allocate small descriptors;
  data values are never copied by these measurements. The input remains alive
  at both append boundaries and through an automatic flush.
- **`flush`**: actual successful bulk, stream, or uncompressed unit counts;
  encoding counts per physical column; logical non-null value bytes selected for
  DICT vs all encodings. `dict_logical_value_bytes / logical_value_bytes` is a
  logical-byte-weighted share, not a memory/disk compression ratio (undefined
  when the denominator is zero). `stream_units > 0` directly confirms streaming;
  `integer_buckets > 0` confirms the sink encoder even when units fit its 1 MiB
  bulk staging limit. `legacy_buckets` identifies materializing buckets.
- **Flush capacities**: maximum observed raw encoding/page buffers and compressed
  output/retained slots. `tracked_temporary_peak_bytes` is the maximum simultaneous
  sum at the instrumented allocation boundaries, not the sum of the two separate
  maxima. Includes the 16 KiB uncompressed output buffer when used. Excludes Zstd
  context/window memory, stack scratch, encoding plans/dictionary ordering
  vectors, transient reallocation copies, and application-owned output buffers.
  It is not the complete native heap or a process peak measurement.
- **RSS/HWM**: `/proc/self/status` VmRSS/VmHWM on Linux/Android. These describe the
  whole process, including concurrent work and allocator-retained pages. Missing
  fields/platform support produce `na`, not zero. HWM is cumulative for the
  process lifetime; it cannot locate a later local peak below an earlier one.

Do not sum input capacities, dictionary lower bounds, cache totals, temporary
maxima, and RSS as if they were an exhaustive, simultaneous allocation census.
The logs support attribution to measured components and time intervals; they
cannot guarantee an exact call-stack cause for every unexplained process peak.

## Cost and validation

Enabled append sampling performs O(physical columns + integer blocks + input
buffer descriptors) metadata work twice, with one input metadata traversal, and
reads `/proc/self/status` once per snapshot. FFI adds lightweight RSS-only
boundaries, plus **two RSS/HWM reads per synchronous output callback** and one
summary line per sampled API call. This cost depends on callback count; disabling
diagnostics bypasses these reads and summaries. It does not scan column values or
retain a second batch/dictionary. Creation adds two snapshots (or an error snapshot). Flush maintains fixed-size counters and makes
one extra encoding selection for legacy buckets (variable dictionary key-length
scans can occur there). Formatting uses bounded stack buffers; log writes are
synchronous. Costs depend on schema width, append frequency and the log sink.
Disabled mode retains an optional pointer and conditional checks; there are no
sampling allocations or log operations. It is not claimed to have zero cost.

Tests cover stage ordering, sampling intervals, invalid input, failure/retry and
idempotent close behavior, constant/all-null/varying/reset classification,
per-bucket dictionary lifetime, shared/sliced/foreign/nested Arrow input, bounded Android
log strings, and byte-for-byte files with diagnostics on vs off across integer
bulk/stream and legacy paged/monolithic compression paths.

## This revision's measured cost

On ARM64 Linux, a synthetic FFI workload with 8,779 columns, 150 rows per
append, 20 appends per file and two file rotations was run five times per
configuration. With diagnostics off, the median process maximum RSS changed
from 69,240 to 67,696 KiB after early schema-wrapper release (about 1.51 MiB).
Total writing time was 3.309 versus 3.300 s. This is not a customer saving estimate.

For this revision, enabling diagnostics changed the wide case from 3.300 to
3.374 s (about 2.23%, or 1.84 ms per append amortizing constructor/close too).
A 200-column paged case with many callbacks changed from 83.1 to 108.4 ms
(30.4%, 25.2 ms absolute). Callback sampling is diagnostic work and can be
material for callback-heavy workloads; disable it for production performance
measurements. All eight tested workload variants produced identical files with
old/new code and diagnostics on/off where tested. These measurements use Linux
stderr files, not Android logcat. No full Android device validation is claimed.
