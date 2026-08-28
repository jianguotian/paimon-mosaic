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

mod filter;
mod fmt;
mod input;
mod jsonout;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use arrow::array::timezone::Tz;
use arrow::array::types::{
    Float64Type, TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
};
use arrow::array::{new_null_array, ArrayRef, PrimitiveArray, RecordBatch, StringArray};
use arrow::compute::kernels::cast_utils::{string_to_datetime, Parser as ArrowValueParser};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use clap::{Parser, Subcommand};
use paimon_mosaic_core::reader::{MosaicReader, ReaderAccess};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::Value;

use crate::input::FileInput;

/// Mosaic file inspector — the cat/meta/schema/pages toolkit.
#[derive(Parser)]
#[command(name = "mosaic", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the column names, types, nullability and bucket assignment.
    Schema {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Print row-group / bucket / stats metadata.
    Meta {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Print per-column encoding and slot size for each row group.
    Pages {
        file: PathBuf,
        /// Comma-separated columns to show (default: all).
        #[arg(short, long)]
        columns: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print rows as a table (default: all rows; use -n to limit).
    Cat {
        file: PathBuf,
        /// Limit to N rows.
        #[arg(short = 'n', long)]
        num: Option<usize>,
        /// Comma-separated columns to project.
        #[arg(short, long)]
        columns: Option<String>,
        /// Row filter, e.g. `id>100` or `kind=a` (one condition).
        #[arg(long)]
        r#where: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the first N rows (default 10).
    Head {
        file: PathBuf,
        #[arg(short = 'n', long, default_value_t = 10)]
        num: usize,
        #[arg(short, long)]
        columns: Option<String>,
        #[arg(long)]
        r#where: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the total row count.
    Count {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Print the file footer: version, buckets, compression, offsets.
    Footer {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Print on-disk bytes per column (summed over row groups).
    ColumnSize {
        file: PathBuf,
        /// Comma-separated columns to show (default: all).
        #[arg(short, long)]
        columns: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the dictionary of a dict-encoded column.
    Dictionary {
        file: PathBuf,
        /// Column name to dump.
        #[arg(short = 'c', long)]
        column: String,
        #[arg(long)]
        json: bool,
    },
    /// Print bucket layout per row group (Mosaic's column grouping).
    Buckets {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Create a Mosaic file from JSON, with legacy single-file CSV compatibility.
    Convert {
        /// JSON data file; non-JSON paths use the legacy default CSV conversion.
        input: PathBuf,
        /// Output .mosaic path.
        #[arg(short = 'o', long = "output", visible_alias = "out")]
        out: PathBuf,
        /// Columns to keep; each occurrence accepts a comma-separated list.
        #[arg(short = 'c', long = "column")]
        columns: Vec<String>,
        /// Columns to build min/max stats for (comma-separated); `cat --where`
        /// uses them to skip row groups.
        #[arg(long)]
        stats: Option<String>,
        /// Overwrite the output file if it already exists.
        #[arg(long)]
        overwrite: bool,
    },
    /// Create a Mosaic file from CSV data.
    ConvertCsv {
        /// Input CSV path(s).
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Output .mosaic path.
        #[arg(short = 'o', long = "output", visible_alias = "out")]
        out: PathBuf,
        /// Do not allow null values for inferred fields; repeat for multiple fields.
        #[arg(long)]
        require: Vec<String>,
        /// Delimiter character.
        #[arg(long, default_value = ",")]
        delimiter: String,
        /// Escape character (disabled by default).
        #[arg(long)]
        escape: Option<String>,
        /// Quote character.
        #[arg(long, default_value = "\"")]
        quote: String,
        /// Don't use first line as CSV header.
        #[arg(long, conflicts_with = "header")]
        no_header: bool,
        /// Line to use as a header. Must match the CSV settings.
        #[arg(long, conflicts_with = "no_header")]
        header: Option<String>,
        /// Lines to skip before CSV start.
        #[arg(long, default_value_t = 0)]
        skip_lines: usize,
        /// Columns to build min/max stats for (comma-separated); `cat --where`
        /// uses them to skip row groups.
        #[arg(long)]
        stats: Option<String>,
        /// Overwrite the output file if it already exists.
        #[arg(long)]
        overwrite: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::Schema { file, json } => schema(&file, json),
        Cmd::Meta { file, json } => meta(&file, json),
        Cmd::Pages {
            file,
            columns,
            json,
        } => pages(&file, columns, json),
        Cmd::Cat {
            file,
            num,
            columns,
            r#where,
            json,
        } => cat(&file, num.unwrap_or(usize::MAX), columns, r#where, json),
        Cmd::Head {
            file,
            num,
            columns,
            r#where,
            json,
        } => cat(&file, num, columns, r#where, json),
        Cmd::Count { file, json } => count(&file, json),
        Cmd::Footer { file, json } => footer(&file, json),
        Cmd::ColumnSize {
            file,
            columns,
            json,
        } => column_size(&file, columns, json),
        Cmd::Dictionary { file, column, json } => dictionary(&file, &column, json),
        Cmd::Buckets { file, json } => buckets(&file, json),
        Cmd::Convert {
            input,
            out,
            columns,
            stats,
            overwrite,
        } => convert(&input, &out, &columns, stats.as_deref(), overwrite),
        Cmd::ConvertCsv {
            inputs,
            out,
            require,
            delimiter,
            escape,
            quote,
            no_header,
            header,
            skip_lines,
            stats,
            overwrite,
        } => {
            let options = CsvConvertOptions {
                delimiter,
                escape,
                quote,
                no_header,
                header,
                skip_lines,
            };
            convert_csv(
                &inputs,
                &out,
                &require,
                options,
                stats.as_deref(),
                overwrite,
                false,
            )
        }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn open(file: &Path) -> std::io::Result<MosaicReader<FileInput>> {
    let input = FileInput::open(file)?;
    let len = input.len();
    MosaicReader::new(input, len)
}

/// Columns in original (write) order rather than the name-sorted layout.
fn original_order(s: &paimon_mosaic_core::schema::MosaicSchema) -> Vec<usize> {
    let mut by_sorted = vec![0usize; s.columns.len()];
    for (orig, &sorted) in s.original_order.iter().enumerate() {
        by_sorted[sorted] = orig;
    }
    let mut cols: Vec<usize> = (0..s.columns.len()).collect();
    cols.sort_by_key(|&i| by_sorted[i]);
    cols
}

/// Split a comma list into trimmed, non-empty names (e.g. `-c a, b,` -> [a, b]).
fn parse_comma_list(l: &str) -> Vec<String> {
    l.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

/// Parse a `-c a,b` list into a name set, or `None` for "all columns".
fn col_filter(
    columns: &Option<String>,
    s: &paimon_mosaic_core::schema::MosaicSchema,
) -> std::io::Result<Option<std::collections::HashSet<String>>> {
    let Some(l) = columns else { return Ok(None) };
    let set: std::collections::HashSet<String> = parse_comma_list(l).into_iter().collect();
    if let Some(bad) = set
        .iter()
        .find(|n| !s.columns.iter().any(|c| &c.name == *n))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("column '{bad}' not found in schema"),
        ));
    }
    Ok(Some(set))
}

/// True when `name` is selected by a `-c` set (`None` = all columns).
fn selected(want: &Option<std::collections::HashSet<String>>, name: &str) -> bool {
    want.as_ref().is_none_or(|w| w.contains(name))
}

/// Add `total` across `cols`, distributing the remainder so the parts sum exactly.
fn split_evenly(total: usize, cols: &[usize], acc: &mut [usize]) {
    if cols.is_empty() {
        return;
    }
    let share = total / cols.len();
    let mut rem = total % cols.len();
    for &c in cols {
        acc[c] += share
            + if rem > 0 {
                rem -= 1;
                1
            } else {
                0
            };
    }
}

fn schema(file: &Path, json: bool) -> std::io::Result<()> {
    let reader = open(file)?;
    let s = reader.schema();
    let cols = original_order(s);
    if json {
        let fields = cols
            .iter()
            .map(|&i| {
                let c = &s.columns[i];
                jsonout::SchemaField {
                    name: c.name.clone(),
                    ty: format!("{:?}", c.data_type),
                    nullable: c.nullable,
                    bucket: c.bucket_id as u32,
                }
            })
            .collect();
        println!(
            "{}",
            jsonout::line(&jsonout::Schema {
                columns: s.columns.len(),
                buckets: s.num_buckets,
                fields,
            })
        );
        return Ok(());
    }
    println!("{} columns, {} buckets", s.columns.len(), s.num_buckets);
    for i in cols {
        let c = &s.columns[i];
        let null = if c.nullable { "" } else { " not null" };
        println!(
            "  {}: {:?}{} [bucket {}]",
            fmt::safe(&c.name),
            c.data_type,
            null,
            c.bucket_id
        );
    }
    Ok(())
}

fn meta(file: &Path, json: bool) -> std::io::Result<()> {
    let reader = open(file)?;
    let s = reader.schema();
    let nrg = reader.num_row_groups();
    let total: usize = (0..nrg)
        .map(|i| reader.row_group_num_rows(i))
        .sum::<std::io::Result<usize>>()?;
    if json {
        let mut row_groups = Vec::new();
        for rg in 0..nrg {
            let stats = reader
                .row_group_stats(rg)?
                .iter()
                .map(|x| {
                    let (min, max) = match (&x.min, &x.max) {
                        (Some(lo), Some(hi)) => {
                            (Some(fmt::render_json(lo)), Some(fmt::render_json(hi)))
                        }
                        _ => (None, None),
                    };
                    jsonout::Stat {
                        column: s.columns[x.column_index].name.clone(),
                        nulls: x.null_count,
                        min,
                        max,
                    }
                })
                .collect();
            row_groups.push(jsonout::MetaRg {
                rows: reader.row_group_num_rows(rg)?,
                stats,
            });
        }
        println!(
            "{}",
            jsonout::line(&jsonout::Meta {
                rows: total,
                columns: s.columns.len(),
                buckets: s.num_buckets,
                row_groups,
            })
        );
        return Ok(());
    }
    println!(
        "file: {} rows, {} columns, {} buckets, {} row groups",
        total,
        s.columns.len(),
        s.num_buckets,
        nrg
    );
    for rg in 0..nrg {
        println!("row group {rg}: {} rows", reader.row_group_num_rows(rg)?);
        for st in reader.row_group_stats(rg)? {
            let mm = match (&st.min, &st.max) {
                (Some(lo), Some(hi)) => format!(
                    "min={} max={}",
                    fmt::render_value(lo),
                    fmt::render_value(hi)
                ),
                _ => "no min/max".to_string(),
            };
            println!(
                "    {}: nulls={} {}",
                fmt::safe(&s.columns[st.column_index].name),
                st.null_count,
                mm
            );
        }
    }
    Ok(())
}

fn pages(file: &Path, columns: Option<String>, json: bool) -> std::io::Result<()> {
    let reader = open(file)?;
    let s = reader.schema();
    let want = col_filter(&columns, s)?;
    let cols: Vec<usize> = (0..s.columns.len())
        .filter(|&i| selected(&want, &s.columns[i].name))
        .collect();
    let nrg = reader.num_row_groups();
    if json {
        let mut row_groups = Vec::new();
        for rg in 0..nrg {
            let pgs = reader
                .page_infos_projected(rg, &cols)?
                .iter()
                .map(|p| jsonout::Page {
                    column: s.columns[p.column_index].name.clone(),
                    bucket: p.bucket,
                    encoding: fmt::encoding_name(p.encoding),
                    slot_size: p.slot_size,
                })
                .collect();
            row_groups.push(pgs);
        }
        println!("{}", jsonout::line(&jsonout::Pages { row_groups }));
        return Ok(());
    }
    for rg in 0..nrg {
        println!("row group {rg}:");
        for p in reader.page_infos_projected(rg, &cols)? {
            let c = &s.columns[p.column_index];
            println!(
                "    {}: bucket {} encoding={} slot={}B",
                fmt::safe(&c.name),
                p.bucket,
                fmt::encoding_name(p.encoding),
                p.slot_size
            );
        }
    }
    Ok(())
}

fn count(file: &Path, json: bool) -> std::io::Result<()> {
    let reader = open(file)?;
    let n: usize = (0..reader.num_row_groups())
        .map(|i| reader.row_group_num_rows(i))
        .sum::<std::io::Result<usize>>()?;
    if json {
        println!("{}", jsonout::line(&jsonout::Count { rows: n }));
    } else {
        println!("{}", n);
    }
    Ok(())
}

fn convert(
    input: &Path,
    out: &Path,
    columns: &[String],
    stats: Option<&str>,
    overwrite: bool,
) -> std::io::Result<()> {
    use arrow::error::ArrowError;
    let bad = |e: ArrowError| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string());
    if !is_json_input(input) {
        if !columns.is_empty() {
            return Err(invalid_schema(
                "--column is only supported for JSON input; use convert-csv for CSV options",
            ));
        }
        let inputs = [input.to_path_buf()];
        return convert_csv(
            &inputs,
            out,
            &[],
            CsvConvertOptions {
                delimiter: ",".to_string(),
                escape: None,
                quote: "\"".to_string(),
                no_header: false,
                header: None,
                skip_lines: 0,
            },
            stats,
            overwrite,
            true,
        );
    }
    let columns = parse_convert_columns(columns)?;
    ensure_can_write(out, overwrite)?;
    ensure_regular_inferred_input(input, "JSON")?;
    let open =
        || -> std::io::Result<_> { Ok(std::io::BufReader::new(std::fs::File::open(input)?)) };
    let selected = (!columns.is_empty()).then_some(columns.as_slice());
    let schema = infer_json_schema_lossless(open()?, selected).map_err(bad)?;
    let schema = project_convert_schema(schema, &columns)?;
    reject_null_inferred_fields(&schema)?;
    let reader = arrow::json::ReaderBuilder::new(Arc::new(schema.clone()))
        .build(open()?)
        .map_err(bad)?;
    write_mosaic(out, overwrite, &schema, stats, |writer, rows| {
        for batch in reader {
            let batch = batch
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            *rows += batch.num_rows();
            writer.write_batch(&batch)?;
        }
        Ok(())
    })
}

struct CsvConvertOptions {
    delimiter: String,
    escape: Option<String>,
    quote: String,
    no_header: bool,
    header: Option<String>,
    skip_lines: usize,
}

fn convert_csv(
    inputs: &[PathBuf],
    out: &Path,
    required_fields: &[String],
    options: CsvConvertOptions,
    stats: Option<&str>,
    overwrite: bool,
    allow_empty_legacy_input: bool,
) -> std::io::Result<()> {
    if inputs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CSV path is required",
        ));
    }
    ensure_can_write(out, overwrite)?;
    for input in inputs {
        ensure_regular_inferred_input(input, "CSV")?;
    }
    use arrow::error::ArrowError;
    let bad = |e: ArrowError| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string());
    let format = csv_format(&options)?;
    let mut inferred: Option<Schema> = None;
    for input in inputs {
        let inspected = inspect_csv_input(open_csv(input)?, &options, &format)?;
        if let Some(schema) = inspected {
            inferred = Some(match inferred.take() {
                Some(prev) => merge_csv_inferred_schema(prev, schema, input, !options.no_header)?,
                None => schema,
            });
        }
    }
    let schema = match inferred {
        Some(schema) => schema,
        None if allow_empty_legacy_input
            && inputs.len() == 1
            && std::fs::metadata(&inputs[0])?.len() == 0 =>
        {
            Schema::empty()
        }
        None => return Err(invalid_schema("no CSV data to infer a schema from")),
    };
    let schema = apply_required_fields(csv_schema_with_null_fallback(schema), required_fields)?;
    let schema_index = csv_schema_index(&schema);
    write_mosaic(out, overwrite, &schema, stats, |writer, rows| {
        for input in inputs {
            let mut reader = open_csv(input)?;
            let layout = inspect_csv_layout(&mut reader, &options)?;
            if !layout.has_records {
                continue;
            }
            let reader_schema = csv_reader_schema(&schema, &schema_index, &layout);
            let source_mapping = csv_output_mapping(&schema, &schema_index, &layout);
            validate_csv_mapping(&schema, &layout, &source_mapping, input)?;
            let (projection, mapping) = csv_projection(&source_mapping);
            let reader = build_csv_replay_reader(reader_schema, format.clone(), projection, reader)
                .map_err(bad)?;
            for batch in reader {
                let batch = batch.map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?;
                let batch = align_csv_batch_to_schema(batch, &schema, &mapping, input)?;
                *rows += batch.num_rows();
                writer.write_batch(&batch)?;
            }
        }
        Ok(())
    })
}

fn write_mosaic<F>(
    out: &Path,
    overwrite: bool,
    schema: &Schema,
    stats: Option<&str>,
    write: F,
) -> std::io::Result<()>
where
    F: FnOnce(
        &mut paimon_mosaic_core::writer::MosaicWriter<paimon_mosaic_core::writer::FileSink>,
        &mut usize,
    ) -> std::io::Result<()>,
{
    use paimon_mosaic_core::writer::{FileSink, MosaicWriter, WriterOptions};
    ensure_can_write(out, overwrite)?;
    let opts = WriterOptions {
        stats_columns: stats.map(parse_comma_list).unwrap_or_default(),
        ..Default::default()
    };
    // Write to a unique sibling temp file and install it on success, so a mid-stream
    // failure never leaves a truncated .mosaic — and a process-unique suffix
    // avoids clobbering an unrelated `out.mosaic.tmp` the user may already have.
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = out.with_extension(format!("mosaic.{}.{uniq}.tmp", std::process::id()));
    let mut rows = 0;
    let res = (|| {
        let sink = FileSink::create(&tmp)?;
        let mut w = MosaicWriter::new(sink, schema, opts)?;
        write(&mut w, &mut rows)?;
        w.close()
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    match install_mosaic_output(&tmp, out, overwrite) {
        Ok(InstallOutcome::Clean) => {}
        Ok(InstallOutcome::CommittedWithCleanupError(error)) => {
            eprintln!(
                "warning: output was committed to {}, but temporary file {} could not be removed: {error}",
                fmt::safe_path(out),
                fmt::safe_path(&tmp)
            );
        }
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
    }
    let plural = |n: usize, w: &str| {
        if n == 1 {
            format!("1 {w}")
        } else {
            format!("{n} {w}s")
        }
    };
    println!(
        "wrote {} ({}, {})",
        fmt::safe_path(out),
        plural(rows, "row"),
        plural(schema.fields().len(), "column")
    );
    Ok(())
}

#[derive(Debug)]
enum InstallOutcome {
    Clean,
    CommittedWithCleanupError(std::io::Error),
}

fn install_mosaic_output(
    tmp: &Path,
    out: &Path,
    overwrite: bool,
) -> std::io::Result<InstallOutcome> {
    if overwrite {
        #[cfg(windows)]
        if std::fs::symlink_metadata(out).is_ok() {
            std::fs::remove_file(out)?;
        }
        std::fs::rename(tmp, out)?;
        return Ok(InstallOutcome::Clean);
    }
    install_mosaic_output_no_replace(
        tmp,
        out,
        |from, to| std::fs::hard_link(from, to),
        rename_no_replace,
        |path| std::fs::remove_file(path),
    )
}

fn install_mosaic_output_no_replace<H, R, C>(
    tmp: &Path,
    out: &Path,
    hard_link: H,
    rename_no_replace: R,
    cleanup_temp: C,
) -> std::io::Result<InstallOutcome>
where
    H: FnOnce(&Path, &Path) -> std::io::Result<()>,
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
    C: FnOnce(&Path) -> std::io::Result<()>,
{
    // Both paths are siblings. Prefer a hard link for a portable atomic
    // no-replace install, but filesystems such as VFAT do not support links;
    // use the platform's no-replace rename there.
    match hard_link(tmp, out) {
        Ok(()) => match cleanup_temp(tmp) {
            Ok(()) => Ok(InstallOutcome::Clean),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(InstallOutcome::Clean),
            Err(error) => Ok(InstallOutcome::CommittedWithCleanupError(error)),
        },
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(output_exists_error(out))
        }
        Err(hard_link_error) => match rename_no_replace(tmp, out) {
            Ok(()) => Ok(InstallOutcome::Clean),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(output_exists_error(out))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Err(hard_link_error),
            Err(error) => Err(error),
        },
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both CStrings are NUL-terminated and remain alive for the
    // duration of the syscall; renameat2 does not retain their pointers.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_vendor = "apple")]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both CStrings are NUL-terminated and remain alive for the
    // duration of renamex_np, which does not retain their pointers.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    // std::fs::rename does not replace an existing destination on Windows.
    std::fs::rename(from, to)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn rename_no_replace(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

fn project_convert_schema(schema: Schema, columns: &[String]) -> std::io::Result<Schema> {
    if columns.is_empty() {
        return Ok(schema);
    }
    let by_name: std::collections::HashMap<&str, usize> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| (field.name().as_str(), index))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut fields = Vec::new();
    for name in columns {
        if name.is_empty() {
            return Err(invalid_schema("--column field name cannot be empty"));
        }
        let index = by_name.get(name.as_str()).copied().ok_or_else(|| {
            invalid_schema(format!(
                "--column '{}' not found in schema",
                fmt::safe(name)
            ))
        })?;
        if seen.insert(index) {
            fields.push(schema.fields()[index].as_ref().clone());
        }
    }
    Ok(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

fn parse_convert_columns(arguments: &[String]) -> std::io::Result<Vec<String>> {
    if arguments.is_empty() {
        return Ok(Vec::new());
    }
    let columns: Vec<String> = arguments
        .iter()
        .flat_map(|argument| parse_comma_list(argument))
        .collect();
    if columns.is_empty() {
        return Err(invalid_schema("--column field name cannot be empty"));
    }
    Ok(columns)
}

fn infer_json_schema_lossless<R: std::io::BufRead>(
    mut reader: R,
    columns: Option<&[String]>,
) -> Result<Schema, arrow::error::ArrowError> {
    use arrow::error::ArrowError;

    let columns: Option<std::collections::HashSet<&str>> =
        columns.map(|columns| columns.iter().map(String::as_str).collect());
    let mut inexact_f64_integer_paths = std::collections::BTreeSet::new();
    let mut inexact_f64_integer_path_index = JsonNumberPathTrie::default();
    let schema = {
        let mut line = String::new();
        let values = std::iter::from_fn(|| loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return None,
                Err(error) => {
                    return Some(Err(ArrowError::JsonError(format!(
                        "Failed to read JSON record: {error}"
                    ))));
                }
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => {
                    let record = line.trim();
                    let value = if columns.is_some() {
                        // Projection must not materialize or numerically validate
                        // unselected values: a caller can deliberately exclude an
                        // otherwise unsupported number.
                        parse_json_record_lossless(
                            record,
                            columns.as_ref(),
                            &mut inexact_f64_integer_paths,
                        )
                        .map_err(|error| ArrowError::JsonError(format!("Not valid JSON: {error}")))
                    } else {
                        match serde_json::from_str::<Value>(record) {
                            Ok(value) if !json_value_might_need_lossless_numbers(&value) => {
                                Ok(value)
                            }
                            Ok(value) => {
                                match scan_json_numbers(
                                    record,
                                    &mut inexact_f64_integer_paths,
                                    &mut inexact_f64_integer_path_index,
                                ) {
                                    Ok(()) => Ok(value),
                                    Err(JsonNumberScanError::Syntax) => parse_json_record_lossless(
                                        record,
                                        None,
                                        &mut inexact_f64_integer_paths,
                                    )
                                    .map_err(|error| {
                                        ArrowError::JsonError(format!("Not valid JSON: {error}"))
                                    }),
                                }
                            }
                            Err(error) => match parse_json_record_lossless(
                                record,
                                None,
                                &mut inexact_f64_integer_paths,
                            ) {
                                Err(lossless_error)
                                    if lossless_error
                                        .to_string()
                                        .contains("out of range for Float64") =>
                                {
                                    Err(ArrowError::JsonError(format!(
                                        "Not valid JSON: {lossless_error}"
                                    )))
                                }
                                Ok(_) | Err(_) => {
                                    Err(ArrowError::JsonError(format!("Not valid JSON: {error}")))
                                }
                            },
                        }
                    };
                    return Some(value);
                }
            }
        });
        arrow::json::reader::infer_json_schema_from_iterator(values)?
    };
    // Walk the inferred schema once: resolving every raw path independently is
    // quadratic for wide objects. Sibling fields can retain independent types.
    if let Some(path) = find_inexact_f64_path(&schema, &inexact_f64_integer_paths) {
        return Err(ArrowError::JsonError(format!(
            "numeric value in JSON field '{}' cannot be represented exactly as Float64",
            fmt::safe(&format_json_number_path(path))
        )));
    }
    Ok(schema)
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum JsonNumberPathSegment {
    Field(String),
    Element,
}

const MAX_JSON_NESTING_DEPTH: usize = 128;

fn collect_inexact_f64_integer_paths(
    raw: &RawValue,
    path: &mut Vec<JsonNumberPathSegment>,
    inexact: &mut std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
    parent_depth: usize,
) -> Result<(), String> {
    let literal = raw.get().trim();
    match literal.as_bytes().first().copied() {
        Some(b'{') => {
            if parent_depth == MAX_JSON_NESTING_DEPTH {
                return Err("recursion limit exceeded".to_string());
            }
            let mut deserializer = serde_json::Deserializer::from_str(literal);
            serde::Deserializer::deserialize_map(
                &mut deserializer,
                RawJsonObjectVisitor {
                    path,
                    inexact,
                    depth: parent_depth + 1,
                },
            )
            .map_err(|error| error.to_string())?;
        }
        Some(b'[') => {
            if parent_depth == MAX_JSON_NESTING_DEPTH {
                return Err("recursion limit exceeded".to_string());
            }
            let mut deserializer = serde_json::Deserializer::from_str(literal);
            serde::Deserializer::deserialize_seq(
                &mut deserializer,
                RawJsonArrayVisitor {
                    path,
                    inexact,
                    depth: parent_depth + 1,
                },
            )
            .map_err(|error| error.to_string())?;
        }
        Some(b'-' | b'0'..=b'9') => {
            let promoted = literal.parse::<f64>().map_err(|_| {
                format!(
                    "invalid JSON number at '{}'",
                    fmt::safe(&format_json_number_path(path))
                )
            })?;
            if !promoted.is_finite() {
                return Err(format!(
                    "numeric value in JSON field '{}' is out of range for Float64",
                    fmt::safe(&format_json_number_path(path))
                ));
            }
            if !literal
                .bytes()
                .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
                && is_inexact_f64_integer(literal)
            {
                inexact.insert(path.clone());
            }
        }
        _ => {}
    }
    Ok(())
}

struct RawJsonObjectVisitor<'path, 'inexact> {
    path: &'path mut Vec<JsonNumberPathSegment>,
    inexact: &'inexact mut std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
    depth: usize,
}

impl<'de> Visitor<'de> for RawJsonObjectVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<(), M::Error>
    where
        M: MapAccess<'de>,
    {
        while let Some(name) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            let raw = map.next_value::<&RawValue>()?;
            self.path
                .push(JsonNumberPathSegment::Field(name.into_owned()));
            collect_inexact_f64_integer_paths(raw, self.path, self.inexact, self.depth)
                .map_err(serde::de::Error::custom)?;
            self.path.pop();
        }
        Ok(())
    }
}

struct RawJsonArrayVisitor<'path, 'inexact> {
    path: &'path mut Vec<JsonNumberPathSegment>,
    inexact: &'inexact mut std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
    depth: usize,
}

impl<'de> Visitor<'de> for RawJsonArrayVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON array")
    }

    fn visit_seq<S>(self, mut sequence: S) -> Result<(), S::Error>
    where
        S: SeqAccess<'de>,
    {
        self.path.push(JsonNumberPathSegment::Element);
        while let Some(raw) = sequence.next_element::<&RawValue>()? {
            collect_inexact_f64_integer_paths(raw, self.path, self.inexact, self.depth)
                .map_err(serde::de::Error::custom)?;
        }
        self.path.pop();
        Ok(())
    }
}

fn json_integer_exceeds_exact_f64_range(literal: &str) -> bool {
    const MAX_EXACT_F64_INTEGER: &str = "9007199254740992";
    let digits = literal
        .strip_prefix('-')
        .unwrap_or(literal)
        .trim_start_matches('0');
    digits.len() > MAX_EXACT_F64_INTEGER.len()
        || (digits.len() == MAX_EXACT_F64_INTEGER.len() && digits > MAX_EXACT_F64_INTEGER)
}

fn json_value_might_need_lossless_numbers(value: &Value) -> bool {
    const MAX_EXACT_F64_INTEGER: u64 = 9_007_199_254_740_992;
    match value {
        Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(value), _) => value.unsigned_abs() > MAX_EXACT_F64_INTEGER,
            (_, Some(value)) => value > MAX_EXACT_F64_INTEGER,
            (None, None) => number
                .as_f64()
                .is_some_and(|value| value.abs() > MAX_EXACT_F64_INTEGER as f64),
        },
        Value::Array(values) => values.iter().any(json_value_might_need_lossless_numbers),
        Value::Object(record) => record.values().any(json_value_might_need_lossless_numbers),
        Value::Null | Value::Bool(_) | Value::String(_) => false,
    }
}

#[derive(Clone, Copy)]
enum JsonScanPathSegment {
    Field { start: usize, end: usize },
    Element,
}

#[derive(Debug)]
enum JsonNumberScanError {
    Syntax,
}

fn scan_json_numbers(
    record: &str,
    inexact: &mut std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
    inexact_index: &mut JsonNumberPathTrie,
) -> Result<(), JsonNumberScanError> {
    let mut scanner = JsonNumberScanner {
        bytes: record.as_bytes(),
        position: 0,
        path: Vec::new(),
        inexact,
        inexact_index,
        depth: 0,
    };
    scanner.scan_value()?;
    scanner.skip_whitespace();
    if scanner.position == scanner.bytes.len() {
        Ok(())
    } else {
        Err(JsonNumberScanError::Syntax)
    }
}

struct JsonNumberScanner<'record, 'inexact, 'index> {
    bytes: &'record [u8],
    position: usize,
    path: Vec<JsonScanPathSegment>,
    inexact: &'inexact mut std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
    inexact_index: &'index mut JsonNumberPathTrie,
    depth: usize,
}

impl JsonNumberScanner<'_, '_, '_> {
    fn scan_value(&mut self) -> Result<(), JsonNumberScanError> {
        self.skip_whitespace();
        match self.bytes.get(self.position).copied() {
            Some(b'{') => self.scan_object(),
            Some(b'[') => self.scan_array(),
            Some(b'"') => self.scan_string().map(|_| ()),
            Some(b'-' | b'0'..=b'9') => self.scan_number(),
            Some(b't') => self.scan_keyword(b"true"),
            Some(b'f') => self.scan_keyword(b"false"),
            Some(b'n') => self.scan_keyword(b"null"),
            _ => Err(JsonNumberScanError::Syntax),
        }
    }

    fn scan_object(&mut self) -> Result<(), JsonNumberScanError> {
        self.enter_container()?;
        let result = self.scan_object_contents();
        self.depth -= 1;
        result
    }

    fn scan_object_contents(&mut self) -> Result<(), JsonNumberScanError> {
        self.position += 1;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        loop {
            self.skip_whitespace();
            let (start, end) = self.scan_string()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(JsonNumberScanError::Syntax);
            }
            self.path.push(JsonScanPathSegment::Field { start, end });
            self.scan_value()?;
            self.path.pop();
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(JsonNumberScanError::Syntax);
            }
        }
    }

    fn scan_array(&mut self) -> Result<(), JsonNumberScanError> {
        self.enter_container()?;
        let result = self.scan_array_contents();
        self.depth -= 1;
        result
    }

    fn scan_array_contents(&mut self) -> Result<(), JsonNumberScanError> {
        self.position += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        self.path.push(JsonScanPathSegment::Element);
        loop {
            self.scan_value()?;
            self.skip_whitespace();
            if self.consume(b']') {
                self.path.pop();
                return Ok(());
            }
            if !self.consume(b',') {
                self.path.pop();
                return Err(JsonNumberScanError::Syntax);
            }
        }
    }

    fn enter_container(&mut self) -> Result<(), JsonNumberScanError> {
        if self.depth == MAX_JSON_NESTING_DEPTH {
            return Err(JsonNumberScanError::Syntax);
        }
        self.depth += 1;
        Ok(())
    }

    fn scan_string(&mut self) -> Result<(usize, usize), JsonNumberScanError> {
        let start = self.position;
        if !self.consume(b'"') {
            return Err(JsonNumberScanError::Syntax);
        }
        while let Some(byte) = self.bytes.get(self.position).copied() {
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok((start, self.position));
                }
                b'\\' => {
                    self.position += 1;
                    let escaped = self
                        .bytes
                        .get(self.position)
                        .copied()
                        .ok_or(JsonNumberScanError::Syntax)?;
                    self.position += 1;
                    if escaped == b'u' {
                        if self.position + 4 > self.bytes.len() {
                            return Err(JsonNumberScanError::Syntax);
                        }
                        self.position += 4;
                    }
                }
                0..=0x1f => return Err(JsonNumberScanError::Syntax),
                _ => self.position += 1,
            }
        }
        Err(JsonNumberScanError::Syntax)
    }

    fn scan_number(&mut self) -> Result<(), JsonNumberScanError> {
        let start = self.position;
        self.consume(b'-');
        match self.bytes.get(self.position).copied() {
            Some(b'0') => self.position += 1,
            Some(b'1'..=b'9') => {
                self.position += 1;
                while matches!(self.bytes.get(self.position), Some(b'0'..=b'9')) {
                    self.position += 1;
                }
            }
            _ => return Err(JsonNumberScanError::Syntax),
        }
        if self.consume(b'.') {
            let fraction_start = self.position;
            while matches!(self.bytes.get(self.position), Some(b'0'..=b'9')) {
                self.position += 1;
            }
            if self.position == fraction_start {
                return Err(JsonNumberScanError::Syntax);
            }
        }
        if matches!(self.bytes.get(self.position), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.bytes.get(self.position), Some(b'+' | b'-')) {
                self.position += 1;
            }
            let exponent_start = self.position;
            while matches!(self.bytes.get(self.position), Some(b'0'..=b'9')) {
                self.position += 1;
            }
            if self.position == exponent_start {
                return Err(JsonNumberScanError::Syntax);
            }
        }
        if !self
            .bytes
            .get(self.position)
            .is_none_or(|byte| matches!(*byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}'))
        {
            return Err(JsonNumberScanError::Syntax);
        }
        let literal = std::str::from_utf8(&self.bytes[start..self.position])
            .map_err(|_| JsonNumberScanError::Syntax)?;
        let integer = !literal
            .bytes()
            .any(|byte| matches!(byte, b'.' | b'e' | b'E'));
        if integer
            && json_integer_exceeds_exact_f64_range(literal)
            && !self
                .inexact_index
                .contains_scan_path(self.bytes, &self.path)?
            && is_inexact_f64_integer(literal)
        {
            let path = materialize_json_scan_path(self.bytes, &self.path)?;
            self.inexact_index.insert(&path);
            self.inexact.insert(path);
        }
        Ok(())
    }

    fn scan_keyword(&mut self, keyword: &[u8]) -> Result<(), JsonNumberScanError> {
        if self.bytes.get(self.position..self.position + keyword.len()) != Some(keyword) {
            return Err(JsonNumberScanError::Syntax);
        }
        self.position += keyword.len();
        if self
            .bytes
            .get(self.position)
            .is_none_or(|byte| matches!(*byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}'))
        {
            Ok(())
        } else {
            Err(JsonNumberScanError::Syntax)
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.bytes.get(self.position),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            self.position += 1;
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.bytes.get(self.position) == Some(&expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }
}

fn materialize_json_scan_path(
    bytes: &[u8],
    path: &[JsonScanPathSegment],
) -> Result<Vec<JsonNumberPathSegment>, JsonNumberScanError> {
    path.iter()
        .map(|segment| match segment {
            JsonScanPathSegment::Field { start, end } => json_scan_field_name(bytes, *start, *end)
                .map(std::borrow::Cow::into_owned)
                .map(JsonNumberPathSegment::Field),
            JsonScanPathSegment::Element => Ok(JsonNumberPathSegment::Element),
        })
        .collect()
}

fn json_scan_field_name(
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Result<std::borrow::Cow<'_, str>, JsonNumberScanError> {
    let raw = bytes.get(start..end).ok_or(JsonNumberScanError::Syntax)?;
    let inner = raw
        .get(1..raw.len().saturating_sub(1))
        .ok_or(JsonNumberScanError::Syntax)?;
    if !inner.contains(&b'\\') {
        return std::str::from_utf8(inner)
            .map(std::borrow::Cow::Borrowed)
            .map_err(|_| JsonNumberScanError::Syntax);
    }
    serde_json::from_slice::<String>(raw)
        .map(std::borrow::Cow::Owned)
        .map_err(|_| JsonNumberScanError::Syntax)
}

#[derive(Default)]
struct JsonNumberPathTrie {
    terminal: bool,
    fields: std::collections::HashMap<String, JsonNumberPathTrie>,
    element: Option<Box<JsonNumberPathTrie>>,
}

impl JsonNumberPathTrie {
    fn contains_scan_path(
        &self,
        bytes: &[u8],
        path: &[JsonScanPathSegment],
    ) -> Result<bool, JsonNumberScanError> {
        let mut node = self;
        for segment in path {
            node = match segment {
                JsonScanPathSegment::Field { start, end } => {
                    let name = json_scan_field_name(bytes, *start, *end)?;
                    let Some(child) = node.fields.get(name.as_ref()) else {
                        return Ok(false);
                    };
                    child
                }
                JsonScanPathSegment::Element => {
                    let Some(child) = node.element.as_deref() else {
                        return Ok(false);
                    };
                    child
                }
            };
        }
        Ok(node.terminal)
    }

    fn insert(&mut self, path: &[JsonNumberPathSegment]) {
        let mut node = self;
        for segment in path {
            node = match segment {
                JsonNumberPathSegment::Field(name) => node.fields.entry(name.clone()).or_default(),
                JsonNumberPathSegment::Element => node
                    .element
                    .get_or_insert_with(|| Box::new(JsonNumberPathTrie::default())),
            };
        }
        node.terminal = true;
    }
}

fn is_inexact_f64_integer(literal: &str) -> bool {
    if let Ok(exact) = literal.parse::<i128>() {
        return literal
            .parse::<f64>()
            .ok()
            .and_then(exact_i128_from_f64)
            .is_none_or(|promoted| promoted != exact);
    }

    // Fixed-point formatting recovers the exact integer represented by f64,
    // including large powers of two, without conservatively rejecting them.
    literal
        .parse::<f64>()
        .ok()
        .filter(|promoted| promoted.is_finite())
        .is_none_or(|promoted| format!("{promoted:.0}") != literal)
}

fn find_inexact_f64_path<'a>(
    schema: &Schema,
    inexact: &'a std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
) -> Option<&'a [JsonNumberPathSegment]> {
    let mut path = Vec::new();
    for field in schema.fields() {
        path.push(JsonNumberPathSegment::Field(field.name().clone()));
        let found = find_inexact_f64_path_in_type(field.data_type(), &mut path, inexact);
        path.pop();
        if found.is_some() {
            return found;
        }
    }
    None
}

fn find_inexact_f64_path_in_type<'a>(
    data_type: &DataType,
    path: &mut Vec<JsonNumberPathSegment>,
    inexact: &'a std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
) -> Option<&'a [JsonNumberPathSegment]> {
    match data_type {
        DataType::Float64 => inexact.get(path.as_slice()).map(Vec::as_slice),
        DataType::Struct(fields) => {
            for field in fields {
                path.push(JsonNumberPathSegment::Field(field.name().clone()));
                let found = find_inexact_f64_path_in_type(field.data_type(), path, inexact);
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        DataType::List(field)
        | DataType::ListView(field)
        | DataType::FixedSizeList(field, _)
        | DataType::LargeList(field)
        | DataType::LargeListView(field) => {
            path.push(JsonNumberPathSegment::Element);
            let found = find_inexact_f64_path_in_type(field.data_type(), path, inexact);
            path.pop();
            found
        }
        _ => None,
    }
}

fn format_json_number_path(path: &[JsonNumberPathSegment]) -> String {
    let mut formatted = String::new();
    for segment in path {
        match segment {
            JsonNumberPathSegment::Field(name) => {
                if !formatted.is_empty() {
                    formatted.push('.');
                }
                formatted.push_str(name);
            }
            JsonNumberPathSegment::Element => formatted.push_str("[]"),
        }
    }
    formatted
}

fn parse_json_record_lossless<'columns>(
    record: &str,
    columns: Option<&'columns std::collections::HashSet<&'columns str>>,
    inexact_f64_integer_paths: &mut std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
) -> serde_json::Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_str(record);
    let value = FilteredJsonRecordSeed {
        columns,
        inexact_f64_integer_paths,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

struct FilteredJsonRecordSeed<'columns, 'inexact> {
    columns: Option<&'columns std::collections::HashSet<&'columns str>>,
    inexact_f64_integer_paths: &'inexact mut std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
}

impl<'de> DeserializeSeed<'de> for FilteredJsonRecordSeed<'_, '_> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(FilteredJsonRecordVisitor {
            columns: self.columns,
            inexact_f64_integer_paths: self.inexact_f64_integer_paths,
        })
    }
}

struct FilteredJsonRecordVisitor<'columns, 'inexact> {
    columns: Option<&'columns std::collections::HashSet<&'columns str>>,
    inexact_f64_integer_paths: &'inexact mut std::collections::BTreeSet<Vec<JsonNumberPathSegment>>,
}

impl<'de> Visitor<'de> for FilteredJsonRecordVisitor<'_, '_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut projected = serde_json::Map::new();
        while let Some(name) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if self
                .columns
                .is_none_or(|columns| columns.contains(name.as_ref()))
            {
                let name = name.into_owned();
                let raw = map.next_value::<&RawValue>()?;
                let mut path = vec![JsonNumberPathSegment::Field(name.clone())];
                collect_inexact_f64_integer_paths(
                    raw,
                    &mut path,
                    self.inexact_f64_integer_paths,
                    1,
                )
                .map_err(serde::de::Error::custom)?;
                let value = serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                projected.insert(name, value);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Value::Object(projected))
    }
}

/// Mosaic cannot store Arrow `Null` columns, and JSON inference produces
/// `Null` for a field with no non-null value in the input — fail
/// with the column name instead of the writer's late "unsupported DataType".
fn reject_null_inferred_fields(schema: &Schema) -> std::io::Result<()> {
    for field in schema.fields() {
        if matches!(field.data_type(), DataType::Null) {
            return Err(invalid_schema(format!(
                "cannot infer a type for column '{}' (no non-null value in the records)",
                fmt::safe(field.name())
            )));
        }
    }
    Ok(())
}

fn is_json_input(input: &Path) -> bool {
    input
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "json" | "ndjson" | "jsonl"
            )
        })
}

fn ensure_regular_inferred_input(input: &Path, format: &str) -> std::io::Result<()> {
    let metadata = std::fs::metadata(input)?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(invalid_schema(format!(
            "{format} schema inference requires a regular file: {}",
            fmt::safe_path(input)
        )))
    }
}

fn ensure_can_write(out: &Path, overwrite: bool) -> std::io::Result<()> {
    if overwrite {
        return Ok(());
    }
    match std::fs::symlink_metadata(out) {
        Ok(_) => Err(output_exists_error(out)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn output_exists_error(out: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "{} exists (use --overwrite to replace)",
            fmt::safe_path(out)
        ),
    )
}

fn csv_format(options: &CsvConvertOptions) -> std::io::Result<arrow::csv::reader::Format> {
    let delimiter = parse_csv_byte(&options.delimiter, "delimiter")?;
    let escape = parse_optional_csv_byte(options.escape.as_deref(), "escape")?;
    let quote = parse_csv_byte(&options.quote, "quote")?;
    let format = arrow::csv::reader::Format::default()
        .with_header(!options.no_header && options.header.is_none())
        .with_delimiter(delimiter)
        .with_quote(quote);
    Ok(match escape {
        Some(escape) => format.with_escape(escape),
        None => format,
    })
}

fn parse_csv_byte(value: &str, name: &str) -> std::io::Result<u8> {
    let bytes = value.as_bytes();
    if bytes.len() == 1 {
        Ok(bytes[0])
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("--{name} must be exactly one byte"),
        ))
    }
}

fn parse_optional_csv_byte(value: Option<&str>, name: &str) -> std::io::Result<Option<u8>> {
    value.map(|value| parse_csv_byte(value, name)).transpose()
}

fn open_csv(path: &Path) -> std::io::Result<std::io::BufReader<std::fs::File>> {
    Ok(std::io::BufReader::new(std::fs::File::open(path)?))
}

fn skip_csv_lines<R: std::io::BufRead>(reader: &mut R, skip_lines: usize) -> std::io::Result<()> {
    for _ in 0..skip_lines {
        loop {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                return Ok(());
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(buffer.len(), |index| index + 1);
            reader.consume(consumed);
            if newline.is_some() {
                break;
            }
        }
    }
    Ok(())
}

const DEFAULT_CSV_BATCH_SIZE: usize = 1024;
const CSV_REPLAY_CELL_BUDGET: usize = 1 << 20;

fn csv_replay_batch_size(columns: usize) -> usize {
    CSV_REPLAY_CELL_BUDGET
        .checked_div(columns)
        .unwrap_or(DEFAULT_CSV_BATCH_SIZE)
        .clamp(1, DEFAULT_CSV_BATCH_SIZE)
}

fn build_csv_replay_reader<R: std::io::Read>(
    schema: Schema,
    format: arrow::csv::reader::Format,
    projection: Vec<usize>,
    reader: R,
) -> Result<arrow::csv::Reader<R>, arrow::error::ArrowError> {
    let batch_size = csv_replay_batch_size(schema.fields().len());
    arrow::csv::ReaderBuilder::new(Arc::new(schema))
        .with_format(format.with_truncated_rows(false))
        .with_batch_size(batch_size)
        .with_projection(projection)
        .build(reader)
}

struct CsvInputLayout {
    header: Option<Vec<String>>,
    columns: usize,
    has_records: bool,
}

fn inspect_csv_input<R: std::io::BufRead + std::io::Seek>(
    mut reader: R,
    options: &CsvConvertOptions,
    format: &arrow::csv::reader::Format,
) -> std::io::Result<Option<Schema>> {
    let layout = inspect_csv_layout(&mut reader, options)?;
    if !layout.has_records {
        return Ok(None);
    }

    let (schema, rows) = format
        .infer_schema(reader, None)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    // A shard with no data rows has nothing to infer from; it is skipped
    // when reading too.
    if rows == 0 || schema.fields().is_empty() {
        return Ok(None);
    }
    let schema =
        promote_second_precision_csv_timestamps(csv_schema_with_csv_names(schema, options)?);
    Ok(Some(schema))
}

fn inspect_csv_layout<R: std::io::BufRead + std::io::Seek>(
    reader: &mut R,
    options: &CsvConvertOptions,
) -> std::io::Result<CsvInputLayout> {
    skip_csv_lines(reader, options.skip_lines)?;
    let layout = csv_input_layout(&mut *reader, options)?;
    reader.seek(std::io::SeekFrom::Start(0))?;
    skip_csv_lines(reader, options.skip_lines)?;
    Ok(layout)
}

fn csv_input_layout<R: std::io::Read>(
    reader: R,
    options: &CsvConvertOptions,
) -> std::io::Result<CsvInputLayout> {
    let delimiter = parse_csv_byte(&options.delimiter, "delimiter")?;
    let escape = parse_optional_csv_byte(options.escape.as_deref(), "escape")?;
    let quote = parse_csv_byte(&options.quote, "quote")?;
    let mut builder = csv::ReaderBuilder::new();
    builder
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .quote(quote)
        .escape(escape);
    let mut reader = builder.from_reader(reader);
    let mut records = reader.records();
    let (header, has_records) = if let Some(header) = &options.header {
        (
            Some(parse_csv_header(header, options)?),
            records
                .next()
                .transpose()
                .map_err(|e| invalid_schema(format!("invalid CSV record: {e}")))?
                .is_some(),
        )
    } else if options.no_header {
        (
            None,
            records
                .next()
                .transpose()
                .map_err(|e| invalid_schema(format!("invalid CSV record: {e}")))?
                .is_some(),
        )
    } else {
        match records.next() {
            Some(record) => {
                let record =
                    record.map_err(|e| invalid_schema(format!("invalid CSV header: {e}")))?;
                let header: Vec<String> = record.iter().map(ToString::to_string).collect();
                let has_records = records
                    .next()
                    .transpose()
                    .map_err(|e| invalid_schema(format!("invalid CSV record: {e}")))?
                    .is_some();
                if has_records {
                    validate_csv_header_names(&header)?;
                }
                (Some(header), has_records)
            }
            None => (Some(Vec::new()), false),
        }
    };
    let columns = header.as_ref().map_or(0, Vec::len);
    Ok(CsvInputLayout {
        header,
        columns,
        has_records,
    })
}

fn csv_schema_index(schema: &Schema) -> std::collections::HashMap<&str, usize> {
    schema
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| (field.name().as_str(), index))
        .collect()
}

fn csv_reader_schema(
    output_schema: &Schema,
    schema_index: &std::collections::HashMap<&str, usize>,
    layout: &CsvInputLayout,
) -> Schema {
    let positional = layout.header.is_none();
    let columns = if positional {
        layout.columns.max(output_schema.fields().len())
    } else {
        layout.columns
    };
    let fields: Vec<Field> = (0..columns)
        .map(|i| {
            let source = if let Some(header) = &layout.header {
                header
                    .get(i)
                    .and_then(|name| schema_index.get(name.as_str()).copied())
            } else {
                (i < output_schema.fields().len()).then_some(i)
            };
            if let Some(source) = source {
                let output_field = output_schema.fields()[source].as_ref();
                let data_type = match output_field.data_type() {
                    // Arrow's Float64 decoder would round bare integer tokens
                    // before Mosaic can enforce exactness. Preserve raw text
                    // for inferred Float64 and local timestamp fields.
                    DataType::Float64 | DataType::Timestamp(_, _) => DataType::Utf8,
                    data_type => data_type.clone(),
                };
                // Read as nullable: not-null enforcement happens when the batch
                // is re-attached to the output schema, where the error carries
                // the real column name rather than a positional one.
                output_field
                    .clone()
                    .with_data_type(data_type)
                    .with_name(format!("field_{i}"))
                    .with_nullable(true)
            } else {
                Field::new(format!("field_{i}"), DataType::Utf8, true)
            }
        })
        .collect();
    Schema::new(fields)
}

fn csv_output_mapping(
    output_schema: &Schema,
    schema_index: &std::collections::HashMap<&str, usize>,
    layout: &CsvInputLayout,
) -> Vec<Option<usize>> {
    if let Some(header) = &layout.header {
        let mut mapping = vec![None; output_schema.fields().len()];
        for (csv_index, name) in header.iter().enumerate() {
            if let Some(field_index) = schema_index.get(name.as_str()).copied() {
                mapping[field_index] = Some(csv_index);
            }
        }
        mapping
    } else {
        (0..output_schema.fields().len()).map(Some).collect()
    }
}

fn csv_projection(mapping: &[Option<usize>]) -> (Vec<usize>, Vec<Option<usize>>) {
    let mut projection = Vec::new();
    let projected_mapping = mapping
        .iter()
        .map(|source| {
            source.map(|source| {
                let projected = projection.len();
                projection.push(source);
                projected
            })
        })
        .collect();
    (projection, projected_mapping)
}

/// A schema field absent from the CSV header becomes an all-null column, so
/// refuse the conversions that can only be mistakes: a header matching no
/// schema field at all, and a required field that the data cannot supply.
fn validate_csv_mapping(
    schema: &Schema,
    layout: &CsvInputLayout,
    mapping: &[Option<usize>],
    input: &Path,
) -> std::io::Result<()> {
    if layout.header.is_some() && !schema.fields().is_empty() && mapping.iter().all(Option::is_none)
    {
        return Err(invalid_schema(format!(
            "none of the schema fields were found in the CSV header of {}; use --no-header if the file has no header row",
            fmt::safe_path(input)
        )));
    }
    for (field, index) in schema.fields().iter().zip(mapping) {
        if index.is_none() && !field.is_nullable() {
            return Err(invalid_schema(format!(
                "required field '{}' was not found in the CSV header of {}",
                fmt::safe(field.name()),
                fmt::safe_path(input)
            )));
        }
    }
    Ok(())
}

fn align_csv_batch_to_schema(
    batch: RecordBatch,
    schema: &Schema,
    mapping: &[Option<usize>],
    input: &Path,
) -> std::io::Result<RecordBatch> {
    let (_, source_columns, rows) = batch.into_parts();
    let mut source_columns: Vec<Option<ArrayRef>> = source_columns.into_iter().map(Some).collect();
    let columns: Vec<ArrayRef> = schema
        .fields()
        .iter()
        .zip(mapping)
        .map(|(field, index)| match *index {
            Some(index) => {
                let source = source_columns[index]
                    .take()
                    .ok_or_else(|| invalid_schema("invalid duplicate CSV projection"))?;
                if source.data_type() != &DataType::Utf8 {
                    Ok(source)
                } else {
                    match field.data_type() {
                        DataType::Float64 => parse_inferred_csv_float64(&source, field, input),
                        DataType::Timestamp(_, _) => {
                            parse_inferred_csv_timestamp_column(&source, field, input)
                        }
                        _ => Ok(source),
                    }
                }
            }
            None => Ok(new_null_array(field.data_type(), rows)),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    RecordBatch::try_new(Arc::new(schema.clone()), columns)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

fn parse_inferred_csv_timestamp_column(
    array: &ArrayRef,
    field: &Field,
    input: &Path,
) -> std::io::Result<ArrayRef> {
    let values = array
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| invalid_schema("expected a Utf8 CSV timestamp column"))?;
    let DataType::Timestamp(unit, timezone) = field.data_type() else {
        return Err(invalid_schema("expected a Timestamp CSV field"));
    };
    let parser_timezone: Tz = timezone
        .as_deref()
        .unwrap_or("+00:00")
        .parse()
        .map_err(|e| invalid_schema(format!("invalid timestamp timezone: {e}")))?;
    let values = values
        .iter()
        .map(|value| {
            let Some(value) = value else {
                return Ok(None);
            };
            parse_csv_timestamp_value(
                value,
                field,
                unit,
                &parser_timezone,
                timezone.is_none(),
                input,
            )
            .map(Some)
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    csv_timestamp_array(values, unit, timezone.clone())
}

fn parse_csv_timestamp_value(
    value: &str,
    field: &Field,
    unit: &TimeUnit,
    parser_timezone: &Tz,
    reject_timezone: bool,
    input: &Path,
) -> std::io::Result<i64> {
    if reject_timezone && timestamp_has_explicit_timezone(value) {
        return Err(invalid_schema(format!(
            "CSV field '{}' in {} must not include a timezone for an inferred local timestamp",
            fmt::safe(field.name()),
            fmt::safe_path(input)
        )));
    }
    let parse_error = || {
        invalid_schema(format!(
            "cannot parse '{}' as {} for CSV field '{}' in {}",
            fmt::safe(value),
            field.data_type(),
            fmt::safe(field.name()),
            fmt::safe_path(input)
        ))
    };
    let datetime = string_to_datetime(parser_timezone, value).map_err(|_| parse_error())?;
    match unit {
        TimeUnit::Millisecond => Ok(datetime.timestamp_millis()),
        TimeUnit::Microsecond => Ok(datetime.timestamp_micros()),
        TimeUnit::Nanosecond => datetime.timestamp_nanos_opt().ok_or_else(parse_error),
        unit => Err(invalid_schema(format!(
            "CSV conversion does not support timestamp unit {unit:?}"
        ))),
    }
}

fn csv_timestamp_array(
    values: Vec<Option<i64>>,
    unit: &TimeUnit,
    timezone: Option<Arc<str>>,
) -> std::io::Result<ArrayRef> {
    Ok(match unit {
        TimeUnit::Millisecond => Arc::new(
            PrimitiveArray::<TimestampMillisecondType>::from(values)
                .with_timezone_opt(timezone.clone()),
        ),
        TimeUnit::Microsecond => Arc::new(
            PrimitiveArray::<TimestampMicrosecondType>::from(values)
                .with_timezone_opt(timezone.clone()),
        ),
        TimeUnit::Nanosecond => Arc::new(
            PrimitiveArray::<TimestampNanosecondType>::from(values).with_timezone_opt(timezone),
        ),
        unit => {
            return Err(invalid_schema(format!(
                "CSV conversion does not support timestamp unit {unit:?}"
            )));
        }
    })
}

fn parse_inferred_csv_float64(
    array: &ArrayRef,
    field: &Field,
    input: &Path,
) -> std::io::Result<ArrayRef> {
    let values = array
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| invalid_schema("expected a Utf8 CSV column"))?;
    let values = values
        .iter()
        .map(|value| {
            let Some(value) = value else {
                return Ok(None);
            };
            let promoted = Float64Type::parse(value)
                .ok_or_else(|| csv_inferred_float_parse_error(value, field, input))?;
            if !promoted.is_finite() {
                return Err(if is_non_finite_float_literal(value) {
                    csv_non_finite_float_error(value, field, input)
                } else {
                    csv_inferred_float_parse_error(value, field, input)
                });
            }
            let unsigned = value
                .strip_prefix('+')
                .or_else(|| value.strip_prefix('-'))
                .unwrap_or(value);
            let integer_token =
                !unsigned.is_empty() && unsigned.bytes().all(|byte| byte.is_ascii_digit());
            if integer_token {
                let exact = value
                    .parse::<i128>()
                    .map_err(|_| csv_inferred_float_parse_error(value, field, input))?;
                if exact_i128_from_f64(promoted) != Some(exact) {
                    return Err(csv_inferred_float_parse_error(value, field, input));
                }
            }
            Ok(Some(promoted))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    Ok(Arc::new(
        values.into_iter().collect::<PrimitiveArray<Float64Type>>(),
    ))
}

fn exact_i128_from_f64(value: f64) -> Option<i128> {
    let upper_exclusive = 2.0_f64.powi(127);
    if !value.is_finite() || value >= upper_exclusive || value < -upper_exclusive {
        return None;
    }
    let integer = value as i128;
    (integer as f64 == value).then_some(integer)
}

fn is_non_finite_float_literal(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "nan" | "+nan" | "-nan" | "inf" | "+inf" | "-inf" | "infinity" | "+infinity" | "-infinity"
    )
}

fn csv_inferred_float_parse_error(value: &str, field: &Field, input: &Path) -> std::io::Error {
    invalid_schema(format!(
        "numeric value '{}' in CSV field '{}' of {} cannot be represented exactly as Float64",
        fmt::safe(value),
        fmt::safe(field.name()),
        fmt::safe_path(input)
    ))
}

fn csv_non_finite_float_error(value: &str, field: &Field, input: &Path) -> std::io::Error {
    invalid_schema(format!(
        "non-finite Float64 value '{}' in CSV field '{}' of {} is not supported",
        fmt::safe(value),
        fmt::safe(field.name()),
        fmt::safe_path(input)
    ))
}

fn timestamp_has_explicit_timezone(value: &str) -> bool {
    let bytes = value.trim().as_bytes();
    if bytes.len() <= 10 {
        return false;
    }
    let after_date = &bytes[10..];
    matches!(after_date.last(), Some(b'Z' | b'z'))
        || after_date.iter().any(|b| matches!(b, b'+' | b'-'))
}

fn csv_schema_with_csv_names(
    schema: Schema,
    options: &CsvConvertOptions,
) -> std::io::Result<Schema> {
    let names = if let Some(header) = &options.header {
        Some(parse_csv_header(header, options)?)
    } else if options.no_header {
        Some(
            (0..schema.fields().len())
                .map(|i| format!("field_{i}"))
                .collect(),
        )
    } else {
        None
    };
    let Some(names) = names else {
        return Ok(schema);
    };
    if names.len() != schema.fields().len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "CSV header has {} fields but inferred schema has {} fields",
                names.len(),
                schema.fields().len()
            ),
        ));
    }
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .zip(names)
        .map(|(field, name)| field.as_ref().clone().with_name(name))
        .collect();
    Ok(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

fn parse_csv_header(header: &str, options: &CsvConvertOptions) -> std::io::Result<Vec<String>> {
    let delimiter = parse_csv_byte(&options.delimiter, "delimiter")?;
    let escape = parse_optional_csv_byte(options.escape.as_deref(), "escape")?;
    let quote = parse_csv_byte(&options.quote, "quote")?;
    let mut builder = csv::ReaderBuilder::new();
    builder
        .has_headers(false)
        .delimiter(delimiter)
        .quote(quote)
        .escape(escape);
    let mut reader = builder.from_reader(header.as_bytes());
    let mut records = reader.records();
    let record = records
        .next()
        .ok_or_else(|| invalid_schema("--header must contain at least one field"))?
        .map_err(|e| invalid_schema(format!("invalid --header CSV: {e}")))?;
    if let Some(next) = records.next() {
        next.map_err(|e| invalid_schema(format!("invalid --header CSV: {e}")))?;
        return Err(invalid_schema(
            "--header must contain exactly one CSV record",
        ));
    }
    let header: Vec<String> = record.iter().map(ToString::to_string).collect();
    validate_csv_header_names(&header)?;
    Ok(header)
}

fn validate_csv_header_names(header: &[String]) -> std::io::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for name in header {
        if name.is_empty() {
            return Err(invalid_schema("empty column name"));
        }
        if !seen.insert(name.as_str()) {
            return Err(invalid_schema(format!(
                "duplicate CSV header field '{}'",
                fmt::safe(name)
            )));
        }
    }
    Ok(())
}

fn csv_schema_with_null_fallback(schema: Schema) -> Schema {
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|field| {
            let field = field.as_ref().clone();
            if matches!(field.data_type(), DataType::Null) {
                field.with_data_type(DataType::Utf8)
            } else {
                field
            }
        })
        .collect();
    Schema::new_with_metadata(fields, schema.metadata().clone())
}

fn promote_second_precision_csv_timestamps(schema: Schema) -> Schema {
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|field| {
            let field = field.as_ref().clone();
            match field.data_type().clone() {
                DataType::Timestamp(TimeUnit::Second, timezone) => {
                    field.with_data_type(DataType::Timestamp(TimeUnit::Millisecond, timezone))
                }
                _ => field,
            }
        })
        .collect();
    Schema::new_with_metadata(fields, schema.metadata().clone())
}

fn merge_csv_inferred_schema(
    prev: Schema,
    next: Schema,
    input: &Path,
    merge_by_name: bool,
) -> std::io::Result<Schema> {
    if !merge_by_name && prev.fields().len() != next.fields().len() {
        return Err(csv_schema_mismatch(input));
    }
    let next_fields: std::collections::HashMap<&str, &Field> = next
        .fields()
        .iter()
        .map(|field| (field.name().as_str(), field.as_ref()))
        .collect();
    let prev_names: std::collections::HashSet<&str> = prev
        .fields()
        .iter()
        .map(|field| field.name().as_str())
        .collect();
    let mut fields: Vec<Field> = prev
        .fields()
        .iter()
        .map(
            |left| match next_fields.get(left.name().as_str()).copied() {
                Some(right) => merge_csv_inferred_field(left.as_ref(), right, input),
                None if merge_by_name => Ok(left.as_ref().clone().with_nullable(true)),
                None => Err(csv_schema_mismatch(input)),
            },
        )
        .collect::<std::io::Result<_>>()?;
    if merge_by_name {
        fields.extend(
            next.fields()
                .iter()
                .filter(|field| !prev_names.contains(field.name().as_str()))
                .map(|field| field.as_ref().clone().with_nullable(true)),
        );
    }
    Ok(Schema::new_with_metadata(fields, prev.metadata().clone()))
}

fn merge_csv_inferred_field(left: &Field, right: &Field, input: &Path) -> std::io::Result<Field> {
    if left.name() != right.name() {
        return Err(csv_schema_mismatch(input));
    }
    let nullable = left.is_nullable() || right.is_nullable();
    let field = match (left.data_type(), right.data_type()) {
        (left_type, right_type) if left_type == right_type => left.clone().with_nullable(nullable),
        (DataType::Null, _) => right.clone().with_nullable(true),
        (_, DataType::Null) => left.clone().with_nullable(true),
        (DataType::Int64, DataType::Float64) | (DataType::Float64, DataType::Int64) => left
            .clone()
            .with_data_type(DataType::Float64)
            .with_nullable(nullable),
        (
            DataType::Timestamp(left_unit, left_timezone),
            DataType::Timestamp(right_unit, right_timezone),
        ) if left_timezone == right_timezone => left
            .clone()
            .with_data_type(DataType::Timestamp(
                finer_timestamp_unit(left_unit, right_unit),
                left_timezone.clone(),
            ))
            .with_nullable(nullable),
        _ => return Err(csv_schema_mismatch(input)),
    };
    Ok(field)
}

fn finer_timestamp_unit(left: &TimeUnit, right: &TimeUnit) -> TimeUnit {
    match (left, right) {
        (TimeUnit::Nanosecond, _) | (_, TimeUnit::Nanosecond) => TimeUnit::Nanosecond,
        (TimeUnit::Microsecond, _) | (_, TimeUnit::Microsecond) => TimeUnit::Microsecond,
        (TimeUnit::Millisecond, _) | (_, TimeUnit::Millisecond) => TimeUnit::Millisecond,
        _ => TimeUnit::Second,
    }
}

fn csv_schema_mismatch(input: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "{} has a schema that is incompatible with the other CSV inputs",
            fmt::safe_path(input)
        ),
    )
}

fn apply_required_fields(schema: Schema, required_fields: &[String]) -> std::io::Result<Schema> {
    if required_fields.is_empty() {
        return Ok(schema);
    }
    let schema_names: std::collections::HashSet<&str> = schema
        .fields()
        .iter()
        .map(|field| field.name().as_str())
        .collect();
    let mut required = std::collections::HashSet::with_capacity(required_fields.len());
    for name in required_fields {
        if name.is_empty() {
            return Err(invalid_schema("--require field name cannot be empty"));
        }
        if !schema_names.contains(name.as_str()) {
            return Err(invalid_schema(format!(
                "--require column '{}' not found in schema",
                fmt::safe(name)
            )));
        }
        required.insert(name.as_str());
    }
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|field| {
            let field = field.as_ref().clone();
            if required.contains(field.name().as_str()) {
                field.with_nullable(false)
            } else {
                field
            }
        })
        .collect();
    Ok(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

fn invalid_schema(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg.into())
}

fn cat(
    file: &Path,
    num: usize,
    columns: Option<String>,
    filter: Option<String>,
    json: bool,
) -> std::io::Result<()> {
    let mut reader = open(file)?;
    let pred = filter
        .as_deref()
        .map(filter::parse_where)
        .transpose()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let pred_col = match &pred {
        Some(p) => Some(
            reader
                .schema()
                .columns
                .iter()
                .position(|c| c.name == p.column)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("--where: column '{}' not found", p.column),
                    )
                })?,
        ),
        None => None,
    };
    // The display columns; the filter column is read even if projected out, then
    // dropped before printing, so `--where` works on a hidden column.
    let mut display: Vec<String> = Vec::new();
    if let Some(list) = &columns {
        display = parse_comma_list(list);
        let mut read: Vec<&str> = display.iter().map(String::as_str).collect();
        if let Some(p) = &pred {
            if !read.contains(&p.column.as_str()) {
                read.push(&p.column);
            }
        }
        reader.project(&read)?;
    }
    // Column index of the filter target, for stats-based row-group skipping.
    let bounded_text = !json && num != usize::MAX;
    let mut batches: Vec<RecordBatch> = Vec::new();
    let mut got = 0usize;
    let mut printed_any = false;
    for rg in 0..reader.num_row_groups() {
        if got >= num {
            break;
        }
        // Pushdown: skip a row group when its min/max prove no row can match.
        if let (Some(p), Some(ci)) = (&pred, pred_col) {
            if let Some(st) = reader
                .row_group_stats(rg)?
                .iter()
                .find(|s| s.column_index == ci)
            {
                if filter::stats_exclude(p, &st.min, &st.max) {
                    continue;
                }
            }
        }
        let mut batch = reader.row_group_reader(rg)?.read_columns()?;
        if let Some(p) = &pred {
            batch = filter::apply_where(&batch, p)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        }
        // Drop the filter-only column so it isn't printed when -c excluded it.
        if !display.is_empty() {
            let keep: Vec<usize> = display
                .iter()
                .filter_map(|n| batch.schema().index_of(n).ok())
                .collect();
            batch = batch
                .project(&keep)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        let batch_rows = batch.num_rows();
        // JSON rows are independent, so stream each group out instead of holding
        // every batch. Text output only buffers for bounded requests so global
        // column widths can be computed; unbounded `cat` prints one table per
        // row group and stays bounded.
        if json {
            print!("{}", fmt::ndjson(&[batch], num - got)?);
            printed_any = printed_any || batch_rows > 0;
        } else if bounded_text {
            batches.push(batch);
        } else if batch_rows > 0 {
            print!("{}", fmt::pretty_table(&[batch], usize::MAX));
            printed_any = true;
        }
        got += batch_rows;
    }
    if json {
        // (no rows) stays silent for JSON; nothing to print.
    } else if bounded_text {
        if batches.iter().all(|b| b.num_rows() == 0) {
            println!("(no rows)");
        } else {
            print!("{}", fmt::pretty_table(&batches, num));
        }
    } else if !printed_any {
        println!("(no rows)");
    }
    Ok(())
}

fn footer(file: &Path, json: bool) -> std::io::Result<()> {
    use paimon_mosaic_core::spec::{COMPRESSION_ZSTD, MAGIC, VERSION};
    let reader = open(file)?;
    let s = reader.schema();
    let comp = if reader.compression() == COMPRESSION_ZSTD {
        "zstd"
    } else {
        "none"
    };
    let magic = std::str::from_utf8(&MAGIC).unwrap_or("MOSA");
    if json {
        println!(
            "{}",
            jsonout::line(&jsonout::Footer {
                magic: magic.to_string(),
                version: VERSION as u32,
                buckets: s.num_buckets,
                row_groups: reader.num_row_groups(),
                compression: comp.to_string(),
            })
        );
    } else {
        println!(
            "magic={} version={} buckets={} row_groups={} compression={}",
            magic,
            VERSION,
            s.num_buckets,
            reader.num_row_groups(),
            comp
        );
    }
    Ok(())
}

fn column_size(file: &Path, columns: Option<String>, json: bool) -> std::io::Result<()> {
    let reader = open(file)?;
    let s = reader.schema();
    let want = col_filter(&columns, s)?;
    let mut bytes = vec![0usize; s.columns.len()];
    let mut approx = vec![false; s.columns.len()];
    for rg in 0..reader.num_row_groups() {
        // Paged buckets store each column in its own slot → exact per-column bytes.
        // Read slot sizes from the directory only (no slot decode/decompress).
        for (ci, sz) in reader.slot_sizes(rg)?.into_iter().enumerate() {
            bytes[ci] += sz;
        }
        // Monolithic buckets are one blob; split evenly and mark approximate when
        // more than one column shares the bucket (a single-column bucket is exact).
        for b in reader.bucket_infos(rg)? {
            if b.kind != paimon_mosaic_core::reader::BucketKind::Monolithic || b.columns.is_empty()
            {
                continue;
            }
            split_evenly(b.size, &b.columns, &mut bytes);
            if b.columns.len() > 1 {
                for &c in &b.columns {
                    approx[c] = true;
                }
            }
        }
    }
    let cols: Vec<usize> = original_order(s)
        .into_iter()
        .filter(|&i| selected(&want, &s.columns[i].name))
        .collect();
    let comp: usize = cols.iter().map(|&i| bytes[i]).sum();
    let any_approx = cols.iter().any(|&i| approx[i]);
    if json {
        let columns = cols
            .iter()
            .map(|&i| jsonout::ColumnBytes {
                column: s.columns[i].name.clone(),
                bytes: bytes[i],
                approximate: approx[i],
            })
            .collect();
        println!(
            "{}",
            jsonout::line(&jsonout::ColumnSize {
                columns,
                total_bytes: comp,
            })
        );
    } else {
        for i in cols {
            println!(
                "  {}: {} B{}",
                fmt::safe(&s.columns[i].name),
                bytes[i],
                if approx[i] { " (approx)" } else { "" }
            );
        }
        println!(
            "  total: {} B{}",
            comp,
            if any_approx {
                " (some columns approximate)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn dictionary(file: &Path, column: &str, json: bool) -> std::io::Result<()> {
    let reader = open(file)?;
    let col = reader
        .schema()
        .columns
        .iter()
        .position(|c| c.name == column)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("column '{column}' not found"),
            )
        })?;
    // For nested columns the first physical slot is the ARRAY/MAP length column,
    // not the logical values — its dictionary would mislead. Only primitive
    // leaves have a meaningful one, so reject List/Map rather than print junk.
    use arrow::datatypes::DataType;
    if matches!(
        reader.schema().columns[col].data_type,
        DataType::List(_) | DataType::LargeList(_) | DataType::Map(_, _)
    ) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("dictionary: column '{column}' is nested; only primitive columns supported"),
        ));
    }
    if json {
        let mut row_groups = Vec::new();
        for rg in 0..reader.num_row_groups() {
            row_groups.push(
                reader
                    .dictionary(rg, col)?
                    .map(|vals| vals.iter().map(fmt::render_json).collect()),
            );
        }
        println!(
            "{}",
            jsonout::line(&jsonout::Dictionary {
                column: column.to_string(),
                row_groups,
            })
        );
        return Ok(());
    }
    for rg in 0..reader.num_row_groups() {
        match reader.dictionary(rg, col)? {
            Some(vals) => {
                println!("row group {rg}: {} entries", vals.len());
                for (i, v) in vals.iter().enumerate() {
                    println!("    {i}: {}", fmt::render_value(v));
                }
            }
            None => println!("row group {rg}: not dict-encoded"),
        }
    }
    Ok(())
}

fn buckets(file: &Path, json: bool) -> std::io::Result<()> {
    let reader = open(file)?;
    let s = reader.schema();
    let raw_name = |i: usize| s.columns[i].name.clone();
    let text_name = |i: usize| fmt::safe(&s.columns[i].name);
    let mut rgs = Vec::new();
    for rg in 0..reader.num_row_groups() {
        let infos = reader.bucket_infos(rg)?;
        if json {
            let items = infos
                .iter()
                .map(|b| jsonout::Bucket {
                    bucket: b.bucket,
                    kind: fmt::bucket_kind(b.kind),
                    size: b.size,
                    uncompressed: b.uncompressed,
                    columns: b.columns.iter().map(|&i| raw_name(i)).collect(),
                })
                .collect();
            rgs.push(items);
        } else {
            println!("row group {rg}:");
            for b in &infos {
                let cols: Vec<String> = b.columns.iter().map(|&i| text_name(i)).collect();
                println!(
                    "    bucket {}: {} {}B{} [{}]",
                    b.bucket,
                    fmt::bucket_kind(b.kind),
                    b.size,
                    fmt::ratio(b.size, b.uncompressed),
                    cols.join(", ")
                );
            }
        }
    }
    if json {
        println!("{}", jsonout::line(&jsonout::Buckets { row_groups: rgs }));
    }
    Ok(())
}

#[cfg(test)]
mod install_tests {
    use super::*;

    #[test]
    fn lossless_json_parser_rejects_out_of_range_value() {
        let mut inexact = std::collections::BTreeSet::new();

        let error =
            parse_json_record_lossless(r#"{"value":1e400}"#, None, &mut inexact).unwrap_err();

        let error = error.to_string();
        assert!(error.contains("out of range for Float64"), "{error}");
        assert!(inexact.is_empty());
    }

    #[test]
    fn json_number_scanner_records_nested_inexact_integer_paths() {
        let mut inexact = std::collections::BTreeSet::new();
        let mut inexact_index = JsonNumberPathTrie::default();

        for _ in 0..2 {
            scan_json_numbers(
                r#"{"integer":9007199254740993,"nested":[1.5,9007199254740993]}"#,
                &mut inexact,
                &mut inexact_index,
            )
            .unwrap();
        }

        assert_eq!(
            inexact,
            std::collections::BTreeSet::from([
                vec![JsonNumberPathSegment::Field("integer".to_string())],
                vec![
                    JsonNumberPathSegment::Field("nested".to_string()),
                    JsonNumberPathSegment::Element,
                ],
            ])
        );
    }

    #[test]
    fn json_value_fast_gate_only_scans_large_numbers() {
        let common =
            serde_json::from_str::<Value>(r#"{"id":9007199254740992,"v":1.5,"s":"1e400"}"#)
                .unwrap();
        let large_integer = serde_json::from_str::<Value>(r#"{"id":9007199254740993}"#).unwrap();
        let negative_out_of_i64 =
            serde_json::from_str::<Value>(r#"{"id":-9223372036854775809}"#).unwrap();
        let large_float = serde_json::from_str::<Value>(r#"{"value":1e308}"#).unwrap();

        assert!(!json_value_might_need_lossless_numbers(&common));
        assert!(json_value_might_need_lossless_numbers(&large_integer));
        assert!(json_value_might_need_lossless_numbers(&negative_out_of_i64));
        assert!(json_value_might_need_lossless_numbers(&large_float));
    }

    #[test]
    fn csv_layout_inspection_rewinds_reader_after_skip_lines() {
        let options = CsvConvertOptions {
            delimiter: ",".to_string(),
            escape: None,
            quote: "\"".to_string(),
            no_header: false,
            header: None,
            skip_lines: 1,
        };
        let format = csv_format(&options).unwrap();
        let reader = std::io::BufReader::new(std::io::Cursor::new(
            b"ignored preamble\nid,name\n1,alice\n2,bob\n".to_vec(),
        ));

        let schema = inspect_csv_input(reader, &options, &format)
            .unwrap()
            .unwrap();

        assert_eq!(schema.fields().len(), 2);
        assert_eq!(schema.field(0).name(), "id");
        assert_eq!(schema.field(1).name(), "name");
    }

    #[test]
    fn inexact_json_path_only_matches_float64_schema_fields() {
        let schema = Schema::new(vec![
            Field::new("integer", DataType::Int64, true),
            Field::new("float", DataType::Float64, true),
        ]);
        let inexact = std::collections::BTreeSet::from([
            vec![JsonNumberPathSegment::Field("integer".to_string())],
            vec![JsonNumberPathSegment::Field("float".to_string())],
        ]);

        let path = find_inexact_f64_path(&schema, &inexact).unwrap();

        assert_eq!(format_json_number_path(path), "float");
    }

    #[test]
    fn inexact_json_path_traverses_nested_structs_and_lists() {
        let schema = Schema::new(vec![Field::new(
            "payload",
            DataType::Struct(
                vec![
                    Field::new(
                        "integers",
                        DataType::List(Arc::new(Field::new("item", DataType::Int64, true))),
                        true,
                    ),
                    Field::new(
                        "floats",
                        DataType::List(Arc::new(Field::new("item", DataType::Float64, true))),
                        true,
                    ),
                ]
                .into(),
            ),
            true,
        )]);
        let inexact = std::collections::BTreeSet::from([
            vec![
                JsonNumberPathSegment::Field("payload".to_string()),
                JsonNumberPathSegment::Field("integers".to_string()),
                JsonNumberPathSegment::Element,
            ],
            vec![
                JsonNumberPathSegment::Field("payload".to_string()),
                JsonNumberPathSegment::Field("floats".to_string()),
                JsonNumberPathSegment::Element,
            ],
        ]);

        let path = find_inexact_f64_path(&schema, &inexact).unwrap();

        assert_eq!(format_json_number_path(path), "payload.floats[]");
    }

    #[test]
    fn no_replace_install_falls_back_when_hard_links_are_unsupported() {
        let dir = std::env::temp_dir().join(format!(
            "mosaic_install_fallback_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let tmp = dir.join("out.tmp");
        let out = dir.join("out.mosaic");
        std::fs::write(&tmp, b"complete").unwrap();

        let outcome = install_mosaic_output_no_replace(
            &tmp,
            &out,
            |_, _| {
                // Linux VFAT reports EPERM (PermissionDenied) for hard links.
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "hard links unsupported",
                ))
            },
            rename_no_replace,
            |path| std::fs::remove_file(path),
        )
        .unwrap();

        assert!(matches!(outcome, InstallOutcome::Clean));
        assert_eq!(std::fs::read(&out).unwrap(), b"complete");
        assert!(!tmp.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))]
    #[test]
    fn no_replace_fallback_preserves_an_existing_output() {
        let dir = std::env::temp_dir().join(format!(
            "mosaic_install_no_clobber_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let tmp = dir.join("out.tmp");
        let out = dir.join("out.mosaic");
        std::fs::write(&tmp, b"complete").unwrap();
        std::fs::write(&out, b"sentinel").unwrap();

        let error = install_mosaic_output_no_replace(
            &tmp,
            &out,
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "hard links unsupported",
                ))
            },
            rename_no_replace,
            |path| std::fs::remove_file(path),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&out).unwrap(), b"sentinel");
        assert_eq!(std::fs::read(&tmp).unwrap(), b"complete");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_replace_install_succeeds_after_output_commit_when_temp_cleanup_fails() {
        let dir = std::env::temp_dir().join(format!(
            "mosaic_install_cleanup_failure_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let tmp = dir.join("out.tmp");
        let out = dir.join("out.mosaic");
        std::fs::write(&tmp, b"complete").unwrap();

        let outcome = install_mosaic_output_no_replace(
            &tmp,
            &out,
            |from, to| std::fs::hard_link(from, to),
            |_, _| panic!("hard-link success must not fall back to rename"),
            |_| Err(std::io::Error::from_raw_os_error(libc::EIO)),
        )
        .unwrap();

        let InstallOutcome::CommittedWithCleanupError(error) = outcome else {
            panic!("expected committed output with cleanup error");
        };
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert_eq!(std::fs::read(&out).unwrap(), b"complete");
        assert!(tmp.exists());
        std::fs::remove_file(&tmp).unwrap();
        std::fs::remove_file(&out).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn csv_replay_rejects_truncated_rows() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Utf8, true),
            Field::new("b", DataType::Utf8, true),
        ]);
        let format = arrow::csv::reader::Format::default().with_header(true);
        let mut reader = build_csv_replay_reader(
            schema,
            format,
            vec![0, 1],
            std::io::Cursor::new(b"a,b\n1\n"),
        )
        .unwrap();

        let error = reader.next().unwrap().unwrap_err();
        assert!(error.to_string().contains("expected 2 got 1"), "{error}");
    }

    #[test]
    fn csv_replay_batch_size_bounds_wide_batches() {
        assert_eq!(csv_replay_batch_size(0), DEFAULT_CSV_BATCH_SIZE);
        assert_eq!(csv_replay_batch_size(1), DEFAULT_CSV_BATCH_SIZE);
        assert_eq!(csv_replay_batch_size(1024), 1024);
        assert_eq!(csv_replay_batch_size(16_384), 64);
        assert_eq!(csv_replay_batch_size(CSV_REPLAY_CELL_BUDGET * 2), 1);
    }

    #[test]
    fn csv_alignment_moves_and_converts_columns_without_changing_values() {
        use arrow::array::{Array, Float64Array, Int64Array};

        let source_schema = Arc::new(Schema::new(vec![
            Field::new("field_0", DataType::Utf8, true),
            Field::new("field_1", DataType::Int64, true),
        ]));
        let batch = RecordBatch::try_new(
            source_schema,
            vec![
                Arc::new(StringArray::from(vec!["1.5", "2.0"])),
                Arc::new(Int64Array::from(vec![1, 2])),
            ],
        )
        .unwrap();
        let output_schema = Schema::new(vec![
            Field::new("value", DataType::Float64, true),
            Field::new("id", DataType::Int64, true),
            Field::new("missing", DataType::Utf8, true),
        ]);

        let aligned = align_csv_batch_to_schema(
            batch,
            &output_schema,
            &[Some(0), Some(1), None],
            Path::new("input.csv"),
        )
        .unwrap();

        let values = aligned
            .column(0)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let ids = aligned
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(values.values(), &[1.5, 2.0]);
        assert_eq!(ids.values(), &[1, 2]);
        assert_eq!(aligned.column(2).null_count(), 2);
    }
}
