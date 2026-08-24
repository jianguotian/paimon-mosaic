#!/usr/bin/env python3

# Licensed to the Apache Software Foundation (ASF) under one or more
# contributor license agreements.  See the NOTICE file distributed with
# this work for additional information regarding copyright ownership.
# The ASF licenses this file to You under the Apache License, Version 2.0
# (the "License"); you may not use this file except in compliance with
# the License.  You may obtain a copy of the License at
#
#    http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Verify main and classifier JAR licensing matches their bundled content."""

from __future__ import annotations

import argparse
import posixpath
import re
import stat
import sys
import xml.etree.ElementTree as ET
import zlib
from pathlib import Path, PurePosixPath
from zipfile import BadZipFile, ZipFile, ZipInfo

import archive_guard
from native_binary import TARGET_ARCHITECTURE, verify_native_target


MAX_JAVA_CLASS_SIZE = 16 * 1024 * 1024
ARCHIVE_READ_CHUNK_SIZE = 1024 * 1024
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
NATIVE_ENTRIES = {
    "native/linux/x86_64/libpaimon_mosaic_jni.so": "x86_64-unknown-linux-gnu",
    "native/linux/aarch64/libpaimon_mosaic_jni.so": "aarch64-unknown-linux-gnu",
    "native/macos/aarch64/libpaimon_mosaic_jni.dylib": "aarch64-apple-darwin",
    "native/windows/x86_64/paimon_mosaic_jni.dll": "x86_64-pc-windows-msvc",
}
NATIVE_SUFFIXES = (".so", ".dylib", ".dll")
JAVA_CLASS_MAGIC = b"\xca\xfe\xba\xbe"
MACHO_MAGICS = {
    JAVA_CLASS_MAGIC,
    b"\xbe\xba\xfe\xca",
    b"\xca\xfe\xba\xbf",
    b"\xbf\xba\xfe\xca",
    b"\xce\xfa\xed\xfe",
    b"\xcf\xfa\xed\xfe",
    b"\xfe\xed\xfa\xce",
    b"\xfe\xed\xfa\xcf",
}
NESTED_LICENSE_MARKERS = (
    "For Zstandard software",
    "Apache Arrow",
)
PUBLIC_JAVA_TYPE = re.compile(
    r"(?m)^public\s+"
    r"(?:(?:abstract|final|sealed|non-sealed|strictfp)\s+)*"
    r"(?:class|interface|enum|record|@interface)\s+"
)


def _validate_target_matrix() -> None:
    if not (
        set(TARGETS)
        == set(NATIVE_ENTRIES.values())
        == set(TARGET_ARCHITECTURE)
    ):
        raise RuntimeError("Java native target matrices are inconsistent")


_validate_target_matrix()


def repository_root() -> Path:
    return Path(__file__).resolve().parent.parent


def validated_entries(archive: ZipFile) -> dict[str, ZipInfo]:
    entries = archive_guard.validated_entries(archive, "JAR")
    # ZipExtFile validates a member's CRC only when it is read through EOF.
    # Header-only format detection below is therefore insufficient for ordinary
    # resources, so stream every bounded member once before accepting the JAR.
    for info in entries.values():
        if info.is_dir():
            continue
        with archive.open(info) as source:
            while source.read(ARCHIVE_READ_CHUNK_SIZE):
                pass
    return entries


def read_java_u2(data: bytes, offset: int) -> tuple[int, int] | None:
    if offset > len(data) - 2:
        return None
    return int.from_bytes(data[offset : offset + 2], "big"), offset + 2


def skip_java_attributes(
    data: bytes, offset: int, count: int
) -> int | None:
    for _ in range(count):
        if offset > len(data) - 6:
            return None
        length = int.from_bytes(data[offset + 2 : offset + 6], "big")
        offset += 6
        if length > len(data) - offset:
            return None
        offset += length
    return offset


def skip_java_members(data: bytes, offset: int) -> int | None:
    result = read_java_u2(data, offset)
    if result is None:
        return None
    count, offset = result
    for _ in range(count):
        if offset > len(data) - 8:
            return None
        attributes_count = int.from_bytes(data[offset + 6 : offset + 8], "big")
        offset = skip_java_attributes(data, offset + 8, attributes_count)
        if offset is None:
            return None
    return offset


def java_class_provenance(
    data: bytes,
) -> tuple[str, frozenset[str]] | None:
    """Return a complete class file's name and declared enclosing classes."""
    if len(data) < 10 or not data.startswith(JAVA_CLASS_MAGIC):
        return None
    major_version = int.from_bytes(data[6:8], "big")
    constant_pool_count = int.from_bytes(data[8:10], "big")
    if not 45 <= major_version <= 100 or constant_pool_count == 0:
        return None

    utf8_entries: dict[int, bytes] = {}
    class_entries: dict[int, int] = {}
    offset = 10
    index = 1
    while index < constant_pool_count:
        if offset >= len(data):
            return None
        tag = data[offset]
        offset += 1
        if tag == 1:
            result = read_java_u2(data, offset)
            if result is None:
                return None
            length, offset = result
            if length > len(data) - offset:
                return None
            utf8_entries[index] = data[offset : offset + length]
            offset += length
        elif tag in (3, 4, 9, 10, 11, 12, 17, 18):
            offset += 4
        elif tag in (5, 6):
            offset += 8
            index += 1
        elif tag == 7:
            result = read_java_u2(data, offset)
            if result is None:
                return None
            class_entries[index], offset = result
        elif tag in (8, 16, 19, 20):
            offset += 2
        elif tag == 15:
            offset += 3
        else:
            return None
        if offset > len(data):
            return None
        index += 1

    if offset > len(data) - 8:
        return None
    this_class = int.from_bytes(data[offset + 2 : offset + 4], "big")
    def class_name(class_index: int) -> str | None:
        name_index = class_entries.get(class_index)
        name_bytes = (
            utf8_entries.get(name_index) if name_index is not None else None
        )
        if name_bytes is None:
            return None
        try:
            name = name_bytes.decode("utf-8")
        except UnicodeDecodeError:
            return None
        return name or None

    internal_name = class_name(this_class)
    if internal_name is None:
        return None

    interfaces_count = int.from_bytes(data[offset + 6 : offset + 8], "big")
    offset += 8
    if interfaces_count > (len(data) - offset) // 2:
        return None
    offset += interfaces_count * 2

    offset = skip_java_members(data, offset)
    if offset is None:
        return None
    offset = skip_java_members(data, offset)
    if offset is None:
        return None
    result = read_java_u2(data, offset)
    if result is None:
        return None
    attributes_count, offset = result
    enclosing_classes = set()
    for _ in range(attributes_count):
        if offset > len(data) - 6:
            return None
        name_index = int.from_bytes(data[offset : offset + 2], "big")
        length = int.from_bytes(data[offset + 2 : offset + 6], "big")
        offset += 6
        if length > len(data) - offset:
            return None
        attribute = data[offset : offset + length]
        offset += length
        attribute_name = utf8_entries.get(name_index)
        if attribute_name == b"InnerClasses":
            if length < 2:
                return None
            classes_count = int.from_bytes(attribute[:2], "big")
            if length != 2 + 8 * classes_count:
                return None
            for entry_offset in range(2, length, 8):
                inner_class = int.from_bytes(
                    attribute[entry_offset : entry_offset + 2], "big"
                )
                outer_class = int.from_bytes(
                    attribute[entry_offset + 2 : entry_offset + 4], "big"
                )
                if inner_class == this_class and outer_class:
                    owner = class_name(outer_class)
                    if owner is None:
                        return None
                    enclosing_classes.add(owner)
        elif attribute_name == b"EnclosingMethod":
            if length != 4:
                return None
            owner = class_name(int.from_bytes(attribute[:2], "big"))
            if owner is None:
                return None
            enclosing_classes.add(owner)
    if offset != len(data):
        return None
    return internal_name, frozenset(enclosing_classes)


def java_class_internal_name(data: bytes) -> str | None:
    """Return a complete class file's declared internal name."""
    provenance = java_class_provenance(data)
    return provenance[0] if provenance is not None else None


def is_java_class(data: bytes) -> bool:
    """Distinguish a complete Java class from Mach-O's shared CAFEBABE magic."""
    return java_class_internal_name(data) is not None


def native_binary_magic(source, size: int, name: str) -> str | None:
    """Return the native executable format, excluding valid Java class files."""
    header = source.read(min(size, 64))
    if header.startswith(b"\x7fELF"):
        return "ELF"
    if (
        header.startswith(JAVA_CLASS_MAGIC)
        and name.lower().endswith(".class")
    ):
        if size <= MAX_JAVA_CLASS_SIZE:
            source.seek(0)
            if is_java_class(source.read(MAX_JAVA_CLASS_SIZE + 1)):
                return None
    if header[:4] in MACHO_MAGICS:
        return "Mach-O"
    if header.startswith(b"MZ") and len(header) >= 64:
        pe_offset = int.from_bytes(header[0x3C:0x40], "little")
        if pe_offset <= size - 4:
            source.seek(pe_offset)
            if source.read(4) == b"PE\0\0":
                return "PE"
    return None


def native_archive_entries(
    archive: ZipFile, entries: dict[str, ZipInfo]
) -> set[str]:
    native_entries = set()
    for name, info in entries.items():
        if info.is_dir():
            continue
        with archive.open(info) as source:
            magic = native_binary_magic(source, info.file_size, name)
        if name.lower().endswith(NATIVE_SUFFIXES) or magic is not None:
            native_entries.add(name)
    return native_entries


def java_source_files(root: Path) -> dict[str, Path]:
    source_root = root / "java/src/main/java"
    return {
        path.relative_to(source_root).as_posix(): path
        for path in source_root.rglob("*.java")
        if path.is_file()
    }


MAVEN_POM_NAMESPACE = "{http://maven.apache.org/POM/4.0.0}"
# Written into every JAR by the ASF parent build rather than declared by the
# project, so they are expected payload.
BUILD_METADATA_ENTRIES = frozenset(
    {"META-INF/MANIFEST.MF", "META-INF/DEPENDENCIES"}
)
CLASSIFIER_LEGAL_ENTRIES = frozenset({"META-INF/LICENSE", "META-INF/NOTICE"})


def maven_descriptor_entries(root: Path) -> set[str]:
    """Return the archived Maven descriptor paths implied by the POM coordinates.

    Reads the repository's own version-controlled POM, not a downloaded
    artifact, so this is inside the trust boundary the verifiers police.
    """
    try:
        document = ET.parse(root / "java/pom.xml").getroot()
    except ET.ParseError as error:
        raise ValueError(f"java/pom.xml is not well-formed XML: {error}") from error

    def coordinate(name: str) -> str:
        for path in (
            f"{MAVEN_POM_NAMESPACE}{name}",
            f"{MAVEN_POM_NAMESPACE}parent/{MAVEN_POM_NAMESPACE}{name}",
        ):
            element = document.find(path)
            if element is not None and element.text:
                return element.text.strip()
        raise ValueError(f"java/pom.xml declares no {name}")

    prefix = f"META-INF/maven/{coordinate('groupId')}/{coordinate('artifactId')}"
    return {f"{prefix}/pom.xml", f"{prefix}/pom.properties"}


def generated_javadoc_files(root: Path) -> dict[str, Path]:
    javadoc_root = root / "java/target/apidocs"
    if not javadoc_root.is_dir():
        raise ValueError("generated Javadoc directory does not exist")
    return {
        path.relative_to(javadoc_root).as_posix(): path
        for path in javadoc_root.rglob("*")
        if path.is_file()
    }


def verify_classifier_payload_entries(
    entries: dict[str, ZipInfo],
    expected_payload: set[str],
    required_metadata: set[str],
    noun: str,
) -> None:
    actual_payload = {
        name for name, info in entries.items() if not info.is_dir()
    }
    expected_entries = (
        expected_payload | CLASSIFIER_LEGAL_ENTRIES | required_metadata
    )
    missing = sorted(expected_entries - actual_payload)
    if missing:
        raise ValueError(f"{noun} is missing expected entries: {missing}")
    unexpected = sorted(actual_payload - expected_entries)
    if unexpected:
        raise ValueError(f"{noun} contains unexpected entries: {unexpected}")


def verify_compiled_java_classes(
    archive: ZipFile, entries: dict[str, ZipInfo], root: Path
) -> set[str]:
    sources = java_source_files(root)
    if not sources:
        raise ValueError("repository contains no Java sources")

    required_classes = {
        str(PurePosixPath(source_path).with_suffix(".class"))
        for source_path in sources
    }
    missing = sorted(required_classes - entries.keys())
    if missing:
        raise ValueError(f"main JAR is missing compiled Java classes: {missing}")

    class_entries = {name for name in entries if name.endswith(".class")}
    class_owners: dict[str, frozenset[str]] = {}
    for name in sorted(class_entries):
        class_file = entries[name]
        if class_file.file_size > MAX_JAVA_CLASS_SIZE:
            raise ValueError(
                f"compiled Java class {name!r} exceeds the size limit of "
                f"{MAX_JAVA_CLASS_SIZE} bytes: {class_file.file_size} bytes"
            )
        with archive.open(class_file) as class_stream:
            class_bytes = class_stream.read(MAX_JAVA_CLASS_SIZE + 1)
        provenance = java_class_provenance(class_bytes)
        if provenance is None:
            raise ValueError(f"invalid compiled Java class: {name}")
        internal_name, owners = provenance
        expected_name = name.removesuffix(".class")
        if internal_name != expected_name:
            raise ValueError(
                f"compiled Java class {name!r} declares {internal_name!r}"
            )
        class_owners[internal_name] = owners

    source_classes = {
        name.removesuffix(".class") for name in required_classes
    }

    def has_source_provenance(
        class_name: str, visiting: frozenset[str] = frozenset()
    ) -> bool:
        if class_name in source_classes:
            return True
        if class_name in visiting:
            return False
        return any(
            class_name.startswith(owner + "$")
            and owner in class_owners
            and has_source_provenance(owner, visiting | {class_name})
            for owner in class_owners.get(class_name, ())
        )

    for class_name in sorted(class_owners):
        if not has_source_provenance(class_name):
            raise ValueError(
                f"main JAR class {class_name + '.class'!r} has no matching "
                "repository source or declared enclosing class"
            )
    return class_entries


def verify_main_jar(path: Path, root: Path, require_all_natives: bool) -> None:
    binary_resources = root / "java/src/main/binary-resources/META-INF"
    with ZipFile(path) as archive:
        entries = validated_entries(archive)
        required = {"META-INF/LICENSE", "META-INF/NOTICE"}
        required.update(
            f"META-INF/licenses/{target}/THIRD-PARTY-LICENSES.html"
            for target in TARGETS
        )
        missing = sorted(required - entries.keys())
        if missing:
            raise ValueError(f"missing legal files: {missing}")
        if "META-INF/DEPENDENCIES.rust.tsv" in entries:
            raise ValueError(
                "main JAR contains the cross-target repository dependency inventory"
            )

        expected_license = (binary_resources / "LICENSE").read_bytes()
        if archive.read(entries["META-INF/LICENSE"]) != expected_license:
            raise ValueError("main JAR LICENSE is not the binary-specific LICENSE")
        license_text = expected_license.decode("utf-8")
        expected_notice = (binary_resources / "NOTICE").read_bytes()
        if archive.read(entries["META-INF/NOTICE"]) != expected_notice:
            raise ValueError("main JAR NOTICE is not the binary-specific NOTICE")
        if b"Apache Arrow" not in expected_notice:
            raise ValueError("main JAR NOTICE omits the bundled Apache Arrow notice")

        for target in TARGETS:
            report_path = f"META-INF/licenses/{target}/THIRD-PARTY-LICENSES.html"
            if report_path not in license_text:
                raise ValueError(f"LICENSE does not point to {report_path}")
            expected_report = (
                binary_resources
                / "licenses"
                / target
                / "THIRD-PARTY-LICENSES.html"
            ).read_bytes()
            actual_report = archive.read(entries[report_path])
            if actual_report != expected_report:
                raise ValueError(f"{report_path} differs from its generated source")

            report_text = actual_report.decode("utf-8")
            if target not in report_text:
                raise ValueError(f"{report_path} does not identify its target")
            for marker in NESTED_LICENSE_MARKERS:
                if marker not in report_text:
                    raise ValueError(f"{report_path} is missing {marker!r}")

        class_entries = verify_compiled_java_classes(archive, entries, root)

        packaged_natives = native_archive_entries(archive, entries)
        unexpected_natives = packaged_natives - set(NATIVE_ENTRIES)
        if unexpected_natives:
            raise ValueError(f"unexpected native entries: {sorted(unexpected_natives)}")

        # Presence checks alone let anything else ride along, so state the whole
        # expected payload and reject the difference. Directory entries carry no
        # payload and their names are already validated.
        expected_payload = (
            required
            | BUILD_METADATA_ENTRIES
            | maven_descriptor_entries(root)
            | class_entries
            | packaged_natives
        )
        actual_payload = {
            name for name, info in entries.items() if not info.is_dir()
        }
        missing_payload = sorted(expected_payload - actual_payload)
        if missing_payload:
            raise ValueError(
                f"main JAR is missing expected entries: {missing_payload}"
            )
        unexpected_payload = sorted(
            actual_payload - expected_payload
        )
        if unexpected_payload:
            raise ValueError(
                f"main JAR contains unexpected entries: {unexpected_payload}"
            )
        if require_all_natives and packaged_natives != set(NATIVE_ENTRIES):
            raise ValueError(
                "release JAR native entries differ from the four declared targets: "
                + repr(sorted(packaged_natives))
            )
        for native_entry in packaged_natives:
            verify_native_target(
                archive.read(entries[native_entry]),
                NATIVE_ENTRIES[native_entry],
                native_entry,
                symbol_family="JNI",
            )

    print(f"verified main JAR: {path}")


def _verify_classifier_entries(
    archive: ZipFile, entries: dict[str, ZipInfo], root: Path
) -> None:
    for required in ("META-INF/LICENSE", "META-INF/NOTICE"):
        if required not in entries:
            raise ValueError(f"missing {required}")

    expected_license = (root / "LICENSE").read_bytes()
    if archive.read(entries["META-INF/LICENSE"]) != expected_license:
        raise ValueError("classifier LICENSE differs from repository root LICENSE")
    expected_notice = (root / "NOTICE").read_bytes()
    if archive.read(entries["META-INF/NOTICE"]) != expected_notice:
        raise ValueError("classifier NOTICE differs from repository root NOTICE")

    forbidden = sorted(
        name
        for name in entries
        if name.startswith("native/")
        or name == "META-INF/DEPENDENCIES.rust.tsv"
        or posixpath.basename(name) == "THIRD-PARTY-LICENSES.html"
    )
    forbidden.extend(
        sorted(native_archive_entries(archive, entries) - set(forbidden))
    )
    if forbidden:
        raise ValueError(f"classifier contains binary-only files: {forbidden}")


def verify_sources_jar(path: Path, root: Path | None = None) -> None:
    if root is None:
        root = repository_root()
    with ZipFile(path) as archive:
        entries = validated_entries(archive)
        _verify_classifier_entries(archive, entries, root)
        print(f"verified classifier JAR: {path}")
        expected = java_source_files(root)
        if not expected:
            raise ValueError("repository contains no Java sources")
        packaged = {name for name in entries if name.endswith(".java")}
        if packaged != set(expected):
            missing = sorted(set(expected) - packaged)
            unexpected = sorted(packaged - set(expected))
            raise ValueError(
                "sources JAR Java files differ from the repository sources: "
                f"missing {missing}, unexpected {unexpected}"
            )
        verify_classifier_payload_entries(
            entries,
            set(expected),
            set(BUILD_METADATA_ENTRIES) | maven_descriptor_entries(root),
            "sources JAR",
        )
        for archive_path, source_path in expected.items():
            if archive.read(entries[archive_path]) != source_path.read_bytes():
                raise ValueError(
                    f"sources JAR entry {archive_path} differs from "
                    f"{source_path.as_posix()}"
                )
    print(f"verified sources JAR payload: {path}")


def verify_javadoc_jar(path: Path, root: Path | None = None) -> None:
    if root is None:
        root = repository_root()
    with ZipFile(path) as archive:
        entries = validated_entries(archive)
        _verify_classifier_entries(archive, entries, root)
        print(f"verified classifier JAR: {path}")
        sources = java_source_files(root)
        if not sources:
            raise ValueError("repository contains no Java sources")
        expected = generated_javadoc_files(root)
        if not expected:
            raise ValueError("generated Javadoc directory contains no files")
        documented_sources = {
            source_path
            for source_path, source in sources.items()
            if PUBLIC_JAVA_TYPE.search(source.read_text(encoding="utf-8"))
        }
        if not documented_sources:
            raise ValueError("repository contains no public Java API")
        required = {
            "index.html",
            *(
                f"{source_path.removesuffix('.java')}.html"
                for source_path in documented_sources
            ),
        }
        missing = sorted(required - entries.keys())
        if missing:
            raise ValueError(f"javadoc JAR is missing documentation pages: {missing}")
        empty = sorted(
            name
            for name in required
            if entries[name].is_dir() or entries[name].file_size == 0
        )
        if empty:
            raise ValueError(f"javadoc JAR contains empty documentation pages: {empty}")
        verify_classifier_payload_entries(
            entries,
            set(expected),
            set(BUILD_METADATA_ENTRIES),
            "javadoc JAR",
        )
        for archive_path, generated_path in expected.items():
            if archive.read(entries[archive_path]) != generated_path.read_bytes():
                raise ValueError(
                    f"javadoc JAR entry {archive_path} differs from "
                    f"{generated_path.as_posix()}"
                )
    print(f"verified javadoc JAR payload: {path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--main", required=True, type=Path)
    parser.add_argument("--sources", required=True, type=Path)
    parser.add_argument("--javadoc", required=True, type=Path)
    parser.add_argument("--require-all-natives", action="store_true")
    args = parser.parse_args()
    root = repository_root()

    # The classifier checks share one code path, so their messages carry no
    # artifact path; name the artifact here as the wheel verifier does. Unlike
    # the wheel verifier this loop stops at the first failure, because a broken
    # main JAR makes the classifier results uninteresting.
    checks = (
        (args.main, lambda: verify_main_jar(args.main, root, args.require_all_natives)),
        (args.sources, lambda: verify_sources_jar(args.sources, root)),
        (args.javadoc, lambda: verify_javadoc_jar(args.javadoc, root)),
    )
    for artifact, check in checks:
        try:
            check()
        except (
            BadZipFile,
            KeyError,
            OSError,
            TypeError,
            ValueError,
            zlib.error,
        ) as error:
            print(
                f"Java artifact verification failed: {artifact}: {error}",
                file=sys.stderr,
            )
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
