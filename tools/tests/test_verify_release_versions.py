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

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest


TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))

import verify_release_versions as verifier  # noqa: E402


VERSION = "0.3.0"
TAG = f"v{VERSION}-rc1"


def write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def initialize_versions(tmp_path: Path) -> Path:
    root = tmp_path / "repo"
    write(
        root / "Cargo.toml",
        '[workspace]\nmembers = ["core", "ffi", "helper"]\nresolver = "2"\n',
    )
    write(
        root / "core/Cargo.toml",
        f'[package]\nname = "paimon-mosaic-core"\nversion = "{VERSION}"\n',
    )
    write(
        root / "ffi/Cargo.toml",
        f'[package]\nname = "paimon-mosaic-ffi"\nversion = "{VERSION}"\n',
    )
    write(
        root / "helper/Cargo.toml",
        '[package]\nname = "fixture-helper"\nversion = "9.9.9"\n',
    )
    write(
        root / "Cargo.lock",
        "version = 4\n\n"
        "[[package]]\n"
        'name = "paimon-mosaic-core"\n'
        f'version = "{VERSION}"\n\n'
        "[[package]]\n"
        'name = "paimon-mosaic-ffi"\n'
        f'version = "{VERSION}"\n\n'
        "[[package]]\n"
        'name = "fixture-helper"\n'
        'version = "9.9.9"\n',
    )
    write(
        root / "java/pom.xml",
        '<?xml version="1.0"?>\n'
        '<project xmlns="http://maven.apache.org/POM/4.0.0">\n'
        "  <modelVersion>4.0.0</modelVersion>\n"
        "  <groupId>org.apache.paimon</groupId>\n"
        "  <artifactId>mosaic</artifactId>\n"
        f"  <version>{VERSION}</version>\n"
        "</project>\n",
    )
    write(
        root / "python/pyproject.toml",
        '[project]\nname = "paimon-mosaic"\n'
        f'version = "{VERSION}"\n',
    )
    return root


def replace(path: Path, old: str, new: str) -> None:
    source = path.read_text(encoding="utf-8")
    assert old in source
    path.write_text(source.replace(old, new, 1), encoding="utf-8")


def initialize_git_repository(root: Path) -> None:
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(
        ["git", "config", "user.name", "Release Test"],
        cwd=root,
        check=True,
    )
    subprocess.run(
        ["git", "config", "user.email", "release-test@example.invalid"],
        cwd=root,
        check=True,
    )
    subprocess.run(["git", "add", "."], cwd=root, check=True)
    subprocess.run(["git", "commit", "-qm", "fixture"], cwd=root, check=True)


@pytest.fixture(scope="session")
def signing_home(tmp_path_factory: pytest.TempPathFactory) -> tuple[Path, str]:
    if shutil.which("gpg") is None:
        pytest.skip("gpg is required for release tag tests")
    home = tmp_path_factory.mktemp("release-tag-gnupg")
    home.chmod(0o700)
    env = os.environ.copy()
    env["GNUPGHOME"] = str(home)
    subprocess.run(
        [
            "gpg",
            "--batch",
            "--passphrase",
            "",
            "--quick-gen-key",
            "Release Tag Test <release-tag-test@example.invalid>",
            "rsa1024",
            "sign",
            "0",
        ],
        cwd=home,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    listing = subprocess.run(
        ["gpg", "--batch", "--with-colons", "--list-secret-keys"],
        cwd=home,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    ).stdout
    fingerprint = next(
        line.split(":")[9]
        for line in listing.splitlines()
        if line.startswith("fpr:")
    )
    return home, fingerprint


def sign_release_tag(
    root: Path,
    signing_home: tuple[Path, str],
) -> dict[str, str]:
    home, fingerprint = signing_home
    env = os.environ.copy()
    env["GNUPGHOME"] = str(home)
    subprocess.run(
        ["git", "config", "user.signingkey", fingerprint],
        cwd=root,
        check=True,
    )
    subprocess.run(
        ["git", "tag", "-s", "-u", fingerprint, TAG, "-m", "signed release tag"],
        cwd=root,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return env


def run_verifier(
    root: Path,
    *,
    env: dict[str, str] | None = None,
    tag: str = TAG,
    verify_signature: bool = False,
) -> subprocess.CompletedProcess[str]:
    command = [
        sys.executable,
        str(TOOLS / "verify_release_versions.py"),
        tag,
        "--root",
        str(root),
    ]
    if verify_signature:
        command.append("--verify-signature")
    return subprocess.run(
        command,
        cwd=root,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def test_all_release_versions_match_tag(tmp_path: Path) -> None:
    root = initialize_versions(tmp_path)

    assert verifier.verify_release_versions(root, TAG) == VERSION


def test_cli_verify_signature_rejects_unsigned_tag(tmp_path: Path) -> None:
    root = initialize_versions(tmp_path)
    initialize_git_repository(root)
    subprocess.run(
        ["git", "tag", "-a", TAG, "-m", "unsigned release tag"],
        cwd=root,
        check=True,
    )

    result = run_verifier(root, verify_signature=True)

    assert result.returncode == 1
    assert "not a verifiable signed tag" in result.stderr


def test_cli_verify_signature_accepts_signed_tag(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    root = initialize_versions(tmp_path)
    initialize_git_repository(root)
    env = sign_release_tag(root, signing_home)

    result = run_verifier(root, env=env, verify_signature=True)

    assert result.returncode == 0, result.stderr
    assert f"verified release tag {TAG}" in result.stdout


def test_cli_verify_signature_rejects_tag_not_at_head(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    root = initialize_versions(tmp_path)
    initialize_git_repository(root)
    env = sign_release_tag(root, signing_home)
    write(root / "after-tag.txt", "new commit\n")
    subprocess.run(["git", "add", "after-tag.txt"], cwd=root, check=True)
    subprocess.run(["git", "commit", "-qm", "after tag"], cwd=root, check=True)

    result = run_verifier(root, env=env, verify_signature=True)

    assert result.returncode == 1
    assert "not current HEAD" in result.stderr


def test_cli_verify_signature_rejects_alias_for_different_tag_name(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    root = initialize_versions(tmp_path)
    initialize_git_repository(root)
    env = sign_release_tag(root, signing_home)
    tag_object = subprocess.run(
        ["git", "rev-parse", TAG],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout.strip()
    alias = f"v{VERSION}"
    subprocess.run(
        ["git", "update-ref", f"refs/tags/{alias}", tag_object],
        cwd=root,
        check=True,
    )

    result = run_verifier(
        root,
        env=env,
        tag=alias,
        verify_signature=True,
    )

    assert result.returncode == 1
    assert f"signed tag object names {TAG}, not {alias}" in result.stderr


def test_cli_verifies_the_frozen_tag_object(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    root = initialize_versions(tmp_path)
    initialize_git_repository(root)
    env = sign_release_tag(root, signing_home)
    signed_tag_object = subprocess.run(
        ["git", "rev-parse", f"{TAG}^{{tag}}"],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout.strip()
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout.strip()
    forged_tag_object = subprocess.run(
        ["git", "hash-object", "-t", "tag", "-w", "--stdin"],
        cwd=root,
        check=True,
        input=(
            f"object {commit}\n"
            "type commit\n"
            f"tag {TAG}\n"
            "tagger Release Test <release-test@example.invalid> 0 +0000\n\n"
            "forged release tag\n\n"
            "-----BEGIN PGP SIGNATURE-----\n"
            "invalid\n"
            "-----END PGP SIGNATURE-----\n"
        ),
        stdout=subprocess.PIPE,
        text=True,
    ).stdout.strip()
    subprocess.run(
        ["git", "update-ref", f"refs/tags/{TAG}", forged_tag_object],
        cwd=root,
        check=True,
    )

    wrapper_dir = tmp_path / "bin"
    wrapper_dir.mkdir()
    wrapper = wrapper_dir / "git"
    write(
        wrapper,
        "#!/bin/sh\n"
        'for argument in "$@"; do\n'
        '  if [ "$argument" = "verify-tag" ] && [ ! -e "$MOVE_MARKER" ]; then\n'
        '    : > "$MOVE_MARKER"\n'
        '    "$REAL_GIT" -C "$RELEASE_REPO" update-ref '
        '"refs/tags/$RELEASE_TAG" "$SIGNED_TAG_OBJECT"\n'
        "    break\n"
        "  fi\n"
        "done\n"
        'exec "$REAL_GIT" "$@"\n',
    )
    wrapper.chmod(0o755)
    move_marker = tmp_path / "tag-moved-before-verification"
    env.update(
        {
            "MOVE_MARKER": str(move_marker),
            "PATH": f"{wrapper_dir}{os.pathsep}{env['PATH']}",
            "REAL_GIT": shutil.which("git") or "git",
            "RELEASE_REPO": str(root),
            "RELEASE_TAG": TAG,
            "SIGNED_TAG_OBJECT": signed_tag_object,
        }
    )

    result = run_verifier(root, env=env, verify_signature=True)

    assert move_marker.is_file()
    assert result.returncode == 1
    assert "not a verifiable signed tag" in result.stderr


@pytest.mark.parametrize(
    ("relative_path", "old", "new", "expected"),
    (
        (
            "ffi/Cargo.toml",
            'version = "0.3.0"',
            'version = "0.3.1"',
            "ffi/Cargo.toml",
        ),
        (
            "Cargo.lock",
            'name = "paimon-mosaic-ffi"\nversion = "0.3.0"',
            'name = "paimon-mosaic-ffi"\nversion = "0.3.1"',
            "Cargo.lock",
        ),
        (
            "java/pom.xml",
            "<version>0.3.0</version>",
            "<version>0.3.0-SNAPSHOT</version>",
            "java/pom.xml",
        ),
        (
            "python/pyproject.toml",
            'version = "0.3.0"',
            'version = "0.3.0rc1"',
            "python/pyproject.toml",
        ),
    ),
)
def test_rejects_cross_language_version_mismatch(
    tmp_path: Path,
    relative_path: str,
    old: str,
    new: str,
    expected: str,
) -> None:
    root = initialize_versions(tmp_path)
    replace(root / relative_path, old, new)

    with pytest.raises(ValueError, match=expected):
        verifier.verify_release_versions(root, TAG)


def test_rejects_missing_workspace_package_in_cargo_lock(tmp_path: Path) -> None:
    root = initialize_versions(tmp_path)
    lock = root / "Cargo.lock"
    source = lock.read_text(encoding="utf-8")
    source = source.replace(
        "\n[[package]]\n"
        'name = "paimon-mosaic-ffi"\n'
        'version = "0.3.0"\n',
        "",
    )
    lock.write_text(source, encoding="utf-8")

    with pytest.raises(ValueError, match="Cargo.lock.*paimon-mosaic-ffi"):
        verifier.verify_release_versions(root, TAG)


@pytest.mark.parametrize(
    "dependency_table",
    (
        "[dependencies]",
        "[target.'cfg(unix)'.dev-dependencies]",
    ),
)
def test_rejects_mismatched_versioned_workspace_path_dependency(
    tmp_path: Path,
    dependency_table: str,
) -> None:
    root = initialize_versions(tmp_path)
    write(
        root / "ffi/Cargo.toml",
        f'[package]\nname = "paimon-mosaic-ffi"\nversion = "{VERSION}"\n\n'
        f"{dependency_table}\n"
        'paimon-mosaic-core = { path = "../core", version = "0.2.0" }\n',
    )

    with pytest.raises(
        ValueError,
        match=(
            "ffi/Cargo.toml.*paimon-mosaic-core path dependency version "
            "is '0.2.0', expected '0.3.0'"
        ),
    ):
        verifier.verify_release_versions(root, TAG)


def test_checks_path_dependencies_from_non_release_workspace_members(
    tmp_path: Path,
) -> None:
    root = initialize_versions(tmp_path)
    write(
        root / "helper/Cargo.toml",
        '[package]\nname = "fixture-helper"\nversion = "9.9.9"\n\n'
        "[build-dependencies]\n"
        'core-under-test = { package = "paimon-mosaic-core", '
        'path = "../core", version = "0.2.0" }\n',
    )

    with pytest.raises(
        ValueError,
        match=(
            "helper/Cargo.toml.*core-under-test path dependency version "
            "is '0.2.0', expected '0.3.0'"
        ),
    ):
        verifier.verify_release_versions(root, TAG)


def test_checks_automatically_included_path_workspace_members(
    tmp_path: Path,
) -> None:
    root = initialize_versions(tmp_path)
    replace(
        root / "Cargo.toml",
        'members = ["core", "ffi", "helper"]',
        'members = ["ffi", "helper"]',
    )
    write(
        root / "ffi/Cargo.toml",
        f'[package]\nname = "paimon-mosaic-ffi"\nversion = "{VERSION}"\n\n'
        "[dependencies]\n"
        'paimon-mosaic-core = { path = "../core", version = "0.2.0" }\n',
    )

    with pytest.raises(
        ValueError,
        match=(
            "ffi/Cargo.toml.*paimon-mosaic-core path dependency version "
            "is '0.2.0', expected '0.3.0'"
        ),
    ):
        verifier.verify_release_versions(root, TAG)


def test_rejects_noncanonical_path_dependency_requirement(
    tmp_path: Path,
) -> None:
    root = initialize_versions(tmp_path)
    write(
        root / "ffi/Cargo.toml",
        f'[package]\nname = "paimon-mosaic-ffi"\nversion = "{VERSION}"\n\n'
        "[dependencies]\n"
        'paimon-mosaic-core = { path = "../core", version = "^0.3.0" }\n',
    )

    with pytest.raises(
        ValueError,
        match=(
            "ffi/Cargo.toml.*paimon-mosaic-core path dependency version "
            r"is '\^0\.3\.0', expected '0\.3\.0'"
        ),
    ):
        verifier.verify_release_versions(root, TAG)


@pytest.mark.parametrize(
    "tag",
    (
        "0.3.0",
        "v0.3",
        "v0.3.0-RC1",
        "v0.3.0-rc",
        "v0.3.0-rc1-extra",
    ),
)
def test_rejects_non_release_tag(tag: str, tmp_path: Path) -> None:
    root = initialize_versions(tmp_path)

    with pytest.raises(ValueError, match="release tag"):
        verifier.verify_release_versions(root, tag)


def test_cli_is_read_only_and_reports_mismatches(tmp_path: Path) -> None:
    root = initialize_versions(tmp_path)
    pom = root / "java/pom.xml"
    replace(pom, "<version>0.3.0</version>", "<version>0.3.1</version>")
    before = pom.read_bytes()

    result = run_verifier(root)

    assert result.returncode == 1
    assert "java/pom.xml" in result.stderr
    assert pom.read_bytes() == before


def test_workspace_package_can_inherit_workspace_version(tmp_path: Path) -> None:
    root = initialize_versions(tmp_path)
    replace(
        root / "Cargo.toml",
        '[workspace]\nmembers = ["core", "ffi", "helper"]\n',
        '[workspace]\nmembers = ["core", "ffi", "helper"]\n'
        f'[workspace.package]\nversion = "{VERSION}"\n',
    )
    replace(
        root / "core/Cargo.toml",
        f'version = "{VERSION}"',
        "version.workspace = true",
    )

    assert verifier.verify_release_versions(root, TAG) == VERSION


def test_workspace_exclude_is_not_a_workspace_package(tmp_path: Path) -> None:
    root = initialize_versions(tmp_path)
    replace(
        root / "Cargo.toml",
        'members = ["core", "ffi", "helper"]',
        'members = ["core", "ffi", "helper", "excluded"]\n'
        'exclude = ["excluded"]',
    )
    write(
        root / "excluded/Cargo.toml",
        '[package]\nname = "paimon-mosaic-excluded"\nversion = "9.9.9"\n',
    )

    assert verifier.verify_release_versions(root, TAG) == VERSION
