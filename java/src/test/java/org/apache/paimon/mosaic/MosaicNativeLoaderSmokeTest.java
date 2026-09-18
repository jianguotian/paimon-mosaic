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

/** Standalone smoke test for loading the packaged Mosaic JNI library. */
public final class MosaicNativeLoaderSmokeTest {

    private MosaicNativeLoaderSmokeTest() {}

    public static void main(String[] args) {
        try {
            Class.forName(
                    "org.apache.paimon.mosaic.NativeLib",
                    true,
                    MosaicNativeLoaderSmokeTest.class.getClassLoader());
        } catch (ClassNotFoundException e) {
            throw new AssertionError("NativeLib is missing", e);
        }

        long estimatedSize = NativeLib.nativeWriterEstimatedSize(0L);
        if (estimatedSize != 0L) {
            throw new AssertionError(
                    "Expected nativeWriterEstimatedSize(0L) to return 0, got " + estimatedSize);
        }
    }
}
