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

Writer memory diagnostics are enabled by default: every append emits one line.
On Android, it goes directly to **logcat at INFO level with tag
`mosaic-memory`**, through the NDK's `liblog`. No logger initialization, stderr
redirection, C/JNI callback, or business-code change is needed. Other platforms
write to standard error.

## Android customer collection

1. Rebuild and integrate this branch using the application's usual process.
   Keep input data, append intervals, row-group sizes, close intervals and
   allocator reclamation unchanged.
2. On the computer connected to the device, start collection before restarting
   the business process and running the workload:

   ```sh
   adb logcat -v threadtime -T 1 'mosaic-memory:I' '*:S' | tee mosaic-memory.log
   ```

   After the first append, verify a **new** `[mosaic-memory]` line with the
   business process's PID appears on screen. This verifies collection before
   investing in a full run. Keep collection running through at least one normal
   ten-minute write/close cycle, then press Ctrl-C. Do not clear the device's
   existing logs. With multiple connected devices, add `adb -s SERIAL`.
3. Return `mosaic-memory.log` and the overall process peak from the same existing
   monitoring tool used for the existing old/new comparison. No heap profile is needed.

The command reads the Android main log buffer; it does not depend on the business
application's logger. As with other Android INFO logs, device log filtering and
access must allow collection. If no new record appears after a confirmed append,
check the loaded build and diagnostic override, then the device's tag filtering
and logcat access **before continuing the run**. This library does not modify
process-wide loggers, device properties or log priorities. The API is described
in the [Android NDK logging reference](https://developer.android.com/ndk/reference/group/logging).

## Other platforms

Capture standard error when launching the process:

```sh
./your-collector 2>mosaic-memory.log
```

Run the usual workload and return `mosaic-memory.log` together with the process
peak reported by the existing monitoring tool. No changes to schemas, batch
sizes, row-group sizes, collection intervals or allocator reclamation are needed.
After the first append, check for a line starting with `[mosaic-memory]` to
confirm collection. No environment variable is needed.

## Optional sampling override

The optional `MOSAIC_MEMORY_DIAGNOSTICS` environment variable is read once per
writer construction. Unset means every append (`1`); `0` or invalid values
disable diagnostics. A positive integer N samples the first append and then
every Nth append of each writer. For example, `MOSAIC_MEMORY_DIAGNOSTICS=100`
reduces log volume for workloads with frequent writes, and
`MOSAIC_MEMORY_DIAGNOSTICS=0` explicitly disables diagnostics. Apply overrides
in the process's launch environment before creating writers; exporting a
variable in an unrelated shell does not change an already running service.
All bindings use the same Rust implementation; no C struct layout or function
signature changes are required.
This switch controls diagnostics only, not integer cache compaction.

## Evaluating the optimization

Use two separate measurements:

| Question | Evidence |
| --- | --- |
| How much did the customer's overall peak improve? | Compare the old and optimized builds with the same input/configuration, process restart policy and existing peak monitor. Report both peaks and `(old_peak - new_peak) / old_peak`. For repeatability, use three matching runs per build and retain the individual peaks. |
| Is integer compaction working, and on how much data? | For the same writer and append position, compare `integer_logical_bytes` with `integer_payload_bytes`; check `integer_capacity_bytes + integer_metadata_bytes` for the explicitly counted integer storage. |

If the logical and compacted payloads are close, the data in that sample has
little compaction benefit; the width counters show the resulting storage widths.
If payload reduction is large but the overall peak reduction is small, the
integer optimization is working but does not explain the process peak by itself.
Use the log's timestamps and row counts to choose the next investigation; do not
assign unmeasured bytes to the business application by subtraction.

An additional run with diagnostics enabled explains the earlier comparison;
it is not a replacement for the original old/new peak measurement. When making
a new precise performance comparison, disable diagnostics on the optimized
build with `MOSAIC_MEMORY_DIAGNOSTICS=0` to avoid charging it for logging only.

## Reading the log

Every line starts with `[mosaic-memory] v=1` and contains space-separated
`key=value` fields. All byte counts are bytes, not KiB or MB. Each line identifies
the process, writer, append sequence, cumulative top-level row count, and current
row group. `row_group` is zero-based; nested element counts are included in the
buffer measurements rather than in `total_rows`.

Sampling happens **after the input has entered the writer cache and before an
automatic row-group flush**. The input Arrow batch is still alive. The stage is
named `append_buffered`, not `append_succeeded`: a subsequent automatic flush
can still fail. `total_rows` includes unsampled appends; rejected input batches
do not advance the counters. Empty valid appends do advance the append sequence.

| Field | Meaning |
| --- | --- |
| `unix_ms` | Timestamp in milliseconds since the Unix epoch, for alignment with the existing memory trace. |
| `integer_logical_bytes` | Non-null Int16/Int32/Int64 values multiplied by their schema widths, including integer children and generated length columns. This is the uncompacted **payload equivalent**, not an actual measurement of an older binary's allocations. |
| `integer_payload_bytes` | Bytes currently used by those integer blocks after compaction. |
| `integer_capacity_bytes` | Sum of the integer block byte vectors' capacities, including unused reserved space. |
| `integer_metadata_bytes` | Allocated block descriptor capacity plus the boxed integer buffer objects. |
| `width1_values`, `width2_values`, `width4_values`, `width8_values` | Non-null values in blocks currently using each storage width; a late outlier can promote an entire current block. |
| `other_value_capacity_bytes` | Capacity of the other column value buffers, including Int8, variable-width values and other physical types. |
| `null_capacity_bytes` | Capacity of the null bitmaps. |
| `dictionary_entries` | Number of currently tracked dictionary entries; **not** dictionary allocation bytes. |
| `rss_bytes` | Whole-process VmRSS from `/proc/self/status` on Linux/Android. |
| `rss_hwm_bytes` | Whole-process VmHWM, the RSS high-water mark since process start; it may precede this writer/sample. |

Use `integer_logical_bytes - integer_payload_bytes` to quantify the integer
payload reduction. Use `integer_capacity_bytes + integer_metadata_bytes` for
the explicitly counted integer buffer storage. Compare samples with matching
row counts and row-group positions. An RSS difference is not a direct measure
of either quantity.

These fields do **not** form an exhaustive writer/heap allocation census:
dictionary allocations, other structural vectors, statistics, allocator
metadata and fragmentation, Arrow input, application caches, compression state,
compressed output and file mappings are not all measured by the capacity fields.
In particular, `rss_bytes` minus the counted buffers must not be labelled
"application memory". RSS fields are kernel snapshots, not exact heap totals.
Unavailable RSS fields are `na`, never a misleading zero. Non-Linux/Android
platforms still emit buffer statistics with `na` for RSS.

This is append-boundary sampling. A transient peak during Arrow conversion,
buffer reallocation, encoding or another thread can occur between samples.
Continue to use the customer's existing process peak monitor. No user values,
column names or file paths are logged.

## Cost

When disabled, each writer has one optional pointer-sized field and each append
checks it once. There are no diagnostic counter updates, metadata walks, RSS
reads or log writes in this path. The environment is checked only at construction.

When enabled, each writer owns four u64 counters/settings (32 bytes on ARM64,
excluding allocator overhead). A sampled append walks column/block metadata,
without scanning values or dictionary entries, reads up to 4 KiB of proc status,
formats a line in 2 KiB of stack storage (including a reserved NUL terminator),
and writes it once to Android liblog or stderr. It keeps
no sample history and starts no thread. This is O(columns + integer blocks) per
sample, not a constant-time operation; choose a larger sampling interval if
necessary. Mosaic retains no log history; Android's system log buffers and the
collection file are managed separately. A slow logging destination can delay
the append; log errors
are ignored and never change writer success/failure or encoded bytes.

For the existing synthetic example, compare separate processes with the variable
set to `0`, unset (the default, equivalent to `1`), and optionally set to a larger
interval. Capture stderr to a regular file, use the same data and binary,
alternate run order, and compare
append timing, CPU time, peak RSS and output hashes. Figures from this experiment
describe the test workload, not a guaranteed overhead on an Android application.

### ARM64 Linux measurement

Measured at revision `40b03fb`, before diagnostics became enabled by default,
with the environment variable unset (off at that revision) or set to `1`.
For current Linux builds, use `0` to reproduce the off path; the sampled
work is unchanged. These measurements do not measure Android liblog overhead. Regular-file stderr, three integer columns, Zstd level 1, dictionary
disabled, one large row group. Every case contains 5,000,000 rows. Medians of
seven repetitions for the first case and five for the others:

| Workload | Append total, off / on (ms) | Process CPU, off / on (s) | Peak RSS, off / on (KiB) |
| --- | ---: | ---: | ---: |
| 20 x 250,000 rows, widened unsigned | 14.362 / 14.909 | 0.25 / 0.26 | 81,844 / 81,840 |
| 20 x 250,000 rows, full-range signed | 23.969 / 25.782 | 0.12 / 0.12 | 146,060 / 146,056 |
| 2,000 x 2,500 rows, widened unsigned | 19.146 / 40.343 | 0.27 / 0.29 | 90,192 / 90,180 |

Process CPU includes input construction and close; `/usr/bin/time` reports it
with 0.01-second precision here. Small RSS differences are measurement variation,
not evidence that diagnostics save memory. In the first case, 20 samples added
about 0.55 ms in total (27 microseconds per append). Per-append logging more than
doubled append time in the frequent-small-batch case; sampling every 100 appends
reduced it to 20.559 ms, producing 21 lines instead of 2,000.

The 68-run matrix also included the parent PR's binary without diagnostics and
sampled logging. Disabled-diagnostics append medians differed from the parent
by -0.9% to +1.5%, and peak RSS by at most 8 KiB. All output files were
byte-identical within each workload. These are synthetic tests, not a vehicle's
measured overhead; synchronous log destination performance can change the cost.
