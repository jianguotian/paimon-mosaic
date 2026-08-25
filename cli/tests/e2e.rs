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

//! End-to-end tests: drive the `mosaic` binary against a fixture file and
//! assert stdout. Zero external dev-deps — uses CARGO_BIN_EXE and std only.

use std::process::Command;
use std::sync::Arc;

use arrow::array::{
    BooleanArray, Date32Array, Float32Array, Int32Array, Int64Array, RecordBatch, StringArray,
};
use arrow::datatypes::{DataType, Field, Schema};
use paimon_mosaic_core::writer::{FileSink, MosaicWriter, WriterOptions};

/// Write a small fixture and return its path under the test temp dir.
fn fixture(name: &str) -> String {
    fixture_threshold(name, 1)
}

/// Like `fixture` but with an explicit `page_size_threshold`; threshold 1 forces
/// paged buckets, the default (32 KiB) keeps small files monolithic.
fn fixture_threshold(name: &str, threshold: usize) -> String {
    let path = format!(
        "{}/mosaic_e2e_{}.mosaic",
        std::env::temp_dir().display(),
        name
    );
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("kind", DataType::Utf8, true),
        Field::new("flag", DataType::Int32, true),
    ]);
    let out = FileSink::create(std::path::Path::new(&path)).unwrap();
    let opts = WriterOptions {
        num_buckets: 3,
        page_size_threshold: threshold,
        stats_columns: vec!["id".into()],
        ..Default::default()
    };
    let mut w = MosaicWriter::new(out, &schema, opts).unwrap();
    let n = 200;
    let ids: Vec<i32> = (0..n).collect();
    let kinds: Vec<&str> = (0..n).map(|i| ["a", "b", "c"][(i % 3) as usize]).collect();
    let flags = vec![7; n as usize];
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(Int32Array::from(ids)),
            Arc::new(StringArray::from(kinds)),
            Arc::new(Int32Array::from(flags)),
        ],
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.close().unwrap();
    path
}

fn run(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_mosaic"))
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
        out.status.success(),
    )
}

fn sibling_temp_exists(dir: &std::path::Path, output_name: &str) -> bool {
    let prefix = format!("{output_name}.");
    std::fs::read_dir(dir).unwrap().any(|entry| {
        let name = entry.unwrap().file_name();
        let name = name.to_string_lossy();
        name.starts_with(&prefix) && name.ends_with(".tmp")
    })
}

fn unique_temp_dir(name: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mosaic_e2e_{name}_{}_{}",
        std::process::id(),
        nonce
    ));
    std::fs::create_dir(&dir).unwrap();
    dir
}

#[test]
fn schema_lists_columns() {
    let f = fixture("schema");
    let (out, _, ok) = run(&["schema", &f]);
    assert!(ok);
    assert!(out.contains("3 columns, 3 buckets"));
    assert!(out.contains("id: Int32 not null"));
    assert!(out.contains("kind: Utf8"));
}

#[test]
fn meta_shows_stats() {
    let f = fixture("meta");
    let (out, _, ok) = run(&["meta", &f]);
    assert!(ok);
    assert!(out.contains("200 rows"));
    assert!(out.contains("id: nulls=0 min=0 max=199"));
}

#[test]
fn pages_shows_encodings() {
    let f = fixture("pages");
    let (out, _, ok) = run(&["pages", &f]);
    assert!(ok);
    assert!(out.contains("flag: bucket 0 encoding=const"));
    assert!(out.contains("kind: bucket 2 encoding=dict"));
}

#[test]
fn cat_truncates_and_projects() {
    let f = fixture("cat");
    let (out, _, ok) = run(&["cat", &f, "-n", "2"]);
    assert!(ok);
    assert!(out.contains("| id | kind | flag |"));
    assert_eq!(out.matches('\n').count(), 6); // 3 borders + header + 2 rows
    let (proj, _, _) = run(&["cat", &f, "-c", "kind,id", "-n", "1"]);
    assert!(proj.contains("| kind | id |"));
}

#[test]
fn head_defaults_to_preview_rows_and_num_overrides() {
    let f = fixture("head");
    let (out, _, ok) = run(&["head", &f, "--json"]);
    assert!(ok);
    assert_eq!(out.lines().count(), 10);
    let (limited, _, ok) = run(&["head", &f, "-n", "1", "--json"]);
    assert!(ok);
    assert_eq!(limited.lines().count(), 1);
}

#[test]
fn pages_unknown_column_errors() {
    let f = fixture("badcol");
    let (_, _, ok) = run(&["pages", &f, "-c", "nope"]);
    assert!(!ok); // typo in -c fails instead of silently printing nothing
    let (_, _, ok2) = run(&["column-size", &f, "-c", "nope"]);
    assert!(!ok2);
}

#[test]
fn count_reports_total() {
    let f = fixture("count");
    let (out, _, ok) = run(&["count", &f]);
    assert!(ok && out.trim() == "200");
    let (j, _, _) = run(&["count", &f, "--json"]);
    assert!(j.contains("\"rows\":200"));
}

#[test]
fn cat_defaults_to_all_rows_and_num_limits() {
    let f = fixture("all");
    let (out, _, ok) = run(&["cat", &f, "--json"]);
    assert!(ok);
    assert_eq!(out.lines().count(), 200); // cat scans all rows by default
    let (limited, _, ok) = run(&["cat", &f, "-n", "10", "--json"]);
    assert!(ok);
    assert_eq!(limited.lines().count(), 10);
}

#[test]
fn cat_where_filters_rows() {
    let f = fixture("where");
    let (num, _, ok) = run(&["cat", &f, "--where", "id>197", "--json"]);
    assert!(ok && num.lines().count() == 2); // 198, 199
    let (str_eq, _, _) = run(&["cat", &f, "--where", "kind=b", "--json"]);
    assert!(str_eq.lines().count() > 0 && str_eq.lines().all(|l| l.contains("\"kind\":\"b\"")));
    let (none, _, _) = run(&["cat", &f, "--where", "id>9999"]);
    assert!(none.contains("(no rows)"));
    let (_, _, bad) = run(&["cat", &f, "--where", "nope??"]);
    assert!(!bad); // unparseable filter fails
    let (_, _, str_ord) = run(&["cat", &f, "--where", "kind>5"]);
    assert!(!str_ord); // ordering on a string column errors, not silent drop
                       // != with a non-numeric value matches all rows (nothing equals it).
    let (ne, _, _) = run(&["cat", &f, "--where", "id!=abc", "--json"]);
    assert_eq!(ne.lines().count(), 200);
    // Filtering a column dropped by -c works and doesn't leak into output.
    let (hid, _, ok) = run(&["cat", &f, "-c", "kind", "--where", "id>197", "--json"]);
    assert!(
        ok && hid.lines().count() == 2 && !hid.contains("\"id\""),
        "{hid}"
    );
}

#[test]
fn cat_where_unknown_column_errors_before_reading_rows() {
    let path = format!(
        "{}/mosaic_e2e_empty_unknown_where.mosaic",
        std::env::temp_dir().display()
    );
    let schema = Schema::new(vec![Field::new("id", DataType::Int32, false)]);
    let out = FileSink::create(std::path::Path::new(&path)).unwrap();
    let mut w = MosaicWriter::new(out, &schema, WriterOptions::default()).unwrap();
    w.close().unwrap();

    let (_, err, ok) = run(&["cat", &path, "--where", "missing=1"]);
    assert!(!ok);
    assert!(err.contains("column 'missing' not found"), "{err}");
}

/// Fixture with a Boolean column so `--where` on bools can be exercised.
fn fixture_bool(name: &str) -> String {
    let path = format!(
        "{}/mosaic_e2e_{}.mosaic",
        std::env::temp_dir().display(),
        name
    );
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("active", DataType::Boolean, true),
    ]);
    let out = FileSink::create(std::path::Path::new(&path)).unwrap();
    let opts = WriterOptions {
        num_buckets: 1,
        stats_columns: vec!["id".into()],
        ..Default::default()
    };
    let mut w = MosaicWriter::new(out, &schema, opts).unwrap();
    let n = 10;
    let ids: Vec<i32> = (0..n).collect();
    let active: Vec<bool> = (0..n).map(|i| i % 2 == 0).collect();
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(Int32Array::from(ids)),
            Arc::new(BooleanArray::from(active)),
        ],
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.close().unwrap();
    path
}

#[test]
fn cat_where_boolean_filters() {
    let f = fixture_bool("wherebool");
    // 5 of 10 rows are active=true; must not silently drop them all.
    let (t, _, ok) = run(&["cat", &f, "--where", "active=true", "--json"]);
    assert!(ok && t.lines().count() == 5, "{t}");
    assert!(t.lines().all(|l| l.contains("\"active\":true")));
    let (f2, _, _) = run(&["cat", &f, "--where", "active!=true", "--json"]);
    assert_eq!(f2.lines().count(), 5);
    // A non-bool literal on a bool column errors instead of returning nothing.
    let (_, _, bad) = run(&["cat", &f, "--where", "active=yes"]);
    assert!(!bad);
}

/// Fixture with a Float32 column whose value is 0.1 — the f64 literal 0.1 does
/// not equal 0.1f32 widened, so `--where price=0.1` must round the RHS to f32.
fn fixture_f32(name: &str) -> String {
    let path = format!(
        "{}/mosaic_e2e_{}.mosaic",
        std::env::temp_dir().display(),
        name
    );
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("price", DataType::Float32, false),
    ]);
    let out = FileSink::create(std::path::Path::new(&path)).unwrap();
    let mut w = MosaicWriter::new(
        out,
        &schema,
        WriterOptions {
            num_buckets: 1,
            stats_columns: vec!["price".into()], // exercise pushdown (stats_exclude) too
            ..Default::default()
        },
    )
    .unwrap();
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(Int32Array::from(vec![1, 2])),
            Arc::new(Float32Array::from(vec![0.1f32, 0.2f32])),
        ],
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.close().unwrap();
    path
}

fn fixture_i64(name: &str) -> String {
    let path = format!(
        "{}/mosaic_e2e_{}.mosaic",
        std::env::temp_dir().display(),
        name
    );
    let schema = Schema::new(vec![Field::new("id", DataType::Int64, false)]);
    let out = FileSink::create(std::path::Path::new(&path)).unwrap();
    let mut w = MosaicWriter::new(
        out,
        &schema,
        WriterOptions {
            num_buckets: 1,
            stats_columns: vec!["id".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(Int64Array::from(vec![
            1_700_000_000_000_000_001i64,
            1_700_000_000_000_000_003i64,
        ]))],
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.close().unwrap();
    path
}

fn fixture_date32(name: &str) -> String {
    let path = format!(
        "{}/mosaic_e2e_{}.mosaic",
        std::env::temp_dir().display(),
        name
    );
    let schema = Schema::new(vec![Field::new("d", DataType::Date32, false)]);
    let out = FileSink::create(std::path::Path::new(&path)).unwrap();
    let mut w = MosaicWriter::new(
        out,
        &schema,
        WriterOptions {
            num_buckets: 1,
            stats_columns: vec!["d".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(Date32Array::from(vec![18_262, 18_628]))],
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.close().unwrap();
    path
}

#[test]
fn cat_where_float32_precision() {
    let f = fixture_f32("wheref32");
    // RHS must be compared at f32 precision; stored 0.1f32 should match "0.1".
    let (m, _, ok) = run(&["cat", &f, "--where", "price=0.1", "--json"]);
    assert!(ok && m.lines().count() == 1, "expected 1 match, got: {m}");
}

#[test]
fn convert_csv_then_inspect() {
    let csv = format!("{}/mosaic_e2e_in.csv", std::env::temp_dir().display());
    std::fs::write(&csv, "id,kind,score\n1,a,10.5\n2,b,20\n3,a,30.5\n").unwrap();
    let out = format!("{}/mosaic_e2e_conv.mosaic", std::env::temp_dir().display());
    let (msg, _, ok) = run(&["convert-csv", &csv, "-o", &out, "--overwrite"]);
    assert!(ok && msg.contains("3 rows"));
    let (c, _, _) = run(&["count", &out]);
    assert_eq!(c.trim(), "3");
    let (s, _, _) = run(&["schema", &out]);
    assert!(s.contains("id:") && s.contains("score:")); // inferred schema
}

#[test]
fn convert_csv_keeps_out_long_option_alias() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_convert_csv_out_alias.csv", dir.display());
    std::fs::write(&csv, "id\n1\n").unwrap();
    let out = format!("{}/mosaic_e2e_convert_csv_out_alias.mosaic", dir.display());
    let _ = std::fs::remove_file(&out);
    let (msg, err, ok) = run(&["convert-csv", &csv, "--out", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (count, err, ok) = run(&["count", &out]);
    assert!(ok, "stdout: {count}\nstderr: {err}");
    assert_eq!(count.trim(), "1");
}

#[test]
fn convert_csv_help_requires_at_least_one_input() {
    let (help, err, ok) = run(&["convert-csv", "--help"]);
    assert!(ok, "stdout: {help}\nstderr: {err}");
    assert!(help.contains("<INPUTS>..."), "{help}");
    assert!(!help.contains("[INPUTS]..."), "{help}");
}

#[test]
fn convert_csv_applies_dialect_flags() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_csv_dialect.csv", dir.display());
    std::fs::write(&csv, "ignored preamble\nid|name\n1|'a\\'b'\n").unwrap();
    let out = format!("{}/mosaic_e2e_csv_dialect.mosaic", dir.display());
    let (msg, err, ok) = run(&[
        "convert-csv",
        &csv,
        "-o",
        &out,
        "--delimiter",
        "|",
        "--quote",
        "'",
        "--escape",
        "\\",
        "--skip-lines",
        "1",
        "--overwrite",
    ]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, err, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "stdout: {rows}\nstderr: {err}");
    assert_eq!(rows, "{\"id\":1,\"name\":\"a'b\"}\n");
}

#[test]
fn convert_csv_skip_lines_discards_non_utf8_preamble() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_csv_non_utf8_preamble.csv", dir.display());
    std::fs::write(&csv, b"\xff\nid,name\n1,ok\n").unwrap();
    let out = format!("{}/mosaic_e2e_csv_non_utf8_preamble.mosaic", dir.display());
    let (msg, err, ok) = run(&[
        "convert-csv",
        &csv,
        "-o",
        &out,
        "--skip-lines",
        "1",
        "--overwrite",
    ]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, err, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "stdout: {rows}\nstderr: {err}");
    assert_eq!(rows, "{\"id\":1,\"name\":\"ok\"}\n");
}

#[test]
fn convert_csv_preserves_backslashes_by_default() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_backslashes.csv", dir.display());
    std::fs::write(
        &csv,
        r#"path
"C:\temp\file"
"#,
    )
    .unwrap();
    let out = format!("{}/mosaic_e2e_backslashes.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert-csv", &csv, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, err, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "stdout: {rows}\nstderr: {err}");
    assert_eq!(rows, "{\"path\":\"C:\\\\temp\\\\file\"}\n");
}

#[test]
fn convert_csv_promotes_ints_and_floats_without_rounding_integers() {
    let dir = std::env::temp_dir();
    let ints = format!("{}/mosaic_e2e_multi_numeric_ints.csv", dir.display());
    let floats = format!("{}/mosaic_e2e_multi_numeric_floats.csv", dir.display());
    std::fs::write(&ints, "value\n1\n9007199254740992\n").unwrap();
    std::fs::write(&floats, "value\n3.5\n4.5\n").unwrap();
    let out = format!("{}/mosaic_e2e_multi_numeric.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert-csv", &ints, &floats, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (schema, _, ok) = run(&["schema", &out]);
    assert!(ok && schema.contains("value: Float64"), "{schema}");
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert_eq!(
        rows.lines().collect::<Vec<_>>(),
        [
            r#"{"value":1.0}"#,
            r#"{"value":9.007199254740992e15}"#,
            r#"{"value":3.5}"#,
            r#"{"value":4.5}"#
        ]
    );
}

#[test]
fn convert_csv_rejects_lossy_bare_integer_float_promotion() {
    let dir = std::env::temp_dir();
    for (case, integer) in [
        ("f64_boundary", "9007199254740993"),
        ("i64_max", "9223372036854775807"),
    ] {
        let ints = format!("{}/mosaic_e2e_lossy_{case}_integer.csv", dir.display());
        let floats = format!("{}/mosaic_e2e_lossy_{case}_float.csv", dir.display());
        std::fs::write(&ints, format!("value\n{integer}\n")).unwrap();
        std::fs::write(&floats, "value\n1.5\n").unwrap();
        for (order, first, second) in [
            ("integer_first", ints.as_str(), floats.as_str()),
            ("float_first", floats.as_str(), ints.as_str()),
        ] {
            let out = format!("{}/mosaic_e2e_lossy_{case}_{order}.mosaic", dir.display());
            let _ = std::fs::remove_file(&out);
            let (_, err, ok) = run(&["convert-csv", first, second, "-o", &out, "--overwrite"]);
            assert!(!ok, "{case}/{order} unexpectedly succeeded");
            assert!(
                err.contains("cannot be represented exactly as Float64"),
                "{case}/{order}: {err}"
            );
            assert!(!std::path::Path::new(&out).exists(), "{case}/{order}");
        }
    }
}

#[test]
fn convert_csv_rejects_finite_float_overflow() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_float_overflow.csv", dir.display());
    std::fs::write(&csv, "value\n1.5\n1e400\n").unwrap();
    let out = format!("{}/mosaic_e2e_float_overflow.mosaic", dir.display());
    let _ = std::fs::remove_file(&out);
    let (_, err, ok) = run(&["convert-csv", &csv, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(
        err.contains("cannot be represented exactly as Float64"),
        "{err}"
    );
    assert!(!std::path::Path::new(&out).exists());
}

#[test]
fn convert_csv_rejects_non_finite_float_values() {
    let dir = std::env::temp_dir();
    for (case, value) in [("nan", "NaN"), ("infinity", "inf")] {
        let csv = format!("{}/mosaic_e2e_non_finite_{case}.csv", dir.display());
        std::fs::write(&csv, format!("value\n{value}\n1.5\n")).unwrap();
        let out = format!("{}/mosaic_e2e_non_finite_{case}.mosaic", dir.display());
        let _ = std::fs::remove_file(&out);
        let (_, err, ok) = run(&["convert-csv", &csv, "-o", &out]);
        assert!(!ok, "{case} unexpectedly succeeded");
        assert!(err.contains("non-finite Float64"), "{case}: {err}");
        assert!(!std::path::Path::new(&out).exists(), "{case}");
    }
}

#[test]
fn convert_csv_preserves_inferred_timestamp_precision() {
    let dir = std::env::temp_dir();
    let seconds = format!("{}/mosaic_e2e_timestamp_seconds.csv", dir.display());
    let nanos = format!("{}/mosaic_e2e_timestamp_nanos.csv", dir.display());
    std::fs::write(&seconds, "ts\n2026-08-20T12:34:56\n").unwrap();
    std::fs::write(&nanos, "ts\n2026-08-20T12:34:56.123456789\n").unwrap();
    let out = format!("{}/mosaic_e2e_timestamp_precision.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert-csv", &seconds, &nanos, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (schema, _, ok) = run(&["schema", &out]);
    assert!(
        ok && schema.contains("ts: Timestamp(Nanosecond, None)"),
        "{schema}"
    );
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert_eq!(
        rows.lines().collect::<Vec<_>>(),
        [
            r#"{"ts":"2026-08-20T12:34:56"}"#,
            r#"{"ts":"2026-08-20T12:34:56.123456789"}"#
        ]
    );
}

#[test]
fn convert_csv_rejects_offsets_for_inferred_local_timestamps() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_timestamp_offset.csv", dir.display());
    std::fs::write(&csv, "ts\n2026-08-20T12:34:56+08:00\n").unwrap();
    let out = format!("{}/mosaic_e2e_timestamp_offset.mosaic", dir.display());
    let _ = std::fs::remove_file(&out);
    let (_, err, ok) = run(&["convert-csv", &csv, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("must not include a timezone"), "{err}");
    assert!(!std::path::Path::new(&out).exists());
}

#[test]
fn convert_csv_matches_multi_input_fields_by_name() {
    let dir = std::env::temp_dir();
    let first = format!("{}/mosaic_e2e_multi_order_first.csv", dir.display());
    let reordered = format!("{}/mosaic_e2e_multi_order_reordered.csv", dir.display());
    std::fs::write(&first, "id,kind\n1,a\n").unwrap();
    std::fs::write(&reordered, "kind,id\nb,2\n").unwrap();
    let out = format!("{}/mosaic_e2e_multi_order.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert-csv", &first, &reordered, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert!(rows.contains(r#"{"id":1,"kind":"a"}"#), "{rows}");
    assert!(rows.contains(r#"{"id":2,"kind":"b"}"#), "{rows}");
}

#[test]
fn convert_csv_custom_header_preserves_first_data_row() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_custom_header.csv", dir.display());
    std::fs::write(&csv, "1,a\n2,b\n").unwrap();
    let out = format!("{}/mosaic_e2e_custom_header.mosaic", dir.display());
    let (msg, err, ok) = run(&[
        "convert-csv",
        &csv,
        "-o",
        &out,
        "--header",
        "id,kind",
        "--overwrite",
    ]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert_eq!(
        rows.lines().collect::<Vec<_>>(),
        [r#"{"id":1,"kind":"a"}"#, r#"{"id":2,"kind":"b"}"#]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn convert_csv_rejects_non_regular_inferred_input_without_opening_it() {
    let dir = std::env::temp_dir();
    let fifo = format!("{}/mosaic_e2e_inferred_fifo.csv", dir.display());
    let out = format!("{}/mosaic_e2e_inferred_fifo.mosaic", dir.display());
    let _ = std::fs::remove_file(&fifo);
    let _ = std::fs::remove_file(&out);
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let output = std::process::Command::new("timeout")
        .args([
            "5",
            env!("CARGO_BIN_EXE_mosaic"),
            "convert-csv",
            &fifo,
            "-o",
            &out,
            "--overwrite",
        ])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&fifo);
    assert_ne!(
        output.status.code(),
        Some(124),
        "conversion blocked on FIFO"
    );
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("requires a regular file"), "{err}");
    assert!(!std::path::Path::new(&out).exists());
}

#[test]
fn convert_csv_all_null_column_falls_back_to_utf8() {
    let csv = format!(
        "{}/mosaic_e2e_all_null_col.csv",
        std::env::temp_dir().display()
    );
    std::fs::write(&csv, "id,empty\n1,\n2,\n").unwrap();
    let out = format!(
        "{}/mosaic_e2e_all_null_col.mosaic",
        std::env::temp_dir().display()
    );
    let (msg, err, ok) = run(&["convert-csv", &csv, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (s, _, _) = run(&["schema", &out]);
    assert!(s.contains("empty: Utf8"), "{s}");
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert!(
        rows.lines().all(|line| line.contains("\"empty\":null")),
        "{rows}"
    );
}

#[test]
fn convert_csv_rejects_empty_header_field() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_empty_header_field.csv", dir.display());
    std::fs::write(&csv, ",id\nx,1\n").unwrap();
    let out = format!("{}/mosaic_e2e_empty_header_field.mosaic", dir.display());
    let _ = std::fs::remove_file(&out);
    let (_, err, ok) = run(&["convert-csv", &csv, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("empty column name"), "{err}");
    assert!(!std::path::Path::new(&out).exists());
}

#[test]
fn convert_csv_errors_on_duplicate_header_field() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_duplicate_header.csv", dir.display());
    std::fs::write(&csv, "id,id\n1,2\n3,4\n").unwrap();
    let out = format!("{}/mosaic_e2e_duplicate_header.mosaic", dir.display());
    let (_, err, ok) = run(&["convert-csv", &csv, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("duplicate CSV header field 'id'"), "{err}");
}

#[test]
fn convert_csv_require_marks_inferred_field_not_null() {
    let csv = format!("{}/mosaic_e2e_require.csv", std::env::temp_dir().display());
    std::fs::write(&csv, "id,name\n1,a\n2,b\n").unwrap();
    let out = format!(
        "{}/mosaic_e2e_require.mosaic",
        std::env::temp_dir().display()
    );
    let (msg, err, ok) = run(&[
        "convert-csv",
        &csv,
        "-o",
        &out,
        "--require",
        "id",
        "--overwrite",
    ]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (s, _, _) = run(&["schema", &out]);
    assert!(s.contains("id: Int64 not null"), "{s}");
    assert!(s.contains("name: Utf8"), "{s}");
}

#[test]
fn convert_csv_stats_enables_where_pushdown() {
    // stats on id let id>100 skip the row group; boundaries must not drop matches.
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_csv_stats.csv", dir.display());
    std::fs::write(&csv, "id,kind\n1,a\n2,b\n3,a\n").unwrap();
    let out = format!("{}/mosaic_e2e_csv_stats.mosaic", dir.display());
    let (msg, err, ok) = run(&[
        "convert-csv",
        &csv,
        "-o",
        &out,
        "--stats",
        "id",
        "--overwrite",
    ]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (meta, err, ok) = run(&["meta", &out, "--json"]);
    assert!(ok, "stdout: {meta}\nstderr: {err}");
    let meta: serde_json::Value = serde_json::from_str(meta.trim()).unwrap();
    let stats = &meta["row_groups"][0]["stats"][0];
    assert_eq!(stats["column"], "id");
    assert_eq!(stats["min"], "1");
    assert_eq!(stats["max"], "3");
    let (none, _, _) = run(&["cat", &out, "--where", "id>100"]);
    assert!(none.contains("(no rows)"), "{none}");
    let (keep, _, _) = run(&["cat", &out, "--where", "id>=3", "--json"]);
    assert_eq!(keep.lines().count(), 1, "{keep}");
}

#[test]
fn convert_json_supports_stats() {
    let dir = std::env::temp_dir();
    let js = format!("{}/mosaic_e2e_json_stats.json", dir.display());
    std::fs::write(&js, "{\"id\":1}\n{\"id\":2}\n").unwrap();
    let out = format!("{}/mosaic_e2e_json_stats.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert", &js, "-o", &out, "--stats", "id", "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (meta, err, ok) = run(&["meta", &out, "--json"]);
    assert!(ok, "stdout: {meta}\nstderr: {err}");
    let meta: serde_json::Value = serde_json::from_str(meta.trim()).unwrap();
    let stats = &meta["row_groups"][0]["stats"][0];
    assert_eq!(stats["column"], "id");
    assert_eq!(stats["min"], "1");
    assert_eq!(stats["max"], "2");
    let (none, _, _) = run(&["cat", &out, "--where", "id>100"]);
    assert!(none.contains("(no rows)"), "{none}");
}

#[test]
fn convert_csv_multiple_inputs_share_schema() {
    let dir = std::env::temp_dir();
    let a = format!("{}/mosaic_e2e_multi_a.csv", dir.display());
    let b = format!("{}/mosaic_e2e_multi_b.csv", dir.display());
    std::fs::write(&a, "id,kind\n1,a\n2,b\n").unwrap();
    std::fs::write(&b, "id,kind\n3,c\n").unwrap();
    let out = format!("{}/mosaic_e2e_multi.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert-csv", &a, &b, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (c, _, _) = run(&["count", &out]);
    assert_eq!(c.trim(), "3");
    // A file whose inferred schema differs must be rejected with a hint.
    let c_path = format!("{}/mosaic_e2e_multi_c.csv", dir.display());
    std::fs::write(&c_path, "id,kind\nx,y\n").unwrap();
    let (_, err, ok) = run(&["convert-csv", &a, &c_path, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("incompatible"), "{err}");
}

#[test]
fn convert_csv_multiple_inputs_merges_null_and_typed_columns() {
    let dir = std::env::temp_dir();
    let a = format!("{}/mosaic_e2e_multi_null_a.csv", dir.display());
    let b = format!("{}/mosaic_e2e_multi_null_b.csv", dir.display());
    std::fs::write(&a, "id,value\n1,\n2,\n").unwrap();
    std::fs::write(&b, "id,value\n3,9\n4,10\n").unwrap();
    let out = format!("{}/mosaic_e2e_multi_null.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert-csv", &a, &b, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (schema, _, ok) = run(&["schema", &out]);
    assert!(ok, "{schema}");
    assert!(schema.contains("value: Int64"), "{schema}");
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert!(rows.contains(r#"{"id":1,"value":null}"#), "{rows}");
    assert!(rows.contains(r#"{"id":4,"value":10}"#), "{rows}");
}

#[test]
fn convert_csv_skips_empty_inputs_and_rejects_all_empty() {
    let dir = std::env::temp_dir();
    let a = format!("{}/mosaic_e2e_empty_shard_a.csv", dir.display());
    let empty = format!("{}/mosaic_e2e_empty_shard_b.csv", dir.display());
    std::fs::write(&a, "id,kind\n1,a\n2,b\n").unwrap();
    std::fs::write(&empty, "").unwrap();
    let out = format!("{}/mosaic_e2e_empty_shard.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert-csv", &a, &empty, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (c, _, _) = run(&["count", &out]);
    assert_eq!(c.trim(), "2");
    let (_, err, ok) = run(&["convert-csv", &empty, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("no CSV data"), "{err}");
}

#[test]
fn convert_csv_skips_header_only_inputs() {
    let dir = std::env::temp_dir();
    let header_only = format!("{}/mosaic_e2e_header_only_shard_a.csv", dir.display());
    let data = format!("{}/mosaic_e2e_header_only_shard_b.csv", dir.display());
    std::fs::write(&header_only, "id,kind\n").unwrap();
    std::fs::write(&data, "id,kind\n1,a\n2,b\n").unwrap();
    let out = format!("{}/mosaic_e2e_header_only_shard.mosaic", dir.display());
    let (msg, err, ok) = run(&[
        "convert-csv",
        &header_only,
        &data,
        "-o",
        &out,
        "--overwrite",
    ]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (c, _, _) = run(&["count", &out]);
    assert_eq!(c.trim(), "2");
}

#[test]
fn convert_csv_skips_header_only_inputs_with_unrelated_or_duplicate_headers() {
    let dir = std::env::temp_dir();
    let unrelated = format!("{}/mosaic_e2e_header_only_unrelated.csv", dir.display());
    let duplicate = format!("{}/mosaic_e2e_header_only_duplicate.csv", dir.display());
    let data = format!("{}/mosaic_e2e_header_only_data.csv", dir.display());
    std::fs::write(&unrelated, "other,value\n").unwrap();
    std::fs::write(&duplicate, "id,id\n").unwrap();
    std::fs::write(&data, "id,kind\n1,a\n2,b\n").unwrap();
    let out = format!("{}/mosaic_e2e_header_only_extra.mosaic", dir.display());
    let (msg, err, ok) = run(&[
        "convert-csv",
        &unrelated,
        &duplicate,
        &data,
        "-o",
        &out,
        "--overwrite",
    ]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert_eq!(rows.lines().count(), 2, "{rows}");
}

#[test]
fn convert_csv_not_null_violation_names_the_schema_field() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_null_violation.csv", dir.display());
    std::fs::write(&csv, "id,name\n1,a\n,b\n").unwrap();
    let out = format!("{}/mosaic_e2e_null_violation.mosaic", dir.display());
    let (_, err, ok) = run(&[
        "convert-csv",
        &csv,
        "-o",
        &out,
        "--require",
        "id",
        "--overwrite",
    ]);
    assert!(!ok);
    assert!(err.contains("'id'") && !err.contains("field_0"), "{err}");
}

#[test]
fn convert_csv_no_header_maps_fields_by_position() {
    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_no_header.csv", dir.display());
    std::fs::write(&csv, "1,a\n2,b\n").unwrap();
    let out = format!("{}/mosaic_e2e_no_header.mosaic", dir.display());
    let (msg, err, ok) = run(&[
        "convert-csv",
        &csv,
        "-o",
        &out,
        "--no-header",
        "--overwrite",
    ]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (s, _, _) = run(&["schema", &out]);
    assert!(s.contains("field_0: Int64"), "{s}");
    assert!(s.contains("field_1: Utf8"), "{s}");
}

#[test]
fn convert_refuses_existing_output_without_overwrite() {
    let csv = format!(
        "{}/mosaic_e2e_no_overwrite.csv",
        std::env::temp_dir().display()
    );
    std::fs::write(&csv, "id\n1\n").unwrap();
    let out = format!(
        "{}/mosaic_e2e_no_overwrite.mosaic",
        std::env::temp_dir().display()
    );
    std::fs::write(&out, "keep me").unwrap();
    let (_, err, ok) = run(&["convert-csv", &csv, "-o", &out]);
    assert!(!ok);
    assert!(err.contains("use --overwrite"), "{err}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "keep me");
}

#[cfg(unix)]
#[test]
fn convert_refuses_dangling_symlink_output_without_overwrite() {
    use std::os::unix::fs::symlink;

    let dir = std::env::temp_dir();
    let csv = format!("{}/mosaic_e2e_dangling_output.csv", dir.display());
    std::fs::write(&csv, "id\n1\n").unwrap();
    let target = dir.join("mosaic_e2e_dangling_output_missing_target");
    let out = dir.join("mosaic_e2e_dangling_output.mosaic");
    let _ = std::fs::remove_file(&out);
    symlink(&target, &out).unwrap();

    let (_, err, ok) = run(&["convert-csv", &csv, "-o", out.to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("use --overwrite"), "{err}");
    assert_eq!(std::fs::read_link(&out).unwrap(), target);
}

#[cfg(unix)]
fn run_output_creation_race_attempt() -> bool {
    use std::fmt::Write;
    use std::time::{Duration, Instant};

    const RACE_WINDOW_ROWS: usize = 50_000;
    const RACE_WINDOW_PAYLOAD_BYTES: usize = 64;

    let dir = unique_temp_dir("concurrent_output");
    let csv = dir.join("input.csv");
    let mut contents = String::from("id,payload\n");
    let payload = "x".repeat(RACE_WINDOW_PAYLOAD_BYTES);
    for id in 0..RACE_WINDOW_ROWS {
        writeln!(&mut contents, "{id},{payload}").unwrap();
    }
    std::fs::write(&csv, contents).unwrap();
    let out = dir.join("out.mosaic");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mosaic"))
        .args([
            "convert-csv",
            csv.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if sibling_temp_exists(&dir, "out.mosaic") {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&out)
            {
                Ok(mut sentinel) => {
                    std::io::Write::write_all(&mut sentinel, b"keep me").unwrap();
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "stdout: {}\nstderr: {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    std::fs::remove_dir_all(&dir).unwrap();
                    return false;
                }
                Err(error) => panic!("cannot create competing output: {error}"),
            }
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "conversion exited before creating its sibling temp"
        );
        assert!(
            Instant::now() < deadline,
            "timed out waiting for sibling temp"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("use --overwrite"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "keep me");
    assert!(!sibling_temp_exists(&dir, "out.mosaic"));
    std::fs::remove_dir_all(&dir).unwrap();
    true
}

#[cfg(unix)]
#[test]
fn convert_refuses_output_created_during_conversion() {
    assert!(
        (0..3).any(|_| run_output_creation_race_attempt()),
        "conversion won all attempts before the competing output could be created"
    );
}

#[test]
fn convert_cleans_temp_when_output_install_fails() {
    let dir = unique_temp_dir("install_failure");
    let csv = dir.join("input.csv");
    std::fs::write(&csv, "id\n1\n").unwrap();
    let out = dir.join("out.mosaic");
    std::fs::create_dir(&out).unwrap();

    let (_, err, ok) = run(&[
        "convert-csv",
        csv.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--overwrite",
    ]);
    assert!(!ok);
    assert!(
        err.contains("Is a directory") || err.contains("Access is denied"),
        "{err}"
    );
    assert!(!sibling_temp_exists(&dir, "out.mosaic"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn convert_refuses_existing_output_before_opening_input() {
    let dir = std::env::temp_dir();

    let csv_out = format!(
        "{}/mosaic_e2e_existing_before_input_csv.mosaic",
        dir.display()
    );
    std::fs::write(&csv_out, "keep csv").unwrap();
    let missing_csv = format!("{}/mosaic_e2e_missing_before_existing.csv", dir.display());
    let _ = std::fs::remove_file(&missing_csv);
    let (_, err, ok) = run(&["convert-csv", &missing_csv, "-o", &csv_out]);
    assert!(!ok);
    assert!(err.contains("use --overwrite"), "{err}");
    assert_eq!(std::fs::read_to_string(&csv_out).unwrap(), "keep csv");

    let json_out = format!(
        "{}/mosaic_e2e_existing_before_input_json.mosaic",
        dir.display()
    );
    std::fs::write(&json_out, "keep json").unwrap();
    let missing_json = format!("{}/mosaic_e2e_missing_before_existing.json", dir.display());
    let _ = std::fs::remove_file(&missing_json);
    let (_, err, ok) = run(&["convert", &missing_json, "-o", &json_out]);
    assert!(!ok);
    assert!(err.contains("use --overwrite"), "{err}");
    assert_eq!(std::fs::read_to_string(&json_out).unwrap(), "keep json");
}

#[test]
fn convert_rejects_csv_input() {
    let csv = format!(
        "{}/mosaic_e2e_convert_rejects_csv.csv",
        std::env::temp_dir().display()
    );
    std::fs::write(&csv, "id\n1\n").unwrap();
    let out = format!(
        "{}/mosaic_e2e_convert_rejects_csv.mosaic",
        std::env::temp_dir().display()
    );
    let (_, err, ok) = run(&["convert", &csv, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("use convert-csv for CSV data"), "{err}");
}

#[test]
fn convert_keeps_out_long_option_alias() {
    let dir = std::env::temp_dir();
    let js = format!("{}/mosaic_e2e_convert_out_alias.json", dir.display());
    std::fs::write(&js, "{\"id\":1}\n").unwrap();
    let out = format!("{}/mosaic_e2e_convert_out_alias.mosaic", dir.display());
    let _ = std::fs::remove_file(&out);
    let (msg, err, ok) = run(&["convert", &js, "--out", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (count, err, ok) = run(&["count", &out]);
    assert!(ok, "stdout: {count}\nstderr: {err}");
    assert_eq!(count.trim(), "1");
}

#[test]
fn convert_accepts_jsonl_and_rejects_other_extensions() {
    let dir = std::env::temp_dir();
    let js = format!("{}/mosaic_e2e_convert_jsonl.jsonl", dir.display());
    std::fs::write(&js, "{\"id\":1}\n").unwrap();
    let out = format!("{}/mosaic_e2e_convert_jsonl.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert", &js, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");

    let upper = format!("{}/mosaic_e2e_convert_upper.JSON", dir.display());
    std::fs::write(&upper, "{\"id\":1}\n").unwrap();
    let (msg, err, ok) = run(&["convert", &upper, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");

    let not_json = format!("{}/mosaic_e2e_convert_rejects.notjson", dir.display());
    std::fs::write(&not_json, "{\"id\":1}\n").unwrap();
    let (_, err, ok) = run(&["convert", &not_json, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("convert only supports JSON inputs"), "{err}");

    let txt = format!("{}/mosaic_e2e_convert_rejects.txt", dir.display());
    std::fs::write(&txt, "{\"id\":1}\n").unwrap();
    let (_, err, ok) = run(&["convert", &txt, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("convert only supports JSON inputs"), "{err}");
}

#[test]
fn convert_json_all_null_column_errors_with_column_name() {
    let dir = std::env::temp_dir();
    let js = format!("{}/mosaic_e2e_json_all_null.json", dir.display());
    std::fs::write(&js, "{\"id\":1,\"v\":null}\n{\"id\":2,\"v\":null}\n").unwrap();
    let out = format!("{}/mosaic_e2e_json_all_null.mosaic", dir.display());
    let (_, err, ok) = run(&["convert", &js, "-o", &out, "--overwrite"]);
    assert!(!ok);
    assert!(err.contains("'v'"), "{err}");
    // Projecting the unusable column away converts the rest.
    let (msg, err, ok) = run(&["convert", &js, "-o", &out, "-c", "id", "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
}

#[test]
fn convert_json_infers_fields_after_initial_records() {
    let dir = std::env::temp_dir();
    let js = format!("{}/mosaic_e2e_json_late_field.json", dir.display());
    let mut text = String::new();
    for id in 0..20 {
        text.push_str(&format!(r#"{{"id":{id}}}"#));
        text.push('\n');
    }
    text.push_str(r#"{"id":20,"late":"present"}"#);
    text.push('\n');
    std::fs::write(&js, text).unwrap();
    let out = format!("{}/mosaic_e2e_json_late_field.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert", &js, "-o", &out, "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (schema, _, ok) = run(&["schema", &out]);
    assert!(ok, "{schema}");
    assert!(schema.contains("late: Utf8"), "{schema}");
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert!(rows.contains(r#"{"id":20,"late":"present"}"#), "{rows}");
}

#[test]
fn convert_json_then_inspect() {
    let js = format!("{}/mosaic_e2e_in.json", std::env::temp_dir().display());
    std::fs::write(
        &js,
        "{\"id\":1,\"kind\":\"a\"}\n{\"id\":2,\"kind\":\"b\"}\n",
    )
    .unwrap();
    let out = format!("{}/mosaic_e2e_jconv.mosaic", std::env::temp_dir().display());
    let (msg, _, ok) = run(&["convert", &js, "-o", &out, "--overwrite"]);
    assert!(ok && msg.contains("2 rows"), "{msg}");
    let (j, _, _) = run(&["cat", &out, "--json"]);
    assert_eq!(j.lines().count(), 2);
    assert!(j.contains("\"kind\":\"a\""));
}

#[test]
fn convert_json_projects_columns() {
    let js = format!(
        "{}/mosaic_e2e_json_project.json",
        std::env::temp_dir().display()
    );
    std::fs::write(
        &js,
        "{\"id\":1,\"kind\":\"a\",\"drop\":\"x\"}\n{\"id\":2,\"kind\":\"b\",\"drop\":\"y\"}\n",
    )
    .unwrap();
    let out = format!(
        "{}/mosaic_e2e_json_project.mosaic",
        std::env::temp_dir().display()
    );
    let (msg, err, ok) = run(&["convert", &js, "-o", &out, "-c", "kind,id", "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, _, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "{rows}");
    assert!(rows.contains(r#"{"kind":"a","id":1}"#), "{rows}");
    assert!(!rows.contains("drop"), "{rows}");
}

#[test]
fn convert_json_projection_ignores_unselected_type_conflicts() {
    let dir = std::env::temp_dir();
    let js = format!("{}/mosaic_e2e_json_project_conflict.json", dir.display());
    std::fs::write(
        &js,
        "{\"id\":1,\"drop\":7}\n{\"id\":2,\"drop\":{\"nested\":true}}\n",
    )
    .unwrap();
    let out = format!("{}/mosaic_e2e_json_project_conflict.mosaic", dir.display());
    let (msg, err, ok) = run(&["convert", &js, "-o", &out, "-c", "id", "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, err, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "stdout: {rows}\nstderr: {err}");
    assert_eq!(rows, "{\"id\":1}\n{\"id\":2}\n");
}

#[test]
fn convert_json_projection_ignores_unselected_out_of_range_number() {
    let dir = std::env::temp_dir();
    let js = format!(
        "{}/mosaic_e2e_json_project_out_of_range.json",
        dir.display()
    );
    std::fs::write(&js, "{\"id\":1,\"drop\":1e400}\n").unwrap();
    let out = format!(
        "{}/mosaic_e2e_json_project_out_of_range.mosaic",
        dir.display()
    );
    let (msg, err, ok) = run(&["convert", &js, "-o", &out, "-c", "id", "--overwrite"]);
    assert!(ok, "stdout: {msg}\nstderr: {err}");
    let (rows, err, ok) = run(&["cat", &out, "--json"]);
    assert!(ok, "stdout: {rows}\nstderr: {err}");
    assert_eq!(rows.trim(), r#"{"id":1}"#);
}

#[test]
fn where_pushdown_keeps_correct_rows() {
    // stats on id let id>100 skip the row group; boundaries must not drop matches.
    let f = fixture("pd");
    let (none, _, _) = run(&["cat", &f, "--where", "id>1000"]);
    assert!(none.contains("(no rows)"));
    let (keep, _, _) = run(&["cat", &f, "--where", "id>=199", "--json"]);
    assert_eq!(keep.lines().count(), 1); // boundary kept, not skipped
}

#[test]
fn bigint_where_is_exact() {
    // Snowflake-scale ids differ below f64 precision; equality must be exact.
    let f = fixture_i64("sf");
    let (j, _, ok) = run(&["cat", &f, "--where", "id=1700000000000000003", "--json"]);
    assert!(ok && j.lines().count() == 1 && j.contains("003"), "{j}");
}

#[test]
fn date_column_pushdown_keeps_match() {
    // Date filters accept ISO literals while stats pushdown still compares the
    // stored epoch-day bounds.
    let f = fixture_date32("date");
    let (j, _, ok) = run(&["cat", &f, "--where", "d>2020-12-31", "--json"]);
    assert!(
        ok && j.lines().count() == 1 && j.contains("2021-01-01"),
        "{j}"
    );
}

#[test]
fn cat_json_is_ndjson() {
    let f = fixture("json");
    let (out, _, ok) = run(&["cat", &f, "-n", "2", "--json"]);
    assert!(ok);
    assert_eq!(
        out,
        "{\"id\":0,\"kind\":\"a\",\"flag\":7}\n{\"id\":1,\"kind\":\"b\",\"flag\":7}\n"
    );
}

#[test]
fn missing_file_fails() {
    let (_, err, ok) = run(&["schema", "/no/such/file.mosaic"]);
    assert!(!ok);
    assert!(err.contains("error:"));
}

#[test]
fn footer_shows_format() {
    let f = fixture("footer");
    let (out, _, ok) = run(&["footer", &f]);
    assert!(ok);
    assert!(out.contains("magic=MOSA"));
    assert!(out.contains("buckets=3"));
    assert!(out.contains("compression=zstd"));
    let (j, _, ok) = run(&["footer", &f, "--json"]);
    assert!(ok);
    assert!(j.contains("\"magic\":\"MOSA\"") && j.contains("\"compression\":\"zstd\""));
}

#[test]
fn dictionary_dumps_entries() {
    let f = fixture("dict");
    let (out, _, ok) = run(&["dictionary", &f, "-c", "kind"]);
    assert!(ok);
    assert!(out.contains("3 entries"));
    assert!(out.contains("a") && out.contains("b") && out.contains("c"));
    let (j, _, ok) = run(&["dictionary", &f, "-c", "kind", "--json"]);
    assert!(ok);
    assert_eq!(
        j,
        "{\"column\":\"kind\",\"row_groups\":[[\"a\",\"b\",\"c\"]]}\n"
    );
}

#[test]
fn column_size_sums_bytes() {
    let f = fixture("size");
    let (out, _, ok) = run(&["column-size", &f]);
    assert!(ok);
    assert!(out.contains("id:") && out.contains("kind:"));
    // Every column attributes its on-disk bucket bytes (even the const flag bucket).
    assert!(out.contains("flag: 15 B") && !out.contains(": 0 B"));
    // Paged buckets lack uncompressed sizes, so no (misleading) total ratio.
    assert!(
        !out.contains("uncompressed"),
        "paged total must omit ratio: {out}"
    );
}

#[test]
fn column_size_nonzero_on_monolithic() {
    // Default threshold keeps small files monolithic; bytes must still attribute
    // (regression: monolithic buckets previously reported 0 B everywhere).
    let f = fixture_threshold("size_mono", 32 * 1024);
    let (b, _, _) = run(&["buckets", &f]);
    assert!(
        b.contains("monolithic"),
        "default file should be monolithic: {b}"
    );
    let (out, _, ok) = run(&["column-size", &f]);
    assert!(ok);
    assert!(
        out.contains("id: ") && !out.contains("id: 0 B"),
        "id must be non-zero: {out}"
    );
    assert!(
        out.contains("kind: ") && !out.contains("kind: 0 B"),
        "kind must be non-zero: {out}"
    );
    // Single-column buckets are exact, so nothing is flagged approximate.
    assert!(
        out.contains("total:") && !out.contains("approx"),
        "single-col exact: {out}"
    );
}

#[test]
fn buckets_show_layout() {
    let f = fixture("buckets");
    let (out, _, ok) = run(&["buckets", &f]);
    assert!(ok);
    assert!(out.contains("row group 0:"));
    assert!(out.contains("[flag]") && out.contains("[id]") && out.contains("[kind]"));
    assert!(out.contains("monolithic") || out.contains("paged"));
    let (j, _, ok) = run(&["buckets", &f, "--json"]);
    assert!(ok);
    assert!(
        j.contains("\"bucket\":0") && j.contains("\"columns\":") && j.contains("\"uncompressed\":")
    );
    // const flag bucket is monolithic, so its uncompressed size + ratio show.
    assert!(
        out.contains("uncompressed") && out.contains("x)"),
        "ratio: {out}"
    );
}

#[test]
fn buckets_json_keeps_column_names_lossless() {
    let path = format!(
        "{}/mosaic_e2e_control_name.mosaic",
        std::env::temp_dir().display()
    );
    let schema = Schema::new(vec![Field::new("name\x1b", DataType::Int32, false)]);
    let out = FileSink::create(std::path::Path::new(&path)).unwrap();
    let mut w = MosaicWriter::new(
        out,
        &schema,
        WriterOptions {
            num_buckets: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(Int32Array::from(vec![1, 2]))],
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.close().unwrap();

    let (j, _, ok) = run(&["buckets", &path, "--json"]);
    assert!(ok, "{j}");
    assert!(j.contains("name\\u001b"), "{j}");
    assert!(!j.contains(&format!("name{}", '\u{fffd}')), "{j}");
}
