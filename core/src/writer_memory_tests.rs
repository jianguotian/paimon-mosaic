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
                let events = &logged.memory_diagnostics.as_ref().unwrap().events;
                let current = &events[(n - 1) * 5..n * 5];
                assert_eq!(
                    current.iter().map(|s| s.stage).collect::<Vec<_>>(),
                    [
                        "append_begin",
                        "append_buffered",
                        "flush_begin",
                        "flush_encoded",
                        "flush_reset"
                    ]
                );
                assert_eq!(current[0].append, n as u64);
                assert_eq!(current[0].total_rows, ((n - 1) * 1000) as u64);
                assert_eq!(current[0].row_group_rows, 0);
                assert_eq!(current[0].input, current[1].input);
                assert!(current
                    .windows(2)
                    .all(|w| w[0].sequence + 1 == w[1].sequence));
            }
            normal.close().unwrap();
            logged.close().unwrap();
            assert_eq!(normal.output().0, logged.output().0);
            let events = logged.memory_diagnostics.as_ref().unwrap().events.len();
            logged.close().unwrap();
            assert_eq!(
                logged.memory_diagnostics.as_ref().unwrap().events.len(),
                events
            );
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

#[test]
fn column_states_dictionaries_and_reset_are_distinct() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("null", DataType::Int16, true),
        Field::new("constant", DataType::Float64, false),
        Field::new("dict", DataType::Int32, false),
        Field::new("plain", DataType::Int32, false),
        Field::new("text", DataType::Utf8, false),
    ]));
    let make = |n: usize, constant: bool| {
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int16Array::from(vec![None::<i16>; n])),
                Arc::new(Float64Array::from(vec![13.75; n])),
                Arc::new(Int32Array::from_iter_values((0..n).map(|i| {
                    if constant {
                        1
                    } else {
                        (i % 2) as i32
                    }
                }))),
                Arc::new(Int32Array::from_iter_values((0..n).map(|i| i as i32))),
                Arc::new(StringArray::from_iter_values((0..n).map(|i| {
                    if i % 2 == 0 {
                        "ab"
                    } else {
                        "cd"
                    }
                }))),
            ],
        )
        .unwrap()
    };
    let mut writer = MosaicWriter::new(
        Bytes::default(),
        &schema,
        WriterOptions {
            num_buckets: 1,
            row_group_max_size: u64::MAX,
            max_dict_entries: 8,
            ..Default::default()
        },
    )
    .unwrap();
    writer.memory_diagnostics = Some(MemoryDiagnostics::for_test(1));
    writer.write_batch(&make(100, false)).unwrap();
    let sample = writer
        .memory_diagnostics
        .as_ref()
        .unwrap()
        .last_sample
        .unwrap();
    assert_eq!(sample.buffers.columns.map(|c| c.columns), [1, 1, 3, 0]);
    assert_eq!(sample.buffers.columns[1].payload_bytes, 800);
    assert_eq!(sample.buffers.columns[0].payload_bytes, 0);
    assert_eq!(sample.buffers.dictionary.fixed_entries, 2);
    assert_eq!(sample.buffers.dictionary.variable_entries, 2);
    assert_eq!(sample.buffers.dictionary.key_bytes, 6); // one-byte length + two bytes per key
    assert_eq!(sample.buffers.dictionary.pending_columns, 2);
    assert_eq!(sample.buffers.dictionary.disabled_columns, 1);
    let classes_capacity: usize = sample
        .buffers
        .columns
        .iter()
        .map(|c| {
            c.integer_capacity_bytes
                + c.other_capacity_bytes
                + c.integer_metadata_bytes
                + c.null_capacity_bytes
        })
        .sum();
    assert_eq!(
        classes_capacity,
        sample.buffers.integer_capacity_bytes
            + sample.buffers.other_value_capacity_bytes
            + sample.buffers.integer_metadata_bytes
            + sample.buffers.null_capacity_bytes
    );
    writer.flush_row_group().unwrap();
    let events = &writer.memory_diagnostics.as_ref().unwrap().events;
    let encoded = events
        .iter()
        .find(|s| s.stage == "flush_encoded")
        .unwrap()
        .flush
        .unwrap();
    assert_eq!(encoded.encoding_columns[ENCODING_DICT as usize], 2);
    assert_eq!(encoded.encoding_columns[ENCODING_CONST as usize], 1);
    assert_eq!(encoded.encoding_columns[ENCODING_ALL_NULL as usize], 1);
    assert_eq!(encoded.encoding_columns[ENCODING_PLAIN as usize], 1);
    assert_eq!(encoded.dict_logical_value_bytes, 100 * 4 + 100 * 3);
    let reset = events.last().unwrap();
    assert_eq!(reset.stage, "flush_reset");
    assert_eq!(reset.row_group, 0);
    assert_eq!(reset.row_group_rows, 100);
    assert_eq!(reset.buffers.columns.map(|c| c.columns), [0, 0, 0, 5]);
    assert_eq!(reset.buffers.dictionary_entries, 0);
    assert_eq!(reset.buffers.dictionary.key_bytes, 0);
    assert!(
        reset
            .buffers
            .dictionary
            .inactive_table_payload_capacity_lower_bound
            > 0
    );
    writer.write_batch(&make(2, true)).unwrap();
    let sample = writer
        .memory_diagnostics
        .as_ref()
        .unwrap()
        .last_sample
        .unwrap();
    assert!(
        sample
            .buffers
            .dictionary
            .inactive_table_payload_capacity_lower_bound
            > 0
    );
    writer.close().unwrap();
}

#[test]
fn flush_records_actual_bulk_stream_and_legacy_paths_with_file_parity() {
    for rows in [128usize, 300_000] {
        for integer in [true, false] {
            for page_threshold in [1, usize::MAX] {
                let values: ArrayRef = if integer {
                    Arc::new(Int32Array::from_iter_values(
                        (0..rows).map(|i| (i % 251) as i32),
                    ))
                } else {
                    Arc::new(Float64Array::from_iter_values(
                        (0..rows).map(|i| i as f64 / 10.0),
                    ))
                };
                let batch = RecordBatch::try_from_iter([("value", values)]).unwrap();
                let options = || WriterOptions {
                    num_buckets: 1,
                    row_group_max_size: u64::MAX,
                    max_dict_entries: 0,
                    page_size_threshold: page_threshold,
                    ..Default::default()
                };
                let mut normal =
                    MosaicWriter::new(Bytes::default(), &batch.schema(), options()).unwrap();
                normal.memory_diagnostics = None;
                let mut logged =
                    MosaicWriter::new(Bytes::default(), &batch.schema(), options()).unwrap();
                logged.memory_diagnostics = Some(MemoryDiagnostics::for_test(1));
                normal.write_batch(&batch).unwrap();
                logged.write_batch(&batch).unwrap();
                normal.close().unwrap();
                logged.close().unwrap();
                assert_eq!(normal.output().0, logged.output().0);
                let d = logged.memory_diagnostics.as_ref().unwrap();
                let f = d
                    .events
                    .iter()
                    .find(|e| e.stage == "flush_encoded")
                    .unwrap()
                    .flush
                    .unwrap();
                assert_eq!(f.integer_buckets, usize::from(integer));
                assert_eq!(f.legacy_buckets, usize::from(!integer));
                assert_eq!(f.stream_units, usize::from(integer && rows == 300_000));
                assert_eq!(f.bulk_units, usize::from(!(integer && rows == 300_000)));
                assert!(f.tracked_temporary_peak_bytes >= f.encoding_capacity_peak_bytes);
                assert!(f.tracked_temporary_peak_bytes >= f.compressed_capacity_peak_bytes);
                if integer && rows == 300_000 {
                    assert_eq!(f.encoding_capacity_peak_bytes, 0);
                }
                assert_eq!(d.events.last().unwrap().stage, "close_end");
            }
        }
    }
}

#[test]
fn input_metadata_handles_foreign_shared_and_nested_buffers() {
    use arrow_buffer::{Buffer, ScalarBuffer};
    let mut values = vec![10i32, 20, 30, 40];
    // SAFETY: The Arc owns the stable vector allocation for the buffer's lifetime.
    let buffer = unsafe {
        Buffer::from_custom_allocation(
            std::ptr::NonNull::new(values.as_mut_ptr().cast()).unwrap(),
            values.len() * 4,
            Arc::new(values),
        )
    };
    assert_eq!(buffer.capacity(), 16);
    let array: ArrayRef = Arc::new(Int32Array::new(ScalarBuffer::new(buffer, 0, 4), None));
    let nested: ArrayRef = Arc::new(StructArray::from(vec![(
        Arc::new(Field::new("child", DataType::Int32, false)),
        array.clone(),
    )]));
    let batch =
        RecordBatch::try_from_iter([("a", array.clone()), ("alias", array), ("nested", nested)])
            .unwrap()
            .slice(1, 2);
    let memory = InputMemory::from_batch(&batch);
    assert_eq!(memory.rows, 2);
    assert_eq!(memory.reported_capacity_bytes_sum, 48); // three references to the allocation
    assert_eq!(memory.buffer_bytes_sum, 24); // three references to an 8-byte slice
    assert_eq!(memory.integer_buffer_bytes_sum, 16); // top-level integer columns only
}

#[test]
fn failed_flush_reports_error_without_a_success_or_retry_sample() {
    #[derive(Default)]
    struct FailingSink {
        bytes: Bytes,
        fail: bool,
    }
    impl OutputFile for FailingSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            if self.fail {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                self.bytes.write(bytes)
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn pos(&self) -> u64 {
            self.bytes.pos()
        }
    }
    let batch = input();
    for auto_flush in [true, false] {
        let mut writer = MosaicWriter::new(
            FailingSink::default(),
            &batch.schema(),
            WriterOptions {
                row_group_max_size: if auto_flush { 1 } else { u64::MAX },
                ..Default::default()
            },
        )
        .unwrap();
        writer.memory_diagnostics = Some(MemoryDiagnostics::for_test(1));
        writer.output_mut().fail = true;
        if auto_flush {
            assert!(writer.write_batch(&batch).is_err());
        } else {
            writer.write_batch(&batch).unwrap();
            assert!(writer.close().is_err());
        }
        let events = &writer.memory_diagnostics.as_ref().unwrap().events;
        assert_eq!(
            events.last().unwrap().stage,
            if auto_flush {
                "append_error"
            } else {
                "close_error"
            }
        );
        assert!(!events
            .iter()
            .any(|s| matches!(s.stage, "flush_encoded" | "flush_reset" | "close_end")));
        let count = events.len();
        assert!(writer.close().is_err());
        assert!(writer.write_batch(&batch).is_err());
        assert_eq!(
            writer.memory_diagnostics.as_ref().unwrap().events.len(),
            count
        );
    }
}
