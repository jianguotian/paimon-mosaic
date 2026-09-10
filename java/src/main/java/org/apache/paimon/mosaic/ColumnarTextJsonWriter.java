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

package org.apache.paimon.mosaic;

import java.io.IOException;
import java.io.OutputStream;
import java.util.Objects;

/**
 * Streams a row group as a JSON object whose values are comma-separated text columns.
 *
 * <p>For example, an integer column {@code [10, null, 12]} becomes {@code {"speed":"10,,12"}}.
 * Columns follow the reader's projection order and values follow logical row order. Null and empty
 * strings both produce empty entries. Commas in string values remain literal; only JSON escaping
 * is applied. This format aggregates text and is not a lossless representation of typed rows.
 *
 * <p>Finite doubles follow {@link Double#toString(double)} on the calling JVM. Decimals follow
 * {@link java.math.BigDecimal#toPlainString()}. Integers use base ten without grouping.
 */
public final class ColumnarTextJsonWriter {

    /** Normal outcomes that do not represent corrupt input or I/O failure. */
    public enum Status {
        WRITTEN,
        UNSUPPORTED
    }

    private ColumnarTextJsonWriter() {}

    /**
     * Writes one row group directly from its scalar column encodings, without Arrow arrays.
     *
     * <p>Supports integer, DOUBLE, Decimal128, and UTF8 columns in ALL_NULL, CONST, DICT, and PLAIN
     * encodings, plus all-null columns of other scalar types. Nested columns, non-finite doubles,
     * and more than 65,536 distinct doubles requiring JVM formatting return {@link
     * Status#UNSUPPORTED} before touching {@code output}. The same row group remains available for
     * {@link MosaicRowGroupReader#readColumns}.
     *
     * <p>Structure and DOUBLE text are checked before output starts. Other values are validated
     * during streaming. If decoding or I/O fails, discard the partial result; do not append a
     * fallback to it. The caller owns {@code output}; this method never closes or flushes it.
     */
    public static Status write(MosaicRowGroupReader rowGroup, OutputStream output)
            throws IOException {
        Objects.requireNonNull(rowGroup, "rowGroup");
        Objects.requireNonNull(output, "output");
        return rowGroup.writeColumnarTextJson(output) ? Status.WRITTEN : Status.UNSUPPORTED;
    }
}
