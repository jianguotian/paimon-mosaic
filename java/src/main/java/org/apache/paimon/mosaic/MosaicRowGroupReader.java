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
 *
 * <p>An instance permits only one active read or write. Concurrent or reentrant use fails instead
 * of waiting. Calling {@link #close()} during an active operation immediately closes the instance
 * to new calls and defers native release until that operation returns.
 */
public final class MosaicRowGroupReader implements AutoCloseable {

    private enum State {
        IDLE,
        IN_USE,
        CLOSE_PENDING,
        CLOSED
    }

    private long handle;
    private final boolean columnarJsonAllowed;
    private State state = State.IDLE;

    MosaicRowGroupReader(long handle, boolean columnarJsonAllowed) {
        this.handle = handle;
        this.columnarJsonAllowed = columnarJsonAllowed;
    }

    /**
     * Materializes this row group's selected columns as Arrow vectors.
     *
     * <p>The caller owns and must close the returned root.
     */
    public VectorSchemaRoot readColumns(BufferAllocator allocator) {
        long currentHandle = beginUse();
        try {
            try (ArrowArray arrowArray = ArrowArray.allocateNew(allocator);
                    ArrowSchema arrowSchema = ArrowSchema.allocateNew(allocator)) {
                int rc =
                        NativeLib.nativeRowGroupReaderReadColumns(
                                currentHandle,
                                arrowArray.memoryAddress(),
                                arrowSchema.memoryAddress());
                if (rc != 0) {
                    throw new RuntimeException("readColumns failed");
                }
                return Data.importVectorSchemaRoot(allocator, arrowArray, arrowSchema, null);
            }
        } finally {
            endUse();
        }
    }

    boolean writeGeelyColumnarJson(OutputStream output) throws IOException {
        long currentHandle = beginUse();
        try {
            if (!columnarJsonAllowed) {
                throw new IllegalStateException(
                        "Geely columnar JSON requires an unprojected Mosaic row group");
            }
            return NativeLib.nativeRowGroupReaderWriteGeelyColumnarJson(currentHandle, output);
        } finally {
            endUse();
        }
    }

    private synchronized long beginUse() {
        switch (state) {
            case IDLE:
                state = State.IN_USE;
                return handle;
            case IN_USE:
                throw new IllegalStateException("row group reader is already in use");
            case CLOSE_PENDING:
            case CLOSED:
                throw new IllegalStateException("row group reader is closed");
            default:
                throw new AssertionError("unknown row group reader state " + state);
        }
    }

    private synchronized void endUse() {
        switch (state) {
            case IN_USE:
                state = State.IDLE;
                break;
            case CLOSE_PENDING:
                freeHandleWhileLocked();
                break;
            default:
                throw new AssertionError("cannot end row group reader use in state " + state);
        }
    }

    private void freeHandleWhileLocked() {
        NativeLib.nativeRowGroupReaderFree(handle);
        handle = 0;
        state = State.CLOSED;
    }

    @Override
    public synchronized void close() {
        switch (state) {
            case IDLE:
                freeHandleWhileLocked();
                break;
            case IN_USE:
                state = State.CLOSE_PENDING;
                break;
            case CLOSE_PENDING:
            case CLOSED:
                break;
            default:
                throw new AssertionError("unknown row group reader state " + state);
        }
    }
}
