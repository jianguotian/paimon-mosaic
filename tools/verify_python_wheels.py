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

"""Verify assembled paimon-mosaic native wheels."""

from __future__ import annotations

import argparse
import base64
import csv
import email
import hashlib
import hmac
import io
import re
import sys
import tomllib
import zlib
from pathlib import Path
from zipfile import BadZipFile, ZipFile, ZipInfo

import archive_guard


TARGETS = {
    "x86_64-unknown-linux-gnu": {
        "tag": "py3-none-manylinux_2_28_x86_64",
        "native": "mosaic/libpaimon_mosaic_ffi.so",
        "kind": "ELF",
    },
    "aarch64-unknown-linux-gnu": {
        "tag": "py3-none-manylinux_2_28_aarch64",
        "native": "mosaic/libpaimon_mosaic_ffi.so",
        "kind": "ELF",
    },
    "aarch64-apple-darwin": {
        "tag": "py3-none-macosx_11_0_arm64",
        "native": "mosaic/libpaimon_mosaic_ffi.dylib",
        "kind": "Mach-O",
    },
    "x86_64-pc-windows-msvc": {
        "tag": "py3-none-win_amd64",
        "native": "mosaic/paimon_mosaic_ffi.dll",
        "kind": "PE",
    },
}
NATIVE_SUFFIXES = (".so", ".dylib", ".dll")
METADATA_VERSION = "2.4"
DYNAMIC_METADATA = ["license-file"]
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


def repository_root() -> Path:
    return Path(__file__).resolve().parent.parent


def normalized_distribution(value: str) -> str:
    return re.sub(r"[-_.]+", "-", value).lower()


def parse_wheel_filename(name: str) -> tuple[str, str, str]:
    if not name.endswith(".whl"):
        raise ValueError(f"not a wheel filename: {name}")
    parts = name[:-4].split("-")
    if len(parts) != 5 or any(not part for part in parts):
        raise ValueError(f"invalid wheel filename: {name}")
    distribution, version, python_tag, abi_tag, platform_tag = parts
    return distribution, version, f"{python_tag}-{abi_tag}-{platform_tag}"


def target_for_tag(tag: str) -> str:
    targets = [target for target, values in TARGETS.items() if values["tag"] == tag]
    if len(targets) != 1:
        raise ValueError(
            f"unsupported wheel tag {tag!r}; expected one of "
            f"{sorted(values['tag'] for values in TARGETS.values())}"
        )
    return targets[0]


def python_modules(root: Path) -> dict[str, Path]:
    package = root / "python/mosaic"
    modules = {
        path.relative_to(root / "python").as_posix(): path
        for path in package.rglob("*.py")
        if path.is_file()
    }
    if not modules:
        raise ValueError("repository Python package contains no modules")
    return modules


def _project(root: Path) -> dict:
    with (root / "python/pyproject.toml").open("rb") as source:
        return tomllib.load(source)["project"]


def _normalize_requirement(value: str) -> str:
    requirement, separator, marker = value.partition(";")
    normalized = " ".join(requirement.split())
    if separator:
        normalized += "; " + " ".join(marker.split()).replace("'", '"')
    return normalized


def _expected_metadata(root: Path) -> tuple[str, list[str], list[str]]:
    project = _project(root)
    extras = sorted(project.get("optional-dependencies", {}))
    requirements = [
        _normalize_requirement(requirement)
        for requirement in project.get("dependencies", [])
    ]
    requirements.extend(
        _normalize_requirement(f'{requirement}; extra == "{extra}"')
        for extra in extras
        for requirement in project["optional-dependencies"][extra]
    )
    return project["requires-python"], extras, sorted(requirements)


def _single_header(message, name: str) -> str:
    values = message.get_all(name, [])
    if len(values) != 1:
        raise ValueError(f"expected one {name} field, found {values}")
    return values[0]


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


def _verify_record(
    archive: ZipFile,
    file_names: set[str],
    record_path: str,
) -> None:
    try:
        text = archive.read(record_path).decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValueError(f"{record_path} is not UTF-8") from error

    rows = {}
    for line_number, row in enumerate(csv.reader(io.StringIO(text, newline="")), 1):
        if len(row) != 3:
            raise ValueError(
                f"{record_path}:{line_number} must have three fields"
            )
        name, hash_field, size_field = row
        if name in rows:
            raise ValueError(f"{record_path} lists {name!r} more than once")
        rows[name] = (hash_field, size_field)

    listed = set(rows)
    if listed != file_names:
        raise ValueError(
            f"{record_path} omits wheel entries {sorted(file_names - listed)} "
            f"or lists missing entries {sorted(listed - file_names)}"
        )
    if rows.get(record_path) != ("", ""):
        raise ValueError(f"{record_path} must list itself with blank hash and size")

    for name in sorted(file_names - {record_path}):
        hash_field, size_field = rows[name]
        if not hash_field.startswith("sha256="):
            raise ValueError(f"{record_path} has invalid hash for {name}")
        try:
            expected_size = int(size_field)
        except ValueError as error:
            raise ValueError(f"{record_path} has invalid size for {name}") from error
        contents = archive.read(name)
        digest = hashlib.sha256(contents).digest()
        actual_hash = base64.urlsafe_b64encode(digest).rstrip(b"=").decode()
        expected_hash = hash_field.removeprefix("sha256=").rstrip("=")
        if not hmac.compare_digest(actual_hash, expected_hash):
            raise ValueError(f"{record_path} hash mismatch for {name}")
        if len(contents) != expected_size:
            raise ValueError(
                f"{record_path} size mismatch for {name}: "
                f"found {len(contents)}, expected {expected_size}"
            )


def _archive_files(entries: dict[str, ZipInfo]) -> set[str]:
    return {name for name, info in entries.items() if not info.is_dir()}


def verify_wheel(wheel: Path, root: Path | None = None) -> str:
    root = root or repository_root()
    if not wheel.is_file():
        raise ValueError(f"wheel is not a regular file: {wheel}")
    filename_distribution, filename_version, filename_tag = parse_wheel_filename(
        wheel.name
    )
    target = target_for_tag(filename_tag)
    project = _project(root)
    if normalized_distribution(filename_distribution) != normalized_distribution(
        project["name"]
    ):
        raise ValueError(
            f"wheel distribution {filename_distribution!r} does not match "
            f"{project['name']!r}"
        )

    with ZipFile(wheel) as archive:
        entries = archive_guard.validated_entries(archive, "wheel")
        file_names = _archive_files(entries)
        dist_infos = sorted(
            {
                name.split("/", 1)[0]
                for name in file_names
                if "/" in name and name.split("/", 1)[0].endswith(".dist-info")
            }
        )
        expected_dist_info = (
            f"{filename_distribution}-{filename_version}.dist-info"
        )
        if dist_infos != [expected_dist_info]:
            raise ValueError(
                f"wheel dist-info directories are {dist_infos}, "
                f"expected {[expected_dist_info]}"
            )
        dist_info = dist_infos[0]
        metadata_path = f"{dist_info}/METADATA"
        wheel_path = f"{dist_info}/WHEEL"
        record_path = f"{dist_info}/RECORD"
        for required in (metadata_path, wheel_path, record_path):
            if required not in file_names:
                raise ValueError(f"missing wheel metadata {required}")
        _verify_record(archive, file_names, record_path)

        metadata = email.message_from_bytes(archive.read(metadata_path))
        metadata_version_contract = _single_header(
            metadata, "Metadata-Version"
        )
        if metadata_version_contract != METADATA_VERSION:
            raise ValueError(
                f"METADATA Metadata-Version is {metadata_version_contract!r}, "
                f"expected {METADATA_VERSION!r}"
            )
        metadata_name = _single_header(metadata, "Name")
        if normalized_distribution(metadata_name) != normalized_distribution(
            filename_distribution
        ):
            raise ValueError(
                f"METADATA Name {metadata_name!r} does not match filename"
            )
        metadata_version = _single_header(metadata, "Version")
        if metadata_version != filename_version:
            raise ValueError(
                f"METADATA Version {metadata_version!r} does not match "
                f"{filename_version!r}"
            )
        summary = _single_header(metadata, "Summary")
        if summary != project["description"]:
            raise ValueError(
                f"METADATA Summary is {summary!r}, "
                f"expected {project['description']!r}"
            )
        license_expression = _single_header(metadata, "License-Expression")
        if license_expression != project["license"]:
            raise ValueError(
                f"METADATA License-Expression is {license_expression!r}, "
                f"expected {project['license']!r}"
            )
        expected_license_files = [
            f"licenses/{target}/{name}"
            for name in ("LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.html")
        ]
        license_files = metadata.get_all("License-File", [])
        if license_files != expected_license_files:
            raise ValueError(
                f"METADATA License-File is {license_files}, "
                f"expected {expected_license_files}"
            )
        dynamic = metadata.get_all("Dynamic", [])
        if dynamic != DYNAMIC_METADATA:
            raise ValueError(
                f"METADATA Dynamic is {dynamic}, expected {DYNAMIC_METADATA}"
            )
        expected_python, expected_extras, expected_requirements = _expected_metadata(
            root
        )
        if _single_header(metadata, "Requires-Python") != expected_python:
            raise ValueError("METADATA Requires-Python differs from pyproject.toml")
        extras = sorted(metadata.get_all("Provides-Extra", []))
        if extras != expected_extras:
            raise ValueError(
                f"METADATA Provides-Extra is {extras}, expected {expected_extras}"
            )
        requirements = sorted(
            _normalize_requirement(value)
            for value in metadata.get_all("Requires-Dist", [])
        )
        if requirements != expected_requirements:
            raise ValueError(
                f"METADATA Requires-Dist is {requirements}, "
                f"expected {expected_requirements}"
            )

        wheel_metadata = email.message_from_bytes(archive.read(wheel_path))
        if wheel_metadata.get_all("Wheel-Version", []) != ["1.0"]:
            raise ValueError(
                "WHEEL must contain exactly one Wheel-Version: 1.0 field"
            )
        if wheel_metadata.get_all("Root-Is-Purelib", []) != ["false"]:
            raise ValueError("WHEEL must contain Root-Is-Purelib: false")
        if wheel_metadata.get_all("Tag", []) != [filename_tag]:
            raise ValueError(
                f"WHEEL tags {wheel_metadata.get_all('Tag', [])} "
                f"do not match filename tag {filename_tag}"
            )

        modules = python_modules(root)
        packaged_modules = {
            name
            for name in file_names
            if name.startswith("mosaic/") and name.endswith(".py")
        }
        if packaged_modules != set(modules):
            raise ValueError(
                "wheel Python modules differ from repository: "
                f"missing {sorted(set(modules) - packaged_modules)}, "
                f"unexpected {sorted(packaged_modules - set(modules))}"
            )
        for name, source in modules.items():
            if archive.read(entries[name]) != source.read_bytes():
                raise ValueError(
                    f"wheel Python module {name} differs from repository"
                )

        target_legal = root / "python/licenses" / target
        legal = {}
        for legal_name in ("LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.html"):
            source = target_legal / legal_name
            legal[f"mosaic/{legal_name}"] = source
            legal[
                f"{dist_info}/licenses/licenses/{target}/{legal_name}"
            ] = source
        for name, source in legal.items():
            if (
                name not in file_names
                or archive.read(entries[name]) != source.read_bytes()
            ):
                raise ValueError(f"wheel legal file {name} is missing or incorrect")

        native_payload = {
            name for name in file_names if name.lower().endswith(NATIVE_SUFFIXES)
        }
        expected_native = TARGETS[target]["native"]
        if native_payload != {expected_native}:
            raise ValueError(
                f"wheel native payload is {sorted(native_payload)}, "
                f"expected {[expected_native]}"
            )
        if not _valid_native_magic(
            archive.read(entries[expected_native]), TARGETS[target]["kind"]
        ):
            raise ValueError(f"invalid native magic for {expected_native}")

        top_level = f"{dist_info}/top_level.txt"
        optional = set()
        if top_level in file_names:
            optional.add(top_level)
            if archive.read(entries[top_level]) != b"mosaic\n":
                raise ValueError(f"{top_level} must contain exactly 'mosaic\\n'")
        allowed = (
            set(modules)
            | set(legal)
            | {
                expected_native,
                metadata_path,
                wheel_path,
                record_path,
            }
            | optional
        )
        unexpected = sorted(file_names - allowed)
        if unexpected:
            raise ValueError(f"unexpected wheel payload: {unexpected}")

    print(f"verified {wheel.name}: {target}")
    return target


def verify_wheels(
    wheels: list[Path],
    root: Path | None = None,
    *,
    require_all_targets: bool = False,
) -> list[str]:
    root = root or repository_root()
    targets = [verify_wheel(wheel, root) for wheel in wheels]
    if require_all_targets and (
        len(targets) != len(TARGETS) or set(targets) != set(TARGETS)
    ):
        raise ValueError(
            "wheel set differs from the four release targets: "
            f"found {targets}, expected {list(TARGETS)}"
        )
    return targets


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wheels", nargs="+", type=Path)
    parser.add_argument("--require-all-targets", action="store_true")
    args = parser.parse_args()

    failed = False
    targets = []
    for wheel in args.wheels:
        try:
            targets.append(verify_wheel(wheel, repository_root()))
        except (
            BadZipFile,
            csv.Error,
            KeyError,
            OSError,
            TypeError,
            ValueError,
            zlib.error,
        ) as error:
            failed = True
            print(f"wheel verification failed: {wheel}: {error}", file=sys.stderr)
    if args.require_all_targets and (
        len(targets) != len(TARGETS) or set(targets) != set(TARGETS)
    ):
        failed = True
        print(
            "wheel set differs from the four release targets: "
            f"found {targets}, expected {list(TARGETS)}",
            file=sys.stderr,
        )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
