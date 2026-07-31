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
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import org.apache.arrow.c.ArrowArray;
import org.apache.arrow.c.ArrowSchema;
import org.apache.arrow.c.Data;
import org.apache.arrow.memory.BufferAllocator;
import org.apache.arrow.vector.FieldVector;
import org.apache.arrow.vector.VectorSchemaRoot;
import org.apache.arrow.vector.types.pojo.Schema;

public class MosaicWriter implements AutoCloseable {

    private long handle;
    private boolean closed;
    private final BufferAllocator allocator;
    private List<Map<String, ColumnStatistics>> rowGroupStats;

    public MosaicWriter(OutputStream outputStream, Schema arrowSchema, BufferAllocator allocator) {
        this(outputStream, arrowSchema, new WriterOptions(), allocator);
    }

    public MosaicWriter(OutputStream outputStream, Schema arrowSchema, WriterOptions options, BufferAllocator allocator) {
        this.allocator = allocator;
        try (ArrowSchema cSchema = ArrowSchema.allocateNew(allocator)) {
            try {
                Data.exportSchema(allocator, arrowSchema, null, cSchema);
                this.handle = NativeLib.nativeWriterOpen(
                        outputStream,
                        cSchema.memoryAddress(),
                        options.getNumBuckets(),
                        options.getCompression(),
                        options.getZstdLevel(),
                        options.getRowGroupMaxSize(),
                        options.getMaxDictTotalBytes(),
                        options.getMaxDictEntries(),
                        options.getStatsColumns(),
                        options.getPageSizeThreshold());
            } finally {
                releaseExported(cSchema);
            }
        }
        if (this.handle == 0) {
            throw new RuntimeException("failed to open writer");
        }
    }

    public void write(VectorSchemaRoot root) {
        if (closed || handle == 0) {
            throw new IllegalStateException("writer is closed");
        }
        BufferAllocator exportAllocator = exportAllocator(root);
        try (ArrowArray arrowArray = ArrowArray.allocateNew(exportAllocator);
             ArrowSchema arrowSchema = ArrowSchema.allocateNew(exportAllocator)) {
            try {
                Data.exportVectorSchemaRoot(
                        exportAllocator, root, null, arrowArray, arrowSchema);
                NativeLib.nativeWriterWriteBatch(handle, arrowArray.memoryAddress(), arrowSchema.memoryAddress());
            } finally {
                releaseExported(arrowArray);
                releaseExported(arrowSchema);
            }
        }
    }

    private BufferAllocator exportAllocator(VectorSchemaRoot root) {
        List<FieldVector> vectors = root.getFieldVectors();
        if (vectors.isEmpty()) {
            return allocator;
        }

        BufferAllocator exportRoot = vectors.get(0).getAllocator().getRoot();
        for (int i = 1; i < vectors.size(); i++) {
            FieldVector vector = vectors.get(i);
            if (vector.getAllocator().getRoot() != exportRoot) {
                throw new IllegalArgumentException(
                        "All field vectors must share the same allocator root; field '"
                                + vector.getField().getName()
                                + "' uses a different root");
            }
        }
        return exportRoot;
    }

    private static void releaseExported(ArrowSchema schema) {
        if (schema.snapshot().release != 0) {
            schema.release();
        }
    }

    private static void releaseExported(ArrowArray array) {
        if (array.snapshot().release != 0) {
            array.release();
        }
    }

    public long estimatedFileSize() {
        return NativeLib.nativeWriterEstimatedSize(handle);
    }

    public int numRowGroups() {
        if (rowGroupStats == null) {
            throw new IllegalStateException("writer is not closed yet");
        }
        return rowGroupStats.size();
    }

    /**
     * Returns column statistics for the given row group, keyed by column name.
     */
    public Map<String, ColumnStatistics> getRowGroupStatistics(int rgIndex) {
        if (rowGroupStats == null) {
            throw new IllegalStateException("writer is not closed yet");
        }
        return rowGroupStats.get(rgIndex);
    }

    @Override
    public void close() {
        if (!closed && handle != 0) {
            closed = true;
            try {
                NativeLib.nativeWriterClose(handle);
                collectStatistics();
            } finally {
                NativeLib.nativeWriterFree(handle);
                handle = 0;
            }
        }
    }

    private void collectStatistics() {
        int numRg = NativeLib.nativeWriterNumRowGroups(handle);
        List<Map<String, ColumnStatistics>> allStats = new ArrayList<>(numRg);
        for (int rg = 0; rg < numRg; rg++) {
            String[] names = NativeLib.nativeWriterRowGroupStatNames(handle, rg);
            if (names == null || names.length == 0) {
                allStats.add(Collections.emptyMap());
                continue;
            }
            long[] nullCounts = NativeLib.nativeWriterRowGroupStatNullCounts(handle, rg);
            byte[][] mins = NativeLib.nativeWriterRowGroupStatMins(handle, rg);
            byte[][] maxs = NativeLib.nativeWriterRowGroupStatMaxs(handle, rg);
            Map<String, ColumnStatistics> rgStats = new LinkedHashMap<>(names.length);
            for (int i = 0; i < names.length; i++) {
                rgStats.put(names[i], new ColumnStatistics(nullCounts[i], mins[i], maxs[i]));
            }
            allStats.add(Collections.unmodifiableMap(rgStats));
        }
        this.rowGroupStats = Collections.unmodifiableList(allStats);
    }
}
