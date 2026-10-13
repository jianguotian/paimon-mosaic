// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Bounded, default-on sampling of writer buffer metadata and process RSS.
//! Buffer capacities are requested storage, not allocator usable size or RSS.

use std::fmt::{self, Write as _};
use std::io;
#[cfg(any(not(target_os = "android"), test))]
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const ENV: &str = "MOSAIC_MEMORY_DIAGNOSTICS";
static NEXT_WRITER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BufferMemory {
    pub integer_logical_bytes: usize,
    pub integer_payload_bytes: usize,
    pub integer_capacity_bytes: usize,
    pub integer_metadata_bytes: usize,
    pub integer_width_values: [usize; 4],
    pub other_value_capacity_bytes: usize,
    pub null_capacity_bytes: usize,
    // Entry count is intentionally not presented as dictionary allocation size.
    pub dictionary_entries: usize,
    // ALL_NULL, CONST-so-far, varying, and empty/reset physical columns.
    pub columns: [ColumnMemory; 4],
    pub dictionary: DictionaryMemory,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ColumnMemory {
    pub columns: usize,
    pub payload_bytes: usize,
    pub integer_capacity_bytes: usize,
    pub other_capacity_bytes: usize,
    pub integer_metadata_bytes: usize,
    pub null_capacity_bytes: usize,
}

impl ColumnMemory {
    pub fn add(&mut self, other: Self) {
        self.columns += other.columns;
        self.payload_bytes += other.payload_bytes;
        self.integer_capacity_bytes += other.integer_capacity_bytes;
        self.other_capacity_bytes += other.other_capacity_bytes;
        self.integer_metadata_bytes += other.integer_metadata_bytes;
        self.null_capacity_bytes += other.null_capacity_bytes;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DictionaryMemory {
    pub fixed_entries: usize,
    pub fixed_capacity: usize,
    pub variable_entries: usize,
    pub variable_capacity: usize,
    pub key_bytes: usize,
    pub table_payload_capacity_lower_bound: usize,
    pub inactive_table_payload_capacity_lower_bound: usize,
    pub pending_columns: usize,
    pub active_columns: usize,
    pub disabled_columns: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InputMemory {
    pub rows: usize,
    pub columns: usize,
    pub buffer_bytes_sum: usize,
    pub reported_capacity_bytes_sum: usize,
    pub integer_buffer_bytes_sum: usize,
    pub float64_buffer_bytes_sum: usize,
}

impl InputMemory {
    pub fn from_batch(batch: &arrow_array::RecordBatch) -> Self {
        use arrow_schema::DataType;
        let mut result = Self {
            rows: batch.num_rows(),
            columns: batch.num_columns(),
            ..Self::default()
        };
        for array in batch.columns() {
            // Walk buffer metadata, not values. Length and reported capacity
            // have different meanings for sliced/shared buffers. No values are copied.
            let bytes = visible_buffer_bytes(&array.to_data());
            result.buffer_bytes_sum += bytes;
            result.reported_capacity_bytes_sum += array.get_buffer_memory_size();
            match array.data_type() {
                DataType::Int16 | DataType::Int32 | DataType::Int64 => {
                    result.integer_buffer_bytes_sum += bytes
                }
                DataType::Float64 => result.float64_buffer_bytes_sum += bytes,
                _ => {}
            }
        }
        result
    }
}

fn visible_buffer_bytes(data: &arrow_data::ArrayData) -> usize {
    data.buffers().iter().map(|b| b.len()).sum::<usize>()
        + data.nulls().map_or(0, |n| n.buffer().len())
        + data
            .child_data()
            .iter()
            .map(visible_buffer_bytes)
            .sum::<usize>()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FlushMemory {
    pub integer_buckets: usize,
    pub legacy_buckets: usize,
    pub bulk_units: usize,
    pub stream_units: usize,
    pub uncompressed_units: usize,
    pub max_unit_bytes: usize,
    pub encoding_columns: [usize; 4],
    pub logical_value_bytes: usize,
    pub dict_logical_value_bytes: usize,
    pub encoding_capacity_peak_bytes: usize,
    pub compressed_capacity_peak_bytes: usize,
    pub tracked_temporary_peak_bytes: usize,
}

impl FlushMemory {
    pub fn observe(&mut self, encoding: usize, compressed: usize) {
        self.encoding_capacity_peak_bytes = self.encoding_capacity_peak_bytes.max(encoding);
        self.compressed_capacity_peak_bytes = self.compressed_capacity_peak_bytes.max(compressed);
        self.tracked_temporary_peak_bytes = self
            .tracked_temporary_peak_bytes
            .max(encoding.saturating_add(compressed));
    }
}

pub(crate) struct MemoryDiagnostics {
    writer_id: u64,
    every: u64,
    appends: u64,
    total_rows: u64,
    samples: u64,
    #[cfg(test)]
    pub last_sample: Option<Sample>,
    #[cfg(test)]
    pub events: Vec<Sample>,
}

impl MemoryDiagnostics {
    pub fn from_env() -> Option<Box<Self>> {
        let every = match std::env::var(ENV) {
            Ok(value) => parse_interval(Some(&value)),
            Err(std::env::VarError::NotPresent) => parse_interval(None),
            Err(std::env::VarError::NotUnicode(_)) => None,
        }?;
        Some(Box::new(Self::new(every)))
    }

    fn new(every: u64) -> Self {
        Self {
            writer_id: NEXT_WRITER_ID.fetch_add(1, Ordering::Relaxed),
            every,
            appends: 0,
            total_rows: 0,
            samples: 0,
            #[cfg(test)]
            last_sample: None,
            #[cfg(test)]
            events: Vec::new(),
        }
    }

    /// Always sample the first append, then every Nth append. No value scan.
    pub fn begin_append(&mut self) -> bool {
        self.appends = self.appends.saturating_add(1);
        self.appends == 1 || self.appends.is_multiple_of(self.every)
    }

    pub fn on_buffered(&mut self, rows: usize) {
        self.total_rows = self.total_rows.saturating_add(rows as u64);
    }

    #[cold]
    #[inline(never)]
    pub fn emit(
        &mut self,
        stage: &'static str,
        row_group: usize,
        row_group_rows: usize,
        buffers: BufferMemory,
        input: Option<InputMemory>,
        flush: Option<FlushMemory>,
    ) {
        self.samples = self.samples.saturating_add(1);
        let sample = Sample {
            sequence: self.samples,
            stage,
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            pid: std::process::id(),
            writer_id: self.writer_id,
            append: self.appends,
            total_rows: self.total_rows,
            row_group,
            row_group_rows,
            buffers,
            input,
            flush,
            rss: ProcessRss::read(),
        };
        #[cfg(test)]
        {
            if stage == "append_buffered" {
                self.last_sample = Some(sample);
            }
            self.events.push(sample);
        }
        // A logging failure must never fail a data write.
        let _ = sample.emit();
    }

    #[cfg(test)]
    pub fn for_test(every: u64) -> Box<Self> {
        assert!(every > 0);
        Box::new(Self::new(every))
    }
}

fn parse_interval(value: Option<&str>) -> Option<u64> {
    value.unwrap_or("1").parse::<u64>().ok().filter(|&n| n > 0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProcessRss {
    current: Option<u64>,
    high_watermark: Option<u64>,
}

impl ProcessRss {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn read() -> Self {
        use std::io::Read;

        let Ok(mut file) = std::fs::File::open("/proc/self/status") else {
            return Self::default();
        };
        // Bounded stack storage; unavailable/truncated fields remain "na".
        let mut buffer = [0u8; 4096];
        let mut used = 0;
        while used < buffer.len() {
            match file.read(&mut buffer[used..]) {
                Ok(0) => break,
                Ok(n) => used += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Self::default(),
            }
        }
        Self::parse(&buffer[..used])
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn read() -> Self {
        Self::default()
    }

    #[cfg(any(target_os = "linux", target_os = "android", test))]
    fn parse(status: &[u8]) -> Self {
        let field = |name: &[u8]| -> Option<u64> {
            let line = status
                .split(|&b| b == b'\n')
                .find_map(|line| line.strip_prefix(name))?;
            let mut words = std::str::from_utf8(line).ok()?.split_whitespace();
            let value = words.next()?.parse::<u64>().ok()?;
            if words.next()? != "kB" {
                return None;
            }
            value.checked_mul(1024)
        };
        Self {
            current: field(b"VmRSS:"),
            high_watermark: field(b"VmHWM:"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub sequence: u64,
    pub stage: &'static str,
    unix_ms: u128,
    pid: u32,
    writer_id: u64,
    pub append: u64,
    pub total_rows: u64,
    pub row_group: usize,
    pub row_group_rows: usize,
    pub buffers: BufferMemory,
    pub input: Option<InputMemory>,
    pub flush: Option<FlushMemory>,
    rss: ProcessRss,
}

impl Sample {
    fn emit(&self) -> io::Result<()> {
        #[cfg(target_os = "android")]
        {
            self.write_records(|line| line.write_android())
        }
        #[cfg(not(target_os = "android"))]
        {
            self.write_to(&mut io::stderr().lock())
        }
    }

    #[cfg(any(not(target_os = "android"), test))]
    fn write_to(&self, output: &mut impl Write) -> io::Result<()> {
        self.write_records(|line| output.write_all(&line.bytes[..line.len]))
    }

    fn write_records(&self, mut write: impl FnMut(&LogLine) -> io::Result<()>) -> io::Result<()> {
        write(&self.format_line()?)?;
        for (state, c) in ["all_null", "constant", "varying", "empty"]
            .into_iter()
            .zip(self.buffers.columns)
        {
            let mut line = self.prefix("columns")?;
            writeln!(line, " state={} columns={} payload_bytes={} integer_capacity_bytes={} other_capacity_bytes={} integer_metadata_bytes={} null_capacity_bytes={}", state, c.columns, c.payload_bytes, c.integer_capacity_bytes, c.other_capacity_bytes, c.integer_metadata_bytes, c.null_capacity_bytes).map_err(log_overflow)?;
            write(&line)?;
        }
        let d = self.buffers.dictionary;
        let mut line = self.prefix("dictionary")?;
        writeln!(line, " fixed_entries={} fixed_capacity={} variable_entries={} variable_capacity={} key_bytes={} table_payload_capacity_lower_bound_bytes={} inactive_table_payload_capacity_lower_bound_bytes={} pending_columns={} active_columns={} disabled_columns={}", d.fixed_entries, d.fixed_capacity, d.variable_entries, d.variable_capacity, d.key_bytes, d.table_payload_capacity_lower_bound, d.inactive_table_payload_capacity_lower_bound, d.pending_columns, d.active_columns, d.disabled_columns).map_err(log_overflow)?;
        write(&line)?;
        if let Some(i) = self.input {
            let mut line = self.prefix("input")?;
            writeln!(line, " rows={} columns={} arrow_visible_buffer_bytes_sum={} arrow_reported_capacity_bytes_sum={} integer_arrow_visible_buffer_bytes_sum={} float64_arrow_visible_buffer_bytes_sum={}", i.rows, i.columns, i.buffer_bytes_sum, i.reported_capacity_bytes_sum, i.integer_buffer_bytes_sum, i.float64_buffer_bytes_sum).map_err(log_overflow)?;
            write(&line)?;
        }
        if let Some(f) = self.flush {
            use crate::spec::*;
            let mut line = self.prefix("flush")?;
            writeln!(line, " integer_buckets={} legacy_buckets={} bulk_units={} stream_units={} uncompressed_units={} max_unit_bytes={} all_null_columns={} constant_columns={} dict_columns={} plain_columns={} logical_value_bytes={} dict_logical_value_bytes={} encoding_capacity_peak_bytes={} compressed_capacity_peak_bytes={} tracked_temporary_peak_bytes={}", f.integer_buckets, f.legacy_buckets, f.bulk_units, f.stream_units, f.uncompressed_units, f.max_unit_bytes, f.encoding_columns[ENCODING_ALL_NULL as usize], f.encoding_columns[ENCODING_CONST as usize], f.encoding_columns[ENCODING_DICT as usize], f.encoding_columns[ENCODING_PLAIN as usize], f.logical_value_bytes, f.dict_logical_value_bytes, f.encoding_capacity_peak_bytes, f.compressed_capacity_peak_bytes, f.tracked_temporary_peak_bytes).map_err(log_overflow)?;
            write(&line)?;
        }
        Ok(())
    }

    fn prefix(&self, component: &str) -> io::Result<LogLine> {
        let mut line = LogLine {
            bytes: [0; 2048],
            len: 0,
        };
        write!(line, "[mosaic-memory] v=2 unix_ms={} pid={} writer={} sample={} stage={} component={} append={} total_rows={} row_group={} row_group_rows={}", self.unix_ms, self.pid, self.writer_id, self.sequence, self.stage, component, self.append, self.total_rows, self.row_group, self.row_group_rows).map_err(log_overflow)?;
        Ok(line)
    }

    fn format_line(&self) -> io::Result<LogLine> {
        let mut line = self.prefix("buffers")?;
        let b = self.buffers;
        // One bounded line; no retained log history.
        writeln!(
            line,
            " integer_logical_bytes={} integer_payload_bytes={} integer_capacity_bytes={} integer_metadata_bytes={} width1_values={} width2_values={} width4_values={} width8_values={} other_value_capacity_bytes={} null_capacity_bytes={} dictionary_entries={} rss_bytes={} rss_hwm_bytes={}",
            b.integer_logical_bytes,
            b.integer_payload_bytes, b.integer_capacity_bytes, b.integer_metadata_bytes,
            b.integer_width_values[0], b.integer_width_values[1],
            b.integer_width_values[2], b.integer_width_values[3],
            b.other_value_capacity_bytes, b.null_capacity_bytes, b.dictionary_entries,
            OptionalBytes(self.rss.current), OptionalBytes(self.rss.high_watermark),
        ).map_err(|_| io::Error::other("memory diagnostic line too long"))?;
        Ok(line)
    }
}

fn log_overflow(_: fmt::Error) -> io::Error {
    io::Error::other("memory diagnostic line too long")
}

struct OptionalBytes(Option<u64>);
impl fmt::Display for OptionalBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(value) => value.fmt(f),
            None => f.write_str("na"),
        }
    }
}

struct LogLine {
    bytes: [u8; 2048],
    len: usize,
}
impl LogLine {
    #[cfg(any(target_os = "android", test))]
    fn as_c_str(&self) -> io::Result<&std::ffi::CStr> {
        std::ffi::CStr::from_bytes_with_nul(&self.bytes[..=self.len]).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid diagnostic log string")
        })
    }

    #[cfg(target_os = "android")]
    fn write_android(&self) -> io::Result<()> {
        use std::ffi::{c_char, c_int};

        #[link(name = "log")]
        unsafe extern "C" {
            fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
        }

        const ANDROID_LOG_INFO: c_int = 4;
        let message = self.as_c_str()?;
        // SAFETY: Both strings are NUL-terminated and remain alive for the call.
        // liblog copies the message. Do not install a process-wide logger or
        // change the application's log filtering; collection uses this tag.
        unsafe {
            __android_log_write(
                ANDROID_LOG_INFO,
                c"mosaic-memory".as_ptr(),
                message.as_ptr(),
            );
        }
        Ok(())
    }
}

impl fmt::Write for LogLine {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        // Keep the final byte zero for Android's C logging API.
        let capacity = self.bytes.len() - 1;
        let destination = self.bytes[..capacity]
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?;
        destination.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_defaults_to_every_append_and_allows_overrides() {
        let mut default = MemoryDiagnostics::new(parse_interval(None).unwrap());
        for _ in 0..3 {
            assert!(default.begin_append());
            default.on_buffered(10);
        }
        for value in ["", "0", "-1", "true", "999999999999999999999999999999"] {
            assert_eq!(parse_interval(Some(value)), None);
        }
        assert_eq!(parse_interval(Some("1")), Some(1));
        let mut d = MemoryDiagnostics::new(parse_interval(Some("3")).unwrap());
        assert!(d.begin_append());
        d.on_buffered(10);
        assert!(!d.begin_append());
        d.on_buffered(20);
        assert!(d.begin_append());
        d.on_buffered(30);
        assert_eq!(d.total_rows, 60);
        assert_eq!(d.appends, 3);
    }

    #[test]
    fn proc_fields_use_bytes_and_missing_values_are_not_zero() {
        assert_eq!(
            ProcessRss::parse(b"Name:\t\xff\nVmRSS:\t25 kB\nVmHWM:\t40 kB\n"),
            ProcessRss {
                current: Some(25 * 1024),
                high_watermark: Some(40 * 1024)
            }
        );
        for data in [
            b"".as_slice(),
            b"VmRSS: 25",
            b"VmRSS: 3 MB",
            b"VmRSS: 18446744073709551615 kB",
        ] {
            assert_eq!(ProcessRss::parse(data).current, None);
        }
    }

    #[test]
    fn bounded_record_handles_maximum_counters_and_sink_errors() {
        let b = BufferMemory {
            integer_logical_bytes: usize::MAX,
            integer_payload_bytes: usize::MAX,
            integer_capacity_bytes: usize::MAX,
            integer_metadata_bytes: usize::MAX,
            integer_width_values: [usize::MAX; 4],
            other_value_capacity_bytes: usize::MAX,
            null_capacity_bytes: usize::MAX,
            dictionary_entries: usize::MAX,
            columns: [ColumnMemory {
                columns: usize::MAX,
                payload_bytes: usize::MAX,
                integer_capacity_bytes: usize::MAX,
                other_capacity_bytes: usize::MAX,
                integer_metadata_bytes: usize::MAX,
                null_capacity_bytes: usize::MAX,
            }; 4],
            dictionary: DictionaryMemory {
                fixed_entries: usize::MAX,
                fixed_capacity: usize::MAX,
                variable_entries: usize::MAX,
                variable_capacity: usize::MAX,
                key_bytes: usize::MAX,
                table_payload_capacity_lower_bound: usize::MAX,
                inactive_table_payload_capacity_lower_bound: usize::MAX,
                pending_columns: usize::MAX,
                active_columns: usize::MAX,
                disabled_columns: usize::MAX,
            },
        };
        let sample = Sample {
            sequence: u64::MAX,
            stage: "append_buffered",
            unix_ms: u128::MAX,
            pid: u32::MAX,
            writer_id: u64::MAX,
            append: u64::MAX,
            total_rows: u64::MAX,
            row_group: usize::MAX,
            row_group_rows: usize::MAX,
            buffers: b,
            input: Some(InputMemory {
                rows: usize::MAX,
                columns: usize::MAX,
                buffer_bytes_sum: usize::MAX,
                reported_capacity_bytes_sum: usize::MAX,
                integer_buffer_bytes_sum: usize::MAX,
                float64_buffer_bytes_sum: usize::MAX,
            }),
            flush: Some(FlushMemory {
                integer_buckets: usize::MAX,
                legacy_buckets: usize::MAX,
                bulk_units: usize::MAX,
                stream_units: usize::MAX,
                uncompressed_units: usize::MAX,
                max_unit_bytes: usize::MAX,
                encoding_columns: [usize::MAX; 4],
                logical_value_bytes: usize::MAX,
                dict_logical_value_bytes: usize::MAX,
                encoding_capacity_peak_bytes: usize::MAX,
                compressed_capacity_peak_bytes: usize::MAX,
                tracked_temporary_peak_bytes: usize::MAX,
            }),
            rss: ProcessRss::default(),
        };
        let mut bytes = Vec::new();
        sample.write_to(&mut bytes).unwrap();
        let line = std::str::from_utf8(&bytes).unwrap();
        assert!(line.contains("rss_bytes=na rss_hwm_bytes=na\n"));
        assert_eq!(line.lines().count(), 8);
        assert!(line.lines().all(|line| line.len() < 2047));
        sample
            .write_records(|line| {
                assert!(line.len < 2048);
                line.as_c_str()?;
                Ok(())
            })
            .unwrap();
        let formatted = sample.format_line().unwrap();
        assert_eq!(
            formatted.as_c_str().unwrap().to_bytes(),
            &bytes[..formatted.len]
        );
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(sample.write_to(&mut Failing).is_err());
    }

    #[test]
    fn log_line_reserves_terminator_and_rejects_embedded_nul() {
        let mut line = LogLine {
            bytes: [0; 2048],
            len: 0,
        };
        line.write_str(&"x".repeat(2047)).unwrap();
        assert_eq!(line.as_c_str().unwrap().to_bytes().len(), 2047);
        assert!(line.write_str("x").is_err());
        assert_eq!(line.as_c_str().unwrap().to_bytes().len(), 2047);
        line.bytes[10] = 0;
        assert!(line.as_c_str().is_err());
    }
}
