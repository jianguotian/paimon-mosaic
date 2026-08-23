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

"""Admission control shared by the ZIP-based release artifact verifiers.

The wheel and JAR verifiers previously kept private copies of these bounds and
path rules. They drifted twice: the JAR copy accepted drive-relative names and a
normalized '.', and it skipped the directory-payload rule the wheel copy already
enforced, which let a data-carrying 'name/' entry past the JAR payload gate.
"""

from __future__ import annotations

import posixpath
import re
import stat
from zipfile import ZipFile, ZipInfo


MAX_ARCHIVE_ENTRY_SIZE = 256 * 1024 * 1024
MAX_ARCHIVE_TOTAL_SIZE = 1024 * 1024 * 1024
MAX_ARCHIVE_ENTRIES = 65536
WINDOWS_DRIVE_PATH = re.compile(r"^[A-Za-z]:")


def validated_entries(archive: ZipFile, noun: str) -> dict[str, ZipInfo]:
    """Return the archive's entries, rejecting any that are unsafe to trust.

    `noun` names the artifact in error messages ("wheel", "JAR"). Callers add
    their own artifact-specific rules on top; everything here applies to both.
    """
    entries: dict[str, ZipInfo] = {}
    normalized_names: dict[str, str] = {}
    # The per-entry cap does not bound the aggregate, and both verifiers stream
    # every member (through hashlib for RECORD, or to force a CRC check), so
    # bound the total and the entry count too.
    total_size = 0
    infos = archive.infolist()
    if len(infos) > MAX_ARCHIVE_ENTRIES:
        raise ValueError(
            f"{noun} declares more than {MAX_ARCHIVE_ENTRIES} entries: {len(infos)}"
        )
    for info in infos:
        name = info.orig_filename
        if not name or "\x00" in name or name != info.filename:
            raise ValueError(f"invalid {noun} entry path: {name!r}")
        if "\\" in name:
            raise ValueError(f"{noun} entry uses a backslash: {name!r}")
        if name.startswith("/") or WINDOWS_DRIVE_PATH.match(name):
            raise ValueError(
                f"{noun} entry uses an absolute or drive-qualified path: {name!r}"
            )
        if ".." in name.split("/"):
            raise ValueError(f"{noun} entry uses a '..' path component: {name!r}")
        if stat.S_ISLNK(info.external_attr >> 16):
            raise ValueError(f"{noun} entry is a symbolic link: {name!r}")
        # A ZIP directory entry is nothing but a trailing slash, yet it can carry
        # data that ClassLoader.getResourceAsStream and ServiceLoader still read.
        if info.is_dir() and info.file_size != 0:
            raise ValueError(f"{noun} directory entry carries payload: {name!r}")
        if info.file_size > MAX_ARCHIVE_ENTRY_SIZE:
            raise ValueError(
                f"{noun} entry {name!r} exceeds the size limit of "
                f"{MAX_ARCHIVE_ENTRY_SIZE} bytes: {info.file_size} bytes"
            )
        total_size += info.file_size
        if total_size > MAX_ARCHIVE_TOTAL_SIZE:
            raise ValueError(
                f"{noun} exceeds the total size limit of "
                f"{MAX_ARCHIVE_TOTAL_SIZE} bytes"
            )
        if name in entries:
            raise ValueError(f"duplicate {noun} entry path: {name!r}")

        normalized_name = posixpath.normpath(name)
        if normalized_name in ("", ".", "/"):
            raise ValueError(f"invalid {noun} entry path: {name!r}")
        previous_name = normalized_names.get(normalized_name)
        if previous_name is not None:
            raise ValueError(
                f"duplicate normalized {noun} entry path: "
                f"{previous_name!r} and {name!r}"
            )

        entries[name] = info
        normalized_names[normalized_name] = name
    return entries
