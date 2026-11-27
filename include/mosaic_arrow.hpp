/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */

#pragma once

#include "mosaic.hpp"

#include <arrow/api.h>
#include <arrow/c/bridge.h>
#include <arrow/c/helpers.h>
#include <arrow/compute/cast.h>
#include <arrow/type_traits.h>

#include <algorithm>
#include <cstdint>
#include <vector>

namespace mosaic {

namespace detail {
struct ExportedBatch {
    ArrowArray array{};
    ArrowSchema schema{};
    ~ExportedBatch() {
        ArrowArrayRelease(&array);
        ArrowSchemaRelease(&schema);
    }
};
}  // namespace detail

/// Write top-level unsigned Arrow integers as wider signed integers. The file
/// schema passed here must be the same schema used to open the Writer.
/// max_value_bytes bounds the total widened value bytes per submitted slice;
/// validity bitmaps, allocator padding and the Writer's own buffers are separate.
/// If a write fails after earlier slices were appended, discard the partial file.
inline void write_with_unsigned_widening(Writer& writer, const arrow::RecordBatch& input,
                                         const std::shared_ptr<arrow::Schema>& file_schema,
                                         int64_t max_value_bytes) {
    if (max_value_bytes <= 0) throw Error("max_value_bytes must be positive");
    if (!file_schema || file_schema->num_fields() != input.num_columns()) {
        throw Error("file schema must have one field per input column");
    }
    auto status = input.Validate();
    if (!status.ok()) throw Error(status.ToString());

    // Check all columns before submitting the first slice.
    std::vector<int> widen;
    int64_t widened_bytes_per_row = 0;
    for (int i = 0; i < input.num_columns(); ++i) {
        const auto& field = file_schema->field(i);
        const auto& column = input.column(i);
        if (input.schema()->field(i)->name() != field->name()) {
            throw Error("input field name does not match file field " + field->name());
        }
        if (!field->nullable() && column->null_count() > 0) {
            throw Error("null in non-nullable file field " + field->name());
        }
        const auto& from = *column->type();
        const auto& to = *field->type();
        if (from.Equals(to)) continue;
        if (!(arrow::is_unsigned_integer(from) && arrow::is_signed_integer(to) &&
              from.bit_width() < to.bit_width())) {
            throw Error("unsupported conversion for " + field->name() + ": " +
                        from.ToString() + " to " + to.ToString());
        }
        widen.push_back(i);
        widened_bytes_per_row += to.byte_width();
    }
    if (input.num_rows() == 0) return;
    int64_t rows_per_slice = input.num_rows();
    if (widened_bytes_per_row > 0) {
        rows_per_slice = max_value_bytes / widened_bytes_per_row;
        if (rows_per_slice == 0) throw Error("max_value_bytes cannot fit one widened row");
    }

    for (int64_t offset = 0; offset < input.num_rows();) {
        const int64_t count = std::min(rows_per_slice, input.num_rows() - offset);
        auto columns = input.Slice(offset, count)->columns();
        for (const int i : widen) {
            auto converted = arrow::compute::Cast(*columns[i], file_schema->field(i)->type());
            if (!converted.ok()) throw Error(converted.status().ToString());
            columns[i] = std::move(converted).ValueOrDie();
        }
        auto batch = arrow::RecordBatch::Make(file_schema, count, std::move(columns));
        detail::ExportedBatch exported;
        status = arrow::ExportRecordBatch(*batch, &exported.array, &exported.schema);
        if (!status.ok()) throw Error(status.ToString());
        writer.write(&exported.array, &exported.schema);
        offset += count;
    }
}

}  // namespace mosaic
