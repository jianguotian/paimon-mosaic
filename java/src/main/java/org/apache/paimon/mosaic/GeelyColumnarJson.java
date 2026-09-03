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

/** Product-specific bridge for the Geely column-oriented JSON protocol. */
public final class GeelyColumnarJson {

    /** Normal outcomes that do not represent corrupt input or I/O failure. */
    public enum Status {
        WRITTEN,
        UNSUPPORTED
    }

    private GeelyColumnarJson() {}

    /**
     * Attempts the native protocol without touching {@code output} when it is unsupported.
     *
     * <p>The caller owns {@code output}; this method never closes or flushes it. If an I/O failure
     * occurs after writing begins, the partial output must be discarded.
     */
    public static Status write(MosaicRowGroupReader rowGroup, OutputStream output)
            throws IOException {
        Objects.requireNonNull(rowGroup, "rowGroup");
        Objects.requireNonNull(output, "output");
        return rowGroup.writeGeelyColumnarJson(output) ? Status.WRITTEN : Status.UNSUPPORTED;
    }

    /**
     * Attempts the native protocol after the caller has matched the complete physical schema.
     *
     * <p>This variant avoids pre-reading non-DOUBLE values: integers, decimals, and strings are
     * decoded once while generating JSON. Unsupported type or encoding structure still leaves
     * {@code output} untouched. DOUBLE text is prepared before output to preserve Java formatting.
     * If decoding or I/O fails after writing begins, the caller must discard the partial output.
     */
    public static Status writeTrusted(MosaicRowGroupReader rowGroup, OutputStream output)
            throws IOException {
        Objects.requireNonNull(rowGroup, "rowGroup");
        Objects.requireNonNull(output, "output");
        return rowGroup.writeTrustedGeelyColumnarJson(output)
                ? Status.WRITTEN
                : Status.UNSUPPORTED;
    }
}
