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

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest


ROOT = Path(__file__).resolve().parents[2]
SOURCE_SCRIPT = ROOT / "tools/create_source_release.sh"
SOURCE_VERIFIER = ROOT / "tools/verify_source_archive.py"
VERSION_VERIFIER = ROOT / "tools/verify_release_versions.py"
VERSION = "0.3.0"
RC_TAG = f"v{VERSION}-rc1"


def run(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


@pytest.fixture(scope="session")
def signing_home(tmp_path_factory: pytest.TempPathFactory) -> tuple[Path, str]:
    if shutil.which("gpg") is None:
        pytest.skip("gpg is required for source release tests")
    home = tmp_path_factory.mktemp("gnupg")
    home.chmod(0o700)
    env = os.environ.copy()
    env["GNUPGHOME"] = str(home)
    run(
        [
            "gpg",
            "--batch",
            "--passphrase",
            "",
            "--quick-gen-key",
            "Release Test <release-test@example.invalid>",
            "rsa1024",
            "sign",
            "0",
        ],
        cwd=home,
        env=env,
    )
    listing = run(
        ["gpg", "--batch", "--with-colons", "--list-secret-keys"],
        cwd=home,
        env=env,
    ).stdout.decode()
    fingerprint = next(
        line.split(":")[9]
        for line in listing.splitlines()
        if line.startswith("fpr:")
    )
    return home, fingerprint


def initialize_release_repo(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> tuple[Path, dict[str, str], str]:
    repo = tmp_path / "repo"
    tools = repo / "tools"
    tools.mkdir(parents=True)
    shutil.copy2(SOURCE_SCRIPT, tools / SOURCE_SCRIPT.name)
    shutil.copy2(SOURCE_VERIFIER, tools / SOURCE_VERIFIER.name)
    shutil.copy2(VERSION_VERIFIER, tools / VERSION_VERIFIER.name)

    write(
        repo / "Cargo.toml",
        '[workspace]\nmembers = ["core"]\nresolver = "2"\n',
    )
    write(
        repo / "core/Cargo.toml",
        f'[package]\nname = "paimon-mosaic-core"\nversion = "{VERSION}"\n',
    )
    write(
        repo / "Cargo.lock",
        "version = 4\n\n"
        "[[package]]\n"
        'name = "paimon-mosaic-core"\n'
        f'version = "{VERSION}"\n',
    )
    write(
        repo / "java/pom.xml",
        '<?xml version="1.0"?>\n'
        '<project xmlns="http://maven.apache.org/POM/4.0.0">\n'
        "  <modelVersion>4.0.0</modelVersion>\n"
        "  <groupId>org.apache.paimon</groupId>\n"
        "  <artifactId>mosaic</artifactId>\n"
        f"  <version>{VERSION}</version>\n"
        "</project>\n",
    )
    write(
        repo / "python/pyproject.toml",
        '[project]\nname = "paimon-mosaic"\n'
        f'version = "{VERSION}"\n',
    )
    write(repo / "README.md", "source release fixture\n")
    write(repo / ".gitignore", "tools/release/\ntools/.source-release.*\n")

    run(["git", "init", "-q"], cwd=repo)
    run(["git", "config", "user.name", "Release Test"], cwd=repo)
    run(
        ["git", "config", "user.email", "release-test@example.invalid"],
        cwd=repo,
    )
    home, fingerprint = signing_home
    run(["git", "config", "user.signingkey", fingerprint], cwd=repo)
    run(["git", "add", "."], cwd=repo)
    run(["git", "commit", "-q", "-m", "release fixture"], cwd=repo)
    commit = run(["git", "rev-parse", "HEAD"], cwd=repo).stdout.decode().strip()

    env = os.environ.copy()
    env.update(
        {
            "GNUPGHOME": str(home),
            "RELEASE_VERSION": VERSION,
            "RC_TAG": RC_TAG,
        }
    )
    run(
        [
            "git",
            "tag",
            "-s",
            "-u",
            fingerprint,
            "-m",
            f"Release candidate {RC_TAG}",
            RC_TAG,
        ],
        cwd=repo,
        env=env,
    )
    return repo, env, commit


def run_script(
    repo: Path,
    env: dict[str, str],
) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        ["bash", SOURCE_SCRIPT.name],
        cwd=repo / "tools",
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def output(result: subprocess.CompletedProcess[bytes]) -> str:
    return (result.stdout + result.stderr).decode("utf-8", errors="replace")


def artifact_paths(repo: Path) -> tuple[Path, Path, Path]:
    archive = (
        repo
        / "tools/release"
        / f"apache-paimon-mosaic-{VERSION}-src.tgz"
    )
    return archive, Path(f"{archive}.asc"), Path(f"{archive}.sha512")


def test_creates_verified_artifacts_from_signed_head_tag(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    repo, env, git_commit = initialize_release_repo(tmp_path, signing_home)

    result = run_script(repo, env)

    assert result.returncode == 0, output(result)
    archive, signature, checksum = artifact_paths(repo)
    assert archive.is_file()
    assert signature.is_file()
    assert checksum.is_file()
    assert hashlib.sha512(archive.read_bytes()).hexdigest() in checksum.read_text(
        encoding="utf-8"
    )
    run(["gpg", "--verify", str(signature), str(archive)], cwd=repo, env=env)
    run(
        [
            sys.executable,
            str(SOURCE_VERIFIER),
            "verify",
            "--repository",
            str(repo),
            "--commit",
            git_commit,
            "--prefix",
            f"paimon-mosaic-{VERSION}/",
            "--archive",
            str(archive),
        ],
        cwd=repo,
        env=env,
    )


@pytest.mark.parametrize("missing", ("RELEASE_VERSION", "RC_TAG"))
def test_requires_release_version_and_rc_tag(
    tmp_path: Path,
    signing_home: tuple[Path, str],
    missing: str,
) -> None:
    repo, env, _ = initialize_release_repo(tmp_path, signing_home)
    env.pop(missing)

    result = run_script(repo, env)

    assert result.returncode != 0
    assert f"{missing} is unset" in output(result)


def test_rejects_rc_tag_that_does_not_point_to_head(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    repo, env, _ = initialize_release_repo(tmp_path, signing_home)
    write(repo / "README.md", "new head\n")
    run(["git", "add", "README.md"], cwd=repo)
    run(["git", "commit", "-q", "-m", "move head"], cwd=repo)

    result = run_script(repo, env)

    assert result.returncode != 0
    assert "does not resolve to current HEAD" in output(result)


def test_rejects_dirty_worktree(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    repo, env, _ = initialize_release_repo(tmp_path, signing_home)
    write(repo / "README.md", "dirty\n")

    result = run_script(repo, env)

    assert result.returncode != 0
    assert "clean Git worktree" in output(result)


def test_existing_artifact_is_not_overwritten(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    repo, env, _ = initialize_release_repo(tmp_path, signing_home)
    archive, signature, checksum = artifact_paths(repo)
    archive.parent.mkdir(parents=True)
    archive.write_bytes(b"existing artifact")

    result = run_script(repo, env)

    assert result.returncode != 0
    assert "already exists" in output(result)
    assert archive.read_bytes() == b"existing artifact"
    assert not signature.exists()
    assert not checksum.exists()


def test_release_version_must_match_rc_tag(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    repo, env, _ = initialize_release_repo(tmp_path, signing_home)
    env["RELEASE_VERSION"] = "9.9.9"

    result = run_script(repo, env)

    assert result.returncode != 0
    assert "does not match RELEASE_VERSION" in output(result)


def test_signed_tag_with_cross_component_version_mismatch_is_rejected(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    repo, env, _ = initialize_release_repo(tmp_path, signing_home)
    write(
        repo / "python/pyproject.toml",
        '[project]\nname = "paimon-mosaic"\nversion = "0.3.1"\n',
    )
    run(["git", "add", "python/pyproject.toml"], cwd=repo)
    run(["git", "commit", "-q", "-m", "mismatched Python version"], cwd=repo)

    env["RC_TAG"] = f"v{VERSION}-rc2"
    _, fingerprint = signing_home
    run(
        [
            "git",
            "tag",
            "-s",
            "-u",
            fingerprint,
            "-m",
            f"Release candidate {env['RC_TAG']}",
            env["RC_TAG"],
        ],
        cwd=repo,
        env=env,
    )

    result = run_script(repo, env)

    assert result.returncode != 0
    assert "python/pyproject.toml" in output(result)
    assert "expected '0.3.0'" in output(result)
    assert not (repo / "tools/release").exists()


def test_unsigned_tag_is_rejected(
    tmp_path: Path,
    signing_home: tuple[Path, str],
) -> None:
    repo, env, _ = initialize_release_repo(tmp_path, signing_home)
    env["RC_TAG"] = f"v{VERSION}-rc2"
    run(
        ["git", "tag", "-a", "-m", "unsigned release candidate", env["RC_TAG"]],
        cwd=repo,
    )

    result = run_script(repo, env)

    assert result.returncode != 0
    assert "not a locally verifiable GPG-signed tag" in output(result)
