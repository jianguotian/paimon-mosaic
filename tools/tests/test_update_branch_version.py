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
import tomllib


ROOT = Path(__file__).resolve().parents[2]
UPDATE_SCRIPT = ROOT / "tools/update_branch_version.sh"
VERSION_VERIFIER = ROOT / "tools/verify_release_versions.py"


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


def initialize_repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    tools = repo / "tools"
    tools.mkdir(parents=True)
    shutil.copy2(UPDATE_SCRIPT, tools / UPDATE_SCRIPT.name)
    shutil.copy2(VERSION_VERIFIER, tools / VERSION_VERIFIER.name)

    write(
        repo / "Cargo.toml",
        '[workspace]\nmembers = ["core", "cli"]\nresolver = "2"\n',
    )
    write(
        repo / "core/Cargo.toml",
        '[package]\nname = "paimon-mosaic-core"\nversion = "0.3.0"\n',
    )
    write(repo / "core/src/lib.rs", "")
    write(
        repo / "cli/Cargo.toml",
        '[package]\nname = "paimon-mosaic-cli"\nversion = "0.3.0"\n\n'
        "[dependencies]\n"
        'paimon-mosaic-core = { path = "../core", version = "0.3.0" }\n',
    )
    write(repo / "cli/src/main.rs", "fn main() {}\n")
    write(
        repo / "java/pom.xml",
        '<?xml version="1.0"?>\n'
        '<project xmlns="http://maven.apache.org/POM/4.0.0">\n'
        "  <modelVersion>4.0.0</modelVersion>\n"
        "  <groupId>org.apache.paimon</groupId>\n"
        "  <artifactId>mosaic</artifactId>\n"
        "  <version>0.3.0</version>\n"
        "</project>\n",
    )
    write(
        repo / "python/pyproject.toml",
        '[project]\nname = "paimon-mosaic"\nversion = "0.3.0"\n',
    )

    run(["cargo", "generate-lockfile", "--offline"], cwd=repo)
    run(["git", "init", "-q"], cwd=repo)
    run(["git", "config", "user.name", "Version Test"], cwd=repo)
    run(
        ["git", "config", "user.email", "version-test@example.invalid"],
        cwd=repo,
    )
    run(["git", "add", "."], cwd=repo)
    run(["git", "commit", "-q", "-m", "version fixture"], cwd=repo)
    return repo


def run_updater(repo: Path, old: str, new: str) -> None:
    env = os.environ.copy()
    env.update({"OLD_VERSION": old, "NEW_VERSION": new})
    run(["bash", UPDATE_SCRIPT.name], cwd=repo / "tools", env=env)


def package_versions(path: Path) -> dict[str, str]:
    lock = tomllib.loads(path.read_text(encoding="utf-8"))
    return {
        package["name"]: package["version"]
        for package in lock["package"]
        if package["name"].startswith("paimon-mosaic")
    }


def test_documented_version_transitions_refresh_cargo_constraints_and_lock(
    tmp_path: Path,
) -> None:
    repo = initialize_repo(tmp_path)

    run_updater(repo, "0.3.0", "0.4.0-SNAPSHOT")

    assert (
        tomllib.loads((repo / "core/Cargo.toml").read_text(encoding="utf-8"))[
            "package"
        ]["version"]
        == "0.4.0"
    )
    cli = tomllib.loads(
        (repo / "cli/Cargo.toml").read_text(encoding="utf-8")
    )
    assert cli["package"]["version"] == "0.4.0"
    assert cli["dependencies"]["paimon-mosaic-core"]["version"] == "0.4.0"
    assert package_versions(repo / "Cargo.lock") == {
        "paimon-mosaic-cli": "0.4.0",
        "paimon-mosaic-core": "0.4.0",
    }
    assert "<version>0.4.0-SNAPSHOT</version>" in (
        repo / "java/pom.xml"
    ).read_text(encoding="utf-8")
    assert (
        tomllib.loads(
            (repo / "python/pyproject.toml").read_text(encoding="utf-8")
        )["project"]["version"]
        == "0.4.0"
    )
    assert run(["git", "status", "--porcelain"], cwd=repo).stdout == b""

    run_updater(repo, "0.4.0-SNAPSHOT", "0.4.0")

    assert package_versions(repo / "Cargo.lock") == {
        "paimon-mosaic-cli": "0.4.0",
        "paimon-mosaic-core": "0.4.0",
    }
    assert "<version>0.4.0</version>" in (
        repo / "java/pom.xml"
    ).read_text(encoding="utf-8")
    assert run(["git", "status", "--porcelain"], cwd=repo).stdout == b""
    assert (
        run(["git", "rev-list", "--count", "HEAD"], cwd=repo)
        .stdout.decode()
        .strip()
        == "3"
    )
    run(
        [
            sys.executable,
            str(VERSION_VERIFIER),
            "--root",
            str(repo),
            "v0.4.0-rc1",
        ],
        cwd=repo,
    )
