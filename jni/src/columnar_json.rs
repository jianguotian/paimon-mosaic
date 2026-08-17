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

#[cfg(test)]
use arrow_array::{
    Array, ArrayRef, Float64Array, Int16Array, Int32Array, Int64Array, Int8Array, RecordBatch,
    StringArray,
};
use arrow_schema::DataType;
use mosaic_core::bucket_reader::{EncodedColumn, EncodedValueRef};
use mosaic_core::reader::RowGroupReader;
use mosaic_core::spec::{ENCODING_ALL_NULL, ENCODING_CONST, ENCODING_DICT, ENCODING_PLAIN};

const MIN_EXACT_DOUBLE_ABS: f64 = 1.0e-6;
const MAX_EXACT_DOUBLE_ABS: f64 = 1.0e9;
const MAX_COLUMNAR_JSON_ROWS: usize = 1_000_000;
const MAX_UNCOMPRESSED_JSON_BYTES: usize = 256 * 1024 * 1024;
const MAX_DOUBLE_VALUE_BYTES: usize = 32;
const COMMA_BLOCK: [u8; 4 * 1024] = [b','; 4 * 1024];
#[cfg(test)]
const ZERO_INT16_FIRST_BLOCK: &[u8] = b"0,0,0,0,0,0,0,0";
#[cfg(test)]
const ZERO_INT16_BLOCK: &[u8] = b",0,0,0,0,0,0,0,0";
const ZERO_INT16_FIRST_BLOCKS: ZeroInt16Blocks = zero_int16_blocks(true);
const ZERO_INT16_BLOCKS: ZeroInt16Blocks = zero_int16_blocks(false);
const REPEATED_VALUE_BUFFER_BYTES: usize = 64 * 1024;

struct ZeroInt16Blocks {
    bytes: [[u8; 16]; 256],
    lengths: [u8; 256],
}

const fn zero_int16_blocks(first_block: bool) -> ZeroInt16Blocks {
    let mut blocks = ZeroInt16Blocks {
        bytes: [[0; 16]; 256],
        lengths: [0; 256],
    };
    let mut validity = 0;
    while validity < 256 {
        let mut position = 0;
        let mut row = 0;
        while row < 8 {
            if !first_block || row > 0 {
                blocks.bytes[validity][position] = b',';
                position += 1;
            }
            if validity & (1 << row) != 0 {
                blocks.bytes[validity][position] = b'0';
                position += 1;
            }
            row += 1;
        }
        blocks.lengths[validity] = position as u8;
        validity += 1;
    }
    blocks
}

pub(crate) enum SingleUtf8Value {
    NotRequested,
    Invalid,
    Valid(Vec<u8>),
}

pub(crate) struct EncodedPreflight {
    pub(crate) supported: bool,
    pub(crate) single_utf8_value: SingleUtf8Value,
}

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
        io::ErrorKind::InvalidData,
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
            io::ErrorKind::InvalidData,
            format!(
                "columnar JSON row count {} exceeds the {} row budget",
                row_count, MAX_COLUMNAR_JSON_ROWS
            ),
        ));
    }
    Ok(())
}

/// Preflights native JSON and optionally resolves one projected UTF-8 column that must contain
/// exactly one non-empty value across all rows.
///
/// Contract-invalid single-value columns are reported separately from corrupt encoded data so the
/// Java caller can preserve its schema-incompatible error classification without touching output.
pub(crate) fn preflight_encoded(
    row_group: &RowGroupReader,
    single_utf8_column: Option<usize>,
) -> io::Result<EncodedPreflight> {
    check_row_count(row_group.num_rows())?;

    let mut columns = 0usize;
    let mut supported = true;
    let mut single_utf8_value = SingleUtf8Value::NotRequested;
    let mut requested_column_seen = single_utf8_column.is_none();
    let mut output_budget = OutputBudget::new();
    row_group.visit_encoded_columns(|name, data_type, _, column| {
        let column_index = columns;
        columns += 1;
        let mut selected_utf8_prevalidated = false;

        if column.num_rows() != row_group.num_rows() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "column '{}' row count {} does not match row group row count {}",
                    name,
                    column.num_rows(),
                    row_group.num_rows()
                ),
            ));
        }
        output_budget.add_column_structure(name, column_index, column.num_rows())?;

        if single_utf8_column == Some(column_index) {
            requested_column_seen = true;
            if matches!(data_type, DataType::Utf8) {
                let inspection = inspect_single_utf8_column(column, output_budget.remaining())?;
                selected_utf8_prevalidated = inspection.supported;
                if inspection.supported {
                    output_budget.add(inspection.estimated_value_bytes)?;
                }
                single_utf8_value = match inspection.value {
                    Some(value) => SingleUtf8Value::Valid(value),
                    None => SingleUtf8Value::Invalid,
                };
                if supported {
                    supported = inspection.supported;
                }
            } else {
                single_utf8_value = SingleUtf8Value::Invalid;
            }
        }

        if supported && !selected_utf8_prevalidated {
            supported = match data_type {
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
        }
        Ok(())
    })?;
    if !requested_column_seen {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "single UTF-8 column index {} out of range (num_columns={})",
                single_utf8_column.unwrap(),
                columns
            ),
        ));
    }
    Ok(EncodedPreflight {
        supported: columns > 0 && supported,
        single_utf8_value,
    })
}

fn estimate_integer_value_bytes(
    data_type: &DataType,
    column: EncodedColumn<'_>,
    limit: usize,
) -> io::Result<usize> {
    if column.encoding() == ENCODING_ALL_NULL || !has_non_null(&column) {
        return Ok(0);
    }
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
    if column.encoding() == ENCODING_ALL_NULL || !has_non_null(&column) {
        return Ok(0);
    }
    checked_estimated_bytes(column.num_rows(), max_value_bytes, limit)
}

pub(crate) fn write_encoded_supported<W: Write>(
    row_group: &RowGroupReader,
    output: &mut W,
) -> io::Result<()> {
    let mut writer = EncodedJsonWriter {
        output,
        value_buffer: Vec::with_capacity(64),
        repeated_buffer: Vec::with_capacity(REPEATED_VALUE_BUFFER_BYTES),
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
        ENCODING_ALL_NULL | ENCODING_PLAIN => Ok(true),
        ENCODING_CONST => {
            if !has_non_null(&column) {
                return Ok(true);
            }
            let type_matches = matches!(
                (column.data_type(), column.constant()),
                (DataType::Int8, Some(EncodedValueRef::Int8(_)))
                    | (DataType::Int16, Some(EncodedValueRef::Int16(_)))
                    | (DataType::Int32, Some(EncodedValueRef::Int32(_)))
                    | (DataType::Int64, Some(EncodedValueRef::Int64(_)))
            );
            if type_matches {
                Ok(true)
            } else {
                Err(encoded_type_mismatch(column.data_type()))
            }
        }
        ENCODING_DICT => {
            for value in column.values() {
                value?;
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn validate_float64_column(column: EncodedColumn<'_>) -> io::Result<bool> {
    match column.encoding() {
        ENCODING_ALL_NULL => Ok(true),
        ENCODING_CONST => {
            if !has_non_null(&column) {
                return Ok(true);
            }
            match column.constant() {
                Some(EncodedValueRef::Float64(value)) => Ok(is_supported_double(value)),
                _ => Err(encoded_type_mismatch(column.data_type())),
            }
        }
        ENCODING_DICT | ENCODING_PLAIN => {
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
        ENCODING_ALL_NULL => Ok(ColumnValidation {
            supported: true,
            estimated_value_bytes: 0,
        }),
        ENCODING_CONST => {
            if !has_non_null(&column) {
                return Ok(ColumnValidation {
                    supported: true,
                    estimated_value_bytes: 0,
                });
            }
            match column.constant() {
                Some(EncodedValueRef::Utf8(value)) => {
                    std::str::from_utf8(value).map_err(invalid_utf8)?;
                    Ok(ColumnValidation {
                        supported: true,
                        estimated_value_bytes: checked_estimated_bytes(
                            column.num_rows(),
                            escaped_utf8_len(value)?,
                            limit,
                        )?,
                    })
                }
                _ => Err(encoded_type_mismatch(column.data_type())),
            }
        }
        ENCODING_DICT | ENCODING_PLAIN => {
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

#[derive(Debug)]
struct SingleUtf8Inspection {
    supported: bool,
    value: Option<Vec<u8>>,
    estimated_value_bytes: usize,
}

fn inspect_single_utf8_column(
    column: EncodedColumn<'_>,
    limit: usize,
) -> io::Result<SingleUtf8Inspection> {
    if column.num_rows() == 0 {
        return Ok(SingleUtf8Inspection {
            supported: matches!(
                column.encoding(),
                ENCODING_ALL_NULL | ENCODING_CONST | ENCODING_DICT | ENCODING_PLAIN
            ),
            value: None,
            estimated_value_bytes: 0,
        });
    }

    match column.encoding() {
        ENCODING_ALL_NULL => Ok(SingleUtf8Inspection {
            supported: true,
            value: None,
            estimated_value_bytes: 0,
        }),
        ENCODING_CONST => inspect_single_const_utf8_column(column, limit),
        ENCODING_DICT | ENCODING_PLAIN => inspect_single_utf8_values(column.values(), limit),
        encoding => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported single UTF-8 column encoding {}", encoding),
        )),
    }
}

fn inspect_single_const_utf8_column(
    column: EncodedColumn<'_>,
    limit: usize,
) -> io::Result<SingleUtf8Inspection> {
    let has_null = column.has_nulls();
    let has_non_null = has_non_null(&column);

    let (value, estimated_value_bytes) = if has_non_null {
        match column.constant() {
            Some(EncodedValueRef::Utf8(value)) => {
                std::str::from_utf8(value).map_err(invalid_utf8)?;
                (
                    (!has_null && !value.is_empty()).then(|| value.to_vec()),
                    checked_estimated_bytes(column.num_rows(), escaped_utf8_len(value)?, limit)?,
                )
            }
            _ => return Err(encoded_type_mismatch(column.data_type())),
        }
    } else {
        (None, 0)
    };
    Ok(SingleUtf8Inspection {
        supported: true,
        value,
        estimated_value_bytes,
    })
}

fn inspect_single_utf8_values<'a>(
    values: impl Iterator<Item = io::Result<EncodedValueRef<'a>>>,
    limit: usize,
) -> io::Result<SingleUtf8Inspection> {
    let mut unique: Option<Vec<u8>> = None;
    let mut contract_valid = true;
    let mut estimated_value_bytes = 0usize;
    for value in values {
        let value = match value? {
            EncodedValueRef::Null => {
                contract_valid = false;
                unique = None;
                continue;
            }
            EncodedValueRef::Utf8(value) => value,
            _ => return Err(encoded_type_mismatch(&DataType::Utf8)),
        };
        std::str::from_utf8(value).map_err(invalid_utf8)?;
        estimated_value_bytes = estimated_value_bytes
            .checked_add(escaped_utf8_len(value)?)
            .ok_or_else(output_budget_exceeded)?;
        if estimated_value_bytes > limit {
            return Err(output_budget_exceeded());
        }
        if value.is_empty() {
            contract_valid = false;
            unique = None;
            continue;
        }
        if !contract_valid {
            continue;
        }
        match &unique {
            Some(expected) if expected.as_slice() != value => {
                contract_valid = false;
                unique = None;
            }
            Some(_) => {}
            None => unique = Some(value.to_vec()),
        }
    }
    Ok(SingleUtf8Inspection {
        supported: true,
        value: contract_valid.then_some(unique).flatten(),
        estimated_value_bytes,
    })
}

fn invalid_utf8(error: std::str::Utf8Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid UTF-8 column value: {}", error),
    )
}

fn has_non_null(column: &EncodedColumn<'_>) -> bool {
    if column.num_rows() == 0 || column.encoding() == ENCODING_ALL_NULL {
        return false;
    }
    let Some(bitmap) = column.null_bitmap() else {
        return true;
    };

    let full_bytes = column.num_rows() / 8;
    if bitmap[..full_bytes].iter().any(|value| *value != u8::MAX) {
        return true;
    }
    let remaining = column.num_rows() % 8;
    remaining > 0 && bitmap[full_bytes] & ((1u8 << remaining) - 1) != (1u8 << remaining) - 1
}

struct EncodedJsonWriter<'a, W> {
    output: &'a mut W,
    value_buffer: Vec<u8>,
    repeated_buffer: Vec<u8>,
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
        write_escaped_utf8(self.output, name.as_bytes())?;
        self.output.write_all(b"\":\"")?;
        self.write_array(data_type, column)?;
        self.output.write_all(b"\"")
    }

    fn write_array(&mut self, data_type: &DataType, column: EncodedColumn<'_>) -> io::Result<()> {
        match column.encoding() {
            ENCODING_ALL_NULL => write_empty_array(self.output, column.num_rows()),
            ENCODING_CONST => self.write_constant(data_type, column),
            ENCODING_DICT | ENCODING_PLAIN => {
                for (row, value) in column.values().enumerate() {
                    write_encoded_value(self.output, data_type, value?, row > 0)?;
                }
                Ok(())
            }
            encoding => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported column encoding {}", encoding),
            )),
        }
    }

    fn write_constant(
        &mut self,
        data_type: &DataType,
        column: EncodedColumn<'_>,
    ) -> io::Result<()> {
        let value = column
            .constant()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing CONST value"))?;
        if column.has_nulls()
            && matches!(data_type, DataType::Int16)
            && matches!(value, EncodedValueRef::Int16(0))
        {
            return write_nullable_zero_int16_constant(
                self.output,
                column,
                &mut self.repeated_buffer,
            );
        }
        if let (DataType::Utf8, EncodedValueRef::Utf8(value)) = (data_type, value) {
            return write_utf8_constant(
                self.output,
                value,
                column.num_rows(),
                column.has_nulls(),
                |row| column.is_null(row),
                &mut self.value_buffer,
                &mut self.repeated_buffer,
            );
        }
        self.value_buffer.clear();
        write_encoded_value(&mut self.value_buffer, data_type, value, false)?;
        if column.has_nulls() {
            return write_nullable_repeated_value(
                self.output,
                &self.value_buffer,
                column.num_rows(),
                |row| column.is_null(row),
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

fn write_utf8_constant<W, F>(
    output: &mut W,
    value: &[u8],
    row_count: usize,
    has_nulls: bool,
    mut is_null: F,
    value_buffer: &mut Vec<u8>,
    repeated_buffer: &mut Vec<u8>,
) -> io::Result<()>
where
    W: Write,
    F: FnMut(usize) -> bool,
{
    if is_oversized_escaped_utf8(value) {
        for row in 0..row_count {
            if row > 0 {
                output.write_all(b",")?;
            }
            if !has_nulls || !is_null(row) {
                write_escaped_utf8_chunked(output, value)?;
            }
        }
        return Ok(());
    }

    value_buffer.clear();
    write_escaped_utf8(value_buffer, value)?;
    if has_nulls {
        write_nullable_repeated_value(output, value_buffer, row_count, is_null, repeated_buffer)
    } else {
        write_repeated_value(output, value_buffer, row_count, repeated_buffer)
    }
}

fn is_oversized_escaped_utf8(value: &[u8]) -> bool {
    if value.len() >= REPEATED_VALUE_BUFFER_BYTES {
        return true;
    }

    let mut escaped_len = value.len();
    for current in value {
        let extra = match current {
            b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 1,
            0x00..=0x1f => 5,
            _ => 0,
        };
        escaped_len += extra;
        if escaped_len >= REPEATED_VALUE_BUFFER_BYTES {
            return true;
        }
    }
    false
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
            write_escaped_utf8(output, value)
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

fn write_nullable_zero_int16_constant<W: Write>(
    output: &mut W,
    column: EncodedColumn<'_>,
    repeated_buffer: &mut Vec<u8>,
) -> io::Result<()> {
    let null_bitmap = column.null_bitmap().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "nullable CONST column is missing its null bitmap",
        )
    })?;
    repeated_buffer.clear();

    let mut row = 0usize;
    while row + 8 <= column.num_rows() {
        let validity = !null_bitmap[row / 8];
        let block = zero_int16_block(validity, row == 0);
        if !repeated_buffer.is_empty()
            && repeated_buffer.len().saturating_add(block.len()) > REPEATED_VALUE_BUFFER_BYTES
        {
            output.write_all(repeated_buffer)?;
            repeated_buffer.clear();
        }
        repeated_buffer.extend_from_slice(block);
        row += 8;
    }

    while row < column.num_rows() {
        if !repeated_buffer.is_empty()
            && repeated_buffer.len().saturating_add(2) > REPEATED_VALUE_BUFFER_BYTES
        {
            output.write_all(repeated_buffer)?;
            repeated_buffer.clear();
        }
        if row > 0 {
            repeated_buffer.push(b',');
        }
        if null_bitmap[row / 8] & (1 << (row % 8)) == 0 {
            repeated_buffer.push(b'0');
        }
        row += 1;
    }

    if !repeated_buffer.is_empty() {
        output.write_all(repeated_buffer)?;
    }
    Ok(())
}

fn write_nullable_repeated_value<W, F>(
    output: &mut W,
    value: &[u8],
    row_count: usize,
    mut is_null: F,
    repeated_buffer: &mut Vec<u8>,
) -> io::Result<()>
where
    W: Write,
    F: FnMut(usize) -> bool,
{
    repeated_buffer.clear();
    for row in 0..row_count {
        let value_size = if is_null(row) { 0 } else { value.len() };
        let row_size = usize::from(row > 0) + value_size;
        if !repeated_buffer.is_empty()
            && repeated_buffer.len().saturating_add(row_size) > REPEATED_VALUE_BUFFER_BYTES
        {
            output.write_all(repeated_buffer)?;
            repeated_buffer.clear();
        }
        if row > 0 {
            repeated_buffer.push(b',');
        }
        if value_size > 0 {
            repeated_buffer.extend_from_slice(value);
        }
    }
    if !repeated_buffer.is_empty() {
        output.write_all(repeated_buffer)?;
    }
    Ok(())
}

fn zero_int16_block(validity: u8, first_block: bool) -> &'static [u8] {
    let blocks = if first_block {
        &ZERO_INT16_FIRST_BLOCKS
    } else {
        &ZERO_INT16_BLOCKS
    };
    let length = blocks.lengths[validity as usize] as usize;
    &blocks.bytes[validity as usize][..length]
}

fn encoded_type_mismatch(data_type: &DataType) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("encoded value does not match column type {:?}", data_type),
    )
}

/// Writes the customer column-oriented JSON protocol.
///
/// Returns `false` without touching `output` when a column or floating-point value cannot be
/// encoded byte-for-byte like the current Java fast path.
#[cfg(test)]
pub(crate) fn write_if_supported<W: Write>(
    batch: &RecordBatch,
    output: &mut W,
) -> io::Result<bool> {
    if !is_supported(batch)? {
        return Ok(false);
    }

    write_supported(batch, output)?;
    Ok(true)
}

#[cfg(test)]
pub(crate) fn write_supported<W: Write>(batch: &RecordBatch, output: &mut W) -> io::Result<()> {
    output.write_all(b"{")?;
    for (column_index, (field, array)) in batch
        .schema()
        .fields()
        .iter()
        .zip(batch.columns())
        .enumerate()
    {
        if column_index > 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"\"")?;
        write_escaped_utf8(output, field.name().as_bytes())?;
        output.write_all(b"\":\"")?;
        write_array(output, array, field.data_type())?;
        output.write_all(b"\"")?;
    }
    output.write_all(b"}")?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn is_supported(batch: &RecordBatch) -> io::Result<bool> {
    if batch.num_columns() == 0 {
        return Ok(false);
    }

    for (field, array) in batch.schema().fields().iter().zip(batch.columns()) {
        match field.data_type() {
            DataType::Int8 => {
                downcast::<Int8Array>(array, field.name())?;
            }
            DataType::Int16 => {
                downcast::<Int16Array>(array, field.name())?;
            }
            DataType::Int32 => {
                downcast::<Int32Array>(array, field.name())?;
            }
            DataType::Int64 => {
                downcast::<Int64Array>(array, field.name())?;
            }
            DataType::Float64 => {
                let values = downcast::<Float64Array>(array, field.name())?;
                let null_count = values.null_count();
                if null_count == 0 {
                    for &value in values.values().iter() {
                        if !is_supported_double(value) {
                            return Ok(false);
                        }
                    }
                } else if null_count != values.len() {
                    for row in 0..values.len() {
                        if !values.is_null(row) && !is_supported_double(values.value(row)) {
                            return Ok(false);
                        }
                    }
                }
            }
            DataType::Utf8 => {
                downcast::<StringArray>(array, field.name())?;
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

#[cfg(test)]
fn downcast<'a, T: 'static>(array: &'a ArrayRef, name: &str) -> io::Result<&'a T> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Arrow array type does not match column '{}'", name),
        )
    })
}

#[cfg(test)]
fn write_array<W: Write>(output: &mut W, array: &ArrayRef, data_type: &DataType) -> io::Result<()> {
    match data_type {
        DataType::Int8 => write_int8(output, downcast(array, "")?),
        DataType::Int16 => write_int16(output, downcast(array, "")?),
        DataType::Int32 => write_int32(output, downcast(array, "")?),
        DataType::Int64 => write_int64(output, downcast(array, "")?),
        DataType::Float64 => write_float64(output, downcast(array, "")?),
        DataType::Utf8 => write_utf8(output, downcast(array, "")?),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported columnar JSON type {:?}", data_type),
        )),
    }
}

#[cfg(test)]
macro_rules! write_integer_array {
    ($name:ident, $array_type:ty) => {
        fn $name<W: Write>(output: &mut W, array: &$array_type) -> io::Result<()> {
            let row_count = array.len();
            let null_count = array.null_count();
            if null_count == row_count {
                return write_empty_array(output, row_count);
            }

            if null_count == 0 {
                for (row, &value) in array.values().iter().enumerate() {
                    write_i64_value(output, value as i64, row > 0)?;
                }
            } else {
                for row in 0..row_count {
                    if array.is_null(row) {
                        write_separator(output, row)?;
                    } else {
                        write_i64_value(output, array.value(row) as i64, row > 0)?;
                    }
                }
            }
            Ok(())
        }
    };
}

#[cfg(test)]
write_integer_array!(write_int8, Int8Array);
#[cfg(test)]
write_integer_array!(write_int32, Int32Array);
#[cfg(test)]
write_integer_array!(write_int64, Int64Array);

#[cfg(test)]
fn write_int16<W: Write>(output: &mut W, array: &Int16Array) -> io::Result<()> {
    let row_count = array.len();
    let null_count = array.null_count();
    if null_count == row_count {
        return write_empty_array(output, row_count);
    }

    if null_count == 0 {
        let values = array.values();
        let mut row = 0;
        while row + 8 <= row_count {
            if values[row..row + 8].iter().all(|value| *value == 0) {
                output.write_all(if row == 0 {
                    ZERO_INT16_FIRST_BLOCK
                } else {
                    ZERO_INT16_BLOCK
                })?;
            } else {
                for &value in &values[row..row + 8] {
                    write_i64_value(output, value as i64, row > 0)?;
                    row += 1;
                }
                continue;
            }
            row += 8;
        }
        for &value in &values[row..] {
            write_i64_value(output, value as i64, row > 0)?;
            row += 1;
        }
    } else {
        let values = array.values();
        let mut row = 0;
        while row + 8 <= row_count {
            if values[row..row + 8].iter().all(|value| *value == 0) {
                write_zero_int16_block(output, validity_mask(array, row), row == 0)?;
                row += 8;
                continue;
            }
            let block_end = row + 8;
            while row < block_end {
                if array.is_null(row) {
                    write_separator(output, row)?;
                } else {
                    write_i64_value(output, values[row] as i64, row > 0)?;
                }
                row += 1;
            }
        }
        while row < row_count {
            if array.is_null(row) {
                write_separator(output, row)?;
            } else {
                write_i64_value(output, values[row] as i64, row > 0)?;
            }
            row += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
fn validity_mask(array: &Int16Array, row: usize) -> u8 {
    let nulls = array
        .nulls()
        .expect("validity mask is only read for nullable arrays");
    let bit_offset = nulls.offset() + row;
    let byte_offset = bit_offset >> 3;
    let shift = bit_offset & 7;
    let validity = nulls.validity();
    let mut bits = validity[byte_offset] as u16;
    if shift != 0 {
        bits |= (validity[byte_offset + 1] as u16) << 8;
    }
    (bits >> shift) as u8
}

#[cfg(test)]
fn write_zero_int16_block<W: Write>(
    output: &mut W,
    validity: u8,
    first_block: bool,
) -> io::Result<()> {
    output.write_all(zero_int16_block(validity, first_block))
}

#[cfg(test)]
fn write_float64<W: Write>(output: &mut W, array: &Float64Array) -> io::Result<()> {
    let row_count = array.len();
    let null_count = array.null_count();
    if null_count == row_count {
        return write_empty_array(output, row_count);
    }

    if null_count == 0 {
        for (row, &value) in array.values().iter().enumerate() {
            write_double(output, value, row > 0)?;
        }
    } else {
        for row in 0..row_count {
            if array.is_null(row) {
                write_separator(output, row)?;
            } else {
                write_double(output, array.value(row), row > 0)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn write_utf8<W: Write>(output: &mut W, array: &StringArray) -> io::Result<()> {
    let row_count = array.len();
    let null_count = array.null_count();
    if null_count == row_count {
        return write_empty_array(output, row_count);
    }

    if null_count == 0 {
        for row in 0..row_count {
            write_separator(output, row)?;
            write_escaped_utf8(output, array.value(row).as_bytes())?;
        }
    } else {
        for row in 0..row_count {
            write_separator(output, row)?;
            if !array.is_null(row) {
                write_escaped_utf8(output, array.value(row).as_bytes())?;
            }
        }
    }
    Ok(())
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

#[cfg(test)]
fn write_separator<W: Write>(output: &mut W, row: usize) -> io::Result<()> {
    if row > 0 {
        output.write_all(b",")?;
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
    use std::sync::Arc;

    use arrow_array::{
        ArrayRef, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, Int8Array,
        RecordBatch, StringArray,
    };
    use arrow_schema::{DataType, Field, Schema};
    use mosaic_core::bucket_reader::EncodedValueRef;

    use super::{
        check_row_count, inspect_single_utf8_values, is_oversized_escaped_utf8, write_empty_array,
        write_escaped_utf8, write_if_supported, write_utf8_constant, OutputBudget, COMMA_BLOCK,
        MAX_COLUMNAR_JSON_ROWS, MAX_UNCOMPRESSED_JSON_BYTES, REPEATED_VALUE_BUFFER_BYTES,
    };

    #[derive(Default)]
    struct TrackingWriter {
        output: Vec<u8>,
        writes: usize,
        max_write: usize,
    }

    impl Write for TrackingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            self.writes += 1;
            self.max_write = self.max_write.max(buf.len());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn repeated_utf8_expected(
        value: &[u8],
        row_count: usize,
        mut is_null: impl FnMut(usize) -> bool,
    ) -> Vec<u8> {
        let mut escaped = Vec::new();
        write_escaped_utf8(&mut escaped, value).unwrap();

        let mut expected = Vec::new();
        for row in 0..row_count {
            if row > 0 {
                expected.push(b',');
            }
            if !is_null(row) {
                expected.extend_from_slice(&escaped);
            }
        }
        expected
    }

    #[test]
    fn oversized_utf8_constants_stream_without_staging_or_byte_changes() {
        let mut value = vec![b'x'; REPEATED_VALUE_BUFFER_BYTES + 17];
        value[1] = b'"';
        value[2] = b'\\';
        value[3] = b'\n';
        value[4] = 0x01;
        assert!(is_oversized_escaped_utf8(&value));

        let mut output = TrackingWriter::default();
        let mut value_buffer = b"value sentinel".to_vec();
        let mut repeated_buffer = b"repeated sentinel".to_vec();
        write_utf8_constant(
            &mut output,
            &value,
            3,
            true,
            |row| row == 1,
            &mut value_buffer,
            &mut repeated_buffer,
        )
        .unwrap();

        assert_eq!(
            output.output,
            repeated_utf8_expected(&value, 3, |row| row == 1)
        );
        assert!(output.max_write <= REPEATED_VALUE_BUFFER_BYTES);
        assert_eq!(value_buffer, b"value sentinel");
        assert_eq!(repeated_buffer, b"repeated sentinel");
    }

    #[test]
    fn escaped_size_selects_streaming_before_the_repeat_buffer_would_overflow() {
        let safe = vec![b'x'; REPEATED_VALUE_BUFFER_BYTES - 1];
        assert!(!is_oversized_escaped_utf8(&safe));

        let mut escaped_to_limit = safe;
        escaped_to_limit[0] = b'"';
        assert!(is_oversized_escaped_utf8(&escaped_to_limit));
    }

    #[test]
    fn selected_utf8_inspection_replaces_a_second_full_validation_pass() {
        let repeated = [
            Ok(EncodedValueRef::Utf8(b"VIN-001")),
            Ok(EncodedValueRef::Utf8(b"VIN-001")),
        ];
        assert_eq!(
            inspect_single_utf8_values(repeated.into_iter(), MAX_UNCOMPRESSED_JSON_BYTES)
                .unwrap()
                .value,
            Some(b"VIN-001".to_vec())
        );

        let invalid_utf8 = [0xff];
        let contract_invalid_then_corrupt = [
            Ok(EncodedValueRef::Utf8(b"VIN-001")),
            Ok(EncodedValueRef::Utf8(b"VIN-002")),
            Ok(EncodedValueRef::Utf8(&invalid_utf8)),
        ];
        let error = inspect_single_utf8_values(
            contract_invalid_then_corrupt.into_iter(),
            MAX_UNCOMPRESSED_JSON_BYTES,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("invalid UTF-8 column value"));
    }

    #[test]
    fn rejects_row_counts_above_the_native_work_budget() {
        let error = check_row_count(MAX_COLUMNAR_JSON_ROWS + 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("row budget"));
    }

    #[test]
    fn rejects_highly_compressible_output_above_the_uncompressed_budget() {
        let mut budget = OutputBudget::new();
        let row_count = MAX_COLUMNAR_JSON_ROWS;
        let mut column = 0usize;
        loop {
            match budget.add_column_structure("all_null", column, row_count) {
                Ok(()) => column += 1,
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                    assert!(error.to_string().contains("uncompressed output budget"));
                    assert!(column > 0);
                    break;
                }
            }
        }
    }

    #[test]
    fn writes_all_null_arrays_in_large_comma_blocks() {
        let row_count = COMMA_BLOCK.len() * 2 + 17;
        let mut output = TrackingWriter::default();

        write_empty_array(&mut output, row_count).unwrap();

        assert_eq!(output.output.len(), row_count - 1);
        assert!(output.output.iter().all(|value| *value == b','));
        assert_eq!(output.writes, 3);
        assert_eq!(output.max_write, COMMA_BLOCK.len());
    }

    #[test]
    fn writes_exact_primitive_protocol() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("i\"8", DataType::Int8, true),
            Field::new("i16", DataType::Int16, true),
            Field::new("i32", DataType::Int32, true),
            Field::new("i64", DataType::Int64, true),
            Field::new("double", DataType::Float64, true),
            Field::new("text", DataType::Utf8, true),
        ]));
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(Int8Array::from(vec![Some(-1), None, Some(9)])),
            Arc::new(Int16Array::from(vec![Some(0), Some(-7), Some(12)])),
            Arc::new(Int32Array::from(vec![
                Some(i32::MIN),
                Some(0),
                Some(i32::MAX),
            ])),
            Arc::new(Int64Array::from(vec![Some(i64::MIN), None, Some(i64::MAX)])),
            Arc::new(Float64Array::from(vec![
                Some(-0.0),
                Some(1.2),
                Some(9_999_999.0),
            ])),
            Arc::new(StringArray::from(vec![Some("a\"\n"), None, Some("中\t")])),
        ];
        let batch = RecordBatch::try_new(schema, arrays).unwrap();

        let mut output = Vec::new();
        assert!(write_if_supported(&batch, &mut output).unwrap());
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "{\"i\\\"8\":\"-1,,9\",\"i16\":\"0,-7,12\",\
             \"i32\":\"-2147483648,0,2147483647\",\
             \"i64\":\"-9223372036854775808,,9223372036854775807\",\
             \"double\":\"-0.0,1.2,9999999.0\",\"text\":\"a\\\"\\n,,中\\t\"}"
        );
    }

    #[test]
    fn writes_nullable_zero_int16_blocks() {
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("i16", DataType::Int16, true)])),
            vec![Arc::new(Int16Array::from(vec![
                None,
                Some(0),
                None,
                Some(0),
                Some(0),
                None,
                None,
                Some(0),
                Some(0),
                None,
                Some(0),
                None,
                Some(0),
                Some(0),
                None,
                Some(0),
                Some(-7),
            ]))],
        )
        .unwrap();
        let mut output = Vec::new();

        assert!(write_if_supported(&batch, &mut output).unwrap());
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "{\"i16\":\",0,,0,0,,,0,0,,0,,0,0,,0,-7\"}"
        );
    }

    #[test]
    fn unsupported_double_does_not_touch_output() {
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "value",
                DataType::Float64,
                false,
            )])),
            vec![Arc::new(Float64Array::from(vec![f64::MIN_POSITIVE]))],
        )
        .unwrap();
        let mut output = b"sentinel".to_vec();

        assert!(!write_if_supported(&batch, &mut output).unwrap());
        assert_eq!(output, b"sentinel");
    }

    #[test]
    fn writes_java_style_fallback_doubles() {
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "value",
                DataType::Float64,
                false,
            )])),
            vec![Arc::new(Float64Array::from(vec![
                1.25,
                0.0001,
                10_000_000.0,
                429_496_729.5,
                1_812_576.4000000001,
            ]))],
        )
        .unwrap();
        let mut output = Vec::new();

        assert!(write_if_supported(&batch, &mut output).unwrap());
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "{\"value\":\"1.25,1.0E-4,1.0E7,4.294967295E8,1812576.4000000001\"}"
        );
    }

    #[test]
    fn unsupported_type_does_not_touch_output() {
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "value",
                DataType::Float32,
                false,
            )])),
            vec![Arc::new(Float32Array::from(vec![1.0]))],
        )
        .unwrap();
        let mut output = b"sentinel".to_vec();

        assert!(!write_if_supported(&batch, &mut output).unwrap());
        assert_eq!(output, b"sentinel");
    }
}
