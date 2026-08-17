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

import java.io.OutputStream;

import org.apache.arrow.c.ArrowArray;
import org.apache.arrow.c.ArrowSchema;
import org.apache.arrow.c.Data;
import org.apache.arrow.memory.BufferAllocator;
import org.apache.arrow.vector.VectorSchemaRoot;

/**
 * One prepared Mosaic row group.
 *
 * <p>The same decoded row-group state is reused when the constrained native JSON path reports
 * unsupported and the caller falls back to {@link #readColumns(BufferAllocator)}.
 */
public final class MosaicRowGroupReader implements AutoCloseable {

    private long handle;
    private final boolean columnarJsonAllowed;

    MosaicRowGroupReader(long handle, boolean columnarJsonAllowed) {
        this.handle = handle;
        this.columnarJsonAllowed = columnarJsonAllowed;
    }

    public synchronized VectorSchemaRoot readColumns(BufferAllocator allocator) {
        checkOpen();
        try (ArrowArray arrowArray = ArrowArray.allocateNew(allocator);
                ArrowSchema arrowSchema = ArrowSchema.allocateNew(allocator)) {
            int rc =
                    NativeLib.nativeRowGroupReaderReadColumns(
                            handle, arrowArray.memoryAddress(), arrowSchema.memoryAddress());
            if (rc != 0) {
                throw new RuntimeException("readColumns failed");
            }
            return Data.importVectorSchemaRoot(allocator, arrowArray, arrowSchema, null);
        }
    }

    /**
     * Writes this row group using the customer column-oriented JSON protocol and Zstd compression,
     * while resolving one UTF-8 column that must be non-null, non-empty, and constant across all
     * rows.
     *
     * <p>Returns the unique UTF-8 bytes after writing succeeds, an empty byte array when the
     * requested column violates the single-value contract, or {@code null} without touching
     * {@code output} when the byte-exact native JSON path is unsupported. After {@code null}, the
     * caller may invoke {@link #readColumns(BufferAllocator)} on this same object without reopening
     * or decompressing the row group.
     */
    public synchronized byte[] writeColumnarJsonZstd(
            OutputStream output, int zstdLevel, int singleUtf8ColumnIndex) {
        checkOpen();
        if (!columnarJsonAllowed) {
            throw new IllegalStateException(
                    "columnar JSON requires an unprojected Mosaic row group");
        }
        if (output == null) {
            throw new NullPointerException("output");
        }
        if (singleUtf8ColumnIndex < 0) {
            throw new IllegalArgumentException("singleUtf8ColumnIndex must be non-negative");
        }
        return NativeLib.nativeRowGroupReaderWriteColumnarJsonZstdWithSingleUtf8Value(
                handle, output, zstdLevel, singleUtf8ColumnIndex);
    }

    private void checkOpen() {
        if (handle == 0) {
            throw new IllegalStateException("row group reader is closed");
        }
    }

    @Override
    public synchronized void close() {
        if (handle != 0) {
            NativeLib.nativeRowGroupReaderFree(handle);
            handle = 0;
        }
    }
}
