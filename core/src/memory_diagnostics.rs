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
}

pub(crate) struct MemoryDiagnostics {
    writer_id: u64,
    every: u64,
    appends: u64,
    total_rows: u64,
    #[cfg(test)]
    pub last_sample: Option<Sample>,
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
            #[cfg(test)]
            last_sample: None,
        }
    }

    /// Always sample the first append, then every Nth append. No value scan.
    pub fn on_append(&mut self, rows: usize) -> bool {
        self.appends = self.appends.saturating_add(1);
        self.total_rows = self.total_rows.saturating_add(rows as u64);
        self.appends == 1 || self.appends.is_multiple_of(self.every)
    }

    #[cold]
    #[inline(never)]
    pub fn emit(&mut self, row_group: usize, row_group_rows: usize, buffers: BufferMemory) {
        let sample = Sample {
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
            rss: ProcessRss::read(),
        };
        #[cfg(test)]
        {
            self.last_sample = Some(sample);
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
    unix_ms: u128,
    pid: u32,
    writer_id: u64,
    pub append: u64,
    pub total_rows: u64,
    pub row_group: usize,
    pub row_group_rows: usize,
    pub buffers: BufferMemory,
    rss: ProcessRss,
}

impl Sample {
    fn emit(&self) -> io::Result<()> {
        #[cfg(target_os = "android")]
        {
            self.format_line()?.write_android()
        }
        #[cfg(not(target_os = "android"))]
        {
            self.write_to(&mut io::stderr().lock())
        }
    }

    #[cfg(any(not(target_os = "android"), test))]
    fn write_to(&self, output: &mut impl Write) -> io::Result<()> {
        let line = self.format_line()?;
        output.write_all(&line.bytes[..line.len])
    }

    fn format_line(&self) -> io::Result<LogLine> {
        let mut line = LogLine {
            bytes: [0; 2048],
            len: 0,
        };
        let b = self.buffers;
        // One bounded line; no retained log history.
        writeln!(
            line,
            "[mosaic-memory] v=1 unix_ms={} pid={} writer={} stage=append_buffered append={} total_rows={} row_group={} row_group_rows={} integer_logical_bytes={} integer_payload_bytes={} integer_capacity_bytes={} integer_metadata_bytes={} width1_values={} width2_values={} width4_values={} width8_values={} other_value_capacity_bytes={} null_capacity_bytes={} dictionary_entries={} rss_bytes={} rss_hwm_bytes={}",
            self.unix_ms, self.pid, self.writer_id, self.append, self.total_rows,
            self.row_group, self.row_group_rows, b.integer_logical_bytes,
            b.integer_payload_bytes, b.integer_capacity_bytes, b.integer_metadata_bytes,
            b.integer_width_values[0], b.integer_width_values[1],
            b.integer_width_values[2], b.integer_width_values[3],
            b.other_value_capacity_bytes, b.null_capacity_bytes, b.dictionary_entries,
            OptionalBytes(self.rss.current), OptionalBytes(self.rss.high_watermark),
        ).map_err(|_| io::Error::other("memory diagnostic line too long"))?;
        Ok(line)
    }
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
            assert!(default.on_append(10));
        }
        for value in ["", "0", "-1", "true", "999999999999999999999999999999"] {
            assert_eq!(parse_interval(Some(value)), None);
        }
        assert_eq!(parse_interval(Some("1")), Some(1));
        let mut d = MemoryDiagnostics::new(parse_interval(Some("3")).unwrap());
        assert!(d.on_append(10));
        assert!(!d.on_append(20));
        assert!(d.on_append(30));
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
        };
        let sample = Sample {
            unix_ms: u128::MAX,
            pid: u32::MAX,
            writer_id: u64::MAX,
            append: u64::MAX,
            total_rows: u64::MAX,
            row_group: usize::MAX,
            row_group_rows: usize::MAX,
            buffers: b,
            rss: ProcessRss::default(),
        };
        let mut bytes = Vec::new();
        sample.write_to(&mut bytes).unwrap();
        assert!(bytes.len() < 2048);
        let line = std::str::from_utf8(&bytes).unwrap();
        assert!(line.ends_with("rss_bytes=na rss_hwm_bytes=na\n"));
        let formatted = sample.format_line().unwrap();
        assert_eq!(formatted.as_c_str().unwrap().to_bytes(), bytes);
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
