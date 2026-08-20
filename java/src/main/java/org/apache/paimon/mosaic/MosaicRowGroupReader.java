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

import org.apache.arrow.c.ArrowArray;
import org.apache.arrow.c.ArrowSchema;
import org.apache.arrow.c.Data;
import org.apache.arrow.memory.BufferAllocator;
import org.apache.arrow.vector.VectorSchemaRoot;

/**
 * One prepared Mosaic row group.
 *
 * <p>The same native row-group state can be reused for Arrow fallback when a product consumer
 * reports that its fast path is unsupported.
 */
public final class MosaicRowGroupReader implements AutoCloseable {

    private long handle;
    private final boolean columnarJsonAllowed;

    MosaicRowGroupReader(long handle, boolean columnarJsonAllowed) {
        this.handle = handle;
        this.columnarJsonAllowed = columnarJsonAllowed;
    }

    /**
     * Materializes this row group's selected columns as Arrow vectors.
     *
     * <p>The caller owns and must close the returned root.
     */
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

    synchronized boolean writeGeelyColumnarJson(OutputStream output) throws IOException {
        checkOpen();
        if (!columnarJsonAllowed) {
            throw new IllegalStateException(
                    "Geely columnar JSON requires an unprojected Mosaic row group");
        }
        return NativeLib.nativeRowGroupReaderWriteGeelyColumnarJson(handle, output);
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
