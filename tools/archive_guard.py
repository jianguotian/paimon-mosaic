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

"""Common admission checks for ZIP-based release artifacts."""

from __future__ import annotations

import posixpath
import re
import stat
from zipfile import ZIP_DEFLATED, ZIP_STORED, ZipFile, ZipInfo


MAX_ENTRY_COUNT = 65_536
MAX_ENTRY_SIZE = 256 * 1024 * 1024
MAX_TOTAL_SIZE = 1024 * 1024 * 1024
READ_CHUNK_SIZE = 1024 * 1024
SUPPORTED_COMPRESSION = frozenset((ZIP_STORED, ZIP_DEFLATED))
WINDOWS_DRIVE = re.compile(r"^[A-Za-z]:")


def _canonical_name(info: ZipInfo, noun: str) -> str:
    name = info.orig_filename
    if not name or "\x00" in name or name != info.filename:
        raise ValueError(f"invalid {noun} entry path: {name!r}")
    if "\\" in name or name.startswith("/") or WINDOWS_DRIVE.match(name):
        raise ValueError(f"unsafe {noun} entry path: {name!r}")

    is_directory = info.is_dir()
    path = name[:-1] if is_directory else name
    components = path.split("/")
    if (
        not path
        or any(component in ("", ".", "..") for component in components)
        or posixpath.normpath(path) != path
    ):
        raise ValueError(f"non-canonical {noun} entry path: {name!r}")
    return name


def validated_entries(archive: ZipFile, noun: str) -> dict[str, ZipInfo]:
    """Validate names, bounds, compression, duplicates, and every member CRC."""
    infos = archive.infolist()
    if len(infos) > MAX_ENTRY_COUNT:
        raise ValueError(
            f"{noun} has {len(infos)} entries; limit is {MAX_ENTRY_COUNT}"
        )

    entries: dict[str, ZipInfo] = {}
    canonical_names: dict[str, str] = {}
    total_size = 0
    for info in infos:
        name = _canonical_name(info, noun)
        if name in entries:
            raise ValueError(f"duplicate {noun} entry path: {name!r}")
        canonical_name = name.rstrip("/")
        previous = canonical_names.get(canonical_name)
        if previous is not None:
            raise ValueError(
                f"duplicate canonical {noun} entry path: "
                f"{previous!r} and {name!r}"
            )
        if info.flag_bits & 0x1:
            raise ValueError(f"encrypted {noun} entry: {name!r}")
        if info.compress_type not in SUPPORTED_COMPRESSION:
            raise ValueError(
                f"unsupported compression method {info.compress_type} "
                f"for {noun} entry {name!r}"
            )
        if stat.S_ISLNK(info.external_attr >> 16):
            raise ValueError(f"symbolic-link {noun} entry: {name!r}")
        if info.is_dir() and info.file_size:
            raise ValueError(f"{noun} directory entry carries data: {name!r}")
        if info.file_size > MAX_ENTRY_SIZE:
            raise ValueError(
                f"{noun} entry {name!r} is {info.file_size} bytes; "
                f"limit is {MAX_ENTRY_SIZE}"
            )
        total_size += info.file_size
        if total_size > MAX_TOTAL_SIZE:
            raise ValueError(
                f"{noun} expands to {total_size} bytes; limit is {MAX_TOTAL_SIZE}"
            )
        entries[name] = info
        canonical_names[canonical_name] = name

    for name, info in entries.items():
        if info.is_dir():
            continue
        size = 0
        with archive.open(info) as source:
            while chunk := source.read(READ_CHUNK_SIZE):
                size += len(chunk)
        if size != info.file_size:
            raise ValueError(
                f"{noun} entry {name!r} read {size} bytes, "
                f"expected {info.file_size}"
            )
    return entries
