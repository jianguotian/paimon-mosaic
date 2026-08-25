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
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
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
    /// Create a Mosaic file from a JSON data file.
    Convert {
        /// Input JSON data file (.json/.ndjson/.jsonl).
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "convert only supports JSON inputs (.json/.ndjson/.jsonl); use convert-csv for CSV data",
        ));
    }
    let columns = parse_convert_columns(columns)?;
    ensure_can_write(out, overwrite)?;
    let open =
        || -> std::io::Result<_> { Ok(std::io::BufReader::new(std::fs::File::open(input)?)) };
    let schema = if columns.is_empty() {
        arrow::json::reader::infer_json_schema(&mut open()?, None)
            .map(|(schema, _)| schema)
            .map_err(bad)?
    } else {
        infer_projected_json_schema(open()?, &columns).map_err(bad)?
    };
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
) -> std::io::Result<()> {
    if inputs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CSV path is required",
        ));
    }
    ensure_can_write(out, overwrite)?;
    for input in inputs {
        let metadata = std::fs::metadata(input)?;
        if !metadata.file_type().is_file() {
            return Err(invalid_schema(format!(
                "CSV schema inference requires a regular file: {}",
                input.display()
            )));
        }
    }
    use arrow::error::ArrowError;
    let bad = |e: ArrowError| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string());
    let format = csv_format(&options)?;
    let mut inferred: Option<Schema> = None;
    for input in inputs {
        let layout = csv_input_layout(input, &options)?;
        if !layout.has_records {
            continue;
        }
        let (schema, rows) = format
            .infer_schema(open_csv(input, options.skip_lines)?, None)
            .map_err(bad)?;
        // A shard with no data rows has nothing to infer from; it is skipped
        // when reading too.
        if rows == 0 || schema.fields().is_empty() {
            continue;
        }
        let schema =
            promote_second_precision_csv_timestamps(csv_schema_with_csv_names(schema, &options)?);
        inferred = Some(match inferred.take() {
            Some(prev) => merge_csv_inferred_schema(prev, schema, input)?,
            None => schema,
        });
    }
    let schema = inferred.ok_or_else(|| invalid_schema("no CSV data to infer a schema from"))?;
    let schema = apply_required_fields(csv_schema_with_null_fallback(schema), required_fields)?;
    let schema_index = csv_schema_index(&schema);
    write_mosaic(out, overwrite, &schema, stats, |writer, rows| {
        for input in inputs {
            let layout = csv_input_layout(input, &options)?;
            if !layout.has_records {
                continue;
            }
            let reader_schema = csv_reader_schema(&schema, &schema_index, &layout);
            let source_mapping = csv_output_mapping(&schema, &schema_index, &layout);
            validate_csv_mapping(&schema, &layout, &source_mapping, input)?;
            let (projection, mapping) = csv_projection(&source_mapping);
            let reader = arrow::csv::ReaderBuilder::new(Arc::new(reader_schema))
                .with_format(format.clone().with_truncated_rows(true))
                .with_projection(projection)
                .build(open_csv(input, options.skip_lines)?)
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
    if let Err(e) = install_mosaic_output(&tmp, out, overwrite) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
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
        out.display(),
        plural(rows, "row"),
        plural(schema.fields().len(), "column")
    );
    Ok(())
}

fn install_mosaic_output(tmp: &Path, out: &Path, overwrite: bool) -> std::io::Result<()> {
    if overwrite {
        #[cfg(windows)]
        if std::fs::symlink_metadata(out).is_ok() {
            std::fs::remove_file(out)?;
        }
        return std::fs::rename(tmp, out);
    }

    // Both paths are siblings, so a hard link provides an atomic no-replace
    // install on the same filesystem. Removing the temporary name afterwards
    // leaves the completed output visible under only its requested path.
    std::fs::hard_link(tmp, out).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            output_exists_error(out)
        } else {
            error
        }
    })?;
    std::fs::remove_file(tmp)
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
        let index = by_name
            .get(name.as_str())
            .copied()
            .ok_or_else(|| invalid_schema(format!("--column '{name}' not found in schema")))?;
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

fn infer_projected_json_schema<R: std::io::Read>(
    reader: R,
    columns: &[String],
) -> Result<Schema, arrow::error::ArrowError> {
    use arrow::error::ArrowError;

    let columns = columns.iter().map(String::as_str).collect();
    // Keep each root value raw so unselected fields are skipped before
    // serde_json materializes values that cannot be represented as f64.
    let values = serde_json::Deserializer::from_reader(reader)
        .into_iter::<Box<RawValue>>()
        .map(|raw| {
            let raw = raw.map_err(|e| ArrowError::JsonError(e.to_string()))?;
            let mut deserializer = serde_json::Deserializer::from_str(raw.get());
            ProjectedJsonRecordSeed { columns: &columns }
                .deserialize(&mut deserializer)
                .map_err(|e| ArrowError::JsonError(e.to_string()))
        });
    arrow::json::reader::infer_json_schema_from_iterator(values)
}

struct ProjectedJsonRecordSeed<'a> {
    columns: &'a std::collections::HashSet<&'a str>,
}

impl<'de> DeserializeSeed<'de> for ProjectedJsonRecordSeed<'_> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(ProjectedJsonRecordVisitor {
            columns: self.columns,
        })
    }
}

struct ProjectedJsonRecordVisitor<'a> {
    columns: &'a std::collections::HashSet<&'a str>,
}

impl<'de> Visitor<'de> for ProjectedJsonRecordVisitor<'_> {
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
            if self.columns.contains(name.as_ref()) {
                projected.insert(name.into_owned(), map.next_value()?);
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
        format!("{} exists (use --overwrite to replace)", out.display()),
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

fn open_csv(path: &Path, skip_lines: usize) -> std::io::Result<std::io::BufReader<std::fs::File>> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    for _ in 0..skip_lines {
        loop {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                return Ok(reader);
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(buffer.len(), |index| index + 1);
            reader.consume(consumed);
            if newline.is_some() {
                break;
            }
        }
    }
    Ok(reader)
}

struct CsvInputLayout {
    header: Option<Vec<String>>,
    columns: usize,
    has_records: bool,
}

fn csv_input_layout(path: &Path, options: &CsvConvertOptions) -> std::io::Result<CsvInputLayout> {
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
    let mut reader = builder.from_reader(open_csv(path, options.skip_lines)?);
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
            input.display()
        )));
    }
    for (field, index) in schema.fields().iter().zip(mapping) {
        if index.is_none() && !field.is_nullable() {
            return Err(invalid_schema(format!(
                "required field '{}' was not found in the CSV header of {}",
                fmt::safe(field.name()),
                input.display()
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
    let columns: Vec<ArrayRef> = schema
        .fields()
        .iter()
        .zip(mapping)
        .map(|(field, index)| match index {
            Some(index) if batch.column(*index).data_type() == &DataType::Utf8 => {
                match field.data_type() {
                    DataType::Float64 => {
                        parse_inferred_csv_float64(batch.column(*index), field, input)
                    }
                    DataType::Timestamp(_, _) => {
                        parse_inferred_csv_timestamp_column(batch.column(*index), field, input)
                    }
                    _ => Ok(batch.column(*index).clone()),
                }
            }
            Some(index) => Ok(batch.column(*index).clone()),
            None => Ok(new_null_array(field.data_type(), batch.num_rows())),
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
            input.display()
        )));
    }
    let parse_error = || {
        invalid_schema(format!(
            "cannot parse '{}' as {} for CSV field '{}' in {}",
            fmt::safe(value),
            field.data_type(),
            fmt::safe(field.name()),
            input.display()
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
        input.display()
    ))
}

fn csv_non_finite_float_error(value: &str, field: &Field, input: &Path) -> std::io::Error {
    invalid_schema(format!(
        "non-finite Float64 value '{}' in CSV field '{}' of {} is not supported",
        fmt::safe(value),
        fmt::safe(field.name()),
        input.display()
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

fn merge_csv_inferred_schema(prev: Schema, next: Schema, input: &Path) -> std::io::Result<Schema> {
    if prev.fields().len() != next.fields().len() {
        return Err(csv_schema_mismatch(input));
    }
    let next_fields: std::collections::HashMap<&str, &Field> = next
        .fields()
        .iter()
        .map(|field| (field.name().as_str(), field.as_ref()))
        .collect();
    let fields: Vec<Field> = prev
        .fields()
        .iter()
        .map(|left| {
            let right = next_fields
                .get(left.name().as_str())
                .copied()
                .ok_or_else(|| csv_schema_mismatch(input))?;
            merge_csv_inferred_field(left.as_ref(), right, input)
        })
        .collect::<std::io::Result<_>>()?;
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
            input.display()
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
