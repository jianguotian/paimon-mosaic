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

import struct
from pathlib import Path
import stat
import sys
import warnings
from zipfile import ZIP_BZIP2, ZIP_STORED, BadZipFile, ZipFile, ZipInfo

import pytest


TOOLS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(TOOLS))

import archive_guard  # noqa: E402
import verify_java_jars as verifier  # noqa: E402


POM = b"""\
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.apache.paimon</groupId>
  <artifactId>mosaic</artifactId>
  <version>0.3.0-SNAPSHOT</version>
</project>
"""

PE_FIXTURE = bytearray(132)
PE_FIXTURE[:2] = b"MZ"
PE_FIXTURE[0x3C:0x40] = (0x80).to_bytes(4, "little")
PE_FIXTURE[0x80:0x84] = b"PE\0\0"
NATIVE_FIXTURES = {
    "native/linux/x86_64/libpaimon_mosaic_jni.so": b"\x7fELF-x86_64",
    "native/linux/aarch64/libpaimon_mosaic_jni.so": b"\x7fELF-aarch64",
    "native/macos/aarch64/libpaimon_mosaic_jni.dylib": b"\xcf\xfa\xed\xfe-macos",
    "native/windows/x86_64/paimon_mosaic_jni.dll": bytes(PE_FIXTURE),
}


def write_zip(path, entries, compression=ZIP_STORED):
    with ZipFile(path, "w", compression=compression) as archive:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            for name, contents in entries:
                archive.writestr(name, contents)


def file_entries(root):
    return {
        path.relative_to(root).as_posix(): path.read_bytes()
        for path in root.rglob("*")
        if path.is_file()
    }


@pytest.fixture
def java_artifacts(tmp_path):
    root = tmp_path
    (root / "LICENSE").write_bytes(b"repository license\n")
    (root / "NOTICE").write_bytes(b"repository notice\n")
    pom_path = root / "java/pom.xml"
    pom_path.parent.mkdir(parents=True)
    pom_path.write_bytes(POM)

    source = root / "java/src/main/java/org/example/Example.java"
    source.parent.mkdir(parents=True)
    source.write_bytes(b"package org.example;\npublic class Example {}\n")

    binary = root / "java/src/main/binary-resources/META-INF"
    binary.mkdir(parents=True)
    (binary / "LICENSE").write_bytes(b"binary license\n")
    (binary / "NOTICE").write_bytes(b"binary notice with Apache Arrow\n")

    shared_dependencies = (
        root
        / "java/target/maven-shared-archive-resources/META-INF/DEPENDENCIES"
    )
    shared_dependencies.parent.mkdir(parents=True)
    shared_dependencies.write_bytes(b"generated dependencies\n")

    classes = root / "java/target/classes"
    class_payloads = {
        "org/example/Example.class": b"\xca\xfe\xba\xbeouter",
        "org/example/Example$Nested.class": b"\xca\xfe\xba\xbenested",
        "META-INF/DEPENDENCIES": shared_dependencies.read_bytes(),
        "META-INF/LICENSE": (binary / "LICENSE").read_bytes(),
        "META-INF/NOTICE": (binary / "NOTICE").read_bytes(),
    }
    for target in verifier.TARGETS:
        report = binary / f"licenses/{target}/THIRD-PARTY-LICENSES.html"
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_bytes(f"{target} report\n".encode())
        class_payloads[
            f"META-INF/licenses/{target}/THIRD-PARTY-LICENSES.html"
        ] = report.read_bytes()
    for name, contents in NATIVE_FIXTURES.items():
        class_payloads[name] = contents
    for name, contents in class_payloads.items():
        path = classes / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)

    apidocs = root / "java/target/apidocs"
    for name, contents in {
        "index.html": b"<html>index</html>\n",
        "org/example/Example.html": b"<html>Example</html>\n",
    }.items():
        path = apidocs / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)

    coordinates = "META-INF/maven/org.apache.paimon/mosaic"
    properties = (
        b"artifactId=mosaic\n"
        b"groupId=org.apache.paimon\n"
        b"version=0.3.0-SNAPSHOT\n"
    )
    main = root / "java/target/mosaic.jar"
    sources = root / "java/target/mosaic-sources.jar"
    javadoc = root / "java/target/mosaic-javadoc.jar"

    def build_main(overrides=None, omitted=()):
        entries = file_entries(classes)
        entries.update(
            {
                "META-INF/MANIFEST.MF": b"Manifest-Version: 1.0\n",
                f"{coordinates}/pom.xml": POM,
                f"{coordinates}/pom.properties": properties,
            }
        )
        entries.update(overrides or {})
        write_zip(main, [(name, data) for name, data in entries.items() if name not in omitted])

    def build_sources(overrides=None, omitted=()):
        entries = {
            "META-INF/MANIFEST.MF": b"Manifest-Version: 1.0\n",
            "META-INF/DEPENDENCIES": shared_dependencies.read_bytes(),
            "META-INF/LICENSE": (root / "LICENSE").read_bytes(),
            "META-INF/NOTICE": (root / "NOTICE").read_bytes(),
            "org/example/Example.java": source.read_bytes(),
            f"{coordinates}/pom.xml": POM,
            f"{coordinates}/pom.properties": properties,
        }
        entries.update(overrides or {})
        write_zip(
            sources,
            [(name, data) for name, data in entries.items() if name not in omitted],
        )

    def build_javadoc(overrides=None, omitted=()):
        entries = {
            "META-INF/MANIFEST.MF": b"Manifest-Version: 1.0\n",
            "META-INF/DEPENDENCIES": shared_dependencies.read_bytes(),
            "META-INF/LICENSE": (root / "LICENSE").read_bytes(),
            "META-INF/NOTICE": (root / "NOTICE").read_bytes(),
            **file_entries(apidocs),
        }
        entries.update(overrides or {})
        write_zip(
            javadoc,
            [(name, data) for name, data in entries.items() if name not in omitted],
        )

    build_main()
    build_sources()
    build_javadoc()
    return {
        "root": root,
        "classes": classes,
        "apidocs": apidocs,
        "main": main,
        "sources": sources,
        "javadoc": javadoc,
        "build_main": build_main,
        "build_sources": build_sources,
        "build_javadoc": build_javadoc,
        "coordinates": coordinates,
        "properties": properties,
        "shared_dependencies": shared_dependencies,
    }


@pytest.mark.parametrize("entry", ["../escape", "/absolute", "C:relative", "a//b"])
def test_archive_guard_rejects_unsafe_paths(tmp_path, entry):
    archive_path = tmp_path / "unsafe.jar"
    with ZipFile(archive_path, "w") as archive:
        archive.writestr(entry, b"payload")

    with ZipFile(archive_path) as archive:
        with pytest.raises(ValueError, match="entry path"):
            archive_guard.validated_entries(archive, "JAR")


def test_archive_guard_rejects_duplicate_names(tmp_path):
    archive_path = tmp_path / "duplicate.jar"
    write_zip(archive_path, [("same", b"one"), ("same", b"two")])

    with ZipFile(archive_path) as archive:
        with pytest.raises(ValueError, match="duplicate"):
            archive_guard.validated_entries(archive, "JAR")


def test_archive_guard_rejects_file_directory_name_collision(tmp_path):
    archive_path = tmp_path / "collision.jar"
    write_zip(archive_path, [("same", b"file"), ("same/", b"")])

    with ZipFile(archive_path) as archive:
        with pytest.raises(ValueError, match="duplicate canonical"):
            archive_guard.validated_entries(archive, "JAR")


def test_archive_guard_rejects_symbolic_links(tmp_path):
    archive_path = tmp_path / "symlink.jar"
    link = ZipInfo("link")
    link.external_attr = (stat.S_IFLNK | 0o777) << 16
    write_zip(archive_path, [(link, b"target")])

    with ZipFile(archive_path) as archive:
        with pytest.raises(ValueError, match="symbolic-link"):
            archive_guard.validated_entries(archive, "JAR")


def test_archive_guard_rejects_directory_payload(tmp_path):
    archive_path = tmp_path / "directory-data.jar"
    write_zip(archive_path, [("directory/", b"payload")])

    with ZipFile(archive_path) as archive:
        with pytest.raises(ValueError, match="directory entry carries data"):
            archive_guard.validated_entries(archive, "JAR")


@pytest.mark.parametrize(
    ("limit_name", "limit", "entries", "message"),
    [
        ("MAX_ENTRY_SIZE", 3, [("large", b"1234")], "entry"),
        (
            "MAX_TOTAL_SIZE",
            5,
            [("first", b"123"), ("second", b"456")],
            "expands",
        ),
        (
            "MAX_ENTRY_COUNT",
            1,
            [("first", b"1"), ("second", b"2")],
            "entries",
        ),
    ],
)
def test_archive_guard_enforces_bounds(
    tmp_path, monkeypatch, limit_name, limit, entries, message
):
    archive_path = tmp_path / "bounded.jar"
    write_zip(archive_path, entries)
    monkeypatch.setattr(archive_guard, limit_name, limit)

    with ZipFile(archive_path) as archive:
        with pytest.raises(ValueError, match=message):
            archive_guard.validated_entries(archive, "JAR")


def test_archive_guard_rejects_unsupported_compression(tmp_path):
    archive_path = tmp_path / "bzip.jar"
    write_zip(archive_path, [("payload", b"content")], compression=ZIP_BZIP2)

    with ZipFile(archive_path) as archive:
        with pytest.raises(ValueError, match="unsupported compression"):
            archive_guard.validated_entries(archive, "JAR")


def test_archive_guard_reads_to_eof_for_crc(tmp_path):
    archive_path = tmp_path / "damaged.jar"
    write_zip(archive_path, [("payload", b"content")])
    with ZipFile(archive_path) as archive:
        info = archive.getinfo("payload")
        offset = info.header_offset
    data = bytearray(archive_path.read_bytes())
    name_length, extra_length = struct.unpack_from("<HH", data, offset + 26)
    payload_offset = offset + 30 + name_length + extra_length
    data[payload_offset] ^= 0x01
    archive_path.write_bytes(data)

    with ZipFile(archive_path) as archive:
        with pytest.raises(BadZipFile, match="CRC"):
            archive_guard.validated_entries(archive, "JAR")


def test_verifies_realistic_main_sources_and_javadoc(java_artifacts):
    verifier.verify_main_jar(
        java_artifacts["main"],
        java_artifacts["root"],
        java_artifacts["classes"],
    )
    verifier.verify_sources_jar(
        java_artifacts["sources"], java_artifacts["root"]
    )
    verifier.verify_javadoc_jar(
        java_artifacts["javadoc"],
        java_artifacts["root"],
        java_artifacts["apidocs"],
    )


@pytest.mark.parametrize(
    "removed_class",
    ["org/example/Example.class", "org/example/Example$Nested.class"],
)
def test_main_jar_requires_every_target_class(java_artifacts, removed_class):
    java_artifacts["build_main"](omitted={removed_class})

    with pytest.raises(ValueError, match="class entries differ"):
        verifier.verify_main_jar(
            java_artifacts["main"],
            java_artifacts["root"],
            java_artifacts["classes"],
        )


def test_main_jar_requires_target_class_bytes(java_artifacts):
    java_artifacts["build_main"](
        {"org/example/Example$Nested.class": b"\xca\xfe\xba\xbetampered"}
    )

    with pytest.raises(ValueError, match="differs from target/classes"):
        verifier.verify_main_jar(
            java_artifacts["main"],
            java_artifacts["root"],
            java_artifacts["classes"],
        )


@pytest.mark.parametrize(
    ("entry_suffix", "contents", "message"),
    [
        ("pom.xml", b"<project/>", "pom.xml"),
        (
            "pom.properties",
            b"artifactId=mosaic\ngroupId=org.apache.paimon\nversion=9.9.9\n",
            "pom.properties",
        ),
    ],
)
def test_main_jar_rejects_forged_maven_metadata(
    java_artifacts, entry_suffix, contents, message
):
    entry = f"{java_artifacts['coordinates']}/{entry_suffix}"
    java_artifacts["build_main"]({entry: contents})

    with pytest.raises(ValueError, match=message):
        verifier.verify_main_jar(
            java_artifacts["main"],
            java_artifacts["root"],
            java_artifacts["classes"],
        )


def test_main_jar_rejects_wrong_native_magic(java_artifacts):
    native_path = next(iter(verifier.NATIVE_ENTRIES))
    (java_artifacts["classes"] / native_path).write_bytes(b"not native")
    java_artifacts["build_main"]()

    with pytest.raises(ValueError, match="native magic"):
        verifier.verify_main_jar(
            java_artifacts["main"],
            java_artifacts["root"],
            java_artifacts["classes"],
        )


def test_main_jar_requires_exact_native_paths(java_artifacts):
    extra = java_artifacts["classes"] / "native/linux/x86_64/extra.so"
    extra.write_bytes(b"\x7fELFextra")
    java_artifacts["build_main"]()

    with pytest.raises(ValueError, match="native entries"):
        verifier.verify_main_jar(
            java_artifacts["main"],
            java_artifacts["root"],
            java_artifacts["classes"],
        )


def test_main_jar_legal_files_match_binary_resources(java_artifacts):
    (java_artifacts["classes"] / "META-INF/NOTICE").write_bytes(b"forged")
    java_artifacts["build_main"]()

    with pytest.raises(ValueError, match="legal file"):
        verifier.verify_main_jar(
            java_artifacts["main"],
            java_artifacts["root"],
            java_artifacts["classes"],
        )


def test_sources_jar_matches_repository_sources(java_artifacts):
    java_artifacts["build_sources"](
        {"org/example/Example.java": b"public class Forged {}"}
    )

    with pytest.raises(ValueError, match="source"):
        verifier.verify_sources_jar(
            java_artifacts["sources"], java_artifacts["root"]
        )


@pytest.mark.parametrize("classifier", ["sources", "javadoc"])
def test_classifier_dependencies_match_maven_shared_resource(
    java_artifacts, classifier
):
    java_artifacts[f"build_{classifier}"](
        {"META-INF/DEPENDENCIES": b"tampered dependencies\n"}
    )

    verify = getattr(verifier, f"verify_{classifier}_jar")
    arguments = [java_artifacts[classifier], java_artifacts["root"]]
    if classifier == "javadoc":
        arguments.append(java_artifacts["apidocs"])
    with pytest.raises(ValueError, match="DEPENDENCIES"):
        verify(*arguments)


def test_javadoc_jar_matches_generated_output(java_artifacts):
    java_artifacts["build_javadoc"](omitted={"org/example/Example.html"})

    with pytest.raises(ValueError, match="Javadoc payload"):
        verifier.verify_javadoc_jar(
            java_artifacts["javadoc"],
            java_artifacts["root"],
            java_artifacts["apidocs"],
        )
