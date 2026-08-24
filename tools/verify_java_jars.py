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

"""Verify the real Maven main, sources, and javadoc JARs."""

from __future__ import annotations

import argparse
import sys
import xml.etree.ElementTree as ET
import zlib
from pathlib import Path
from zipfile import BadZipFile, ZipFile, ZipInfo

import archive_guard


TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
NATIVE_ENTRIES = {
    "native/linux/x86_64/libpaimon_mosaic_jni.so": "ELF",
    "native/linux/aarch64/libpaimon_mosaic_jni.so": "ELF",
    "native/macos/aarch64/libpaimon_mosaic_jni.dylib": "Mach-O",
    "native/windows/x86_64/paimon_mosaic_jni.dll": "PE",
}
NATIVE_SUFFIXES = (".so", ".dylib", ".dll")
MACHO_MAGICS = frozenset(
    (
        b"\xfe\xed\xfa\xce",
        b"\xce\xfa\xed\xfe",
        b"\xfe\xed\xfa\xcf",
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
        b"\xbe\xba\xfe\xca",
        b"\xca\xfe\xba\xbf",
        b"\xbf\xba\xfe\xca",
    )
)
MAVEN_NAMESPACE = "{http://maven.apache.org/POM/4.0.0}"
MANIFEST = "META-INF/MANIFEST.MF"
DEPENDENCIES = "META-INF/DEPENDENCIES"
LEGAL = ("META-INF/LICENSE", "META-INF/NOTICE")


def repository_root() -> Path:
    return Path(__file__).resolve().parent.parent


def _files(root: Path) -> dict[str, Path]:
    if not root.is_dir():
        raise ValueError(f"expected directory does not exist: {root}")
    return {
        path.relative_to(root).as_posix(): path
        for path in root.rglob("*")
        if path.is_file()
    }


def _archive_files(entries: dict[str, ZipInfo]) -> set[str]:
    return {name for name, info in entries.items() if not info.is_dir()}


def _coordinates(root: Path) -> tuple[str, str, str]:
    pom = root / "java/pom.xml"
    try:
        document = ET.parse(pom).getroot()
    except ET.ParseError as error:
        raise ValueError(f"java/pom.xml is invalid: {error}") from error

    def value(name: str) -> str:
        for path in (
            f"{MAVEN_NAMESPACE}{name}",
            f"{MAVEN_NAMESPACE}parent/{MAVEN_NAMESPACE}{name}",
        ):
            element = document.find(path)
            if element is not None and element.text and element.text.strip():
                return element.text.strip()
        raise ValueError(f"java/pom.xml has no {name}")

    return value("groupId"), value("artifactId"), value("version")


def _maven_paths(root: Path) -> tuple[str, str, dict[str, str]]:
    group_id, artifact_id, version = _coordinates(root)
    prefix = f"META-INF/maven/{group_id}/{artifact_id}"
    return (
        f"{prefix}/pom.xml",
        f"{prefix}/pom.properties",
        {
            "groupId": group_id,
            "artifactId": artifact_id,
            "version": version,
        },
    )


def _properties(contents: bytes, name: str) -> dict[str, str]:
    try:
        text = contents.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValueError(f"{name} is not UTF-8") from error
    parsed = {}
    for line in text.splitlines():
        if not line or line.startswith(("#", "!")):
            continue
        if "=" not in line:
            raise ValueError(f"{name} contains an invalid line: {line!r}")
        key, value = line.split("=", 1)
        if key in parsed:
            raise ValueError(f"{name} repeats {key!r}")
        parsed[key] = value
    return parsed


def _verify_maven_metadata(
    archive: ZipFile,
    entries: dict[str, ZipInfo],
    root: Path,
) -> set[str]:
    pom_entry, properties_entry, expected_properties = _maven_paths(root)
    for required in (pom_entry, properties_entry):
        if required not in entries:
            raise ValueError(f"missing Maven metadata {required}")
    expected_pom = (root / "java/pom.xml").read_bytes()
    if archive.read(entries[pom_entry]) != expected_pom:
        raise ValueError(f"{pom_entry} differs from java/pom.xml")
    actual_properties = _properties(
        archive.read(entries[properties_entry]), properties_entry
    )
    if actual_properties != expected_properties:
        raise ValueError(
            f"{properties_entry} has {actual_properties}, "
            f"expected {expected_properties}"
        )
    return {pom_entry, properties_entry}


def _valid_native_magic(contents: bytes, kind: str) -> bool:
    if not contents:
        return False
    if kind == "ELF":
        return contents.startswith(b"\x7fELF")
    if kind == "Mach-O":
        return contents[:4] in MACHO_MAGICS
    if kind == "PE":
        if len(contents) < 64 or not contents.startswith(b"MZ"):
            return False
        offset = int.from_bytes(contents[0x3C:0x40], "little")
        return (
            offset <= len(contents) - 4
            and contents[offset : offset + 4] == b"PE\0\0"
        )
    raise AssertionError(f"unknown native kind: {kind}")


def _verify_classifier_legal(
    archive: ZipFile, entries: dict[str, ZipInfo], root: Path
) -> None:
    expected = {
        "META-INF/LICENSE": root / "LICENSE",
        "META-INF/NOTICE": root / "NOTICE",
    }
    for name, source in expected.items():
        if name not in entries:
            raise ValueError(f"missing classifier legal file {name}")
        if archive.read(entries[name]) != source.read_bytes():
            raise ValueError(f"classifier legal file {name} differs from {source}")


def _verify_classifier_dependencies(
    archive: ZipFile, entries: dict[str, ZipInfo], root: Path
) -> None:
    authoritative = (
        root
        / "java/target/maven-shared-archive-resources/META-INF/DEPENDENCIES"
    )
    if not authoritative.is_file():
        raise ValueError(
            f"authoritative Maven DEPENDENCIES file does not exist: {authoritative}"
        )
    if DEPENDENCIES not in entries:
        raise ValueError(f"classifier is missing {DEPENDENCIES}")
    if archive.read(entries[DEPENDENCIES]) != authoritative.read_bytes():
        raise ValueError(
            f"classifier {DEPENDENCIES} differs from {authoritative}"
        )


def verify_main_jar(
    path: Path,
    root: Path | None = None,
    classes_dir: Path | None = None,
) -> None:
    root = root or repository_root()
    classes_dir = classes_dir or root / "java/target/classes"
    output_files = _files(classes_dir)
    expected_classes = {
        name for name in output_files if name.endswith(".class")
    }
    if not expected_classes:
        raise ValueError("target/classes contains no compiled Java classes")

    with ZipFile(path) as archive:
        entries = archive_guard.validated_entries(archive, "JAR")
        actual_files = _archive_files(entries)
        actual_classes = {name for name in actual_files if name.endswith(".class")}
        if actual_classes != expected_classes:
            raise ValueError(
                "main JAR class entries differ from target/classes: "
                f"missing {sorted(expected_classes - actual_classes)}, "
                f"unexpected {sorted(actual_classes - expected_classes)}"
            )
        for name in expected_classes:
            if archive.read(entries[name]) != output_files[name].read_bytes():
                raise ValueError(
                    f"main JAR class {name} differs from target/classes"
                )

        maven_entries = _verify_maven_metadata(archive, entries, root)
        expected_files = set(output_files) | maven_entries | {MANIFEST}
        missing = sorted(expected_files - actual_files)
        unexpected = sorted(actual_files - expected_files)
        if missing or unexpected:
            raise ValueError(
                "main JAR payload differs from target/classes and Maven metadata: "
                f"missing {missing}, unexpected {unexpected}"
            )
        if not archive.read(entries[MANIFEST]):
            raise ValueError("main JAR manifest is empty")
        for name, source in output_files.items():
            if archive.read(entries[name]) != source.read_bytes():
                raise ValueError(
                    f"main JAR entry {name} differs from target/classes"
                )

        binary_legal = root / "java/src/main/binary-resources/META-INF"
        expected_legal = {
            "META-INF/LICENSE": binary_legal / "LICENSE",
            "META-INF/NOTICE": binary_legal / "NOTICE",
            **{
                f"META-INF/licenses/{target}/THIRD-PARTY-LICENSES.html": (
                    binary_legal
                    / "licenses"
                    / target
                    / "THIRD-PARTY-LICENSES.html"
                )
                for target in TARGETS
            },
        }
        for name, source in expected_legal.items():
            if (
                name not in entries
                or archive.read(entries[name]) != source.read_bytes()
            ):
                raise ValueError(f"main JAR legal file {name} is missing or incorrect")

        native_entries = {
            name
            for name in actual_files
            if name.startswith("native/") or name.lower().endswith(NATIVE_SUFFIXES)
        }
        if native_entries != set(NATIVE_ENTRIES):
            raise ValueError(
                "main JAR native entries differ from the four release paths: "
                f"{sorted(native_entries)}"
            )
        for name, kind in NATIVE_ENTRIES.items():
            if not _valid_native_magic(archive.read(entries[name]), kind):
                raise ValueError(f"invalid {kind} native magic for {name}")
    print(f"verified main JAR: {path}")


def verify_sources_jar(path: Path, root: Path | None = None) -> None:
    root = root or repository_root()
    source_root = root / "java/src/main/java"
    sources = _files(source_root)
    if not sources:
        raise ValueError("repository contains no Java sources")

    with ZipFile(path) as archive:
        entries = archive_guard.validated_entries(archive, "sources JAR")
        _verify_classifier_legal(archive, entries, root)
        _verify_classifier_dependencies(archive, entries, root)
        maven_entries = _verify_maven_metadata(archive, entries, root)
        allowed = (
            set(sources)
            | maven_entries
            | {MANIFEST, DEPENDENCIES, *LEGAL}
        )
        actual = _archive_files(entries)
        if actual != allowed:
            raise ValueError(
                "sources JAR payload differs from repository sources: "
                f"missing {sorted(allowed - actual)}, "
                f"unexpected {sorted(actual - allowed)}"
            )
        for name, source in sources.items():
            if archive.read(entries[name]) != source.read_bytes():
                raise ValueError(f"sources JAR source {name} differs from repository")
    print(f"verified sources JAR: {path}")


def verify_javadoc_jar(
    path: Path,
    root: Path | None = None,
    javadoc_dir: Path | None = None,
) -> None:
    root = root or repository_root()
    javadoc_dir = javadoc_dir or root / "java/target/apidocs"
    generated = _files(javadoc_dir)
    if "index.html" not in generated or generated["index.html"].stat().st_size == 0:
        raise ValueError("generated Javadoc has no non-empty index.html")

    with ZipFile(path) as archive:
        entries = archive_guard.validated_entries(archive, "javadoc JAR")
        _verify_classifier_legal(archive, entries, root)
        _verify_classifier_dependencies(archive, entries, root)
        allowed = set(generated) | {MANIFEST, DEPENDENCIES, *LEGAL}
        actual = _archive_files(entries)
        if actual != allowed:
            raise ValueError(
                "Javadoc payload differs from generated output: "
                f"missing {sorted(allowed - actual)}, "
                f"unexpected {sorted(actual - allowed)}"
            )
        for name, source in generated.items():
            if archive.read(entries[name]) != source.read_bytes():
                raise ValueError(
                    f"Javadoc entry {name} differs from generated output"
                )
    print(f"verified javadoc JAR: {path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--main", required=True, type=Path)
    parser.add_argument("--sources", required=True, type=Path)
    parser.add_argument("--javadoc", required=True, type=Path)
    parser.add_argument("--classes-dir", type=Path)
    parser.add_argument("--javadoc-dir", type=Path)
    args = parser.parse_args()

    checks = (
        (
            args.main,
            lambda: verify_main_jar(
                args.main, repository_root(), args.classes_dir
            ),
        ),
        (
            args.sources,
            lambda: verify_sources_jar(args.sources, repository_root()),
        ),
        (
            args.javadoc,
            lambda: verify_javadoc_jar(
                args.javadoc, repository_root(), args.javadoc_dir
            ),
        ),
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
