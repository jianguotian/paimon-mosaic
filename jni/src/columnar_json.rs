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

use std::collections::BTreeSet;
use std::io::{self, Write};

use arrow_schema::DataType;
use mosaic_core::bucket_reader::{EncodedColumn, EncodedValueRef};
use mosaic_core::reader::{Encoding, RowGroupReader};

const MIN_EXACT_DOUBLE_ABS: f64 = 1.0e-6;
const MAX_EXACT_DOUBLE_ABS: f64 = 1.0e9;
const MAX_COLUMNAR_JSON_ROWS: usize = 1_000_000;
const MAX_UNCOMPRESSED_JSON_BYTES: usize = 512 * 1024 * 1024;
const MAX_DOUBLE_VALUE_BYTES: usize = 32;
const MAX_DISTINCT_JAVA_DOUBLE_VALUES: usize = 65_536;
const COMMA_BLOCK: [u8; 64] = [b','; 64];
const REPEATED_VALUE_BUFFER_BYTES: usize = 64 * 1024;
const NULLABLE_CONST_PATTERN_COUNT: usize = 256;
const NULLABLE_CONST_CACHE_BYTES: usize = 64 * 1024;

pub(crate) struct EncodedJsonPreflight {
    java_double_bits: Vec<u64>,
}

impl EncodedJsonPreflight {
    pub(crate) fn java_double_bits(&self) -> &[u64] {
        &self.java_double_bits
    }

    pub(crate) fn complete(self, values: Vec<Vec<u8>>) -> io::Result<EncodedJsonPlan> {
        if values.len() != self.java_double_bits.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Java returned {} DOUBLE strings for {} requested values",
                    values.len(),
                    self.java_double_bits.len()
                ),
            ));
        }
        for (&bits, value) in self.java_double_bits.iter().zip(&values) {
            let rendered = std::str::from_utf8(value).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Java returned non-UTF-8 DOUBLE text: {}", error),
                )
            })?;
            let parsed = rendered.parse::<f64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Java returned invalid DOUBLE text '{}': {}",
                        rendered, error
                    ),
                )
            })?;
            if parsed.to_bits() != bits {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Java DOUBLE text '{}' does not round-trip to 0x{:016x}",
                        rendered, bits
                    ),
                ));
            }
        }
        Ok(EncodedJsonPlan {
            java_double_values: self.java_double_bits.into_iter().zip(values).collect(),
        })
    }
}

#[derive(Default)]
pub(crate) struct EncodedJsonPlan {
    java_double_values: Vec<(u64, Vec<u8>)>,
}

impl EncodedJsonPlan {
    fn java_double_value(&self, bits: u64) -> Option<&[u8]> {
        let index = self
            .java_double_values
            .binary_search_by_key(&bits, |(stored_bits, _)| *stored_bits)
            .ok()?;
        Some(self.java_double_values[index].1.as_slice())
    }
}

struct OutputBudget {
    estimated_bytes: usize,
}

impl OutputBudget {
    fn new() -> Self {
        Self { estimated_bytes: 2 }
    }

    fn add(&mut self, bytes: usize) -> io::Result<()> {
        let next = self
            .estimated_bytes
            .checked_add(bytes)
            .ok_or_else(output_budget_exceeded)?;
        if next > MAX_UNCOMPRESSED_JSON_BYTES {
            return Err(output_budget_exceeded());
        }
        self.estimated_bytes = next;
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

fn has_supported_structure(data_type: &DataType, encoding: Encoding) -> bool {
    if encoding == Encoding::AllNull {
        return true;
    }
    matches!(
        data_type,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::Float64
            | DataType::Decimal128(_, _)
            | DataType::Utf8
    ) && matches!(encoding, Encoding::Const | Encoding::Dict | Encoding::Plain)
}

struct StructurePreflight {
    supported_columns: Vec<bool>,
    output_budget: OutputBudget,
    fallback_required: bool,
}

fn prepare_structure(row_group: &RowGroupReader) -> io::Result<StructurePreflight> {
    let mut preflight = StructurePreflight {
        supported_columns: Vec::new(),
        output_budget: OutputBudget::new(),
        fallback_required: false,
    };
    row_group.visit_encoded_columns(|name, data_type, _, column| {
        let column_index = preflight.supported_columns.len();
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

        let supported = has_supported_structure(data_type, column.encoding());
        preflight.supported_columns.push(supported);
        if !supported {
            preflight.fallback_required = true;
            return Ok(());
        }

        if preflight.fallback_required {
            return Ok(());
        }
        if let Err(error) =
            preflight
                .output_budget
                .add_column_structure(name, column_index, column.num_rows())
        {
            if error.kind() == io::ErrorKind::Unsupported {
                preflight.fallback_required = true;
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    })?;
    Ok(preflight)
}

/// Prepares an encoded row group to be emitted byte-for-byte like the Java fast path.
///
/// No output object is created until this preflight succeeds. In addition to type and floating
/// point compatibility, it validates dictionary indexes and UTF-8 payloads which Arrow
/// materialization previously checked before touching output.
pub(crate) fn prepare_encoded(
    row_group: &RowGroupReader,
) -> io::Result<Option<EncodedJsonPreflight>> {
    if let Err(error) = check_row_count(row_group.num_rows()) {
        return if error.kind() == io::ErrorKind::Unsupported {
            Ok(None)
        } else {
            Err(error)
        };
    }

    let StructurePreflight {
        supported_columns,
        mut output_budget,
        mut fallback_required,
    } = match prepare_structure(row_group) {
        Ok(preflight) => preflight,
        Err(error) if error.kind() == io::ErrorKind::Unsupported => return Ok(None),
        Err(error) => return Err(error),
    };
    let columns = supported_columns.len();
    if columns == 0 {
        return Ok(None);
    }

    let mut validated_columns = 0usize;
    let mut java_double_bits = BTreeSet::new();
    let result = row_group.visit_encoded_columns(|_name, data_type, _, column| {
        let column_index = validated_columns;
        validated_columns += 1;
        let structurally_supported = supported_columns.get(column_index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "column count changed during columnar JSON preflight",
            )
        })?;
        if !structurally_supported {
            return Ok(());
        }

        let validation: io::Result<bool> = (|| {
            if column.encoding() == Encoding::AllNull {
                return Ok(true);
            }
            match data_type {
                DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
                    let column_supported = validate_integer_column(column)?;
                    if column_supported && !fallback_required {
                        output_budget.add(estimate_integer_value_bytes(
                            data_type,
                            column,
                            output_budget.remaining(),
                        )?)?;
                    }
                    Ok(column_supported)
                }
                DataType::Float64 => {
                    let column_supported = validate_float64_column(column, &mut java_double_bits)?;
                    if column_supported && !fallback_required {
                        output_budget.add(estimate_fixed_value_bytes(
                            column,
                            MAX_DOUBLE_VALUE_BYTES,
                            output_budget.remaining(),
                        )?)?;
                    }
                    Ok(column_supported)
                }
                DataType::Decimal128(precision, scale) => {
                    let column_supported = validate_decimal_column(column)?;
                    if column_supported && !fallback_required {
                        output_budget.add(estimate_fixed_value_bytes(
                            column,
                            max_decimal_value_bytes(*precision, *scale),
                            output_budget.remaining(),
                        )?)?;
                    }
                    Ok(column_supported)
                }
                DataType::Utf8 => {
                    let remaining = if fallback_required {
                        usize::MAX
                    } else {
                        output_budget.remaining()
                    };
                    let validation = validate_utf8_column(column, remaining)?;
                    if validation.supported && !fallback_required {
                        output_budget.add(validation.estimated_value_bytes)?;
                    }
                    Ok(validation.supported)
                }
                _ => Ok(false),
            }
        })();

        match validation {
            Ok(true) => {}
            Ok(false) => fallback_required = true,
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                fallback_required = true;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    });
    match result {
        Ok(()) if validated_columns == columns && fallback_required => Ok(None),
        Ok(()) if validated_columns == columns => Ok(Some(EncodedJsonPreflight {
            java_double_bits: java_double_bits.into_iter().collect(),
        })),
        Ok(()) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "column count changed during columnar JSON preflight",
        )),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn write_encoded_supported<W: Write>(
    row_group: &RowGroupReader,
    plan: &EncodedJsonPlan,
    output: &mut W,
) -> io::Result<()> {
    let mut writer = EncodedJsonWriter {
        output,
        plan,
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

fn validate_float64_column(
    column: EncodedColumn<'_>,
    java_double_bits: &mut BTreeSet<u64>,
) -> io::Result<bool> {
    match column.encoding() {
        Encoding::AllNull => Ok(true),
        Encoding::Const => {
            if !has_non_null(&column) {
                return Ok(true);
            }
            match column.constant()? {
                Some(EncodedValueRef::Float64(value)) => {
                    prepare_double_value(value, java_double_bits)
                }
                _ => Err(encoded_type_mismatch(column.data_type())),
            }
        }
        Encoding::Dict | Encoding::Plain => {
            let mut supported = true;
            for value in column.values() {
                match value? {
                    EncodedValueRef::Null => {}
                    EncodedValueRef::Float64(value) => {
                        match prepare_double_value(value, java_double_bits) {
                            Ok(value_supported) => supported &= value_supported,
                            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                                supported = false;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    _ => return Err(encoded_type_mismatch(column.data_type())),
                }
            }
            Ok(supported)
        }
        _ => Ok(false),
    }
}

fn prepare_double_value(value: f64, java_double_bits: &mut BTreeSet<u64>) -> io::Result<bool> {
    if !value.is_finite() {
        return Ok(false);
    }
    if !can_format_double_in_rust(value) {
        let bits = value.to_bits();
        if !java_double_bits.contains(&bits)
            && java_double_bits.len() >= MAX_DISTINCT_JAVA_DOUBLE_VALUES
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "columnar JSON requires more than {} distinct Java-formatted DOUBLE values",
                    MAX_DISTINCT_JAVA_DOUBLE_VALUES
                ),
            ));
        }
        java_double_bits.insert(bits);
    }
    Ok(true)
}

fn validate_decimal_column(column: EncodedColumn<'_>) -> io::Result<bool> {
    match column.encoding() {
        Encoding::AllNull => Ok(true),
        Encoding::Const => {
            if !has_non_null(&column) {
                return Ok(true);
            }
            match column.constant()? {
                Some(value) => {
                    decode_decimal_value(column.data_type(), value)?;
                    Ok(true)
                }
                None => Err(encoded_type_mismatch(column.data_type())),
            }
        }
        Encoding::Dict | Encoding::Plain => {
            for value in column.values() {
                let value = value?;
                if !matches!(value, EncodedValueRef::Null) {
                    decode_decimal_value(column.data_type(), value)?;
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn decode_decimal_value(data_type: &DataType, value: EncodedValueRef<'_>) -> io::Result<i128> {
    let (precision, unscaled) = match (data_type, value) {
        (DataType::Decimal128(precision, _), EncodedValueRef::DecimalCompact(value))
            if *precision <= 18 =>
        {
            (*precision, value as i128)
        }
        (DataType::Decimal128(precision, _), EncodedValueRef::DecimalLarge(bytes))
            if *precision > 18 =>
        {
            (*precision, decode_signed_i128(bytes)?)
        }
        _ => return Err(encoded_type_mismatch(data_type)),
    };
    if decimal_digits(unscaled) > precision as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "decimal value exceeds declared precision {} for {:?}",
                precision, data_type
            ),
        ));
    }
    Ok(unscaled)
}

fn decode_signed_i128(bytes: &[u8]) -> io::Result<i128> {
    if bytes.is_empty() || bytes.len() > 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Decimal128 byte width {}", bytes.len()),
        ));
    }
    let negative = bytes[0] & 0x80 != 0;
    let mut decoded = [if negative { 0xff } else { 0x00 }; 16];
    decoded[16 - bytes.len()..].copy_from_slice(bytes);
    Ok(i128::from_be_bytes(decoded))
}

fn decimal_digits(value: i128) -> usize {
    let mut magnitude = value.unsigned_abs();
    let mut digits = 1usize;
    while magnitude >= 10 {
        magnitude /= 10;
        digits += 1;
    }
    digits
}

fn max_decimal_value_bytes(precision: u8, scale: i8) -> usize {
    precision as usize + scale.unsigned_abs() as usize + 3
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
                    match checked_estimated_bytes(
                        non_null_count(&column),
                        escaped_utf8_len(value)?,
                        limit,
                    ) {
                        Ok(estimated_value_bytes) => Ok(ColumnValidation {
                            supported: true,
                            estimated_value_bytes,
                        }),
                        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                            Ok(ColumnValidation {
                                supported: false,
                                estimated_value_bytes: 0,
                            })
                        }
                        Err(error) => Err(error),
                    }
                }
                _ => Err(encoded_type_mismatch(column.data_type())),
            }
        }
        Encoding::Dict | Encoding::Plain => {
            let mut estimated_value_bytes = 0usize;
            let mut supported = true;
            for value in column.values() {
                match value? {
                    EncodedValueRef::Null => {}
                    EncodedValueRef::Utf8(value) => {
                        std::str::from_utf8(value).map_err(invalid_utf8)?;
                        if supported {
                            match estimated_value_bytes.checked_add(escaped_utf8_len(value)?) {
                                Some(next) if next <= limit => {
                                    estimated_value_bytes = next;
                                }
                                _ => supported = false,
                            }
                        }
                    }
                    _ => return Err(encoded_type_mismatch(column.data_type())),
                }
            }
            Ok(ColumnValidation {
                supported,
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
    plan: &'a EncodedJsonPlan,
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
                    write_encoded_value(self.output, data_type, value?, row > 0, self.plan)?;
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
        write_encoded_value(&mut self.value_buffer, data_type, value, false, self.plan)?;
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
    plan: &EncodedJsonPlan,
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
            write_exact_double(output, value, separator, plan)
        }
        (data_type @ DataType::Decimal128(_, scale), value) => write_decimal(
            output,
            decode_decimal_value(data_type, value)?,
            *scale,
            separator,
        ),
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

fn write_decimal<W: Write>(
    output: &mut W,
    value: i128,
    scale: i8,
    separator: bool,
) -> io::Result<()> {
    if separator {
        output.write_all(b",")?;
    }
    if value < 0 {
        output.write_all(b"-")?;
    }

    let mut buffer = [0u8; 39];
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
    let digits = &buffer[position..];

    match scale.cmp(&0) {
        std::cmp::Ordering::Equal => output.write_all(digits),
        std::cmp::Ordering::Less => {
            output.write_all(digits)?;
            // BigDecimal.toPlainString() renders zero with a negative scale as "0".
            if value == 0 {
                Ok(())
            } else {
                write_zeroes(output, scale.unsigned_abs() as usize)
            }
        }
        std::cmp::Ordering::Greater => {
            let scale = scale as usize;
            if digits.len() > scale {
                let decimal_position = digits.len() - scale;
                output.write_all(&digits[..decimal_position])?;
                output.write_all(b".")?;
                output.write_all(&digits[decimal_position..])
            } else {
                output.write_all(b"0.")?;
                write_zeroes(output, scale - digits.len())?;
                output.write_all(digits)
            }
        }
    }
}

fn can_format_double_in_rust(value: f64) -> bool {
    let bits = value.to_bits();
    if bits == 0 || bits == (1u64 << 63) {
        return true;
    }

    value.is_finite() && value.abs() >= MIN_EXACT_DOUBLE_ABS && value.abs() <= MAX_EXACT_DOUBLE_ABS
}

fn write_exact_double<W: Write>(
    output: &mut W,
    value: f64,
    separator: bool,
    plan: &EncodedJsonPlan,
) -> io::Result<()> {
    if can_format_double_in_rust(value) {
        return write_double(output, value, separator);
    }
    if !value.is_finite() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "non-finite DOUBLE reached the columnar JSON writer",
        ));
    }
    let rendered = plan.java_double_value(value.to_bits()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "missing Java DOUBLE rendering for 0x{:016x}",
                value.to_bits()
            ),
        )
    })?;
    if separator {
        output.write_all(b",")?;
    }
    output.write_all(rendered)
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
    use std::collections::BTreeSet;
    use std::io::{self, Write};
    use std::mem::size_of;
    use std::sync::Arc;

    use arrow_array::{BooleanArray, Float64Array, Int32Array, RecordBatch};
    use arrow_schema::DataType;
    use arrow_schema::{Field, Schema};
    use mosaic_core::bucket_reader::EncodedValueRef;
    use mosaic_core::reader::{Encoding, InputFile, MosaicReader, ReaderAccess};
    use mosaic_core::spec::{COMPRESSION_NONE, ENCODING_DICT};
    use mosaic_core::writer::{MosaicWriter, OutputFile, WriterOptions};

    use super::{
        append_nullable_const_block, can_format_double_in_rust, check_row_count,
        checked_estimated_bytes, decode_signed_i128, escaped_utf8_len, has_supported_structure,
        integer_value_matches, is_oversized_escaped_utf8, prepare_double_value, prepare_encoded,
        write_decimal, write_double, write_encoded_value, write_escaped_utf8, write_exact_double,
        write_nullable_repeated_rows, write_repeated_value, write_utf8_constant, EncodedJsonPlan,
        EncodedJsonPreflight, OutputBudget, MAX_COLUMNAR_JSON_ROWS,
        MAX_DISTINCT_JAVA_DOUBLE_VALUES, MAX_UNCOMPRESSED_JSON_BYTES, NULLABLE_CONST_CACHE_BYTES,
        NULLABLE_CONST_PATTERN_COUNT, REPEATED_VALUE_BUFFER_BYTES,
    };

    #[derive(Default)]
    struct MemoryOutput {
        data: Vec<u8>,
    }

    impl OutputFile for MemoryOutput {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            self.data.extend_from_slice(data);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn pos(&self) -> u64 {
            self.data.len() as u64
        }
    }

    struct MemoryInput {
        data: Vec<u8>,
    }

    impl InputFile for MemoryInput {
        fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            let start = offset as usize;
            let end = start
                .checked_add(buffer.len())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "read range overflow"))?;
            let source = self
                .data
                .get(start..end)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read past end"))?;
            buffer.copy_from_slice(source);
            Ok(())
        }
    }

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
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn accounts_for_complete_column_structure() {
        let mut budget = OutputBudget::new();
        budget.add_column_structure("a\"", 0, 3).unwrap();
        budget.add_column_structure("b", 1, 1).unwrap();
        assert_eq!(budget.estimated_bytes, 19);
    }

    #[test]
    fn checks_column_support_without_scanning_values() {
        assert!(has_supported_structure(&DataType::Int32, Encoding::Plain));
        assert!(has_supported_structure(
            &DataType::Boolean,
            Encoding::AllNull
        ));
        assert!(!has_supported_structure(&DataType::Boolean, Encoding::Dict));
        assert!(!has_supported_structure(
            &DataType::Utf8,
            Encoding::Other(99)
        ));
    }

    #[test]
    fn reports_corrupt_supported_column_before_later_unsupported_column() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("z", DataType::Boolean, false),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![
                Arc::new(Int32Array::from(vec![1, 2, 3, 1, 2, 3, 1, 2])),
                Arc::new(BooleanArray::from(vec![
                    true, false, true, false, true, false, true, false,
                ])),
            ],
        )
        .unwrap();
        let mut writer = MosaicWriter::new(
            MemoryOutput::default(),
            &schema,
            WriterOptions {
                compression: COMPRESSION_NONE,
                num_buckets: 1,
                ..Default::default()
            },
        )
        .unwrap();
        writer.write_batch(&batch).unwrap();
        writer.close().unwrap();

        let mut data = writer.output().data.clone();
        assert_eq!(data[0] & 0x03, ENCODING_DICT);
        assert_eq!(data[2], 3);
        // Header bytes: encoding/null flags, both dictionaries, then the first column's indexes.
        // A two-bit dictionary index of 3 is outside the valid 0..3 range.
        data[18] = (data[18] & !0x03) | 0x03;

        let file_len = data.len() as u64;
        let reader = MosaicReader::new(MemoryInput { data }, file_len).unwrap();
        let row_group = reader.row_group_reader(0).unwrap();
        let error = match prepare_encoded(&row_group) {
            Ok(_) => panic!("corrupt dictionary index was accepted as normal fallback"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("corrupt dict index"));
    }

    #[test]
    fn reports_corrupt_supported_column_after_earlier_unsupported_column() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("z", DataType::Boolean, false),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![
                Arc::new(Int32Array::from(vec![1, 2, 3, 1, 2, 3, 1, 2])),
                Arc::new(BooleanArray::from(vec![
                    true, false, true, false, true, false, true, false,
                ])),
            ],
        )
        .unwrap();
        let mut writer = MosaicWriter::new(
            MemoryOutput::default(),
            &schema,
            WriterOptions {
                compression: COMPRESSION_NONE,
                num_buckets: 1,
                ..Default::default()
            },
        )
        .unwrap();
        writer.write_batch(&batch).unwrap();
        writer.close().unwrap();

        let mut data = writer.output().data.clone();
        let mut baseline =
            MosaicReader::new(MemoryInput { data: data.clone() }, data.len() as u64).unwrap();
        baseline.project(&["z", "a"]).unwrap();
        let baseline_row_group = baseline.row_group_reader(0).unwrap();
        assert!(prepare_encoded(&baseline_row_group).unwrap().is_none());

        assert_eq!(data[0] & 0x03, ENCODING_DICT);
        assert_eq!(data[2], 3);
        data[18] = (data[18] & !0x03) | 0x03;

        let file_len = data.len() as u64;
        let mut reader = MosaicReader::new(MemoryInput { data }, file_len).unwrap();
        reader.project(&["z", "a"]).unwrap();
        let row_group = reader.row_group_reader(0).unwrap();
        let error = match prepare_encoded(&row_group) {
            Ok(_) => panic!("corrupt dictionary index was accepted as normal fallback"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("corrupt dict index"));
    }

    #[test]
    fn reports_corrupt_column_after_double_cardinality_fallback() {
        let row_count = MAX_DISTINCT_JAVA_DOUBLE_VALUES + 1;
        let schema = Schema::new(vec![
            Field::new("a", DataType::Float64, false),
            Field::new("z", DataType::Int32, false),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![
                Arc::new(Float64Array::from_iter_values(
                    (1..=row_count as u64).map(f64::from_bits),
                )),
                Arc::new(Int32Array::from_iter_values(
                    (0..row_count).map(|row| (row % 3 + 1) as i32),
                )),
            ],
        )
        .unwrap();
        let mut writer = MosaicWriter::new(
            MemoryOutput::default(),
            &schema,
            WriterOptions {
                compression: COMPRESSION_NONE,
                num_buckets: 2,
                ..Default::default()
            },
        )
        .unwrap();
        writer.write_batch(&batch).unwrap();
        writer.close().unwrap();

        let mut data = writer.output().data.clone();
        let reader =
            MosaicReader::new(MemoryInput { data: data.clone() }, data.len() as u64).unwrap();
        let buckets = reader.bucket_infos(0).unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].columns, vec![0]);
        assert_eq!(buckets[1].columns, vec![1]);
        let baseline_row_group = reader.row_group_reader(0).unwrap();
        assert!(prepare_encoded(&baseline_row_group).unwrap().is_none());

        let int_bucket_start = buckets[0].size;
        assert_eq!(data[int_bucket_start] & 0x03, ENCODING_DICT);
        assert_eq!(data[int_bucket_start + 2], 3);
        // Single-column bucket: two header bytes, one-byte dictionary size, three i32 values.
        let int_indexes_start = int_bucket_start + 2 + 1 + 3 * size_of::<i32>();
        data[int_indexes_start] = (data[int_indexes_start] & !0x03) | 0x03;

        let file_len = data.len() as u64;
        let reader = MosaicReader::new(MemoryInput { data }, file_len).unwrap();
        let row_group = reader.row_group_reader(0).unwrap();
        let error = match prepare_encoded(&row_group) {
            Ok(_) => panic!("corrupt dictionary index was accepted after DOUBLE fallback"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("corrupt dict index"));
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
            &EncodedJsonPlan::default(),
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
    fn identifies_doubles_formatted_in_rust() {
        assert!(can_format_double_in_rust(0.0));
        assert!(can_format_double_in_rust(-0.0));
        assert!(can_format_double_in_rust(1.0e-6));
        assert!(can_format_double_in_rust(1.0e9));
        assert!(!can_format_double_in_rust(f64::MIN_POSITIVE));
        assert!(!can_format_double_in_rust(f64::INFINITY));
        assert!(!can_format_double_in_rust(f64::NAN));
    }

    #[test]
    fn writes_java_supplied_double_text_exactly() {
        let value = f64::from_bits(0x3d20_0000_0000_0000);
        let plan = EncodedJsonPreflight {
            java_double_bits: vec![value.to_bits()],
        }
        .complete(vec![b"2.8421709430404007E-14".to_vec()])
        .unwrap();

        let mut output = Vec::new();
        write_exact_double(&mut output, value, true, &plan).unwrap();
        assert_eq!(output, b",2.8421709430404007E-14");
    }

    #[test]
    fn rejects_java_double_text_for_a_different_value() {
        let value = f64::from_bits(0x3d20_0000_0000_0000);
        let preflight = EncodedJsonPreflight {
            java_double_bits: vec![value.to_bits()],
        };
        let error = match preflight.complete(vec![b"1.0".to_vec()]) {
            Ok(_) => panic!("mismatched Java DOUBLE text was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("does not round-trip"));
    }

    #[test]
    fn bounds_java_formatted_double_cardinality() {
        let mut values = BTreeSet::new();
        for bits in 1..=MAX_DISTINCT_JAVA_DOUBLE_VALUES as u64 {
            assert!(prepare_double_value(f64::from_bits(bits), &mut values).unwrap());
        }
        let error = prepare_double_value(
            f64::from_bits(MAX_DISTINCT_JAVA_DOUBLE_VALUES as u64 + 1),
            &mut values,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("Java-formatted DOUBLE"));
    }

    #[test]
    fn writes_decimal_plain_strings_for_all_scales() {
        let mut output = Vec::new();
        write_decimal(&mut output, 12_340, 3, false).unwrap();
        write_decimal(&mut output, -5, 3, true).unwrap();
        write_decimal(&mut output, 123, -2, true).unwrap();
        write_decimal(&mut output, 0, -2, true).unwrap();
        write_decimal(&mut output, 0, 3, true).unwrap();
        assert_eq!(output, b"12.340,-0.005,12300,0,0.000");
    }

    #[test]
    fn decodes_signed_big_endian_decimal_values() {
        assert_eq!(decode_signed_i128(&[0xff]).unwrap(), -1);
        assert_eq!(decode_signed_i128(&[0x00, 0x80]).unwrap(), 128);
        assert_eq!(
            decode_signed_i128(&i128::MIN.to_be_bytes()).unwrap(),
            i128::MIN
        );
        assert!(decode_signed_i128(&[]).is_err());
        assert!(decode_signed_i128(&[0; 17]).is_err());
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
