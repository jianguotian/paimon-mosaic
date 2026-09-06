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
EOF
}

if [[ $# -ne 2 ]]; then
  usage
  exit 1
fi

TARGET_DIR=$1
VERSION=$2

if ! command -v "$PYTHON" >/dev/null 2>&1; then
  echo "python3 is required to validate Java staging artifacts" >&2
  exit 1
fi

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd "$SCRIPT_DIR/.." && pwd)

"$PYTHON" - \
  "$REPO_DIR" \
  "$TARGET_DIR" \
  "$VERSION" <<'PY'
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


def validate_java_classes(main_jar, archive, entries):
    javap = shutil.which("javap")
    if javap is None:
        fail("javap is required to validate the packaged Java classes")

    actual_classes = {
        name
        for name, info in entries.items()
        if name.endswith(".class") and not info.is_dir()
    }
    if actual_classes != EXPECTED_CLASS_ENTRIES:
        fail(
            "Packaged Java class set is invalid: expected {}, found {}".format(
                sorted(EXPECTED_CLASS_ENTRIES),
                sorted(actual_classes),
            )
        )

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

    has_load = False
    has_executable_load = False
    has_dynamic = False
    for index in range(program_count):
        offset = program_offset + index * program_entry_size
        segment_type = struct.unpack_from("<I", data, offset)[0]
        flags = struct.unpack_from("<I", data, offset + 4)[0]
        file_offset = struct.unpack_from("<Q", data, offset + 8)[0]
        file_size = struct.unpack_from("<Q", data, offset + 32)[0]
        memory_size = struct.unpack_from("<Q", data, offset + 40)[0]
        if (
            file_offset > len(data)
            or file_size > len(data) - file_offset
            or file_size > memory_size
        ):
            fail("Packaged ELF segment is out of bounds: {}".format(name))
        if segment_type == 1 and file_size > 0:
            has_load = True
            has_executable_load = has_executable_load or bool(flags & 0x1)
        elif segment_type == 2 and file_size > 0:
            has_dynamic = True
    if not (has_load and has_executable_load and has_dynamic):
        fail("Packaged ELF is missing load or dynamic segments: {}".format(name))

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
            (section_type, file_offset, file_size, link, entry_size)
        )

    exports = set()
    dynamic_symbol_tables = 0
    for section_type, file_offset, file_size, link, entry_size in sections:
        if section_type != 11:
            continue
        dynamic_symbol_tables += 1
        if (
            entry_size != 24
            or file_size % entry_size != 0
            or link >= len(sections)
        ):
            fail("Packaged ELF dynamic symbol table is invalid: {}".format(name))
        string_type, string_offset, string_size, _, _ = sections[link]
        symbol_count = file_size // entry_size
        if (
            string_type != 3
            or string_size == 0
            or symbol_count > MAX_NATIVE_SYMBOLS
        ):
            fail("Packaged ELF dynamic string table is invalid: {}".format(name))
        string_limit = string_offset + string_size
        for index in range(symbol_count):
            offset = file_offset + index * entry_size
            string_index = struct.unpack_from("<I", data, offset)[0]
            symbol_info = data[offset + 4]
            symbol_visibility = data[offset + 5] & 0x3
            section_index = struct.unpack_from("<H", data, offset + 6)[0]
            symbol_binding = symbol_info >> 4
            if (
                string_index == 0
                or string_index >= string_size
                or section_index == 0
                or symbol_binding not in (1, 2)
                or symbol_visibility not in (0, 3)
            ):
                continue
            exports.add(
                bounded_c_string(
                    data,
                    string_offset + string_index,
                    string_limit,
                    "Packaged ELF dynamic symbol",
                    name,
                )
            )
    if dynamic_symbol_tables == 0:
        fail("Packaged ELF has no dynamic symbol table: {}".format(name))
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
            file_offset = struct.unpack_from("<Q", data, offset + 40)[0]
            file_size = struct.unpack_from("<Q", data, offset + 48)[0]
            initial_protection = struct.unpack_from("<I", data, offset + 60)[0]
            if (
                file_offset > len(data)
                or file_size > len(data) - file_offset
            ):
                fail("Packaged Mach-O segment is out of bounds: {}".format(name))
            if file_size > 0 and initial_protection & 0x4:
                has_executable_segment = True
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

    exports = set()
    string_limit = string_offset + string_size
    for index in range(symbol_count):
        offset = symbol_offset + index * 16
        string_index, symbol_type, section_index = struct.unpack_from(
            "<IBB", data, offset
        )
        if (
            string_index == 0
            or string_index >= string_size
            or symbol_type & 0xE0
            or not symbol_type & 0x01
            or symbol_type & 0x0E == 0
            or section_index == 0
        ):
            continue
        symbol = bounded_c_string(
            data,
            string_offset + string_index,
            string_limit,
            "Packaged Mach-O dynamic symbol",
            name,
        )
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
            (virtual_address, mapped_size, raw_offset, raw_size)
        )

    def rva_to_offset(rva, size, description):
        for virtual_address, mapped_size, raw_offset, raw_size in sections:
            if virtual_address <= rva:
                delta = rva - virtual_address
                if (
                    delta <= mapped_size
                    and size <= mapped_size - delta
                    and delta <= raw_size
                    and size <= raw_size - delta
                ):
                    return raw_offset + delta
        fail("{} is out of bounds: {}".format(description, name))

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
        symbol_offset = rva_to_offset(
            symbol_rva, 1, "Packaged PE export name"
        )
        section_limit = None
        for _, _, raw_offset, raw_size in sections:
            if raw_offset <= symbol_offset < raw_offset + raw_size:
                section_limit = raw_offset + raw_size
                break
        if section_limit is None:
            fail("Packaged PE export name is out of bounds: {}".format(name))
        exports.add(
            bounded_c_string(
                data,
                symbol_offset,
                section_limit,
                "Packaged PE export name",
                name,
            )
        )
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
target_dir = Path(sys.argv[2]).resolve()
version = sys.argv[3]

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
