#!/usr/bin/env bash

# Licensed to the Apache Software Foundation (ASF) under one or more
# contributor license agreements. See the NOTICE file distributed with
# this work for additional information regarding copyright ownership.
# The ASF licenses this file to You under the Apache License, Version 2.0
# (the "License"); you may not use this file except in compliance with
# the License. You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

set -o errexit
set -o nounset
set -o pipefail

PYTHON=${PYTHON:-python3}

usage() {
  cat >&2 <<'EOF'
Usage: validate_java_staging_artifacts.sh TARGET_DIR VERSION
       validate_java_staging_artifacts.sh --validate-java-class-set CLASSES_DIR
EOF
}

MODE=artifacts
if [[ $# -eq 2 && "$1" == "--validate-java-class-set" ]]; then
  MODE=java-class-set
  TARGET_DIR=$2
  VERSION=
elif [[ $# -eq 2 ]]; then
  TARGET_DIR=$1
  VERSION=$2
else
  usage
  exit 1
fi

if ! command -v "$PYTHON" >/dev/null 2>&1; then
  echo "python3 is required to validate Java staging artifacts" >&2
  exit 1
fi

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd "$SCRIPT_DIR/.." && pwd)

"$PYTHON" - \
  "$REPO_DIR" \
  "$TARGET_DIR" \
  "$VERSION" \
  "$MODE" <<'PY'
import os
import posixpath
import re
import shutil
import stat
import struct
import subprocess
import sys
import zipfile
import zlib
import xml.etree.ElementTree as ET
from pathlib import Path, PurePosixPath, PureWindowsPath


MAX_ENTRY_SIZE = 256 * 1024 * 1024
MAX_TOTAL_SIZE = 1024 * 1024 * 1024
MIN_NATIVE_SIZE = 64 * 1024
MAX_NATIVE_SYMBOLS = 64 * 1024
MAX_NATIVE_SYMBOL_LENGTH = 4096
MAX_COMPILED_TREE_ENTRIES = 4096
READ_CHUNK_SIZE = 1024 * 1024
JAVA_CLASS = "org/apache/paimon/mosaic/MosaicReader.class"
JAVA_CLASS_NAME = "org.apache.paimon.mosaic.MosaicReader"
EXPECTED_CLASS_ENTRIES = {
    "org/apache/paimon/mosaic/ColumnStatistics.class",
    "org/apache/paimon/mosaic/InputFile.class",
    "org/apache/paimon/mosaic/MosaicReader.class",
    "org/apache/paimon/mosaic/MosaicWriter$1.class",
    "org/apache/paimon/mosaic/MosaicWriter$RootArrayExporter.class",
    "org/apache/paimon/mosaic/MosaicWriter$RootArrayPrivateData.class",
    "org/apache/paimon/mosaic/MosaicWriter.class",
    "org/apache/paimon/mosaic/NativeLib.class",
    "org/apache/paimon/mosaic/WriterOptions.class",
}
MOSAIC_READER_PUBLIC_API = (
    "implements java.lang.AutoCloseable",
    "public static org.apache.paimon.mosaic.MosaicReader open(",
    "public org.apache.arrow.vector.types.pojo.Schema getSchema();",
    "public int numRowGroups();",
    "public void project(java.lang.String[]);",
    "public org.apache.arrow.vector.VectorSchemaRoot readRowGroup(",
    "public int rowGroupNumRows(int);",
    "public java.util.Map<java.lang.String, "
    "org.apache.paimon.mosaic.ColumnStatistics> getRowGroupStatistics(int);",
    "public void close();",
)
POM_ENTRY = "META-INF/maven/org.apache.paimon/mosaic/pom.xml"
POM_PROPERTIES_ENTRY = (
    "META-INF/maven/org.apache.paimon/mosaic/pom.properties"
)
LEGAL_ENTRIES = (
    "META-INF/LICENSE",
    "META-INF/NOTICE",
    "META-INF/DEPENDENCIES",
)
NATIVE_ENTRIES = {
    "native/linux/x86_64/libpaimon_mosaic_jni.so": ("ELF", 62),
    "native/linux/aarch64/libpaimon_mosaic_jni.so": ("ELF", 183),
    "native/macos/aarch64/libpaimon_mosaic_jni.dylib": (
        "Mach-O",
        0x0100000C,
    ),
    "native/windows/x86_64/paimon_mosaic_jni.dll": ("PE", 0x8664),
}


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


def regular_file(path, description):
    if path.is_symlink() or not path.is_file():
        fail(
            "{} is missing or is not a regular file: {}".format(
                description, path
            )
        )
    if path.stat().st_size == 0:
        fail("{} is empty: {}".format(description, path))


def validated_entries(archive_path):
    try:
        archive = zipfile.ZipFile(str(archive_path))
    except (OSError, zipfile.BadZipFile) as error:
        fail("Invalid JAR {}: {}".format(archive_path, error))

    entries = {}
    normalized_names = {}
    total_size = 0
    try:
        infos = archive.infolist()
        if not infos:
            fail("JAR contains no entries: {}".format(archive_path))
        for info in infos:
            name = info.orig_filename
            if (
                not name
                or "\x00" in name
                or name != info.filename
                or "\\" in name
                or PurePosixPath(name).is_absolute()
                or PureWindowsPath(name).is_absolute()
                or PureWindowsPath(name).drive
                or ".." in name.split("/")
            ):
                fail(
                    "Unsafe JAR entry path in {}: {!r}".format(
                        archive_path, name
                    )
                )
            if stat.S_ISLNK(info.external_attr >> 16):
                fail(
                    "JAR entry is a symbolic link in {}: {!r}".format(
                        archive_path, name
                    )
                )
            if info.file_size > MAX_ENTRY_SIZE:
                fail(
                    "JAR entry is too large in {}: {!r}".format(
                        archive_path, name
                    )
                )
            total_size += info.file_size
            if total_size > MAX_TOTAL_SIZE:
                fail("JAR contents are too large: {}".format(archive_path))
            if name in entries:
                fail(
                    "Duplicate JAR entry in {}: {!r}".format(
                        archive_path, name
                    )
                )
            normalized = posixpath.normpath(name)
            if normalized in normalized_names:
                fail(
                    "Duplicate normalized JAR entries in {}: {!r} and {!r}"
                    .format(
                        archive_path,
                        normalized_names[normalized],
                        name,
                    )
                )
            entries[name] = info
            normalized_names[normalized] = name

        # Reading every bounded entry through EOF verifies its CRC.
        for info in infos:
            if info.is_dir():
                continue
            with archive.open(info) as source:
                while source.read(READ_CHUNK_SIZE):
                    pass
    except (OSError, RuntimeError, zipfile.BadZipFile, zlib.error) as error:
        archive.close()
        fail("Unable to validate JAR {}: {}".format(archive_path, error))
    return archive, entries


def required_bytes(archive, entries, archive_path, name):
    info = entries.get(name)
    if info is None or info.is_dir():
        fail("Packaged JAR is missing required entry: {}".format(name))
    try:
        return archive.read(info)
    except (OSError, RuntimeError, zipfile.BadZipFile) as error:
        fail(
            "Unable to read {} from {}: {}".format(
                name, archive_path, error
            )
        )


def validate_java_class_set(actual_classes, description):
    if actual_classes != EXPECTED_CLASS_ENTRIES:
        fail(
            "{} is invalid: expected {}, found {}".format(
                description,
                sorted(EXPECTED_CLASS_ENTRIES),
                sorted(actual_classes),
            )
        )


def compiled_java_classes(root):
    actual_classes = set()
    visited_entries = 0
    for directory, subdirectories, files in os.walk(
        str(root),
        followlinks=False,
    ):
        for entry in subdirectories + files:
            visited_entries += 1
            if visited_entries > MAX_COMPILED_TREE_ENTRIES:
                fail("Compiled Java classes directory is too large")
            path = Path(directory) / entry
            if path.is_symlink():
                fail(
                    "Compiled Java classes directory contains a symbolic link: {}"
                    .format(path.relative_to(root))
                )
        for filename in files:
            if not filename.endswith(".class"):
                continue
            path = Path(directory) / filename
            if path.is_file():
                actual_classes.add(path.relative_to(root).as_posix())
    return actual_classes


def validate_java_classes(main_jar, archive, entries):
    javap = shutil.which("javap")
    if javap is None:
        fail("javap is required to validate the packaged Java classes")

    actual_classes = {
        name
        for name, info in entries.items()
        if name.endswith(".class") and not info.is_dir()
    }
    validate_java_class_set(actual_classes, "Packaged Java class set")

    javap_outputs = {}
    for entry in sorted(EXPECTED_CLASS_ENTRIES):
        class_bytes = required_bytes(archive, entries, main_jar, entry)
        if (
            len(class_bytes) < 10
            or not class_bytes.startswith(b"\xca\xfe\xba\xbe")
            or struct.unpack_from(">H", class_bytes, 6)[0] != 52
        ):
            fail("Packaged Java class is invalid: {}".format(entry))

        class_name = entry[:-6].replace("/", ".")
        result = subprocess.run(
            [
                javap,
                "-classpath",
                str(main_jar),
                "-public",
                "-s",
                class_name,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
        )
        if result.returncode != 0 or class_name not in result.stdout:
            fail(
                "Unable to parse packaged Java class {}: {}".format(
                    entry,
                    result.stderr.strip() or "javap failed",
                )
            )
        javap_outputs[class_name] = result.stdout

    reader_api = javap_outputs[JAVA_CLASS_NAME]
    missing_api = [
        signature
        for signature in MOSAIC_READER_PUBLIC_API
        if signature not in reader_api
    ]
    if missing_api:
        fail(
            "Packaged MosaicReader public API is incomplete: {}".format(
                ", ".join(missing_api)
            )
        )


def bounded_c_string(data, offset, limit, description, name):
    if offset < 0 or offset >= limit or limit > len(data):
        fail("{} is out of bounds: {}".format(description, name))
    search_limit = min(limit, offset + MAX_NATIVE_SYMBOL_LENGTH + 1)
    end = data.find(b"\x00", offset, search_limit)
    if end < 0:
        fail("{} is invalid or too long: {}".format(description, name))
    return data[offset:end]


def elf_sysv_hash(symbol):
    value = 0
    for byte in symbol:
        value = (value << 4) + byte
        high = value & 0xF0000000
        if high:
            value ^= high >> 24
            value &= ~high
    return value & 0xFFFFFFFF


def elf_gnu_hash(symbol):
    value = 5381
    for byte in symbol:
        value = (value * 33 + byte) & 0xFFFFFFFF
    return value


def macho_uleb128(data, offset, limit, description, name):
    value = 0
    for index in range(10):
        if offset >= limit:
            fail("{} is truncated: {}".format(description, name))
        byte = data[offset]
        offset += 1
        if index == 9 and byte > 1:
            fail("{} overflows 64 bits: {}".format(description, name))
        value |= (byte & 0x7F) << (index * 7)
        if not byte & 0x80:
            return value, offset
    fail("{} overflows 64 bits: {}".format(description, name))


def validate_elf(data, expected_machine, name):
    if (
        len(data) < MIN_NATIVE_SIZE
        or data[:4] != b"\x7fELF"
        or data[4] != 2
        or data[5] != 1
        or data[6] != 1
        or struct.unpack_from("<H", data, 16)[0] != 3
        or struct.unpack_from("<H", data, 18)[0] != expected_machine
        or struct.unpack_from("<I", data, 20)[0] != 1
    ):
        fail(
            "Packaged native entry is not the expected 64-bit ELF: {}".format(
                name
            )
        )

    program_offset = struct.unpack_from("<Q", data, 32)[0]
    header_size = struct.unpack_from("<H", data, 52)[0]
    program_entry_size = struct.unpack_from("<H", data, 54)[0]
    program_count = struct.unpack_from("<H", data, 56)[0]
    if (
        header_size != 64
        or program_entry_size != 56
        or program_count == 0
        or program_count > 128
        or program_offset < header_size
        or program_offset + program_entry_size * program_count > len(data)
    ):
        fail("Packaged ELF program headers are invalid: {}".format(name))

    load_segments = []
    dynamic_segments = []
    for index in range(program_count):
        offset = program_offset + index * program_entry_size
        segment_type = struct.unpack_from("<I", data, offset)[0]
        flags = struct.unpack_from("<I", data, offset + 4)[0]
        file_offset = struct.unpack_from("<Q", data, offset + 8)[0]
        virtual_address = struct.unpack_from("<Q", data, offset + 16)[0]
        file_size = struct.unpack_from("<Q", data, offset + 32)[0]
        memory_size = struct.unpack_from("<Q", data, offset + 40)[0]
        if (
            file_offset > len(data)
            or file_size > len(data) - file_offset
            or file_size > memory_size
        ):
            fail("Packaged ELF segment is out of bounds: {}".format(name))
        if segment_type == 1 and file_size > 0:
            load_segments.append(
                (
                    virtual_address,
                    file_size,
                    memory_size,
                    file_offset,
                    flags,
                )
            )
        elif segment_type == 2 and file_size > 0:
            dynamic_segments.append(
                (virtual_address, file_offset, file_size)
            )
    if (
        not load_segments
        or not any(segment[4] & 0x1 for segment in load_segments)
        or len(dynamic_segments) != 1
    ):
        fail("Packaged ELF is missing load or dynamic segments: {}".format(name))

    def virtual_to_file_offset(address, size, description):
        matches = set()
        for (
            virtual_address,
            file_size,
            _,
            file_offset,
            _,
        ) in load_segments:
            if address < virtual_address:
                continue
            delta = address - virtual_address
            if delta <= file_size and size <= file_size - delta:
                matches.add(file_offset + delta)
        if len(matches) != 1:
            fail("{} is not loader-visible: {}".format(description, name))
        return matches.pop()

    dynamic_address, dynamic_offset, dynamic_size = dynamic_segments[0]
    if (
        virtual_to_file_offset(
            dynamic_address,
            dynamic_size,
            "Packaged ELF dynamic segment",
        )
        != dynamic_offset
    ):
        fail("Packaged ELF dynamic segment is not loader-visible: {}".format(name))
    if dynamic_size % 16 != 0:
        fail("Packaged ELF dynamic segment is invalid: {}".format(name))
    dynamic_tag_names = {
        4: "DT_HASH",
        5: "DT_STRTAB",
        6: "DT_SYMTAB",
        10: "DT_STRSZ",
        11: "DT_SYMENT",
        0x6FFFFEF5: "DT_GNU_HASH",
        0x6FFFFFF0: "DT_VERSYM",
    }
    dynamic_values = {}
    has_dynamic_end = False
    for offset in range(
        dynamic_offset,
        dynamic_offset + dynamic_size,
        16,
    ):
        tag, value = struct.unpack_from("<qQ", data, offset)
        if tag == 0:
            has_dynamic_end = True
            break
        if tag not in dynamic_tag_names:
            continue
        if tag in dynamic_values:
            fail(
                "Packaged ELF dynamic segment has duplicate {}: {}".format(
                    dynamic_tag_names[tag],
                    name,
                )
            )
        dynamic_values[tag] = value
    required_dynamic_tags = {5, 6, 10, 11}
    if (
        not has_dynamic_end
        or not required_dynamic_tags <= set(dynamic_values)
        or not ({4, 0x6FFFFEF5} & set(dynamic_values))
        or dynamic_values[5] == 0
        or dynamic_values[6] == 0
        or dynamic_values[10] == 0
        or dynamic_values[11] != 24
    ):
        fail(
            "Packaged ELF dynamic symbol table is not loader-visible: {}"
            .format(name)
        )

    section_offset = struct.unpack_from("<Q", data, 40)[0]
    section_entry_size = struct.unpack_from("<H", data, 58)[0]
    section_count = struct.unpack_from("<H", data, 60)[0]
    if (
        section_offset == 0
        or section_entry_size != 64
        or section_count == 0
        or section_count > 4096
        or section_offset > len(data)
        or section_entry_size * section_count > len(data) - section_offset
    ):
        fail("Packaged ELF section headers are invalid: {}".format(name))

    sections = []
    for index in range(section_count):
        offset = section_offset + index * section_entry_size
        section_type = struct.unpack_from("<I", data, offset + 4)[0]
        section_flags = struct.unpack_from("<Q", data, offset + 8)[0]
        virtual_address = struct.unpack_from("<Q", data, offset + 16)[0]
        file_offset = struct.unpack_from("<Q", data, offset + 24)[0]
        file_size = struct.unpack_from("<Q", data, offset + 32)[0]
        link = struct.unpack_from("<I", data, offset + 40)[0]
        entry_size = struct.unpack_from("<Q", data, offset + 56)[0]
        if (
            section_type != 8
            and (
                file_offset > len(data)
                or file_size > len(data) - file_offset
            )
        ):
            fail("Packaged ELF section is out of bounds: {}".format(name))
        sections.append(
            (
                section_type,
                section_flags,
                virtual_address,
                file_offset,
                file_size,
                link,
                entry_size,
            )
        )

    dynamic_string_address = dynamic_values[5]
    dynamic_symbol_address = dynamic_values[6]
    dynamic_string_size = dynamic_values[10]
    dynamic_symbol_entry_size = dynamic_values[11]
    dynamic_string_offset = virtual_to_file_offset(
        dynamic_string_address,
        dynamic_string_size,
        "Packaged ELF dynamic string table",
    )

    loader_symbol_tables = []
    for section in sections:
        (
            section_type,
            _,
            virtual_address,
            file_offset,
            file_size,
            link,
            entry_size,
        ) = section
        if section_type != 11:
            continue
        if (
            entry_size != dynamic_symbol_entry_size
            or file_size % entry_size != 0
            or link >= len(sections)
        ):
            fail("Packaged ELF dynamic symbol table is invalid: {}".format(name))
        (
            string_type,
            _,
            string_address,
            string_offset,
            string_size,
            _,
            _,
        ) = sections[link]
        if (
            virtual_address != dynamic_symbol_address
            or file_offset
            != virtual_to_file_offset(
                dynamic_symbol_address,
                file_size,
                "Packaged ELF dynamic symbol table",
            )
            or string_type != 3
            or string_address != dynamic_string_address
            or string_offset != dynamic_string_offset
            or string_size != dynamic_string_size
        ):
            continue
        loader_symbol_tables.append(section)

    if len(loader_symbol_tables) != 1:
        fail(
            "Packaged ELF dynamic symbol table is not loader-visible: {}"
            .format(name)
        )

    (
        _,
        _,
        _,
        file_offset,
        file_size,
        link,
        entry_size,
    ) = loader_symbol_tables[0]
    (
        _,
        _,
        _,
        string_offset,
        string_size,
        _,
        _,
    ) = sections[link]
    symbol_count = file_size // entry_size
    if symbol_count == 0 or symbol_count > MAX_NATIVE_SYMBOLS:
        fail("Packaged ELF dynamic symbol table is invalid: {}".format(name))

    loader_hashes = []
    if 4 in dynamic_values:
        hash_offset = virtual_to_file_offset(
            dynamic_values[4],
            8,
            "Packaged ELF DT_HASH table",
        )
        bucket_count, chain_count = struct.unpack_from(
            "<II", data, hash_offset
        )
        if (
            bucket_count == 0
            or bucket_count > MAX_NATIVE_SYMBOLS
            or chain_count != symbol_count
        ):
            fail("Packaged ELF DT_HASH table is invalid: {}".format(name))
        hash_size = 8 + (bucket_count + chain_count) * 4
        if (
            virtual_to_file_offset(
                dynamic_values[4],
                hash_size,
                "Packaged ELF DT_HASH table",
            )
            != hash_offset
        ):
            fail(
                "Packaged ELF DT_HASH table has inconsistent mapping: {}"
                .format(name)
            )
        buckets_offset = hash_offset + 8
        chains_offset = buckets_offset + bucket_count * 4
        buckets = struct.unpack_from(
            "<{}I".format(bucket_count),
            data,
            buckets_offset,
        )
        chains = struct.unpack_from(
            "<{}I".format(chain_count),
            data,
            chains_offset,
        )
        if (
            chains[0] != 0
            or any(index >= symbol_count for index in buckets)
            or any(index >= symbol_count for index in chains)
        ):
            fail("Packaged ELF DT_HASH table is invalid: {}".format(name))
        for bucket in buckets:
            seen = set()
            index = bucket
            while index:
                if index in seen:
                    fail(
                        "Packaged ELF DT_HASH table contains a cycle: {}"
                        .format(name)
                    )
                seen.add(index)
                index = chains[index]

        def sysv_contains(
            symbol_index,
            symbol,
            buckets=buckets,
            chains=chains,
        ):
            index = buckets[elf_sysv_hash(symbol) % len(buckets)]
            while index:
                if index == symbol_index:
                    return True
                index = chains[index]
            return False

        loader_hashes.append(sysv_contains)

    if 0x6FFFFEF5 in dynamic_values:
        gnu_hash_offset = virtual_to_file_offset(
            dynamic_values[0x6FFFFEF5],
            16,
            "Packaged ELF DT_GNU_HASH table",
        )
        (
            bucket_count,
            symbol_offset,
            bloom_count,
            bloom_shift,
        ) = struct.unpack_from("<IIII", data, gnu_hash_offset)
        if (
            bucket_count == 0
            or bucket_count > MAX_NATIVE_SYMBOLS
            or bloom_count == 0
            or bloom_count > MAX_NATIVE_SYMBOLS
            or symbol_offset > symbol_count
        ):
            fail("Packaged ELF DT_GNU_HASH table is invalid: {}".format(name))
        chain_count = symbol_count - symbol_offset
        gnu_hash_size = (
            16
            + bloom_count * 8
            + bucket_count * 4
            + chain_count * 4
        )
        if (
            virtual_to_file_offset(
                dynamic_values[0x6FFFFEF5],
                gnu_hash_size,
                "Packaged ELF DT_GNU_HASH table",
            )
            != gnu_hash_offset
        ):
            fail(
                "Packaged ELF DT_GNU_HASH table has inconsistent mapping: {}"
                .format(name)
            )
        bloom_offset = gnu_hash_offset + 16
        buckets_offset = bloom_offset + bloom_count * 8
        chains_offset = buckets_offset + bucket_count * 4
        bloom = struct.unpack_from(
            "<{}Q".format(bloom_count),
            data,
            bloom_offset,
        )
        buckets = struct.unpack_from(
            "<{}I".format(bucket_count),
            data,
            buckets_offset,
        )
        chains = struct.unpack_from(
            "<{}I".format(chain_count),
            data,
            chains_offset,
        )
        for bucket in buckets:
            if bucket == 0:
                continue
            if bucket < symbol_offset or bucket >= symbol_count:
                fail(
                    "Packaged ELF DT_GNU_HASH bucket is invalid: {}"
                    .format(name)
                )
            chain_index = bucket - symbol_offset
            while True:
                if chain_index >= chain_count:
                    fail(
                        "Packaged ELF DT_GNU_HASH chain is unterminated: {}"
                        .format(name)
                    )
                if chains[chain_index] & 1:
                    break
                chain_index += 1

        def gnu_contains(
            symbol_index,
            symbol,
            symbol_offset=symbol_offset,
            bloom_shift=bloom_shift,
            bloom=bloom,
            buckets=buckets,
            chains=chains,
        ):
            symbol_hash = elf_gnu_hash(symbol)
            bloom_word = bloom[(symbol_hash // 64) % len(bloom)]
            bloom_mask = (1 << (symbol_hash % 64)) | (
                1 << ((symbol_hash >> bloom_shift) % 64)
            )
            if bloom_word & bloom_mask != bloom_mask:
                return False
            index = buckets[symbol_hash % len(buckets)]
            if (
                index == 0
                or symbol_index < index
                or symbol_index < symbol_offset
            ):
                return False
            while True:
                chain_index = index - symbol_offset
                if chain_index >= len(chains):
                    return False
                chain_hash = chains[chain_index]
                if index == symbol_index:
                    return (chain_hash | 1) == (symbol_hash | 1)
                if chain_hash & 1:
                    return False
                index += 1

        loader_hashes.append(gnu_contains)

    version_offset = None
    if 0x6FFFFFF0 in dynamic_values:
        version_offset = virtual_to_file_offset(
            dynamic_values[0x6FFFFFF0],
            symbol_count * 2,
            "Packaged ELF DT_VERSYM table",
        )

    def is_executable_symbol(section_index, value, size):
        if section_index == 0 or section_index >= len(sections):
            return False
        (
            section_type,
            section_flags,
            section_address,
            _,
            section_size,
            _,
            _,
        ) = sections[section_index]
        extent = max(size, 1)
        if (
            section_type == 8
            or not section_flags & 0x2
            or not section_flags & 0x4
            or value < section_address
            or value - section_address > section_size
            or extent > section_size - (value - section_address)
        ):
            return False
        for (
            segment_address,
            segment_file_size,
            _,
            _,
            segment_flags,
        ) in load_segments:
            if not segment_flags & 0x1 or value < segment_address:
                continue
            delta = value - segment_address
            if (
                delta <= segment_file_size
                and extent <= segment_file_size - delta
            ):
                return True
        return False

    exports = set()
    string_limit = string_offset + string_size
    for index in range(symbol_count):
        offset = file_offset + index * entry_size
        string_index = struct.unpack_from("<I", data, offset)[0]
        symbol_info = data[offset + 4]
        symbol_visibility = data[offset + 5] & 0x3
        section_index = struct.unpack_from("<H", data, offset + 6)[0]
        symbol_value = struct.unpack_from("<Q", data, offset + 8)[0]
        symbol_size = struct.unpack_from("<Q", data, offset + 16)[0]
        symbol_binding = symbol_info >> 4
        symbol_type = symbol_info & 0xF
        if (
            string_index == 0
            or string_index >= string_size
            or symbol_binding not in (1, 2)
            or symbol_type not in (0, 2, 10)
            or symbol_visibility not in (0, 3)
            or not is_executable_symbol(
                section_index,
                symbol_value,
                symbol_size,
            )
        ):
            continue
        symbol = bounded_c_string(
            data,
            string_offset + string_index,
            string_limit,
            "Packaged ELF dynamic symbol",
            name,
        )
        if not all(
            contains(index, symbol) for contains in loader_hashes
        ):
            continue
        if version_offset is not None:
            symbol_version = struct.unpack_from(
                "<H", data, version_offset + index * 2
            )[0]
            if symbol_version & 0x8000 or symbol_version & 0x7FFF == 0:
                continue
        exports.add(symbol)
    return exports


def validate_macho(data, expected_cpu, name):
    if (
        len(data) < MIN_NATIVE_SIZE
        or data[:4] != b"\xcf\xfa\xed\xfe"
        or struct.unpack_from("<I", data, 4)[0] != expected_cpu
        or struct.unpack_from("<I", data, 12)[0] != 6
    ):
        fail(
            "Packaged native entry is not the expected arm64 Mach-O dylib: {}"
            .format(name)
        )

    command_count = struct.unpack_from("<I", data, 16)[0]
    command_size = struct.unpack_from("<I", data, 20)[0]
    command_start = 32
    command_end = command_start + command_size
    if (
        command_count == 0
        or command_count > 128
        or command_size < command_count * 8
        or command_end > len(data)
    ):
        fail("Packaged Mach-O load commands are invalid: {}".format(name))

    has_executable_segment = False
    has_dylib_id = False
    symbol_table = None
    sections = []
    file_backed_segments = []
    dyld_info_export_trie = None
    dedicated_export_trie = None
    offset = command_start
    for _ in range(command_count):
        if offset + 8 > command_end:
            fail("Packaged Mach-O load command is truncated: {}".format(name))
        command = struct.unpack_from("<I", data, offset)[0]
        size = struct.unpack_from("<I", data, offset + 4)[0]
        if size < 8 or size % 8 != 0 or offset + size > command_end:
            fail("Packaged Mach-O load command is invalid: {}".format(name))
        if command == 0x19:
            if size < 72:
                fail("Packaged Mach-O segment command is invalid: {}".format(name))
            virtual_address = struct.unpack_from("<Q", data, offset + 24)[0]
            virtual_size = struct.unpack_from("<Q", data, offset + 32)[0]
            file_offset = struct.unpack_from("<Q", data, offset + 40)[0]
            file_size = struct.unpack_from("<Q", data, offset + 48)[0]
            initial_protection = struct.unpack_from("<I", data, offset + 60)[0]
            section_count = struct.unpack_from("<I", data, offset + 64)[0]
            if (
                file_offset > len(data)
                or file_size > len(data) - file_offset
                or section_count > 255
                or size != 72 + section_count * 80
            ):
                fail("Packaged Mach-O segment is out of bounds: {}".format(name))
            if file_size > 0 and initial_protection & 0x4:
                has_executable_segment = True
            if file_size > 0:
                file_backed_segments.append(
                    (
                        virtual_address,
                        virtual_size,
                        file_offset,
                        file_size,
                    )
                )
            section_offset = offset + 72
            for _ in range(section_count):
                section_address = struct.unpack_from(
                    "<Q", data, section_offset + 32
                )[0]
                section_size = struct.unpack_from(
                    "<Q", data, section_offset + 40
                )[0]
                section_file_offset = struct.unpack_from(
                    "<I", data, section_offset + 48
                )[0]
                section_flags = struct.unpack_from(
                    "<I", data, section_offset + 64
                )[0]
                executable = bool(
                    initial_protection & 0x4
                    and section_flags & 0x80000400
                )
                if executable and (
                    section_address < virtual_address
                    or section_address - virtual_address > virtual_size
                    or section_size
                    > virtual_size - (section_address - virtual_address)
                    or section_file_offset < file_offset
                    or section_file_offset - file_offset > file_size
                    or section_size
                    > file_size - (section_file_offset - file_offset)
                ):
                    fail(
                        "Packaged Mach-O executable section is out of bounds: {}"
                        .format(name)
                    )
                sections.append(
                    (section_address, section_size, executable)
                )
                section_offset += 80
        elif command == 0xD:
            if size < 24:
                fail("Packaged Mach-O dylib id is invalid: {}".format(name))
            name_offset = struct.unpack_from("<I", data, offset + 8)[0]
            if (
                name_offset < 24
                or name_offset >= size
                or b"\x00" not in data[
                    offset + name_offset : offset + size
                ]
            ):
                fail("Packaged Mach-O dylib id is invalid: {}".format(name))
            has_dylib_id = True
        elif command == 0x2:
            if size != 24 or symbol_table is not None:
                fail("Packaged Mach-O symbol table is invalid: {}".format(name))
            symbol_table = struct.unpack_from("<IIII", data, offset + 8)
        elif command in (0x22, 0x80000022):
            if size != 48 or dyld_info_export_trie is not None:
                fail("Packaged Mach-O dyld info is invalid: {}".format(name))
            dyld_info_export_trie = struct.unpack_from(
                "<II", data, offset + 40
            )
        elif command == 0x80000033:
            if size != 16 or dedicated_export_trie is not None:
                fail("Packaged Mach-O export trie is invalid: {}".format(name))
            dedicated_export_trie = struct.unpack_from(
                "<II", data, offset + 8
            )
        offset += size
    if offset != command_end or not (has_executable_segment and has_dylib_id):
        fail(
            "Packaged Mach-O is missing an executable segment or dylib id: {}"
            .format(name)
        )

    if symbol_table is None:
        fail("Packaged Mach-O has no dynamic symbol table: {}".format(name))
    symbol_offset, symbol_count, string_offset, string_size = symbol_table
    if (
        symbol_count > MAX_NATIVE_SYMBOLS
        or symbol_offset > len(data)
        or symbol_count * 16 > len(data) - symbol_offset
        or string_offset > len(data)
        or string_size == 0
        or string_size > len(data) - string_offset
    ):
        fail("Packaged Mach-O symbol table is out of bounds: {}".format(name))

    callable_symbols = {}
    string_limit = string_offset + string_size
    for index in range(symbol_count):
        offset = symbol_offset + index * 16
        string_index, symbol_type, section_index = struct.unpack_from(
            "<IBB", data, offset
        )
        symbol_value = struct.unpack_from("<Q", data, offset + 8)[0]
        if (
            string_index == 0
            or string_index >= string_size
            or symbol_type & 0xE0
            or not symbol_type & 0x01
            or symbol_type & 0x10
            or symbol_type & 0x0E != 0x0E
            or section_index == 0
            or section_index > len(sections)
        ):
            continue
        section_address, section_size, executable = sections[section_index - 1]
        if (
            not executable
            or symbol_value < section_address
            or symbol_value - section_address >= section_size
        ):
            continue
        symbol = bounded_c_string(
            data,
            string_offset + string_index,
            string_limit,
            "Packaged Mach-O dynamic symbol",
            name,
        )
        if symbol in callable_symbols and callable_symbols[symbol] != symbol_value:
            fail("Packaged Mach-O symbol table is ambiguous: {}".format(name))
        callable_symbols[symbol] = symbol_value

    export_trie = (
        dedicated_export_trie
        if dedicated_export_trie is not None
        else dyld_info_export_trie
    )
    if export_trie is None:
        fail("Packaged Mach-O has no loader export trie: {}".format(name))
    trie_offset, trie_size = export_trie
    if (
        trie_size == 0
        or trie_offset > len(data)
        or trie_size > len(data) - trie_offset
    ):
        fail("Packaged Mach-O export trie is out of bounds: {}".format(name))
    if not file_backed_segments:
        fail("Packaged Mach-O has no file-backed segment: {}".format(name))
    image_base = min(segment[0] for segment in file_backed_segments)

    def is_executable_address(address):
        return any(
            executable
            and section_address <= address
            and address - section_address < section_size
            for section_address, section_size, executable in sections
        )

    trie_limit = trie_offset + trie_size
    trie_exports = {}
    active_nodes = set()
    visited_nodes = set()
    stack = [(False, 0, b"")]
    while stack:
        leaving, node_offset, prefix = stack.pop()
        if leaving:
            active_nodes.remove(node_offset)
            continue
        if node_offset in active_nodes:
            fail("Packaged Mach-O export trie contains a cycle: {}".format(name))
        if node_offset in visited_nodes:
            fail(
                "Packaged Mach-O export trie reuses a node: {}".format(name)
            )
        if node_offset >= trie_size:
            fail(
                "Packaged Mach-O export trie child is out of bounds: {}"
                .format(name)
            )
        if len(visited_nodes) >= MAX_NATIVE_SYMBOLS:
            fail("Packaged Mach-O export trie is too large: {}".format(name))
        active_nodes.add(node_offset)
        visited_nodes.add(node_offset)
        stack.append((True, node_offset, b""))

        cursor = trie_offset + node_offset
        terminal_size, cursor = macho_uleb128(
            data,
            cursor,
            trie_limit,
            "Packaged Mach-O export trie terminal size",
            name,
        )
        if terminal_size > trie_limit - cursor:
            fail(
                "Packaged Mach-O export trie terminal is out of bounds: {}"
                .format(name)
            )
        terminal_end = cursor + terminal_size
        if terminal_size:
            flags, cursor = macho_uleb128(
                data,
                cursor,
                terminal_end,
                "Packaged Mach-O export trie flags",
                name,
            )
            if flags & 0x03 == 0x03 or (
                flags & 0x08 and flags & 0x10
            ):
                fail(
                    "Packaged Mach-O export trie flags are invalid: {}"
                    .format(name)
                )
            if flags & 0x08:
                _, cursor = macho_uleb128(
                    data,
                    cursor,
                    terminal_end,
                    "Packaged Mach-O re-export ordinal",
                    name,
                )
                imported_name = bounded_c_string(
                    data,
                    cursor,
                    terminal_end,
                    "Packaged Mach-O re-export name",
                    name,
                )
                cursor += len(imported_name) + 1
            else:
                address, cursor = macho_uleb128(
                    data,
                    cursor,
                    terminal_end,
                    "Packaged Mach-O export address",
                    name,
                )
                if flags & 0x10:
                    _, cursor = macho_uleb128(
                        data,
                        cursor,
                        terminal_end,
                        "Packaged Mach-O resolver address",
                        name,
                    )
                absolute_address = image_base + address
                if (
                    flags & 0x03 == 0
                    and absolute_address <= 0xFFFFFFFFFFFFFFFF
                    and is_executable_address(absolute_address)
                ):
                    trie_exports[prefix] = absolute_address
            if cursor != terminal_end:
                fail(
                    "Packaged Mach-O export trie terminal has trailing data: {}"
                    .format(name)
                )

        cursor = terminal_end
        if cursor >= trie_limit:
            fail(
                "Packaged Mach-O export trie child count is out of bounds: {}"
                .format(name)
            )
        child_count = data[cursor]
        cursor += 1
        child_edges = set()
        children = []
        for child_index in range(child_count):
            edge = bounded_c_string(
                data,
                cursor,
                trie_limit,
                "Packaged Mach-O export trie child edge",
                name,
            )
            cursor += len(edge) + 1
            if not edge or edge in child_edges:
                fail(
                    "Packaged Mach-O export trie child edge is invalid: {}"
                    .format(name)
                )
            child_edges.add(edge)
            child_offset, cursor = macho_uleb128(
                data,
                cursor,
                trie_limit,
                "Packaged Mach-O export trie child offset",
                name,
            )
            child_prefix = prefix + edge
            if len(child_prefix) > MAX_NATIVE_SYMBOL_LENGTH:
                fail(
                    "Packaged Mach-O export symbol is too long: {}"
                    .format(name)
                )
            children.append((child_offset, child_prefix))
        for child_offset, child_prefix in reversed(children):
            stack.append((False, child_offset, child_prefix))

    exports = set()
    for symbol, address in trie_exports.items():
        if callable_symbols.get(symbol) != address:
            continue
        if symbol.startswith(b"_"):
            symbol = symbol[1:]
        exports.add(symbol)
    return exports


def validate_pe(data, expected_machine, name):
    if len(data) < MIN_NATIVE_SIZE or data[:2] != b"MZ":
        fail("Packaged native entry is not a PE DLL: {}".format(name))
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if (
        pe_offset > len(data) - 24
        or data[pe_offset : pe_offset + 4] != b"PE\x00\x00"
        or struct.unpack_from("<H", data, pe_offset + 4)[0]
        != expected_machine
        or not (
            struct.unpack_from("<H", data, pe_offset + 22)[0] & 0x2000
        )
    ):
        fail(
            "Packaged native entry is not the expected x86_64 PE DLL: {}"
            .format(name)
        )

    section_count = struct.unpack_from("<H", data, pe_offset + 6)[0]
    optional_size = struct.unpack_from("<H", data, pe_offset + 20)[0]
    optional_offset = pe_offset + 24
    section_offset = optional_offset + optional_size
    if (
        section_count == 0
        or section_count > 96
        or optional_size < 240
        or section_offset + section_count * 40 > len(data)
        or struct.unpack_from("<H", data, optional_offset)[0] != 0x20B
        or struct.unpack_from("<I", data, optional_offset + 60)[0] > len(data)
        or struct.unpack_from("<I", data, optional_offset + 108)[0] < 1
    ):
        fail("Packaged PE optional or section headers are invalid: {}".format(name))

    export_rva = struct.unpack_from("<I", data, optional_offset + 112)[0]
    export_size = struct.unpack_from("<I", data, optional_offset + 116)[0]
    if export_rva == 0 or export_size == 0:
        fail("Packaged PE DLL has no export directory: {}".format(name))

    has_executable_section = False
    sections = []
    for index in range(section_count):
        offset = section_offset + index * 40
        virtual_size = struct.unpack_from("<I", data, offset + 8)[0]
        virtual_address = struct.unpack_from("<I", data, offset + 12)[0]
        raw_size = struct.unpack_from("<I", data, offset + 16)[0]
        raw_offset = struct.unpack_from("<I", data, offset + 20)[0]
        characteristics = struct.unpack_from("<I", data, offset + 36)[0]
        if raw_offset > len(data) or raw_size > len(data) - raw_offset:
            fail("Packaged PE section is out of bounds: {}".format(name))
        if raw_size > 0 and characteristics & 0x20000000:
            has_executable_section = True
        mapped_size = max(virtual_size, raw_size)
        sections.append(
            (
                virtual_address,
                mapped_size,
                raw_offset,
                raw_size,
                characteristics,
            )
        )

    def rva_to_location(rva, size, description):
        matches = []
        for (
            virtual_address,
            mapped_size,
            raw_offset,
            raw_size,
            characteristics,
        ) in sections:
            if virtual_address <= rva:
                delta = rva - virtual_address
                if (
                    delta <= mapped_size
                    and size <= mapped_size - delta
                    and delta <= raw_size
                    and size <= raw_size - delta
                ):
                    matches.append(
                        (
                            raw_offset + delta,
                            raw_offset + raw_size,
                            characteristics,
                        )
                    )
        if len(matches) != 1:
            fail("{} has an invalid or ambiguous mapping: {}".format(
                description, name
            ))
        return matches[0]

    def rva_to_offset(rva, size, description):
        return rva_to_location(rva, size, description)[0]

    export_offset = rva_to_offset(
        export_rva, export_size, "Packaged PE export directory"
    )
    if not has_executable_section or export_size < 40:
        fail(
            "Packaged PE DLL is missing executable code or valid exports: {}"
            .format(name)
        )
    (
        _,
        _,
        _,
        _,
        _,
        _,
        function_count,
        name_count,
        functions_rva,
        names_rva,
        ordinals_rva,
    ) = struct.unpack_from("<IIHHIIIIIII", data, export_offset)
    if (
        function_count == 0
        or name_count == 0
        or function_count > MAX_NATIVE_SYMBOLS
        or name_count > MAX_NATIVE_SYMBOLS
        or name_count > function_count
    ):
        fail("Packaged PE export table is invalid: {}".format(name))

    functions_offset = rva_to_offset(
        functions_rva,
        function_count * 4,
        "Packaged PE export address table",
    )
    names_offset = rva_to_offset(
        names_rva,
        name_count * 4,
        "Packaged PE export name table",
    )
    ordinals_offset = rva_to_offset(
        ordinals_rva,
        name_count * 2,
        "Packaged PE export ordinal table",
    )

    exports = set()
    previous_symbol = None
    for index in range(name_count):
        symbol_rva = struct.unpack_from("<I", data, names_offset + index * 4)[0]
        ordinal = struct.unpack_from(
            "<H", data, ordinals_offset + index * 2
        )[0]
        if ordinal >= function_count:
            fail("Packaged PE export ordinal is invalid: {}".format(name))
        function_rva = struct.unpack_from(
            "<I", data, functions_offset + ordinal * 4
        )[0]
        if function_rva == 0:
            fail("Packaged PE export address is invalid: {}".format(name))
        callable_export = not (
            export_rva <= function_rva < export_rva + export_size
        )
        if callable_export:
            _, _, function_characteristics = rva_to_location(
                function_rva,
                1,
                "Packaged PE export address",
            )
            callable_export = bool(
                function_characteristics & 0x20000000
            )
        symbol_offset, section_limit, _ = rva_to_location(
            symbol_rva, 1, "Packaged PE export name"
        )
        symbol = bounded_c_string(
            data,
            symbol_offset,
            section_limit,
            "Packaged PE export name",
            name,
        )
        if previous_symbol is not None and symbol <= previous_symbol:
            fail(
                "Packaged PE export names are not strictly sorted: {}"
                .format(name)
            )
        previous_symbol = symbol
        if callable_export:
            exports.add(symbol)
    return exports


def validate_native(data, expected, name, jni_symbols):
    kind, architecture = expected
    if kind == "ELF":
        exports = validate_elf(data, architecture, name)
    elif kind == "Mach-O":
        exports = validate_macho(data, architecture, name)
    elif kind == "PE":
        exports = validate_pe(data, architecture, name)
    else:
        fail("Unknown native validation target: {}".format(kind))
    missing_symbols = [
        symbol.decode("ascii")
        for symbol in jni_symbols
        if symbol not in exports
    ]
    if missing_symbols:
        fail(
            "Packaged native entry is missing JNI exports {}: {}".format(
                ", ".join(missing_symbols),
                name,
            )
        )


def parse_properties(contents):
    properties = {}
    text = contents.decode("ISO-8859-1")
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith(("#", "!")):
            continue
        if "=" not in line:
            fail("Invalid Maven pom.properties line: {!r}".format(raw_line))
        key, value = line.split("=", 1)
        key = key.strip()
        value = value.strip()
        if not key or key in properties:
            fail(
                "Invalid or duplicate Maven pom.properties key: {!r}".format(
                    key
                )
            )
        properties[key] = value
    return properties


repository = Path(sys.argv[1]).resolve()
target_dir_argument = Path(sys.argv[2])
if target_dir_argument.is_symlink():
    fail("Java staging validation target must not be a symbolic link")
target_dir = target_dir_argument.resolve()
version = sys.argv[3]
mode = sys.argv[4]

if mode == "java-class-set":
    if target_dir.is_symlink() or not target_dir.is_dir():
        fail(
            "Compiled Java classes directory does not exist: {}".format(
                target_dir
            )
        )
    actual_classes = compiled_java_classes(target_dir)
    validate_java_class_set(actual_classes, "Compiled Java class set")
    raise SystemExit(0)
if mode != "artifacts":
    fail("Unknown Java staging validation mode: {}".format(mode))

if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
    fail("Invalid Java staging version: {}".format(version))
if target_dir.is_symlink() or not target_dir.is_dir():
    fail(
        "Java staging artifact directory does not exist: {}".format(
            target_dir
        )
    )

artifact_names = (
    "mosaic-{}.jar".format(version),
    "mosaic-{}-sources.jar".format(version),
    "mosaic-{}-javadoc.jar".format(version),
)
candidate_names = artifact_names + ("java-staging-provenance.txt",)
actual_names = sorted(
    path.name
    for path in target_dir.iterdir()
    if path.is_file() and not path.is_symlink()
)
if actual_names != sorted(candidate_names) or any(
    path.is_dir() or path.is_symlink() for path in target_dir.iterdir()
):
    fail(
        "Java staging candidate must contain exactly: {}".format(
            ", ".join(candidate_names)
        )
    )

artifact_paths = [target_dir / name for name in artifact_names]
for path in artifact_paths:
    regular_file(path, "Expected Maven artifact")
regular_file(
    target_dir / "java-staging-provenance.txt",
    "Java staging provenance",
)

main_jar, sources_jar, javadoc_jar = artifact_paths
main_archive, main_entries = validated_entries(main_jar)
sources_archive, sources_entries = validated_entries(sources_jar)
javadoc_archive, javadoc_entries = validated_entries(javadoc_jar)

try:
    validate_java_classes(main_jar, main_archive, main_entries)

    expected_pom = (repository / "java/pom.xml").read_bytes()
    embedded_pom = required_bytes(
        main_archive, main_entries, main_jar, POM_ENTRY
    )
    if embedded_pom != expected_pom:
        fail("Packaged Maven pom.xml does not match the signed source tree")

    properties = parse_properties(
        required_bytes(
            main_archive,
            main_entries,
            main_jar,
            POM_PROPERTIES_ENTRY,
        )
    )
    expected_properties = {
        "groupId": "org.apache.paimon",
        "artifactId": "mosaic",
        "version": version,
    }
    if properties != expected_properties:
        fail(
            "Packaged Maven pom.properties is invalid: expected {}, found {}"
            .format(expected_properties, properties)
        )

    try:
        pom_root = ET.fromstring(expected_pom)
    except ET.ParseError as error:
        fail("Signed-source Java POM is invalid: {}".format(error))

    def pom_text(name):
        matches = [
            element
            for element in pom_root
            if element.tag.rsplit("}", 1)[-1] == name
        ]
        if len(matches) != 1 or not (matches[0].text or "").strip():
            fail("Signed-source Java POM must contain one {}".format(name))
        return (matches[0].text or "").strip()

    project_name = pom_text("name")
    inception_year = pom_text("inceptionYear")
    if not re.fullmatch(r"[0-9]{4}", inception_year):
        fail("Signed-source Java POM inceptionYear is invalid")

    legal_contents = {}
    for legal_entry in LEGAL_ENTRIES:
        main_legal = required_bytes(
            main_archive, main_entries, main_jar, legal_entry
        )
        sources_legal = required_bytes(
            sources_archive, sources_entries, sources_jar, legal_entry
        )
        javadoc_legal = required_bytes(
            javadoc_archive, javadoc_entries, javadoc_jar, legal_entry
        )
        if not (
            main_legal == sources_legal and main_legal == javadoc_legal
        ):
            fail(
                "{} differs across main, sources, and javadoc JARs".format(
                    legal_entry
                )
            )
        legal_contents[legal_entry] = main_legal
    expected_license = b"\n" + (repository / "LICENSE").read_bytes()
    if legal_contents["META-INF/LICENSE"] != expected_license:
        fail("Packaged META-INF/LICENSE does not match the signed source tree")
    notice_pattern = re.compile(
        (
            r"\A\n{}\nCopyright {}(?:-[0-9]{{4}})? "
            r"The Apache Software Foundation\n\n"
            r"This product includes software developed at\n"
            r"The Apache Software Foundation "
            r"\(http://www\.apache\.org/\)\.\n\n\n\Z"
        ).format(re.escape(project_name), inception_year).encode("utf-8")
    )
    if not notice_pattern.fullmatch(legal_contents["META-INF/NOTICE"]):
        fail("Packaged META-INF/NOTICE is invalid")
    dependencies_pattern = re.compile(
        (
            r"\A// -+\n"
            r"// Transitive dependencies of this project determined from the\n"
            r"// maven pom organized by organization\.\n"
            r"// -+\n\n{}\n(?:\n|[^\x00\r])*\Z"
        ).format(re.escape(project_name)).encode("utf-8")
    )
    if not dependencies_pattern.fullmatch(
        legal_contents["META-INF/DEPENDENCIES"]
    ):
        fail("Packaged META-INF/DEPENDENCIES is invalid")

    source_root = repository / "java/src/main/java"
    source_files = sorted(source_root.rglob("*.java"))
    if not source_files:
        fail("Signed source tree contains no Java sources")
    expected_source_entries = {
        source_path.relative_to(source_root).as_posix()
        for source_path in source_files
    }
    packaged_source_entries = {
        name
        for name, info in sources_entries.items()
        if name.endswith(".java") and not info.is_dir()
    }
    if packaged_source_entries != expected_source_entries:
        fail(
            "Packaged Java source set is invalid: expected {}, found {}".format(
                sorted(expected_source_entries),
                sorted(packaged_source_entries),
            )
        )

    public_javadoc_entries = set()
    for source_path in source_files:
        entry = source_path.relative_to(source_root).as_posix()
        packaged_source = required_bytes(
            sources_archive, sources_entries, sources_jar, entry
        )
        if packaged_source != source_path.read_bytes():
            fail(
                "Packaged Java source differs from signed source: {}".format(
                    entry
                )
            )
        source_text = source_path.read_text(encoding="utf-8")
        public_types = re.findall(
            r"(?m)^\s*public\s+(?:(?:abstract|final)\s+)?"
            r"(?:class|interface|enum)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            source_text,
        )
        if len(public_types) > 1:
            fail("Java source contains multiple public top-level types: {}".format(entry))
        if public_types:
            package = source_path.relative_to(source_root).parent.as_posix()
            type_name = public_types[0]
            public_javadoc_entries.add(
                "{}/{}.html".format(package, type_name)
            )
            public_javadoc_entries.add(
                "{}/class-use/{}.html".format(package, type_name)
            )

    required_javadoc_entries = public_javadoc_entries | {
        "allclasses-frame.html",
        "allclasses-noframe.html",
        "index-all.html",
        "index.html",
        "overview-tree.html",
        "package-list",
        "script.js",
        "stylesheet.css",
        "org/apache/paimon/mosaic/package-frame.html",
        "org/apache/paimon/mosaic/package-summary.html",
        "org/apache/paimon/mosaic/package-tree.html",
        "org/apache/paimon/mosaic/package-use.html",
    }
    missing_javadocs = [
        entry
        for entry in sorted(required_javadoc_entries)
        if entry not in javadoc_entries or javadoc_entries[entry].is_dir()
    ]
    if missing_javadocs:
        fail(
            "Packaged javadoc is missing required entries: {}".format(
                ", ".join(missing_javadocs)
            )
        )
    for entry in sorted(public_javadoc_entries):
        contents = required_bytes(
            javadoc_archive, javadoc_entries, javadoc_jar, entry
        )
        type_name = Path(entry).stem.encode("utf-8")
        if b"<html" not in contents.lower() or type_name not in contents:
            fail("Packaged javadoc page is invalid: {}".format(entry))

    native_source = (
        source_root / "org/apache/paimon/mosaic/NativeLib.java"
    )
    native_source_text = native_source.read_text(encoding="utf-8")
    native_methods = re.findall(
        r"\bnative\s+[A-Za-z0-9_.$<>\[\]?]+\s+"
        r"([A-Za-z_$][A-Za-z0-9_$]*)\s*\(",
        native_source_text,
    )
    if not native_methods or len(native_methods) != len(set(native_methods)):
        fail("Signed NativeLib.java native method set is invalid")
    jni_symbols = tuple(
        (
            "Java_org_apache_paimon_mosaic_NativeLib_" + method
        ).encode("ascii")
        for method in sorted(native_methods)
    )

    for entry, expected in NATIVE_ENTRIES.items():
        contents = required_bytes(
            main_archive, main_entries, main_jar, entry
        )
        validate_native(contents, expected, entry, jni_symbols)
finally:
    main_archive.close()
    sources_archive.close()
    javadoc_archive.close()

print("Validated Java staging artifacts.")
PY
