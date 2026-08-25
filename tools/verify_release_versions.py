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

"""Verify that every published component matches a release tag."""

from __future__ import annotations

import argparse
import glob
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parent.parent
RELEASE_TAG = re.compile(
    r"^v(?P<version>[0-9]+\.[0-9]+\.[0-9]+)(?:-rc[0-9]+)?$"
)


def load_toml(path: Path) -> dict:
    with path.open("rb") as source:
        return tomllib.load(source)


def release_version(tag: str) -> str:
    match = RELEASE_TAG.fullmatch(tag)
    if match is None:
        raise ValueError(
            f"invalid release tag {tag!r}; expected vX.Y.Z or vX.Y.Z-rcN"
        )
    return match.group("version")


def run_git(root: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(root), *arguments],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise ValueError(detail or f"git {' '.join(arguments)} failed")
    return result.stdout.strip()


def verify_release_tag(root: Path, tag: str) -> str:
    root = root.resolve()
    release_version(tag)
    if run_git(root, "cat-file", "-t", tag) != "tag":
        raise ValueError(f"{tag} is not an annotated release tag")

    tag_object = run_git(root, "rev-parse", "--verify", f"{tag}^{{tag}}")
    tag_headers = run_git(root, "cat-file", "tag", tag_object).partition("\n\n")[0]
    embedded_names = [
        line.removeprefix("tag ")
        for line in tag_headers.splitlines()
        if line.startswith("tag ")
    ]
    if embedded_names != [tag]:
        found = ", ".join(embedded_names) if embedded_names else "<missing>"
        raise ValueError(f"signed tag object names {found}, not {tag}")

    head_commit = run_git(root, "rev-parse", "--verify", "HEAD^{commit}")
    tag_commit = run_git(
        root,
        "rev-parse",
        "--verify",
        f"{tag_object}^{{commit}}",
    )
    if tag_commit != head_commit:
        raise ValueError(
            f"{tag} resolves to {tag_commit}, not current HEAD {head_commit}"
        )

    result = subprocess.run(
        ["git", "-C", str(root), "verify-tag", tag_object],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        suffix = f": {detail}" if detail else ""
        raise ValueError(f"{tag} is not a verifiable signed tag{suffix}")
    return tag_commit


def excluded_workspace_manifests(root: Path, workspace: dict) -> set[Path]:
    excluded = set()
    for pattern in workspace.get("exclude", []):
        if not isinstance(pattern, str):
            continue
        for match in glob.glob(str(root / pattern)):
            path = Path(match)
            manifest = path if path.name == "Cargo.toml" else path / "Cargo.toml"
            excluded.add(manifest.resolve())
    return excluded


def workspace_manifests(root: Path, workspace: dict) -> list[Path]:
    members = workspace.get("members")
    if not isinstance(members, list) or not all(
        isinstance(member, str) for member in members
    ):
        raise ValueError("Cargo.toml [workspace].members must be a string list")

    excluded = excluded_workspace_manifests(root, workspace)
    manifests = []
    for pattern in members:
        matches = [Path(path) for path in glob.glob(str(root / pattern))]
        if not matches:
            raise ValueError(f"Cargo workspace member does not exist: {pattern}")
        for match in matches:
            manifest = match if match.name == "Cargo.toml" else match / "Cargo.toml"
            if manifest.resolve() in excluded:
                continue
            if not manifest.is_file():
                raise ValueError(
                    f"Cargo workspace member has no Cargo.toml: "
                    f"{manifest.relative_to(root)}"
                )
            manifests.append(manifest)
    return sorted(set(manifests))


def package_version(package: dict, workspace_version: object) -> str:
    version = package.get("version")
    if isinstance(version, str):
        return version
    if (
        isinstance(version, dict)
        and version.get("workspace") is True
        and isinstance(workspace_version, str)
    ):
        return workspace_version
    raise ValueError("Cargo package has no explicit or inherited string version")


def rust_workspace_packages(root: Path) -> dict[str, tuple[str, Path]]:
    root_manifest = load_toml(root / "Cargo.toml")
    workspace = root_manifest.get("workspace")
    if not isinstance(workspace, dict):
        raise ValueError("Cargo.toml has no [workspace] table")
    workspace_package = workspace.get("package", {})
    workspace_version = (
        workspace_package.get("version")
        if isinstance(workspace_package, dict)
        else None
    )

    manifests = discovered_workspace_manifests(root, workspace)
    if isinstance(root_manifest.get("package"), dict):
        manifests.append(root / "Cargo.toml")

    packages = {}
    for manifest in sorted(set(manifests)):
        package = load_toml(manifest).get("package")
        if not isinstance(package, dict):
            raise ValueError(
                f"{manifest.relative_to(root)} has no [package] table"
            )
        name = package.get("name")
        if not isinstance(name, str):
            raise ValueError(
                f"{manifest.relative_to(root)} has no package name"
            )
        if not name.startswith("paimon-mosaic"):
            continue
        if name in packages:
            raise ValueError(f"duplicate Cargo workspace package name: {name}")
        packages[name] = (
            package_version(package, workspace_version),
            manifest.relative_to(root),
        )

    if not packages:
        raise ValueError("Cargo workspace has no paimon-mosaic packages")
    return packages


def dependency_tables(manifest: dict) -> list[dict]:
    tables = []
    for name in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = manifest.get(name)
        if isinstance(table, dict):
            tables.append(table)

    targets = manifest.get("target")
    if isinstance(targets, dict):
        for target in targets.values():
            if not isinstance(target, dict):
                continue
            for name in ("dependencies", "dev-dependencies", "build-dependencies"):
                table = target.get(name)
                if isinstance(table, dict):
                    tables.append(table)

    workspace = manifest.get("workspace")
    if isinstance(workspace, dict):
        table = workspace.get("dependencies")
        if isinstance(table, dict):
            tables.append(table)
    return tables


def discovered_workspace_manifests(root: Path, workspace: dict) -> list[Path]:
    root = root.resolve()
    excluded = excluded_workspace_manifests(root, workspace)
    manifests = {manifest.resolve() for manifest in workspace_manifests(root, workspace)}
    pending = [root / "Cargo.toml", *sorted(manifests)]
    scanned = set()
    while pending:
        manifest = pending.pop()
        if manifest in scanned:
            continue
        scanned.add(manifest)
        for table in dependency_tables(load_toml(manifest)):
            for dependency in table.values():
                if not isinstance(dependency, dict):
                    continue
                path = dependency.get("path")
                if not isinstance(path, str):
                    continue
                dependency_manifest = (manifest.parent / path).resolve()
                if dependency_manifest.name != "Cargo.toml":
                    dependency_manifest /= "Cargo.toml"
                try:
                    dependency_manifest.relative_to(root)
                except ValueError:
                    continue
                if (
                    dependency_manifest in excluded
                    or not dependency_manifest.is_file()
                    or dependency_manifest in manifests
                ):
                    continue
                manifests.add(dependency_manifest)
                pending.append(dependency_manifest)
    return sorted(manifests)


def workspace_path_dependency_failures(
    root: Path,
    packages: dict[str, tuple[str, Path]],
) -> list[str]:
    packages_by_manifest = {
        (root / manifest).resolve(): (name, version)
        for name, (version, manifest) in packages.items()
    }
    root_manifest = load_toml(root / "Cargo.toml")
    workspace = root_manifest.get("workspace")
    if not isinstance(workspace, dict):
        raise ValueError("Cargo.toml has no [workspace] table")
    manifests = {root / "Cargo.toml"}
    manifests.update(discovered_workspace_manifests(root, workspace))

    failures = []
    for manifest in sorted(manifests):
        source = load_toml(manifest)
        for table in dependency_tables(source):
            for dependency_name, dependency in table.items():
                if not isinstance(dependency, dict):
                    continue
                path = dependency.get("path")
                declared_version = dependency.get("version")
                if not isinstance(path, str) or not isinstance(
                    declared_version, str
                ):
                    continue
                dependency_manifest = (manifest.parent / path).resolve()
                if dependency_manifest.name != "Cargo.toml":
                    dependency_manifest /= "Cargo.toml"
                target = packages_by_manifest.get(dependency_manifest)
                if target is None:
                    continue
                target_name, target_version = target
                # The release updater rewrites only the canonical plain
                # version form, not alternative semver requirement syntax.
                if declared_version != target_version:
                    failures.append(
                        f"{manifest.relative_to(root)}: {dependency_name} "
                        f"path dependency version is {declared_version!r}, "
                        f"expected {target_version!r} for {target_name}"
                    )
    return failures


def direct_java_version(root: Path) -> str:
    project = ET.parse(root / "java/pom.xml").getroot()
    namespace = ""
    if project.tag.startswith("{"):
        namespace = project.tag.partition("}")[0] + "}"
    version = project.find(f"{namespace}version")
    if version is None or not version.text:
        raise ValueError("java/pom.xml has no direct project version")
    return version.text.strip()


def verify_release_versions(root: Path, tag: str) -> str:
    root = root.resolve()
    expected = release_version(tag)
    failures = []

    rust_packages = rust_workspace_packages(root)
    for name, (version, manifest) in sorted(rust_packages.items()):
        if version != expected:
            failures.append(
                f"{manifest}: {name} version is {version!r}, expected {expected!r}"
            )
    failures.extend(workspace_path_dependency_failures(root, rust_packages))

    lock = load_toml(root / "Cargo.lock")
    locked_packages = lock.get("package")
    if not isinstance(locked_packages, list):
        raise ValueError("Cargo.lock has no [[package]] entries")
    for name in sorted(rust_packages):
        versions = {
            package.get("version")
            for package in locked_packages
            if isinstance(package, dict) and package.get("name") == name
        }
        if not versions:
            failures.append(f"Cargo.lock has no package entry for {name}")
        elif versions != {expected}:
            failures.append(
                f"Cargo.lock versions for {name} are "
                f"{sorted(str(version) for version in versions)!r}, "
                f"expected only {expected!r}"
            )

    java = direct_java_version(root)
    if java != expected:
        failures.append(
            f"java/pom.xml project version is {java!r}, expected {expected!r}"
        )

    python = load_toml(root / "python/pyproject.toml").get("project", {}).get(
        "version"
    )
    if python != expected:
        failures.append(
            f"python/pyproject.toml project version is {python!r}, "
            f"expected {expected!r}"
        )

    if failures:
        raise ValueError("release version verification failed:\n- " + "\n- ".join(failures))
    return expected


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="release tag: vX.Y.Z or vX.Y.Z-rcN")
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument(
        "--verify-signature",
        action="store_true",
        help="require an annotated signed tag bound to the current HEAD",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.verify_signature:
            verify_release_tag(args.root, args.tag)
        version = verify_release_versions(args.root, args.tag)
    except (OSError, ET.ParseError, tomllib.TOMLDecodeError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    print(f"verified release tag {args.tag}: all component versions are {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
