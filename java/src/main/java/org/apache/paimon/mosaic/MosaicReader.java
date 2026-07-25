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

import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

import org.apache.arrow.c.ArrowArray;
import org.apache.arrow.c.ArrowSchema;
import org.apache.arrow.c.Data;
import org.apache.arrow.memory.BufferAllocator;
import org.apache.arrow.vector.VectorSchemaRoot;
import org.apache.arrow.vector.complex.StructVector;
import org.apache.arrow.vector.types.pojo.Field;
import org.apache.arrow.vector.types.pojo.Schema;

public class MosaicReader implements AutoCloseable {

    private long handle;
    private final Schema schema;
    private Schema readSchema;

    private MosaicReader(long handle, BufferAllocator allocator) {
        this.handle = handle;
        try (ArrowSchema cSchema = ArrowSchema.allocateNew(allocator)) {
            int rc = NativeLib.nativeReaderExportSchema(handle, cSchema.memoryAddress());
            if (rc != 0) {
                throw new RuntimeException("failed to export schema");
            }
            this.schema = Data.importSchema(allocator, cSchema, null);
        }
    }

    public static MosaicReader open(InputFile inputFile, long fileLength, BufferAllocator allocator) {
        long handle = NativeLib.nativeReaderOpen(inputFile, fileLength);
        if (handle == 0) {
            throw new RuntimeException("failed to open reader");
        }
        try {
            return new MosaicReader(handle, allocator);
        } catch (RuntimeException | Error e) {
            NativeLib.nativeReaderFree(handle);
            throw e;
        }
    }

    public Schema getSchema() {
        return schema;
    }

    public int numRowGroups() {
        return NativeLib.nativeReaderNumRowGroups(handle);
    }

    public void project(String[] columns) {
        Map<String, Field> fieldsByName = new HashMap<>();
        for (Field field : schema.getFields()) {
            fieldsByName.put(field.getName(), field);
        }
        Set<String> selected = new HashSet<>();
        List<Field> projected = new ArrayList<>(columns.length);
        for (String name : columns) {
            Field field = fieldsByName.get(name);
            if (field == null) {
                throw new IllegalArgumentException("column '" + name + "' not found in schema");
            }
            if (selected.add(name)) {
                projected.add(field);
            }
        }
        NativeLib.nativeReaderSetProjection(handle, columns);
        readSchema = new Schema(projected);
    }

    public VectorSchemaRoot readRowGroup(int rgIndex, BufferAllocator allocator) {
        long rgHandle = NativeLib.nativeReaderOpenRowGroup(handle, rgIndex);
        if (rgHandle == 0) {
            throw new RuntimeException("failed to open row group " + rgIndex);
        }
        try {
            return readRowGroupHandle(rgHandle, allocator);
        } finally {
            NativeLib.nativeRowGroupReaderFree(rgHandle);
        }
    }

    private VectorSchemaRoot readRowGroupHandle(long rgHandle, BufferAllocator allocator) {
        try (ArrowArray arrowArray = ArrowArray.allocateNew(allocator);
                ArrowSchema arrowSchema = ArrowSchema.allocateNew(allocator)) {
            int rc =
                    NativeLib.nativeRowGroupReaderReadColumns(
                            rgHandle,
                            arrowArray.memoryAddress(),
                            arrowSchema.memoryAddress());
            if (rc != 0) {
                throw new RuntimeException("readColumns failed");
            }
            return Data.importVectorSchemaRoot(allocator, arrowArray, arrowSchema, null);
        }
    }

    /**
     * Reads one row group directly as its Arrow struct root.
     *
     * <p>This avoids importing the same schema again and transferring the struct's children into a
     * second vector tree. The returned vector owns the imported C Data array and must be closed.
     */
    public StructVector readRowGroupStruct(int rgIndex, BufferAllocator allocator) {
        long rgHandle = NativeLib.nativeReaderOpenRowGroup(handle, rgIndex);
        if (rgHandle == 0) {
            throw new RuntimeException("failed to open row group " + rgIndex);
        }
        try {
            return readRowGroupStructHandle(rgHandle, allocator);
        } finally {
            NativeLib.nativeRowGroupReaderFree(rgHandle);
        }
    }

    private StructVector readRowGroupStructHandle(long rgHandle, BufferAllocator allocator) {
        try (ArrowArray arrowArray = ArrowArray.allocateNew(allocator)) {
            int rc =
                    NativeLib.nativeRowGroupReaderReadColumns(
                            rgHandle, arrowArray.memoryAddress(), 0);
            if (rc != 0) {
                throw new RuntimeException("readColumns failed");
            }

            StructVector vector = StructVector.emptyWithDuplicates("", allocator);
            try {
                Schema projectedSchema = readSchema == null ? schema : readSchema;
                vector.initializeChildrenFromFields(projectedSchema.getFields());
                Data.importIntoVector(allocator, arrowArray, vector, null);
                return vector;
            } catch (RuntimeException | Error failure) {
                vector.close();
                throw failure;
            }
        }
    }

    public int rowGroupNumRows(int rgIndex) {
        int result = NativeLib.nativeReaderRowGroupNumRows(handle, rgIndex);
        if (result < 0) {
            throw new RuntimeException("failed to get row group num rows for index " + rgIndex);
        }
        return result;
    }

    /**
     * Returns column statistics for the given row group, keyed by column name.
     */
    public Map<String, ColumnStatistics> getRowGroupStatistics(int rgIndex) {
        String[] names = NativeLib.nativeReaderRowGroupStatNames(handle, rgIndex);
        if (names == null || names.length == 0) {
            return Collections.emptyMap();
        }
        long[] nullCounts = NativeLib.nativeReaderRowGroupStatNullCounts(handle, rgIndex);
        byte[][] mins = NativeLib.nativeReaderRowGroupStatMins(handle, rgIndex);
        byte[][] maxs = NativeLib.nativeReaderRowGroupStatMaxs(handle, rgIndex);
        Map<String, ColumnStatistics> result = new LinkedHashMap<>(names.length);
        for (int i = 0; i < names.length; i++) {
            result.put(names[i], new ColumnStatistics(nullCounts[i], mins[i], maxs[i]));
        }
        return Collections.unmodifiableMap(result);
    }

    @Override
    public void close() {
        if (handle != 0) {
            NativeLib.nativeReaderFree(handle);
            handle = 0;
        }
    }
}
