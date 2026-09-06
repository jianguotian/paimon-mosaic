#!/usr/bin/env python3

#
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
#

"""Prepare and verify the Maven plugin repository used for Java staging."""

import hashlib
import os
import re
import subprocess
import sys
from pathlib import Path, PurePosixPath


MAVEN_CENTRAL_URL = "https://repo.maven.apache.org/maven2"
LOCK_PATTERN = re.compile(
    r"([0-9a-f]{64}) ([1-9][0-9]*) ([A-Za-z0-9._/-]+)"
)
REQUIRED_ARTIFACTS = {
    "org/apache/maven/plugins/maven-gpg-plugin/3.2.8/"
    "maven-gpg-plugin-3.2.8.jar",
    "org/apache/maven/plugins/maven-gpg-plugin/3.2.8/"
    "maven-gpg-plugin-3.2.8.pom",
    "org/sonatype/plugins/nexus-staging-maven-plugin/1.7.0/"
    "nexus-staging-maven-plugin-1.7.0.jar",
    "org/sonatype/plugins/nexus-staging-maven-plugin/1.7.0/"
    "nexus-staging-maven-plugin-1.7.0.pom",
}


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


def parse_lock(path):
    if path.is_symlink() or not path.is_file():
        fail(f"Pinned Maven plugin lock is missing or unsafe: {path}")
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        fail(f"Unable to read pinned Maven plugin lock: {error}")

    entries = {}
    ordered_paths = []
    for line_number, raw_line in enumerate(lines, 1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        match = LOCK_PATTERN.fullmatch(line)
        if match is None:
            fail(
                f"Invalid pinned Maven plugin lock line {line_number}: "
                f"{raw_line!r}"
            )
        digest, size_text, relative_text = match.groups()
        relative = PurePosixPath(relative_text)
        if (
            relative.is_absolute()
            or ".." in relative.parts
            or relative_text.startswith("/")
            or relative_text.endswith("/")
            or relative.suffix not in (".jar", ".pom")
        ):
            fail(
                f"Unsafe pinned Maven plugin path on line {line_number}: "
                f"{relative_text}"
            )
        if relative_text in entries:
            fail(f"Duplicate pinned Maven plugin path: {relative_text}")
        entries[relative_text] = (digest, int(size_text))
        ordered_paths.append(relative_text)

    if not entries:
        fail("Pinned Maven plugin lock is empty")
    if ordered_paths != sorted(ordered_paths):
        fail("Pinned Maven plugin lock paths must be sorted")
    missing = REQUIRED_ARTIFACTS - set(entries)
    if missing:
        fail(
            "Pinned Maven plugin lock is missing required artifacts: "
            + ", ".join(sorted(missing))
        )
    return entries


def file_digest(path, algorithm):
    digest = hashlib.new(algorithm)
    with path.open("rb") as source:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def metadata_files(entries):
    metadata = {}
    for artifact in ("bcprov-jdk18on", "bcutil-jdk18on"):
        prefix = f"org/bouncycastle/{artifact}/"
        if not any(relative.startswith(prefix) for relative in entries):
            continue
        relative = PurePosixPath(prefix + "maven-metadata.xml")
        contents = (
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            "<metadata>\n"
            "  <groupId>org.bouncycastle</groupId>\n"
            f"  <artifactId>{artifact}</artifactId>\n"
            "  <versioning>\n"
            "    <latest>1.81</latest>\n"
            "    <release>1.81</release>\n"
            "    <versions><version>1.81</version></versions>\n"
            "    <lastUpdated>20260101000000</lastUpdated>\n"
            "  </versioning>\n"
            "</metadata>\n"
        ).encode()
        metadata[relative] = contents
    return metadata


def actual_files(root):
    files = set()
    for path in root.rglob("*"):
        relative = PurePosixPath(path.relative_to(root).as_posix())
        if path.is_symlink():
            fail(
                "Pinned Maven plugin repository contains a symlink: "
                f"{relative}"
            )
        if path.is_dir():
            continue
        if not path.is_file():
            fail(
                "Pinned Maven plugin repository contains a non-regular "
                f"entry: {relative}"
            )
        files.add(relative)
    return files


def verify_locked_artifacts(root, entries):
    for relative_text, (expected_digest, expected_size) in entries.items():
        path = root.joinpath(*PurePosixPath(relative_text).parts)
        if path.stat().st_size != expected_size:
            fail(
                "Pinned Maven plugin artifact size mismatch: "
                f"{relative_text}"
            )
        if file_digest(path, "sha256") != expected_digest:
            fail(
                "Pinned Maven plugin artifact digest mismatch: "
                f"{relative_text}"
            )


def write_exclusive(path, contents):
    try:
        descriptor = os.open(
            str(path),
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o600,
        )
        with os.fdopen(descriptor, "wb") as output:
            output.write(contents)
    except OSError as error:
        fail(f"Unable to write pinned Maven metadata: {error}")


def verify_repository(root, entries, *, prepared):
    if root.is_symlink() or not root.is_dir():
        fail("Pinned Maven plugin repository is missing or unsafe")

    locked_paths = {PurePosixPath(relative) for relative in entries}
    metadata = metadata_files(entries)
    if prepared:
        primary_files = locked_paths | set(metadata)
        expected_files = primary_files | {
            PurePosixPath(f"{relative}.sha1") for relative in primary_files
        }
    else:
        expected_files = locked_paths
    found_files = actual_files(root)
    if found_files != expected_files:
        fail(
            "Pinned Maven plugin repository file set mismatch: "
            f"missing={sorted(str(path) for path in expected_files - found_files)}, "
            f"unexpected={sorted(str(path) for path in found_files - expected_files)}"
        )

    verify_locked_artifacts(root, entries)
    if not prepared:
        return
    for relative, contents in metadata.items():
        path = root.joinpath(*relative.parts)
        if path.read_bytes() != contents:
            fail(f"Pinned Maven plugin metadata changed: {relative}")
    for relative in locked_paths | set(metadata):
        path = root.joinpath(*relative.parts)
        checksum = Path(f"{path}.sha1")
        expected = f"{file_digest(path, 'sha1')}\n".encode()
        if checksum.read_bytes() != expected:
            fail(f"Pinned Maven plugin checksum changed: {checksum}")


def write_download_config(path, root, entries):
    if '"' in str(root) or "\n" in str(root):
        fail(f"Pinned Maven plugin repository path is unsafe: {root}")
    lines = []
    for relative in entries:
        destination = root.joinpath(*PurePosixPath(relative).parts)
        destination.parent.mkdir(parents=True, exist_ok=True)
        lines.extend(
            (
                f'url = "{MAVEN_CENTRAL_URL}/{relative}"',
                f'output = "{destination}"',
            )
        )
    write_exclusive(path, ("\n".join(lines) + "\n").encode())


def prepare(lock, root, curl):
    entries = parse_lock(lock)
    if root.is_symlink() or not root.is_dir() or any(root.iterdir()):
        fail("Pinned Maven plugin repository must start as an empty directory")

    config = root.parent / "maven-plugin-downloads.conf"
    write_download_config(config, root, entries)
    command = [
        curl,
        "--proto",
        "=https",
        "--tlsv1.2",
        "--location",
        "--fail",
        "--silent",
        "--show-error",
        "--retry",
        "3",
        "--retry-connrefused",
        "--connect-timeout",
        "10",
        "--max-time",
        "300",
        "--config",
        str(config),
    ]
    result = subprocess.run(command, check=False)
    if result.returncode != 0:
        raise SystemExit(result.returncode)

    verify_repository(root, entries, prepared=False)
    metadata = metadata_files(entries)
    for relative, contents in metadata.items():
        path = root.joinpath(*relative.parts)
        path.parent.mkdir(parents=True, exist_ok=True)
        write_exclusive(path, contents)
    primary_files = {
        PurePosixPath(relative) for relative in entries
    } | set(metadata)
    for relative in primary_files:
        path = root.joinpath(*relative.parts)
        write_exclusive(
            Path(f"{path}.sha1"),
            f"{file_digest(path, 'sha1')}\n".encode(),
        )
    verify_repository(root, entries, prepared=True)
    print(root.resolve().as_uri())


def main():
    if len(sys.argv) not in (4, 5):
        print(
            "Usage: prepare_java_staging_maven_plugins.py "
            "prepare LOCK REPOSITORY CURL | verify LOCK REPOSITORY",
            file=sys.stderr,
        )
        return 2
    mode = sys.argv[1]
    lock = Path(sys.argv[2])
    root = Path(sys.argv[3])
    if mode == "prepare" and len(sys.argv) == 5:
        prepare(lock, root, sys.argv[4])
        return 0
    if mode == "verify" and len(sys.argv) == 4:
        entries = parse_lock(lock)
        verify_repository(root, entries, prepared=True)
        return 0
    print("Invalid Maven plugin repository command", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
