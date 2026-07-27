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

use std::io::{self, Write};

use zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer, ResetDirective};

/// Streaming Zstd encoder whose frame bytes match zstd-jni 1.5.5-11.
///
/// zstd-jni uses `ZSTD_CStreamOutSize()` for its native output buffer. The higher-level Rust
/// `zstd` crate uses a fixed 32 KiB buffer, which changes block boundaries and therefore changes
/// otherwise valid frame bytes.
pub(crate) struct JavaCompatibleZstdEncoder<W: Write> {
    context: CCtx<'static>,
    output: W,
    buffer: Vec<u8>,
}

impl<W: Write> JavaCompatibleZstdEncoder<W> {
    pub(crate) fn new(output: W, level: i32) -> io::Result<Self> {
        let mut context = CCtx::create();
        context
            .set_parameter(CParameter::CompressionLevel(level))
            .map_err(zstd_error)?;
        context
            .reset(ResetDirective::SessionOnly)
            .map_err(zstd_error)?;
        Ok(Self {
            context,
            output,
            buffer: vec![0; CCtx::out_size()],
        })
    }

    pub(crate) fn finish(mut self) -> io::Result<W> {
        loop {
            let mut output_buffer = OutBuffer::around(&mut self.buffer[..]);
            let remaining = self
                .context
                .end_stream(&mut output_buffer)
                .map_err(zstd_error)?;
            let written = output_buffer.pos();
            self.output.write_all(&self.buffer[..written])?;
            if remaining == 0 {
                return Ok(self.output);
            }
            if written == 0 {
                return Err(no_progress("zstd finish"));
            }
        }
    }
}

impl<W: Write> Write for JavaCompatibleZstdEncoder<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut input = InBuffer::around(data);
        while input.pos() < data.len() {
            let previous_input_position = input.pos();
            let mut output_buffer = OutBuffer::around(&mut self.buffer[..]);
            self.context
                .compress_stream(&mut output_buffer, &mut input)
                .map_err(zstd_error)?;
            let written = output_buffer.pos();
            self.output.write_all(&self.buffer[..written])?;
            if input.pos() == previous_input_position && written == 0 {
                return Err(no_progress("zstd compression"));
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        loop {
            let mut output_buffer = OutBuffer::around(&mut self.buffer[..]);
            let remaining = self
                .context
                .flush_stream(&mut output_buffer)
                .map_err(zstd_error)?;
            let written = output_buffer.pos();
            self.output.write_all(&self.buffer[..written])?;
            if remaining == 0 {
                break;
            }
            if written == 0 {
                return Err(no_progress("zstd flush"));
            }
        }
        self.output.flush()
    }
}

fn no_progress(operation: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::WriteZero,
        format!("{} made no progress", operation),
    )
}

fn zstd_error(code: usize) -> io::Error {
    io::Error::other(zstd_safe::get_error_name(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deterministic_input(length: usize) -> Vec<u8> {
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn compress(input: &[u8], chunk_size: usize) -> io::Result<Vec<u8>> {
        let mut encoder = JavaCompatibleZstdEncoder::new(Vec::new(), 3)?;
        for chunk in input.chunks(chunk_size) {
            encoder.write_all(chunk)?;
        }
        encoder.finish()
    }

    fn decode_hex(value: &str) -> Vec<u8> {
        assert_eq!(value.len() % 2, 0);
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn matches_zstd_jni_1_5_5_11_reference_bytes() {
        let input: Vec<u8> = (0..128)
            .map(|index| ((index * 31 + (index >> 3)) & 0xff) as u8)
            .collect();

        // Generated with com.github.luben:zstd-jni:1.5.5-11, level 3.
        let expected = decode_hex(
            "28b52ffd0058010400001f3e5d7c9bbad9f91837567594b3d2f211304f6e8da\
             ccbeb0a29486786a5c4e4032241607f9ebdddfc1b3a597897b6d6f5143352719\
             0afcfee0d2c4b6a89a8c8e70625446382a1c1e0ff1e3d5c7b9abad9f81736557\
             493b3d2f1102f4e6d8caccbea0928476685a5c4e30221405f7e9ebddcfb1a395\
             87797b6d5f413325170",
        );

        assert_eq!(compress(&input, input.len()).unwrap(), expected);
    }

    #[test]
    fn input_chunking_does_not_change_frame_bytes() {
        let input = deterministic_input(CCtx::out_size() * 4 + 17);
        let expected = compress(&input, input.len()).unwrap();
        assert!(expected.len() > CCtx::out_size());

        for chunk_size in [1, 31, 4096, 262_144] {
            assert_eq!(
                compress(&input, chunk_size).unwrap(),
                expected,
                "chunk size {} changed the frame",
                chunk_size
            );
        }
    }

    #[test]
    fn finish_propagates_output_failure() {
        #[derive(Debug)]
        struct FailingOutput;

        impl Write for FailingOutput {
            fn write(&mut self, _data: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "intentional output failure",
                ))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut encoder = JavaCompatibleZstdEncoder::new(FailingOutput, 3).unwrap();
        encoder.write_all(b"payload").unwrap();
        let error = encoder.finish().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "intentional output failure");
    }
}
