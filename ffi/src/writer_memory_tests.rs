// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use super::*;
use arrow_array::{types::Int32Type, Array, Int32Array, ListArray, StringArray};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct SchemaRelease {
    original: *mut std::ffi::c_void,
    release: unsafe extern "C" fn(*mut FFI_ArrowSchema),
    count: Arc<AtomicUsize>,
}
unsafe extern "C" fn release_schema(ptr: *mut FFI_ArrowSchema) {
    let state = Box::from_raw((*ptr).private_data.cast::<SchemaRelease>());
    (*ptr).private_data = state.original;
    (*ptr).release = Some(state.release);
    (state.release)(ptr);
    state.count.fetch_add(1, Ordering::SeqCst);
}
fn track_schema(schema: &mut FFI_ArrowSchema, count: &Arc<AtomicUsize>) {
    let state = SchemaRelease {
        original: schema.private_data,
        release: schema.release.unwrap(),
        count: count.clone(),
    };
    schema.private_data = Box::into_raw(Box::new(state)).cast();
    schema.release = Some(release_schema);
}
struct ArrayRelease {
    original: *mut std::ffi::c_void,
    release: unsafe extern "C" fn(*mut FFI_ArrowArray),
    count: Arc<AtomicUsize>,
}
unsafe extern "C" fn release_array(ptr: *mut FFI_ArrowArray) {
    let state = Box::from_raw((*ptr).private_data.cast::<ArrayRelease>());
    (*ptr).private_data = state.original;
    (*ptr).release = Some(state.release);
    (state.release)(ptr);
    state.count.fetch_add(1, Ordering::SeqCst);
}
fn track_array(array: &mut FFI_ArrowArray, count: &Arc<AtomicUsize>) {
    let state = ArrayRelease {
        original: array.private_data,
        release: array.release.unwrap(),
        count: count.clone(),
    };
    array.private_data = Box::into_raw(Box::new(state)).cast();
    array.release = Some(release_array);
}

#[derive(Default)]
struct Sink {
    bytes: Vec<u8>,
    schema_releases: Arc<AtomicUsize>,
    array_releases: Arc<AtomicUsize>,
    expected_schema_releases: usize,
    expected_array_releases: usize,
    wrong_lifetime: bool,
    fail: bool,
}
impl OutputFile for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn pos(&self) -> u64 {
        self.bytes.len() as u64
    }
}
unsafe extern "C" fn output(ctx: *mut std::ffi::c_void, data: *const u8, len: usize) -> i32 {
    let sink = &mut *ctx.cast::<Sink>();
    sink.wrong_lifetime |= sink.schema_releases.load(Ordering::SeqCst)
        != sink.expected_schema_releases
        || sink.array_releases.load(Ordering::SeqCst) != sink.expected_array_releases;
    if sink.fail {
        return -1;
    }
    sink.bytes
        .extend_from_slice(std::slice::from_raw_parts(data, len));
    0
}
fn batch() -> RecordBatch {
    let list = ListArray::from_iter_primitive::<Int32Type, _, _>([
        Some(vec![Some(1), None, Some(-3)]),
        None,
        Some(vec![]),
        Some(vec![Some(4)]),
    ]);
    RecordBatch::try_from_iter([
        (
            "int",
            Arc::new(Int32Array::from(vec![
                Some(1),
                None,
                Some(-32769),
                Some(i32::MAX),
            ])) as arrow_array::ArrayRef,
        ),
        (
            "text",
            Arc::new(StringArray::from(vec![
                Some("same"),
                None,
                Some("same"),
                Some("other"),
            ])),
        ),
        ("list", Arc::new(list)),
    ])
    .unwrap()
}

#[test]
fn foreign_schema_releases_before_writes_but_array_survives_through_flush() {
    // Exercise both success and sink error; C release callbacks must run once
    // even if flush fails. A nested input ensures imported types are owned.
    for fail in [false, true] {
        let batch = batch();
        let mut sink = Sink {
            expected_schema_releases: 1,
            ..Default::default()
        };
        let mut schema = FFI_ArrowSchema::try_from(batch.schema().as_ref()).unwrap();
        track_schema(&mut schema, &sink.schema_releases);
        let stream = MosaicOutputFile {
            ctx: (&mut sink as *mut Sink).cast(),
            write_fn: Some(output),
            flush_fn: None,
            get_pos_fn: None,
        };
        let mut options = mosaic_writer_options_default();
        options.num_buckets = 2;
        options.row_group_max_size = 1;
        options.page_size_threshold = 1;
        // An option may borrow a name from the consumed schema. It must be
        // copied before releasing that schema.
        let stat_names = [schema.child(0).name];
        options.stats_columns = stat_names.as_ptr();
        options.num_stats_columns = 1;
        unsafe {
            let handle = mosaic_writer_open(stream, &mut schema, options);
            assert!(!handle.is_null());
            assert!(schema.release.is_none());
            assert_eq!(sink.schema_releases.load(Ordering::SeqCst), 1);
            assert!(!sink.wrong_lifetime);
            let data = StructArray::from(batch.clone()).to_data();
            let (mut array, mut schema) = arrow_array::ffi::to_ffi(&data).unwrap();
            track_schema(&mut schema, &sink.schema_releases);
            track_array(&mut array, &sink.array_releases);
            sink.expected_schema_releases = 2;
            sink.fail = fail;
            let status = mosaic_writer_write_batch(handle, &mut array, &mut schema);
            assert_eq!(status, if fail { -1 } else { 0 });
            assert!(schema.release.is_none() && array.release.is_none());
            assert_eq!(sink.schema_releases.load(Ordering::SeqCst), 2);
            assert_eq!(sink.array_releases.load(Ordering::SeqCst), 1);
            assert!(!sink.wrong_lifetime);
            sink.expected_array_releases = 1;
            if !fail {
                assert_eq!(mosaic_writer_close(handle), 0);
            }
            mosaic_writer_free(handle);
            assert_eq!(sink.schema_releases.load(Ordering::SeqCst), 2);
            assert_eq!(sink.array_releases.load(Ordering::SeqCst), 1);
        }
        if !fail {
            let mut direct = MosaicWriter::new(
                Sink::default(),
                &batch.schema(),
                WriterOptions {
                    num_buckets: 2,
                    row_group_max_size: 1,
                    page_size_threshold: 1,
                    stats_columns: vec!["int".into()],
                    ..Default::default()
                },
            )
            .unwrap();
            direct.write_batch(&batch).unwrap();
            direct.close().unwrap();
            assert_eq!(sink.bytes, direct.output().bytes);
        }
    }
}

#[test]
fn failed_import_consumes_foreign_array_and_schema_once() {
    let batch = batch();
    let mut sink = Sink {
        expected_schema_releases: 1,
        ..Default::default()
    };
    let mut schema = FFI_ArrowSchema::try_from(batch.schema().as_ref()).unwrap();
    track_schema(&mut schema, &sink.schema_releases);
    let stream = MosaicOutputFile {
        ctx: (&mut sink as *mut Sink).cast(),
        write_fn: Some(output),
        flush_fn: None,
        get_pos_fn: None,
    };
    unsafe {
        let handle = mosaic_writer_open(stream, &mut schema, mosaic_writer_options_default());
        assert!(!handle.is_null());
        let data = StructArray::from(batch).to_data();
        let mut array = FFI_ArrowArray::new(&data);
        let mut bad_schema = FFI_ArrowSchema::try_new("invalid_format", vec![], None).unwrap();
        track_schema(&mut bad_schema, &sink.schema_releases);
        track_array(&mut array, &sink.array_releases);
        assert_eq!(
            mosaic_writer_write_batch(handle, &mut array, &mut bad_schema),
            -1
        );
        assert!(bad_schema.release.is_none() && array.release.is_none());
        assert_eq!(sink.schema_releases.load(Ordering::SeqCst), 2);
        assert_eq!(sink.array_releases.load(Ordering::SeqCst), 1);
        sink.expected_schema_releases = 2;
        sink.expected_array_releases = 1;
        mosaic_writer_free(handle);
        assert_eq!(sink.schema_releases.load(Ordering::SeqCst), 2);
        assert_eq!(sink.array_releases.load(Ordering::SeqCst), 1);
    }
}
