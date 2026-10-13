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

This diagnostic patch builds on main's integer buffer/encoding optimization and
lazy dictionaries. It changes observation only: schema, encoding choices, file
bytes, row-group thresholds, input ownership, and the public C/JNI/Rust APIs stay
the same. No application logger registration or memory profiler is required.

## Collect one useful run

1. Build and deploy main plus this PR through the application's usual build. Do
   not stack the old optimization or diagnostic PRs. Diagnostics are **on by
   default**; remove any existing `MOSAIC_MEMORY_DIAGNOSTICS=0` override.
2. Before starting the normal workload, collect Android INFO records:
   ```sh
   adb logcat -v threadtime -T 1 'mosaic-memory:I' '*:S' | tee mosaic-memory.log
   ```
   Check that NEW lines contain `[mosaic-memory] v=2`, `stage=append_begin`, and
   the application's PID. Device log filtering still applies. On Linux, capture
   the application's stderr instead, for example `./app 2>mosaic-memory.log`.
3. Keep the same data, 30-second append interval, large row groups, and normal
   memory reclamation. Collect at least two complete file-writing cycles (about
   20 minutes for a 10-minute rotation). Include flush and close records.
4. Return the complete log, the build commit, and the existing peak-memory
   monitor's result/unit and approximate peak time. No new heap profile or
   application counters are needed. Start with a fresh application process when
   practical so the process high-water mark covers this run.

Android writes directly to NDK liblog at INFO with tag `mosaic-memory`; it does
not automatically enter a separate business log file. Other platforms use
stderr. No device properties or process-wide logging configuration are changed.

`MOSAIC_MEMORY_DIAGNOSTICS` is read once when each writer is constructed:

| Value | Behavior |
|---|---|
| Unset or `1` | Both boundaries of every append, plus every flush/close |
| Positive `N` | Both boundaries of the first and every Nth append; all flush/close records |
| `0`, invalid, or non-Unicode | Disabled: no metadata walks, RSS reads, or log output |

Use the default for the diagnostic run. Logging is synchronous and has a cost;
keep an equivalent diagnostics-disabled run for precise performance comparisons.

## What the log can answer

| Investigation order | Evidence | Result/action supported |
|---|---|---|
| P1: constant-column cache | `component=columns state=constant`, value payload/capacity split between compact integers and other types | Rank repeated-value caching by bytes actually retained; high constant field counts alone do not prove large savings. `all_null` isolates bitmap/metadata overhead. |
| P2: dictionaries | Table capacity lower bound, allocated variable key bytes, inactive retained tables; actual DICT column and logical-byte counts at flush | Separate dictionary-building/retention costs from the fraction actually encoded as DICT. Consider retention or tracking changes only if measured costs warrant them. |
| P3: application input | Arrow buffers at entry; `append_begin` vs previous boundary vs `append_buffered` RSS/HWM | Check whether a large input coexists with the writer cache and whether growth occurs before entry or during append. A pre-entry rise is a clue, not proof of which application allocation caused it. |
| P4: flush | `flush_begin` → `flush_encoded` → `flush_reset`, RSS/HWM, actual bulk/stream unit counts, tracked temporary capacities | Confirm the optimized path used; distinguish raw staging, retained compressed slots, and cache retention after reset. A new HWM inside this interval narrows the time window. |

Compare these byte totals at the same stage near the peak. This is a prioritization
order, not a predetermined ranking of savings. If the measured components stay
small while process RSS grows, the log establishes that the current counters do
not explain the growth; it must not label all remaining RSS as business memory.

## Record identity and stages

Every line carries `v=2`, `unix_ms`, `pid`, `writer`, `sample`, `stage`, `component`,
`append`, `total_rows`, `row_group`, and `row_group_rows`. Join components by
process-run + PID + writer + sample, not timestamp alone. Writers share process
RSS; never sum their RSS values. Sample numbers also expose missing records.

| Stage | Exact observation point |
|---|---|
| `append_begin` | Schema/nullability validation succeeded; input is alive, before bucket/statistics mutation. `append` identifies the incoming call; row totals exclude it. |
| `append_buffered` | Bucket caches and statistics updated, before any automatic row-group flush. Row totals now include this batch. |
| `flush_begin` | Nonempty row group is about to encode/write; logged even for unsampled appends. |
| `flush_encoded` | All bucket output written, before cache reset. Includes a summary of capacities observed during this flush; those temporaries may already have been freed. |
| `flush_reset` | Bucket caches reset. Still labels the completed row group/row count; column states are `empty`, exposing retained capacity. |
| `close_begin` / `close_end` | Around final row-group flush, footer, and sink flush. Close can retain some cleared cache capacity until writer destruction. |
| `append_error` / `close_error` | Operation failed after entering mutation; do not treat preceding samples as a completed write/close. |

Validation failures do not advance append counters or emit records. Empty flushes
and repeated successful close calls do not emit flush/close records. Logs do not
instrument writer destruction. Ordinary snapshots have six lines; append
snapshots and `flush_encoded` have seven. Each line is below 2 KiB, including its
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
- **`dictionary`**: fixed/variable entries and capacities come from existing maps.
  `table_payload_capacity_lower_bound_bytes` is capacity × size of a key/index
  pair. It excludes extra hash buckets/control bytes, allocator overhead, and
  allocation-size rounding, so it is explicitly a lower bound, not exact heap
  usage. `key_bytes` adds separately allocated variable-length key contents.
  `inactive_table_payload_capacity_lower_bound_bytes` is a subset of table bytes
  retained outside active tracking. Pending/active/disabled are tracking states,
  not on-disk encodings. Append snapshots do not iterate keys.
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
reads `/proc/self/status` once per snapshot. It does not scan column values or
retain a second batch/dictionary. Flush maintains fixed-size counters and makes
one extra encoding selection for legacy buckets (variable dictionary key-length
scans can occur there). Formatting uses bounded stack buffers; log writes are
synchronous. Costs depend on schema width, append frequency and the log sink.
Disabled mode retains an optional pointer and conditional checks; there are no
sampling allocations or log operations. It is not claimed to have zero cost.

Tests cover stage ordering, sampling intervals, invalid input, failure/retry and
idempotent close behavior, constant/all-null/varying/reset classification,
dictionary retention, shared/sliced/foreign/nested Arrow input, bounded Android
log strings, and byte-for-byte files with diagnostics on vs off across integer
bulk/stream and legacy paged/monolithic compression paths.
