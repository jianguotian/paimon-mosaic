#!/usr/bin/env python3

# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing,
# software distributed under the License is distributed on an
# "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
# KIND, either express or implied.  See the License for the
# specific language governing permissions and limitations
# under the License.

"""Create or verify an exact source archive for one Git commit."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import gzip
import io
import os
from pathlib import Path
import posixpath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zlib


EXCLUDED_ROOT_FILES = {
    ".asf.yaml",
    ".gitattributes",
    ".gitignore",
    "deploysettings.xml",
}
EXCLUDED_ROOT_DIRECTORIES = {".github", ".idea", "target"}
EXCLUDED_BASENAMES = {".DS_Store"}
MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_ARCHIVE_ENTRIES = 65536
WINDOWS_DRIVE = re.compile(r"^[A-Za-z]:")
GIT_ENVIRONMENT = (
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_DIR",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_WORK_TREE",
)


@dataclass(frozen=True)
class ArchiveEntry:
    kind: str
    mode: int
    content: bytes | None = None
    link_target: str | None = None


def git_environment() -> dict[str, str]:
    environment = os.environ.copy()
    for variable in GIT_ENVIRONMENT:
        environment.pop(variable, None)
    environment["GIT_ATTR_NOSYSTEM"] = "1"
    environment["GIT_NO_REPLACE_OBJECTS"] = "1"
    return environment


def run_git(
    repository: Path,
    arguments: list[str],
    **kwargs,
) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        env=git_environment(),
        stderr=subprocess.PIPE,
        **kwargs,
    )


def resolve_commit(repository: Path, commit: str) -> str:
    result = run_git(
        repository,
        ["rev-parse", "--verify", f"{commit}^{{commit}}"],
        stdout=subprocess.PIPE,
        text=True,
    )
    return result.stdout.strip()


def validate_prefix(prefix: str) -> str:
    if (
        not prefix.endswith("/")
        or prefix.startswith("/")
        or "\\" in prefix
        or "\x00" in prefix
        or WINDOWS_DRIVE.match(prefix)
    ):
        raise ValueError(f"invalid archive prefix: {prefix!r}")
    root = prefix[:-1]
    if not root or "/" in root or root in {".", ".."}:
        raise ValueError(f"invalid archive prefix: {prefix!r}")
    return root


def is_excluded(path: str) -> bool:
    parts = path.split("/")
    return (
        path in EXCLUDED_ROOT_FILES
        or parts[0] in EXCLUDED_ROOT_DIRECTORIES
        or parts[-1] in EXCLUDED_BASENAMES
        or parts[-1].endswith(".iml")
    )


def read_blobs(repository: Path, object_ids: set[str]) -> dict[str, bytes]:
    if not object_ids:
        return {}
    ordered_ids = sorted(object_ids)
    process = subprocess.Popen(
        ["git", "-C", str(repository), "cat-file", "--batch"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=git_environment(),
    )
    request = b"".join(f"{object_id}\n".encode("ascii") for object_id in ordered_ids)
    stdout, stderr = process.communicate(request)
    if process.returncode != 0:
        raise subprocess.CalledProcessError(
            process.returncode,
            process.args,
            output=stdout,
            stderr=stderr,
        )

    blobs = {}
    offset = 0
    for requested in ordered_ids:
        line_end = stdout.find(b"\n", offset)
        if line_end < 0:
            raise ValueError("git cat-file returned a truncated header")
        header = stdout[offset:line_end].decode("ascii").split()
        if len(header) != 3 or header[0] != requested or header[1] != "blob":
            raise ValueError(
                f"git cat-file returned an unexpected header for {requested}: "
                f"{' '.join(header)!r}"
            )
        size = int(header[2])
        start = line_end + 1
        end = start + size
        if end >= len(stdout) or stdout[end : end + 1] != b"\n":
            raise ValueError(f"git cat-file returned a truncated blob for {requested}")
        blobs[requested] = stdout[start:end]
        offset = end + 1
    if offset != len(stdout):
        raise ValueError("git cat-file returned unexpected trailing output")
    return blobs


def expected_entries(
    repository: Path,
    commit: str,
    prefix: str,
) -> dict[str, ArchiveEntry]:
    root = validate_prefix(prefix)
    result = run_git(
        repository,
        ["ls-tree", "-r", "-l", "-z", "--full-tree", commit],
        stdout=subprocess.PIPE,
    )
    records = []
    object_ids = set()
    total_blob_bytes = 0
    for raw_record in result.stdout.split(b"\0"):
        if not raw_record:
            continue
        metadata, raw_path = raw_record.split(b"\t", 1)
        mode, object_type, object_id, size_text = metadata.decode("ascii").split()
        path = os.fsdecode(raw_path)
        if is_excluded(path):
            continue
        if object_type != "blob" or mode not in {"100644", "100755", "120000"}:
            raise ValueError(
                f"unsupported Git tree entry {path!r}: mode={mode}, type={object_type}"
            )
        total_blob_bytes += int(size_text)
        if total_blob_bytes > MAX_ARCHIVE_BYTES:
            raise ValueError("Git source tree exceeds the size limit")
        records.append((mode, object_id, path))
        if len(records) > MAX_ARCHIVE_ENTRIES:
            raise ValueError(
                f"Git source tree exceeds the {MAX_ARCHIVE_ENTRIES} entry limit"
            )
        object_ids.add(object_id)

    blobs = read_blobs(repository, object_ids)
    entries = {root: ArchiveEntry("directory", 0o775)}
    directories = set()
    for mode, object_id, path in records:
        parent = posixpath.dirname(path)
        while parent:
            directories.add(parent)
            parent = posixpath.dirname(parent)
        archive_path = prefix + path
        blob = blobs[object_id]
        if mode == "120000":
            target = blob.decode("utf-8", errors="surrogateescape")
            entries[archive_path] = ArchiveEntry(
                "symlink",
                0o777,
                link_target=target,
            )
        else:
            entries[archive_path] = ArchiveEntry(
                "file",
                0o775 if mode == "100755" else 0o664,
                content=blob,
            )
    for directory in directories:
        entries[prefix + directory] = ArchiveEntry("directory", 0o775)
    return entries


def validate_member_path(name: str, root: str, kind: str) -> str:
    if (
        not name
        or name.startswith("/")
        or "\\" in name
        or "\x00" in name
        or WINDOWS_DRIVE.match(name)
    ):
        raise ValueError(f"unsafe archive entry path: {name!r}")
    parts = name.split("/")
    if "" in parts or "." in parts or ".." in parts or posixpath.normpath(name) != name:
        raise ValueError(f"unsafe archive entry path: {name!r}")
    if name == root:
        if kind != "directory":
            raise ValueError("archive root must be a directory")
    elif not name.startswith(root + "/"):
        raise ValueError(f"archive entry is outside the single root directory: {name!r}")
    return name


def validate_link_target(path: str, target: str, root: str) -> str:
    if (
        not target
        or target.startswith("/")
        or "\\" in target
        or "\x00" in target
        or WINDOWS_DRIVE.match(target)
    ):
        raise ValueError(f"unsafe symbolic-link target for {path!r}: {target!r}")
    resolved = posixpath.normpath(posixpath.join(posixpath.dirname(path), target))
    if resolved != root and not resolved.startswith(root + "/"):
        raise ValueError(
            f"symbolic link {path!r} escapes the archive root: {target!r}"
        )
    return target


def archive_entries(
    archive: tarfile.TarFile,
    prefix: str,
) -> dict[str, ArchiveEntry]:
    root = validate_prefix(prefix)
    entries = {}
    total_file_bytes = 0
    for count, member in enumerate(archive, 1):
        if count > MAX_ARCHIVE_ENTRIES:
            raise ValueError(
                f"source archive exceeds the {MAX_ARCHIVE_ENTRIES} entry limit"
            )
        if member.isfile():
            kind = "file"
        elif member.isdir():
            kind = "directory"
        elif member.issym():
            kind = "symlink"
        else:
            raise ValueError(
                f"unsupported archive entry type for {member.name!r}"
            )
        path = validate_member_path(member.name, root, kind)
        if path in entries:
            raise ValueError(f"duplicate archive entry path: {path!r}")

        content = None
        target = None
        if kind == "file":
            if member.size < 0:
                raise ValueError(f"negative file size for {path!r}")
            total_file_bytes += member.size
            if total_file_bytes > MAX_ARCHIVE_BYTES:
                raise ValueError("source archive file contents exceed the size limit")
            extracted = archive.extractfile(member)
            if extracted is None:
                raise ValueError(f"cannot read archive file {path!r}")
            content = extracted.read()
            if len(content) != member.size:
                raise ValueError(f"archive file size differs for {path!r}")
        elif kind == "symlink":
            target = validate_link_target(path, member.linkname, root)

        entries[path] = ArchiveEntry(
            kind=kind,
            mode=member.mode & 0o7777,
            content=content,
            link_target=target,
        )
    if root not in entries:
        raise ValueError(f"source archive has no root directory {root!r}")
    return entries


def read_source_archive(
    path: Path,
    prefix: str,
) -> tuple[dict[str, ArchiveEntry], str | None]:
    if path.stat().st_size > MAX_ARCHIVE_BYTES:
        raise ValueError("compressed source archive exceeds the size limit")
    compressed = path.read_bytes()
    try:
        decompressor = zlib.decompressobj(16 + zlib.MAX_WBITS)
        raw_tar = decompressor.decompress(compressed, MAX_ARCHIVE_BYTES + 1)
        if len(raw_tar) > MAX_ARCHIVE_BYTES or decompressor.unconsumed_tail:
            raise ValueError("uncompressed source archive exceeds the size limit")
        raw_tar += decompressor.flush()
        if len(raw_tar) > MAX_ARCHIVE_BYTES:
            raise ValueError("uncompressed source archive exceeds the size limit")
        if not decompressor.eof:
            raise ValueError("gzip source archive ended before its trailer")
        if decompressor.unused_data or decompressor.unconsumed_tail:
            raise ValueError("source archive contains trailing gzip data")

        with tarfile.open(fileobj=io.BytesIO(raw_tar), mode="r:") as archive:
            embedded_commit = archive.pax_headers.get("comment")
            entries = archive_entries(archive, prefix)
            trailer = raw_tar[archive.offset :]
        if len(trailer) < 1024 or len(trailer) % 512 != 0 or any(trailer):
            raise ValueError("source archive contains trailing tar data")
        return entries, embedded_commit
    except (tarfile.TarError, EOFError, zlib.error) as error:
        raise ValueError(f"cannot read source archive {path}: {error}") from error


def compare_entries(
    actual: dict[str, ArchiveEntry],
    expected: dict[str, ArchiveEntry],
) -> None:
    actual_paths = set(actual)
    expected_paths = set(expected)
    missing = sorted(expected_paths - actual_paths)
    unexpected = sorted(actual_paths - expected_paths)
    if missing:
        raise ValueError(f"source archive is missing entries: {missing}")
    if unexpected:
        raise ValueError(f"source archive has unexpected entries: {unexpected}")
    for path in sorted(expected_paths):
        found = actual[path]
        wanted = expected[path]
        if found.kind != wanted.kind:
            raise ValueError(f"entry type differs for {path!r}")
        if found.mode != wanted.mode:
            raise ValueError(f"entry mode differs for {path!r}")
        if found.link_target != wanted.link_target:
            raise ValueError(f"symbolic-link target differs for {path!r}")
        if found.content != wanted.content:
            raise ValueError(f"file content differs for {path!r}")


def verify_archive(
    archive: Path,
    repository: Path,
    commit: str,
    prefix: str,
) -> str:
    repository = repository.resolve()
    resolved = resolve_commit(repository, commit)
    actual, embedded_commit = read_source_archive(archive, prefix)
    if embedded_commit != resolved:
        raise ValueError(
            f"embedded Git commit differs: found {embedded_commit!r}, "
            f"expected {resolved!r}"
        )
    compare_entries(actual, expected_entries(repository, resolved, prefix))
    return resolved


def reject_local_archive_attributes(repository: Path) -> None:
    result = run_git(
        repository,
        ["rev-parse", "--git-path", "info/attributes"],
        stdout=subprocess.PIPE,
        text=True,
    )
    path = Path(result.stdout.strip())
    if not path.is_absolute():
        path = repository / path
    if path.exists() and path.read_bytes().strip():
        raise ValueError(
            f"repository-local Git attributes can change source output: {path}"
        )


def create_archive(
    output: Path,
    repository: Path,
    commit: str,
    prefix: str,
) -> str:
    repository = repository.resolve()
    validate_prefix(prefix)
    resolved = resolve_commit(repository, commit)
    reject_local_archive_attributes(repository)
    if output.exists() or output.is_symlink():
        raise ValueError(f"output already exists: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)

    temporary_output = None
    try:
        with tempfile.TemporaryDirectory(
            prefix="source-archive-",
            dir=output.parent,
        ) as directory:
            raw_tar = Path(directory) / "source.tar"
            with raw_tar.open("wb") as destination:
                run_git(
                    repository,
                    [
                        "-c",
                        "tar.umask=0002",
                        "-c",
                        "core.attributesFile=/dev/null",
                        "-c",
                        "core.autocrlf=false",
                        "-c",
                        "core.eol=lf",
                        "archive",
                        "--format=tar",
                        f"--prefix={prefix}",
                        resolved,
                        "--",
                        ".",
                        ":(exclude).gitignore",
                        ":(exclude).gitattributes",
                        ":(exclude).asf.yaml",
                        ":(exclude).github",
                        ":(exclude)deploysettings.xml",
                        ":(exclude)target",
                        ":(exclude).idea",
                        ":(exclude,glob)**/*.iml",
                        ":(exclude,glob)**/.DS_Store",
                    ],
                    stdout=destination,
                )
            with tempfile.NamedTemporaryFile(
                mode="wb",
                dir=output.parent,
                prefix=output.name + ".",
                suffix=".tmp",
                delete=False,
            ) as destination:
                temporary_output = Path(destination.name)
                with raw_tar.open("rb") as source, gzip.GzipFile(
                    filename="",
                    mode="wb",
                    fileobj=destination,
                    mtime=0,
                ) as compressed:
                    shutil.copyfileobj(source, compressed)

        assert temporary_output is not None
        verify_archive(temporary_output, repository, resolved, prefix)
        os.chmod(temporary_output, 0o644)
        os.replace(temporary_output, output)
        temporary_output = None
        return resolved
    finally:
        if temporary_output is not None:
            temporary_output.unlink(missing_ok=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    def common(command: argparse.ArgumentParser) -> None:
        command.add_argument("--repository", type=Path, default=Path("."))
        command.add_argument("--commit", required=True)
        command.add_argument("--prefix", required=True)

    create = commands.add_parser("create")
    common(create)
    create.add_argument("--output", required=True, type=Path)

    verify = commands.add_parser("verify")
    common(verify)
    verify.add_argument("--archive", required=True, type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "create":
            commit = create_archive(
                args.output,
                args.repository,
                args.commit,
                args.prefix,
            )
            print(f"created {args.output} from Git commit {commit}")
        else:
            commit = verify_archive(
                args.archive,
                args.repository,
                args.commit,
                args.prefix,
            )
            print(f"verified {args.archive} against Git commit {commit}")
        return 0
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        detail = error.stderr if isinstance(error, subprocess.CalledProcessError) else None
        if isinstance(detail, bytes):
            detail = detail.decode(errors="replace")
        print(
            f"source archive verification failed: {detail or error}",
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    sys.exit(main())
