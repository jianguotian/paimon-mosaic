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

use arrow_schema::DataType;
use mosaic_core::bucket_reader::{EncodedColumn, EncodedValueRef};
use mosaic_core::reader::{Encoding, RowGroupReader};

const MIN_EXACT_DOUBLE_ABS: f64 = 1.0e-6;
const MAX_EXACT_DOUBLE_ABS: f64 = 1.0e9;
const MAX_COLUMNAR_JSON_ROWS: usize = 1_000_000;
const MAX_UNCOMPRESSED_JSON_BYTES: usize = 512 * 1024 * 1024;
const MAX_DOUBLE_VALUE_BYTES: usize = 32;
const COMMA_BLOCK: [u8; 64] = [b','; 64];
const REPEATED_VALUE_BUFFER_BYTES: usize = 64 * 1024;
const NULLABLE_CONST_PATTERN_COUNT: usize = 256;
const NULLABLE_CONST_CACHE_BYTES: usize = 64 * 1024;

struct OutputBudget {
    estimated_bytes: usize,
}

impl OutputBudget {
    fn new() -> Self {
        Self { estimated_bytes: 2 }
    }

    fn add(&mut self, bytes: usize) -> io::Result<()> {
        self.estimated_bytes = self
            .estimated_bytes
            .checked_add(bytes)
            .ok_or_else(output_budget_exceeded)?;
        if self.estimated_bytes > MAX_UNCOMPRESSED_JSON_BYTES {
            return Err(output_budget_exceeded());
        }
        Ok(())
    }

    fn remaining(&self) -> usize {
        MAX_UNCOMPRESSED_JSON_BYTES - self.estimated_bytes
    }

    fn add_column_structure(
        &mut self,
        name: &str,
        column_index: usize,
        row_count: usize,
    ) -> io::Result<()> {
        self.add(usize::from(column_index > 0))?;
        self.add(escaped_utf8_len(name.as_bytes())?)?;
        // Quotes around the name and value, plus the colon.
        self.add(5)?;
        self.add(row_count.saturating_sub(1))
    }
}

fn output_budget_exceeded() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "columnar JSON exceeds the {} byte uncompressed output budget",
            MAX_UNCOMPRESSED_JSON_BYTES
        ),
    )
}

fn checked_estimated_bytes(
    count: usize,
    bytes_per_value: usize,
    limit: usize,
) -> io::Result<usize> {
    let estimated = count
        .checked_mul(bytes_per_value)
        .ok_or_else(output_budget_exceeded)?;
    if estimated > limit {
        return Err(output_budget_exceeded());
    }
    Ok(estimated)
}

fn check_row_count(row_count: usize) -> io::Result<()> {
    if row_count > MAX_COLUMNAR_JSON_ROWS {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "columnar JSON row count {} exceeds the {} row budget",
                row_count, MAX_COLUMNAR_JSON_ROWS
            ),
        ));
    }
    Ok(())
}

/// Checks whether the encoded row group can be emitted byte-for-byte like the Java fast path.
///
/// No output object is created until this preflight succeeds. In addition to type and floating
/// point compatibility, it validates dictionary indexes and UTF-8 payloads which Arrow
/// materialization previously checked before touching output.
pub(crate) fn is_encoded_supported(row_group: &RowGroupReader) -> io::Result<bool> {
    check_row_count(row_group.num_rows())?;

    let mut columns = 0usize;
    let mut output_budget = OutputBudget::new();
    let result = row_group.visit_encoded_columns(|name, data_type, _, column| {
        let column_index = columns;
        columns += 1;
        if column.num_rows() != row_group.num_rows() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "column row count {} does not match row group row count {}",
                    column.num_rows(),
                    row_group.num_rows()
                ),
            ));
        }
        output_budget.add_column_structure(name, column_index, column.num_rows())?;
        let supported = match data_type {
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
                let column_supported = validate_integer_column(column)?;
                if column_supported {
                    output_budget.add(estimate_integer_value_bytes(
                        data_type,
                        column,
                        output_budget.remaining(),
                    )?)?;
                }
                column_supported
            }
            DataType::Float64 => {
                let column_supported = validate_float64_column(column)?;
                if column_supported {
                    output_budget.add(estimate_fixed_value_bytes(
                        column,
                        MAX_DOUBLE_VALUE_BYTES,
                        output_budget.remaining(),
                    )?)?;
                }
                column_supported
            }
            DataType::Utf8 => {
                let validation = validate_utf8_column(column, output_budget.remaining())?;
                if validation.supported {
                    output_budget.add(validation.estimated_value_bytes)?;
                }
                validation.supported
            }
            _ => false,
        };
        if supported {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported column '{}': {:?}", name, data_type),
            ))
        }
    });
    match result {
        Ok(()) => Ok(columns > 0),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn write_encoded_supported<W: Write>(
    row_group: &RowGroupReader,
    output: &mut W,
) -> io::Result<()> {
    let mut writer = EncodedJsonWriter {
        output,
        value_buffer: Vec::with_capacity(64),
        repeated_buffer: Vec::with_capacity(REPEATED_VALUE_BUFFER_BYTES),
        nullable_block_cache: (0..NULLABLE_CONST_PATTERN_COUNT).map(|_| None).collect(),
        nullable_cache_bytes: 0,
        column_index: 0,
    };
    writer.output.write_all(b"{")?;
    row_group.visit_encoded_columns(|name, data_type, _, column| {
        writer.write_column(name, data_type, column)
    })?;
    writer.output.write_all(b"}")
}

fn validate_integer_column(column: EncodedColumn<'_>) -> io::Result<bool> {
    match column.encoding() {
        Encoding::AllNull => Ok(true),
        Encoding::Const => {
            if !has_non_null(&column) {
                return Ok(true);
            }
            match column.constant()? {
                Some(value) if integer_value_matches(column.data_type(), value) => Ok(true),
                _ => Err(encoded_type_mismatch(column.data_type())),
            }
        }
        Encoding::Dict | Encoding::Plain => {
            for value in column.values() {
                let value = value?;
                if !matches!(value, EncodedValueRef::Null)
                    && !integer_value_matches(column.data_type(), value)
                {
                    return Err(encoded_type_mismatch(column.data_type()));
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn integer_value_matches(data_type: &DataType, value: EncodedValueRef<'_>) -> bool {
    matches!(
        (data_type, value),
        (DataType::Int8, EncodedValueRef::Int8(_))
            | (DataType::Int16, EncodedValueRef::Int16(_))
            | (DataType::Int32, EncodedValueRef::Int32(_))
            | (DataType::Int64, EncodedValueRef::Int64(_))
    )
}

fn estimate_integer_value_bytes(
    data_type: &DataType,
    column: EncodedColumn<'_>,
    limit: usize,
) -> io::Result<usize> {
    let max_value_bytes = match data_type {
        DataType::Int8 => 4,
        DataType::Int16 => 6,
        DataType::Int32 => 11,
        DataType::Int64 => 20,
        _ => return Err(encoded_type_mismatch(data_type)),
    };
    estimate_fixed_value_bytes(column, max_value_bytes, limit)
}

fn estimate_fixed_value_bytes(
    column: EncodedColumn<'_>,
    max_value_bytes: usize,
    limit: usize,
) -> io::Result<usize> {
    checked_estimated_bytes(non_null_count(&column), max_value_bytes, limit)
}

fn validate_float64_column(column: EncodedColumn<'_>) -> io::Result<bool> {
    match column.encoding() {
        Encoding::AllNull => Ok(true),
        Encoding::Const => {
            if !has_non_null(&column) {
                return Ok(true);
            }
            match column.constant()? {
                Some(EncodedValueRef::Float64(value)) => Ok(is_supported_double(value)),
                _ => Err(encoded_type_mismatch(column.data_type())),
            }
        }
        Encoding::Dict | Encoding::Plain => {
            for value in column.values() {
                match value? {
                    EncodedValueRef::Null => {}
                    EncodedValueRef::Float64(value) if is_supported_double(value) => {}
                    EncodedValueRef::Float64(_) => return Ok(false),
                    _ => return Err(encoded_type_mismatch(column.data_type())),
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

struct ColumnValidation {
    supported: bool,
    estimated_value_bytes: usize,
}

fn validate_utf8_column(column: EncodedColumn<'_>, limit: usize) -> io::Result<ColumnValidation> {
    match column.encoding() {
        Encoding::AllNull => Ok(ColumnValidation {
            supported: true,
            estimated_value_bytes: 0,
        }),
        Encoding::Const => {
            if !has_non_null(&column) {
                return Ok(ColumnValidation {
                    supported: true,
                    estimated_value_bytes: 0,
                });
            }
            match column.constant()? {
                Some(EncodedValueRef::Utf8(value)) => {
                    std::str::from_utf8(value).map_err(invalid_utf8)?;
                    Ok(ColumnValidation {
                        supported: true,
                        estimated_value_bytes: checked_estimated_bytes(
                            non_null_count(&column),
                            escaped_utf8_len(value)?,
                            limit,
                        )?,
                    })
                }
                _ => Err(encoded_type_mismatch(column.data_type())),
            }
        }
        Encoding::Dict | Encoding::Plain => {
            let mut estimated_value_bytes = 0usize;
            for value in column.values() {
                match value? {
                    EncodedValueRef::Null => {}
                    EncodedValueRef::Utf8(value) => {
                        std::str::from_utf8(value).map_err(invalid_utf8)?;
                        estimated_value_bytes = estimated_value_bytes
                            .checked_add(escaped_utf8_len(value)?)
                            .ok_or_else(output_budget_exceeded)?;
                        if estimated_value_bytes > limit {
                            return Err(output_budget_exceeded());
                        }
                    }
                    _ => return Err(encoded_type_mismatch(column.data_type())),
                }
            }
            Ok(ColumnValidation {
                supported: true,
                estimated_value_bytes,
            })
        }
        _ => Ok(ColumnValidation {
            supported: false,
            estimated_value_bytes: 0,
        }),
    }
}

fn invalid_utf8(error: std::str::Utf8Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid UTF-8 column value: {}", error),
    )
}

fn has_non_null(column: &EncodedColumn<'_>) -> bool {
    non_null_count(column) > 0
}

fn non_null_count(column: &EncodedColumn<'_>) -> usize {
    if column.num_rows() == 0 || column.encoding() == Encoding::AllNull {
        return 0;
    }
    let Some(bitmap) = column.null_bitmap() else {
        return column.num_rows();
    };

    let full_bytes = column.num_rows() / 8;
    let full_nulls: usize = bitmap[..full_bytes]
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum();
    let remaining = column.num_rows() % 8;
    let tail_nulls = if remaining == 0 {
        0
    } else {
        (bitmap[full_bytes] & ((1u8 << remaining) - 1)).count_ones() as usize
    };
    column
        .num_rows()
        .saturating_sub(full_nulls.saturating_add(tail_nulls))
}

struct EncodedJsonWriter<'a, W> {
    output: &'a mut W,
    value_buffer: Vec<u8>,
    repeated_buffer: Vec<u8>,
    nullable_block_cache: Vec<Option<Vec<u8>>>,
    nullable_cache_bytes: usize,
    column_index: usize,
}

impl<W: Write> EncodedJsonWriter<'_, W> {
    fn write_column(
        &mut self,
        name: &str,
        data_type: &DataType,
        column: EncodedColumn<'_>,
    ) -> io::Result<()> {
        if self.column_index > 0 {
            self.output.write_all(b",")?;
        }
        self.column_index += 1;
        self.output.write_all(b"\"")?;
        write_escaped_utf8_chunked(self.output, name.as_bytes())?;
        self.output.write_all(b"\":\"")?;
        self.write_array(data_type, column)?;
        self.output.write_all(b"\"")
    }

    fn write_array(&mut self, data_type: &DataType, column: EncodedColumn<'_>) -> io::Result<()> {
        match column.encoding() {
            Encoding::AllNull => write_empty_array(self.output, column.num_rows()),
            Encoding::Const => self.write_constant(data_type, column),
            Encoding::Dict | Encoding::Plain => {
                for (row, value) in column.values().enumerate() {
                    write_encoded_value(self.output, data_type, value?, row > 0)?;
                }
                Ok(())
            }
            encoding => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported column encoding {:?}", encoding),
            )),
        }
    }

    fn write_constant(
        &mut self,
        data_type: &DataType,
        column: EncodedColumn<'_>,
    ) -> io::Result<()> {
        if !has_non_null(&column) {
            return write_empty_array(self.output, column.num_rows());
        }
        let value = column
            .constant()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing CONST value"))?;
        if column.has_nulls() {
            for block in &mut self.nullable_block_cache {
                *block = None;
            }
            self.nullable_cache_bytes = 0;
        }
        if let (DataType::Utf8, EncodedValueRef::Utf8(value)) = (data_type, value) {
            return write_utf8_constant(
                self.output,
                value,
                column.num_rows(),
                column.null_bitmap(),
                &mut self.value_buffer,
                &mut self.repeated_buffer,
                &mut self.nullable_block_cache,
                &mut self.nullable_cache_bytes,
            );
        }
        self.value_buffer.clear();
        write_encoded_value(&mut self.value_buffer, data_type, value, false)?;
        if column.has_nulls() {
            return write_nullable_repeated_value(
                self.output,
                &self.value_buffer,
                column,
                &mut self.nullable_block_cache,
                &mut self.nullable_cache_bytes,
                &mut self.repeated_buffer,
            );
        }
        write_repeated_value(
            self.output,
            &self.value_buffer,
            column.num_rows(),
            &mut self.repeated_buffer,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn write_utf8_constant<W: Write>(
    output: &mut W,
    value: &[u8],
    row_count: usize,
    null_bitmap: Option<&[u8]>,
    value_buffer: &mut Vec<u8>,
    repeated_buffer: &mut Vec<u8>,
    block_cache: &mut [Option<Vec<u8>>],
    cache_bytes: &mut usize,
) -> io::Result<()> {
    if let Some(null_bitmap) = null_bitmap {
        validate_null_bitmap(row_count, null_bitmap)?;
    }
    if is_oversized_escaped_utf8(value)? {
        return write_utf8_rows_streamed(output, value, row_count, null_bitmap);
    }

    value_buffer.clear();
    write_escaped_utf8(value_buffer, value)?;
    match null_bitmap {
        Some(null_bitmap) => write_nullable_repeated_rows(
            output,
            value_buffer,
            row_count,
            null_bitmap,
            block_cache,
            cache_bytes,
            repeated_buffer,
        ),
        None => write_repeated_value(output, value_buffer, row_count, repeated_buffer),
    }
}

fn is_oversized_escaped_utf8(value: &[u8]) -> io::Result<bool> {
    Ok(escaped_utf8_len(value)? >= REPEATED_VALUE_BUFFER_BYTES)
}

fn write_utf8_rows_streamed<W: Write>(
    output: &mut W,
    value: &[u8],
    row_count: usize,
    null_bitmap: Option<&[u8]>,
) -> io::Result<()> {
    for row in 0..row_count {
        if row > 0 {
            output.write_all(b",")?;
        }
        let is_null = null_bitmap
            .map(|bitmap| bitmap[row / 8] & (1 << (row % 8)) != 0)
            .unwrap_or(false);
        if !is_null {
            write_escaped_utf8_chunked(output, value)?;
        }
    }
    Ok(())
}

fn write_escaped_utf8_chunked<W: Write>(output: &mut W, value: &[u8]) -> io::Result<()> {
    for chunk in value.chunks(REPEATED_VALUE_BUFFER_BYTES) {
        write_escaped_utf8(output, chunk)?;
    }
    Ok(())
}

fn write_encoded_value<W: Write>(
    output: &mut W,
    data_type: &DataType,
    value: EncodedValueRef<'_>,
    separator: bool,
) -> io::Result<()> {
    if matches!(value, EncodedValueRef::Null) {
        return if separator {
            output.write_all(b",")
        } else {
            Ok(())
        };
    }

    match (data_type, value) {
        (DataType::Int8, EncodedValueRef::Int8(value)) => {
            write_i64_value(output, value as i64, separator)
        }
        (DataType::Int16, EncodedValueRef::Int16(value)) => {
            write_i64_value(output, value as i64, separator)
        }
        (DataType::Int32, EncodedValueRef::Int32(value)) => {
            write_i64_value(output, value as i64, separator)
        }
        (DataType::Int64, EncodedValueRef::Int64(value)) => {
            write_i64_value(output, value, separator)
        }
        (DataType::Float64, EncodedValueRef::Float64(value)) => {
            write_double(output, value, separator)
        }
        (DataType::Utf8, EncodedValueRef::Utf8(value)) => {
            if separator {
                output.write_all(b",")?;
            }
            write_escaped_utf8_chunked(output, value)
        }
        _ => Err(encoded_type_mismatch(data_type)),
    }
}

fn write_repeated_value<W: Write>(
    output: &mut W,
    value: &[u8],
    row_count: usize,
    repeated_buffer: &mut Vec<u8>,
) -> io::Result<()> {
    if row_count == 0 {
        return Ok(());
    }
    if value.len() >= REPEATED_VALUE_BUFFER_BYTES {
        repeated_buffer.clear();
        write_bytes_chunked(output, value)?;
        for _ in 1..row_count {
            output.write_all(b",")?;
            write_bytes_chunked(output, value)?;
        }
        return Ok(());
    }
    output.write_all(value)?;
    let mut remaining = row_count - 1;
    if remaining == 0 {
        return Ok(());
    }

    let pattern_size = value.len().saturating_add(1);
    let repeats_per_buffer = (REPEATED_VALUE_BUFFER_BYTES / pattern_size.max(1))
        .max(1)
        .min(remaining);
    repeated_buffer.clear();
    repeated_buffer.push(b',');
    repeated_buffer.extend_from_slice(value);
    let target_size = repeats_per_buffer * pattern_size;
    while repeated_buffer.len() < target_size {
        let current_size = repeated_buffer.len();
        let copy_size = current_size.min(target_size - current_size);
        repeated_buffer.extend_from_within(..copy_size);
    }
    while remaining >= repeats_per_buffer {
        output.write_all(repeated_buffer)?;
        remaining -= repeats_per_buffer;
    }
    if remaining > 0 {
        output.write_all(&repeated_buffer[..remaining * pattern_size])?;
    }
    Ok(())
}

fn write_bytes_chunked<W: Write>(output: &mut W, value: &[u8]) -> io::Result<()> {
    for chunk in value.chunks(REPEATED_VALUE_BUFFER_BYTES) {
        output.write_all(chunk)?;
    }
    Ok(())
}

fn write_nullable_repeated_value<W: Write>(
    output: &mut W,
    value: &[u8],
    column: EncodedColumn<'_>,
    block_cache: &mut [Option<Vec<u8>>],
    cache_bytes: &mut usize,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    let null_bitmap = column.null_bitmap().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "nullable CONST column is missing its null bitmap",
        )
    })?;
    write_nullable_repeated_rows(
        output,
        value,
        column.num_rows(),
        null_bitmap,
        block_cache,
        cache_bytes,
        scratch,
    )
}

fn write_nullable_repeated_rows<W: Write>(
    output: &mut W,
    value: &[u8],
    row_count: usize,
    null_bitmap: &[u8],
    block_cache: &mut [Option<Vec<u8>>],
    cache_bytes: &mut usize,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    if block_cache.len() != NULLABLE_CONST_PATTERN_COUNT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nullable CONST block cache must contain 256 entries",
        ));
    }
    validate_null_bitmap(row_count, null_bitmap)?;

    if value.len().saturating_add(1).saturating_mul(8) > REPEATED_VALUE_BUFFER_BYTES {
        return write_nullable_rows_bounded(output, value, row_count, null_bitmap, scratch);
    }

    let mut row = 0usize;
    if row + 8 <= row_count {
        scratch.clear();
        append_nullable_const_block(scratch, value, !null_bitmap[0], 8, true);
        output.write_all(scratch)?;
        row += 8;
    }

    while row + 8 <= row_count {
        let validity = !null_bitmap[row / 8];
        let cache_index = validity as usize;
        match &block_cache[cache_index] {
            Some(block) => output.write_all(block)?,
            None => {
                scratch.clear();
                append_nullable_const_block(scratch, value, validity, 8, false);
                if cache_bytes.saturating_add(scratch.len()) <= NULLABLE_CONST_CACHE_BYTES {
                    *cache_bytes += scratch.len();
                    block_cache[cache_index] = Some(scratch.clone());
                    output.write_all(block_cache[cache_index].as_ref().unwrap())?;
                } else {
                    output.write_all(scratch)?;
                }
            }
        }
        row += 8;
    }

    if row < row_count {
        scratch.clear();
        append_nullable_const_block(
            scratch,
            value,
            !null_bitmap[row / 8],
            row_count - row,
            row == 0,
        );
        output.write_all(scratch)?;
    }

    Ok(())
}

fn validate_null_bitmap(row_count: usize, null_bitmap: &[u8]) -> io::Result<()> {
    let required_bitmap_bytes = row_count.div_ceil(8);
    if null_bitmap.len() < required_bitmap_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "nullable CONST null bitmap is truncated",
        ));
    }
    Ok(())
}

fn write_nullable_rows_bounded<W: Write>(
    output: &mut W,
    value: &[u8],
    row_count: usize,
    null_bitmap: &[u8],
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    scratch.clear();
    for row in 0..row_count {
        let is_null = null_bitmap[row / 8] & (1 << (row % 8)) != 0;
        let row_size = usize::from(row > 0)
            .checked_add(if is_null { 0 } else { value.len() })
            .ok_or_else(output_budget_exceeded)?;
        if !scratch.is_empty()
            && scratch.len().saturating_add(row_size) > REPEATED_VALUE_BUFFER_BYTES
        {
            output.write_all(scratch)?;
            scratch.clear();
        }
        if row_size > REPEATED_VALUE_BUFFER_BYTES {
            if row > 0 {
                output.write_all(b",")?;
            }
            if !is_null {
                write_bytes_chunked(output, value)?;
            }
            continue;
        }
        if row > 0 {
            scratch.push(b',');
        }
        if !is_null {
            scratch.extend_from_slice(value);
        }
    }
    if !scratch.is_empty() {
        output.write_all(scratch)?;
    }
    Ok(())
}

fn append_nullable_const_block(
    output: &mut Vec<u8>,
    value: &[u8],
    validity: u8,
    rows: usize,
    first_block: bool,
) {
    for row in 0..rows {
        if !first_block || row > 0 {
            output.push(b',');
        }
        if validity & (1 << row) != 0 {
            output.extend_from_slice(value);
        }
    }
}

fn encoded_type_mismatch(data_type: &DataType) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("encoded value does not match column type {:?}", data_type),
    )
}

fn write_empty_array<W: Write>(output: &mut W, row_count: usize) -> io::Result<()> {
    let mut remaining = row_count.saturating_sub(1);
    while remaining > 0 {
        let written = remaining.min(COMMA_BLOCK.len());
        output.write_all(&COMMA_BLOCK[..written])?;
        remaining -= written;
    }
    Ok(())
}

fn write_i64<W: Write>(output: &mut W, value: i64) -> io::Result<()> {
    write_i64_value(output, value, false)
}

fn write_i64_value<W: Write>(output: &mut W, value: i64, separator: bool) -> io::Result<()> {
    let mut buffer = [0u8; 21];
    let mut position = buffer.len();
    let mut magnitude = value.unsigned_abs();
    loop {
        position -= 1;
        buffer[position] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value < 0 {
        position -= 1;
        buffer[position] = b'-';
    }
    if separator {
        position -= 1;
        buffer[position] = b',';
    }
    output.write_all(&buffer[position..])
}

fn is_supported_double(value: f64) -> bool {
    let bits = value.to_bits();
    if bits == 0 || bits == (1u64 << 63) {
        return true;
    }

    value.is_finite() && value.abs() >= MIN_EXACT_DOUBLE_ABS && value.abs() <= MAX_EXACT_DOUBLE_ABS
}

fn write_double<W: Write>(output: &mut W, value: f64, separator: bool) -> io::Result<()> {
    let bits = value.to_bits();
    if bits == 0 {
        return output.write_all(if separator { b",0.0" } else { b"0.0" });
    }
    if bits == (1u64 << 63) {
        return output.write_all(if separator { b",-0.0" } else { b"-0.0" });
    }

    let integer = value as i64;
    if (-9_999_999..=9_999_999).contains(&integer) && value == integer as f64 {
        write_i64_value(output, integer, separator)?;
        return output.write_all(b".0");
    }

    let scaled = value * 10.0;
    let unscaled = scaled as i64;
    if value > -10_000_000.0
        && value < 10_000_000.0
        && scaled == unscaled as f64
        && unscaled % 10 != 0
        && unscaled as f64 / 10.0 == value
    {
        if separator {
            output.write_all(b",")?;
        }
        if unscaled < 0 {
            output.write_all(b"-")?;
        }
        let magnitude = unscaled.unsigned_abs();
        write_i64(output, (magnitude / 10) as i64)?;
        output.write_all(&[b'.', b'0' + (magnitude % 10) as u8])?;
        return Ok(());
    }

    if separator {
        output.write_all(b",")?;
    }
    let mut buffer = ryu::Buffer::new();
    write_java_style_double(output, buffer.format_finite(value))
}

fn write_java_style_double<W: Write>(output: &mut W, raw: &str) -> io::Result<()> {
    let bytes = raw.as_bytes();
    let (sign, unsigned) = if bytes.first() == Some(&b'-') {
        (Some(b'-'), &bytes[1..])
    } else {
        (None, bytes)
    };
    let exponent_index = unsigned
        .iter()
        .position(|value| *value == b'e' || *value == b'E');
    let mantissa_end = exponent_index.unwrap_or(unsigned.len());
    let exponent = match exponent_index {
        Some(index) => parse_exponent(&unsigned[index + 1..])?,
        None => 0,
    };
    let mantissa = &unsigned[..mantissa_end];
    let dot_position = mantissa
        .iter()
        .position(|value| *value == b'.')
        .unwrap_or(mantissa.len());

    let mut digits = [0u8; 24];
    let mut digits_len = 0;
    let mut leading_zeroes = 0;
    let mut significant = false;
    for &value in mantissa {
        if value == b'.' {
            continue;
        }
        if !significant && value == b'0' {
            leading_zeroes += 1;
            continue;
        }
        significant = true;
        digits[digits_len] = value;
        digits_len += 1;
    }
    while digits_len > 1 && digits[digits_len - 1] == b'0' {
        digits_len -= 1;
    }
    if digits_len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid finite double rendering '{}'", raw),
        ));
    }

    let decimal_position = dot_position as i32 + exponent - leading_zeroes;
    if let Some(sign) = sign {
        output.write_all(&[sign])?;
    }
    if decimal_position > 0 && decimal_position <= 7 {
        let decimal_position = decimal_position as usize;
        if digits_len <= decimal_position {
            output.write_all(&digits[..digits_len])?;
            write_zeroes(output, decimal_position - digits_len)?;
            output.write_all(b".0")
        } else {
            output.write_all(&digits[..decimal_position])?;
            output.write_all(b".")?;
            output.write_all(&digits[decimal_position..digits_len])
        }
    } else if decimal_position > -3 && decimal_position <= 0 {
        output.write_all(b"0.")?;
        write_zeroes(output, (-decimal_position) as usize)?;
        output.write_all(&digits[..digits_len])
    } else {
        output.write_all(&digits[..1])?;
        output.write_all(b".")?;
        if digits_len == 1 {
            output.write_all(b"0")?;
        } else {
            output.write_all(&digits[1..digits_len])?;
        }
        output.write_all(b"E")?;
        write_i64(output, (decimal_position - 1) as i64)
    }
}

fn parse_exponent(value: &[u8]) -> io::Result<i32> {
    let (negative, digits) = if value.first() == Some(&b'-') {
        (true, &value[1..])
    } else if value.first() == Some(&b'+') {
        (false, &value[1..])
    } else {
        (false, value)
    };
    if digits.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing floating-point exponent",
        ));
    }
    let mut exponent = 0i32;
    for &digit in digits {
        if !digit.is_ascii_digit() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid floating-point exponent",
            ));
        }
        exponent = exponent
            .checked_mul(10)
            .and_then(|current| current.checked_add((digit - b'0') as i32))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "floating-point exponent overflow",
                )
            })?;
    }
    Ok(if negative { -exponent } else { exponent })
}

fn write_zeroes<W: Write>(output: &mut W, count: usize) -> io::Result<()> {
    const ZEROES: &[u8; 32] = b"00000000000000000000000000000000";
    let mut remaining = count;
    while remaining > 0 {
        let written = remaining.min(ZEROES.len());
        output.write_all(&ZEROES[..written])?;
        remaining -= written;
    }
    Ok(())
}

fn write_escaped_utf8<W: Write>(output: &mut W, value: &[u8]) -> io::Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut safe_start = 0;
    for (index, &current) in value.iter().enumerate() {
        if current >= 0x20 && current != b'"' && current != b'\\' {
            continue;
        }
        if safe_start < index {
            output.write_all(&value[safe_start..index])?;
        }
        match current {
            b'"' => output.write_all(b"\\\"")?,
            b'\\' => output.write_all(b"\\\\")?,
            b'\x08' => output.write_all(b"\\b")?,
            b'\x0c' => output.write_all(b"\\f")?,
            b'\n' => output.write_all(b"\\n")?,
            b'\r' => output.write_all(b"\\r")?,
            b'\t' => output.write_all(b"\\t")?,
            value => {
                output.write_all(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[(value >> 4) as usize],
                    HEX[(value & 0x0f) as usize],
                ])?;
            }
        }
        safe_start = index + 1;
    }
    if safe_start < value.len() {
        output.write_all(&value[safe_start..])?;
    }
    Ok(())
}

fn escaped_utf8_len(value: &[u8]) -> io::Result<usize> {
    let mut length = 0usize;
    for current in value {
        let bytes = match current {
            b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
            0x00..=0x1f => 6,
            _ => 1,
        };
        length = length
            .checked_add(bytes)
            .ok_or_else(output_budget_exceeded)?;
    }
    Ok(length)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use arrow_schema::DataType;
    use mosaic_core::bucket_reader::EncodedValueRef;

    use super::{
        append_nullable_const_block, check_row_count, checked_estimated_bytes, escaped_utf8_len,
        integer_value_matches, is_oversized_escaped_utf8, is_supported_double, write_double,
        write_encoded_value, write_escaped_utf8, write_nullable_repeated_rows,
        write_repeated_value, write_utf8_constant, OutputBudget, MAX_COLUMNAR_JSON_ROWS,
        MAX_UNCOMPRESSED_JSON_BYTES, NULLABLE_CONST_CACHE_BYTES, NULLABLE_CONST_PATTERN_COUNT,
        REPEATED_VALUE_BUFFER_BYTES,
    };

    #[derive(Default)]
    struct TrackingWriter {
        output: Vec<u8>,
        max_write: usize,
    }

    impl Write for TrackingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buffer);
            self.max_write = self.max_write.max(buffer.len());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn validates_integer_value_type_exactly() {
        assert!(integer_value_matches(
            &DataType::Int16,
            EncodedValueRef::Int16(0)
        ));
        assert!(!integer_value_matches(
            &DataType::Int16,
            EncodedValueRef::Int32(0)
        ));
        assert!(!integer_value_matches(
            &DataType::Int32,
            EncodedValueRef::Int16(0)
        ));
    }

    #[test]
    fn rejects_row_count_above_budget() {
        assert!(check_row_count(MAX_COLUMNAR_JSON_ROWS).is_ok());
        let error = check_row_count(MAX_COLUMNAR_JSON_ROWS + 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("row budget"));
    }

    #[test]
    fn rejects_output_above_byte_budget() {
        let mut budget = OutputBudget::new();
        budget.add(MAX_UNCOMPRESSED_JSON_BYTES - 2).unwrap();
        assert_eq!(budget.remaining(), 0);

        let error = budget.add(1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("output budget"));
    }

    #[test]
    fn rejects_value_estimate_overflow() {
        let error = checked_estimated_bytes(usize::MAX, 2, usize::MAX).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("output budget"));
    }

    #[test]
    fn writes_nullable_const_bitmap_block() {
        let mut output = Vec::new();
        append_nullable_const_block(&mut output, b"-7", 0b1001_1010, 8, true);
        assert_eq!(output, b",-7,,-7,-7,,,-7");

        output.clear();
        append_nullable_const_block(&mut output, b"x", 0b0000_0101, 3, false);
        assert_eq!(output, b",x,,x");
    }

    #[test]
    fn oversized_repeated_values_use_bounded_writes_and_buffer() {
        let value = vec![b'x'; REPEATED_VALUE_BUFFER_BYTES + 17];
        let mut output = TrackingWriter::default();
        let mut repeated_buffer = Vec::new();

        write_repeated_value(&mut output, &value, 3, &mut repeated_buffer).unwrap();

        let mut expected = value.clone();
        expected.push(b',');
        expected.extend_from_slice(&value);
        expected.push(b',');
        expected.extend_from_slice(&value);
        assert_eq!(output.output, expected);
        assert!(output.max_write <= REPEATED_VALUE_BUFFER_BYTES);
        assert!(repeated_buffer.len() <= REPEATED_VALUE_BUFFER_BYTES);
    }

    #[test]
    fn large_nullable_constants_use_bounded_writes_and_scratch() {
        let value = vec![b'x'; REPEATED_VALUE_BUFFER_BYTES / 4 + 17];
        let null_bitmap = [0b0001_0010];
        let mut output = TrackingWriter::default();
        let mut block_cache = (0..NULLABLE_CONST_PATTERN_COUNT)
            .map(|_| None)
            .collect::<Vec<_>>();
        let mut cache_bytes = 0;
        let mut scratch = Vec::new();

        write_nullable_repeated_rows(
            &mut output,
            &value,
            8,
            &null_bitmap,
            &mut block_cache,
            &mut cache_bytes,
            &mut scratch,
        )
        .unwrap();

        let mut expected = Vec::new();
        for row in 0..8 {
            if row > 0 {
                expected.push(b',');
            }
            if null_bitmap[0] & (1 << row) == 0 {
                expected.extend_from_slice(&value);
            }
        }
        assert_eq!(output.output, expected);
        assert!(output.max_write <= REPEATED_VALUE_BUFFER_BYTES);
        assert!(scratch.len() <= REPEATED_VALUE_BUFFER_BYTES);
        assert_eq!(cache_bytes, 0);
    }

    #[test]
    fn nullable_const_cache_stays_within_its_total_budget() {
        let value = vec![b'x'; 511];
        let null_bitmap = (0u8..=u8::MAX).collect::<Vec<_>>();
        let row_count = null_bitmap.len() * 8;
        let mut output = TrackingWriter::default();
        let mut block_cache = (0..NULLABLE_CONST_PATTERN_COUNT)
            .map(|_| None)
            .collect::<Vec<_>>();
        let mut cache_bytes = 0;
        let mut scratch = Vec::new();

        write_nullable_repeated_rows(
            &mut output,
            &value,
            row_count,
            &null_bitmap,
            &mut block_cache,
            &mut cache_bytes,
            &mut scratch,
        )
        .unwrap();

        let mut expected = Vec::new();
        for row in 0..row_count {
            if row > 0 {
                expected.push(b',');
            }
            if null_bitmap[row / 8] & (1 << (row % 8)) == 0 {
                expected.extend_from_slice(&value);
            }
        }
        let retained_cache_bytes: usize = block_cache
            .iter()
            .filter_map(|block| block.as_ref())
            .map(Vec::len)
            .sum();
        assert_eq!(output.output, expected);
        assert_eq!(cache_bytes, retained_cache_bytes);
        assert!(cache_bytes <= NULLABLE_CONST_CACHE_BYTES);
        assert!(scratch.len() <= REPEATED_VALUE_BUFFER_BYTES);
    }

    #[test]
    fn oversized_utf8_constants_stream_without_staging() {
        let mut value = vec![b'x'; REPEATED_VALUE_BUFFER_BYTES + 17];
        value[1] = b'"';
        value[2] = b'\\';
        value[3] = b'\n';
        value[4] = 0x01;
        let null_bitmap = [0b0000_0010];
        let mut output = TrackingWriter::default();
        let mut value_buffer = b"value sentinel".to_vec();
        let mut repeated_buffer = b"repeated sentinel".to_vec();
        let mut block_cache = (0..NULLABLE_CONST_PATTERN_COUNT)
            .map(|_| None)
            .collect::<Vec<_>>();
        let mut cache_bytes = 0;

        write_utf8_constant(
            &mut output,
            &value,
            3,
            Some(&null_bitmap),
            &mut value_buffer,
            &mut repeated_buffer,
            &mut block_cache,
            &mut cache_bytes,
        )
        .unwrap();

        let mut escaped = Vec::new();
        write_escaped_utf8(&mut escaped, &value).unwrap();
        let mut expected = escaped.clone();
        expected.extend_from_slice(b",,");
        expected.extend_from_slice(&escaped);
        assert_eq!(output.output, expected);
        assert!(output.max_write <= REPEATED_VALUE_BUFFER_BYTES);
        assert_eq!(value_buffer, b"value sentinel");
        assert_eq!(repeated_buffer, b"repeated sentinel");
    }

    #[test]
    fn oversized_plain_or_dict_utf8_values_use_bounded_writes() {
        let mut value = vec![b'x'; REPEATED_VALUE_BUFFER_BYTES + 17];
        value[1] = b'"';
        value[2] = b'\\';
        value[3] = b'\n';
        value[4] = 0x01;
        let mut output = TrackingWriter::default();

        write_encoded_value(
            &mut output,
            &DataType::Utf8,
            EncodedValueRef::Utf8(&value),
            false,
        )
        .unwrap();

        let mut expected = Vec::new();
        write_escaped_utf8(&mut expected, &value).unwrap();
        assert_eq!(output.output, expected);
        assert!(output.max_write <= REPEATED_VALUE_BUFFER_BYTES);
    }

    #[test]
    fn escaped_size_selects_streaming_at_the_buffer_limit() {
        let safe = vec![b'x'; REPEATED_VALUE_BUFFER_BYTES - 1];
        assert!(!is_oversized_escaped_utf8(&safe).unwrap());

        let mut escaped_to_limit = safe;
        escaped_to_limit[0] = b'"';
        assert!(is_oversized_escaped_utf8(&escaped_to_limit).unwrap());
    }

    #[test]
    fn writes_java_style_fallback_doubles() {
        let values = [
            1.25,
            0.0001,
            10_000_000.0,
            429_496_729.5,
            1_812_576.4000000001,
        ];
        let mut output = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            write_double(&mut output, value, index > 0).unwrap();
        }
        assert_eq!(
            output,
            b"1.25,1.0E-4,1.0E7,4.294967295E8,1812576.4000000001"
        );
    }

    #[test]
    fn rejects_non_finite_and_tiny_doubles() {
        assert!(is_supported_double(0.0));
        assert!(is_supported_double(-0.0));
        assert!(is_supported_double(1.0e-6));
        assert!(is_supported_double(1.0e9));
        assert!(!is_supported_double(f64::MIN_POSITIVE));
        assert!(!is_supported_double(f64::INFINITY));
        assert!(!is_supported_double(f64::NAN));
    }

    #[test]
    fn escaped_length_matches_written_bytes() {
        let value = b"a\"\\\n\t\x01z";
        let mut output = Vec::new();
        write_escaped_utf8(&mut output, value).unwrap();
        assert_eq!(escaped_utf8_len(value).unwrap(), output.len());
        assert_eq!(output, br#"a\"\\\n\t\u0001z"#);
    }
}
