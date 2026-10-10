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

use super::*;
use arrow_schema::{DataType, Field};
use std::sync::Arc;

#[derive(Default)]
struct Bytes(Vec<u8>);
impl OutputFile for Bytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn pos(&self) -> u64 {
        self.0.len() as u64
    }
}

fn input() -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("integer", DataType::Int32, true),
        Field::new("text", DataType::Utf8, false),
        Field::new("byte", DataType::Int8, false),
    ]));
    let values = Int32Array::from(
        (0..1000)
            .map(|i| if i % 2 == 0 { Some(i % 256) } else { None })
            .collect::<Vec<_>>(),
    );
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(values),
            Arc::new(StringArray::from_iter_values(
                (0..1000).map(|i| format!("text-{i}")),
            )),
            Arc::new(Int8Array::from(vec![1i8; 1000])),
        ],
    )
    .unwrap()
}

#[test]
fn sampling_precedes_automatic_flush_and_does_not_change_files() {
    let input = input();
    for compression in [COMPRESSION_NONE, COMPRESSION_ZSTD] {
        for threshold in [1, usize::MAX] {
            let options = || WriterOptions {
                compression,
                num_buckets: 1,
                row_group_max_size: 1,
                page_size_threshold: threshold,
                ..Default::default()
            };
            let mut normal =
                MosaicWriter::new(Bytes::default(), &input.schema(), options()).unwrap();
            normal.memory_diagnostics = None;
            let mut logged =
                MosaicWriter::new(Bytes::default(), &input.schema(), options()).unwrap();
            logged.memory_diagnostics = Some(MemoryDiagnostics::for_test(1));
            for n in 1..=2 {
                normal.write_batch(&input).unwrap();
                logged.write_batch(&input).unwrap();
                assert_eq!(logged.num_row_groups(), n);
                assert_eq!(logged.current_row_group_rows, 0);
                let sample = logged
                    .memory_diagnostics
                    .as_ref()
                    .unwrap()
                    .last_sample
                    .unwrap();
                assert_eq!(sample.append, n as u64);
                assert_eq!(sample.total_rows, (n * 1000) as u64);
                assert_eq!(sample.row_group, n - 1);
                assert_eq!(sample.row_group_rows, 1000);
                assert_eq!(sample.buffers.integer_logical_bytes, 500 * 4);
                assert_eq!(sample.buffers.integer_payload_bytes, 500);
                assert_eq!(sample.buffers.integer_width_values, [500, 0, 0, 0]);
                assert!(sample.buffers.integer_capacity_bytes >= 500);
                assert!(sample.buffers.other_value_capacity_bytes > 1000);
                assert!(sample.buffers.null_capacity_bytes >= 3 * 125);
                assert!(sample.buffers.dictionary_entries > 0);
            }
            normal.close().unwrap();
            logged.close().unwrap();
            assert_eq!(normal.output().0, logged.output().0);
        }
    }
}

#[test]
fn rejected_batch_and_sampling_interval_preserve_row_counts() {
    let input = input();
    let mut writer = MosaicWriter::new(
        Bytes::default(),
        &input.schema(),
        WriterOptions {
            row_group_max_size: u64::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    writer.memory_diagnostics = Some(MemoryDiagnostics::for_test(3));
    let invalid = input.project(&[0]).unwrap();
    assert!(writer.write_batch(&invalid).is_err());
    assert!(writer
        .memory_diagnostics
        .as_ref()
        .unwrap()
        .last_sample
        .is_none());
    writer.write_batch(&input).unwrap();
    writer.write_batch(&input).unwrap();
    assert_eq!(
        writer
            .memory_diagnostics
            .as_ref()
            .unwrap()
            .last_sample
            .unwrap()
            .append,
        1
    );
    writer.write_batch(&input).unwrap();
    let sample = writer
        .memory_diagnostics
        .as_ref()
        .unwrap()
        .last_sample
        .unwrap();
    assert_eq!(sample.append, 3);
    assert_eq!(sample.total_rows, 3000);
    assert_eq!(sample.row_group_rows, 3000);
    assert_eq!(sample.buffers.integer_logical_bytes, 1500 * 4);
    assert_eq!(sample.buffers.integer_payload_bytes, 1500);
    writer.close().unwrap();
}
