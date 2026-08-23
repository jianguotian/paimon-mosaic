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

from __future__ import annotations

import os
import sys
import shutil
import subprocess
from pathlib import Path

import pytest


TOOLS = Path(__file__).resolve().parent.parent
PACKAGES = ("core", "ffi", "jni", "cli")


def run(command, cwd, env=None):
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def release_fixture(tmp_path: Path) -> Path:
    root = tmp_path / "repo"
    tools = root / "tools"
    tools.mkdir(parents=True)
    for name in (
        "update_branch_version.sh",
        "bump_pom_version.py",
        "verify_release_versions.py",
    ):
        shutil.copy2(TOOLS / name, tools / name)
    for name in ("dependencies.py", "generate_license_reports.py"):
        (tools / name).write_text(
            "#!/usr/bin/env python3\n",
            encoding="utf-8",
        )

    (root / "Cargo.toml").write_text(
        "[workspace]\n"
        f"members = {list(PACKAGES)!r}\n".replace("'", '"')
        + 'resolver = "2"\n',
        encoding="utf-8",
    )
    for package in PACKAGES:
        package_root = root / package
        (package_root / "src").mkdir(parents=True)
        (package_root / "src/lib.rs").write_text("", encoding="utf-8")
        (package_root / "Cargo.toml").write_text(
            "[package]\n"
            f'name = "paimon-mosaic-{package}"\n'
            'version = "0.3.0"\n'
            'edition = "2021"\n',
            encoding="utf-8",
        )
        (package_root / "DEPENDENCIES.rust.tsv").write_text(
            "fixture\n", encoding="utf-8"
        )
    (root / "DEPENDENCIES.rust.tsv").write_text("fixture\n", encoding="utf-8")

    java = root / "java"
    java.mkdir()
    (java / "pom.xml").write_text(
        '<project xmlns="http://maven.apache.org/POM/4.0.0">\n'
        "  <version>0.3.0-SNAPSHOT</version>\n"
        "  <dependencies>\n"
        "    <dependency><version>0.3.0-SNAPSHOT</version></dependency>\n"
        "  </dependencies>\n"
        "</project>\n",
        encoding="utf-8",
    )
    resources = java / "src/main/binary-resources"
    resources.mkdir(parents=True)
    (resources / "fixture.txt").write_text("fixture\n", encoding="utf-8")

    python = root / "python"
    python.mkdir()
    (python / "pyproject.toml").write_text(
        "[project]\n"
        'name = "paimon-mosaic"\n'
        'version = "0.3.0"\n',
        encoding="utf-8",
    )
    licenses = python / "licenses"
    licenses.mkdir()
    (licenses / "fixture.txt").write_text("fixture\n", encoding="utf-8")

    run(["git", "init", "-q"], cwd=root)
    run(["git", "config", "user.name", "Version Test"], cwd=root)
    run(
        ["git", "config", "user.email", "version-test@example.invalid"],
        cwd=root,
    )
    run(["cargo", "generate-lockfile", "--offline"], cwd=root)
    run(["git", "add", "."], cwd=root)
    run(["git", "commit", "-q", "-m", "fixture"], cwd=root)
    return root


@pytest.mark.parametrize(
    ("old_version", "new_version", "java_version", "component_version"),
    (
        ("0.3.0", "0.4.0-SNAPSHOT", "0.4.0-SNAPSHOT", "0.4.0"),
        ("0.3.0-SNAPSHOT", "0.3.0", "0.3.0", "0.3.0"),
    ),
    ids=("main-next-snapshot", "release-final"),
)
def test_documented_version_transitions(
    tmp_path,
    old_version,
    new_version,
    java_version,
    component_version,
):
    root = release_fixture(tmp_path)
    env = os.environ.copy()
    env.update({"OLD_VERSION": old_version, "NEW_VERSION": new_version})

    run(["./update_branch_version.sh"], cwd=root / "tools", env=env)

    pom = (root / "java/pom.xml").read_text(encoding="utf-8")
    assert f"<version>{java_version}</version>" in pom
    assert "<dependency><version>0.3.0-SNAPSHOT</version></dependency>" in pom
    assert (
        f'version = "{component_version}"'
        in (root / "python/pyproject.toml").read_text(encoding="utf-8")
    )
    for package in PACKAGES:
        assert (
            f'version = "{component_version}"'
            in (root / package / "Cargo.toml").read_text(encoding="utf-8")
        )
    assert run(["git", "status", "--porcelain"], cwd=root).stdout == ""
    assert (
        run(["git", "log", "-1", "--pretty=%s"], cwd=root).stdout.strip()
        == f"Update version to {new_version}"
    )


def test_multi_module_pom_bump_rewrites_every_module(tmp_path: Path) -> None:
    # The removed POM_LINES_CHANGED guard aborted unless exactly one line across
    # all POMs changed, contradicting bump_pom_version.py, which deliberately
    # rewrites project/parent/version so a child module legitimately changes a
    # second line. Drive the same find invocation the script runs.
    root = release_fixture(tmp_path)
    module = root / "java/child"
    module.mkdir(parents=True)
    (module / "pom.xml").write_text(
        '<project xmlns="http://maven.apache.org/POM/4.0.0">\n'
        "  <parent>\n"
        "    <artifactId>mosaic</artifactId>\n"
        "    <version>0.3.0-SNAPSHOT</version>\n"
        "  </parent>\n"
        "  <artifactId>mosaic-child</artifactId>\n"
        "  <dependencies>\n"
        "    <dependency><version>0.3.0-SNAPSHOT</version></dependency>\n"
        "  </dependencies>\n"
        "</project>\n",
        encoding="utf-8",
    )

    run(
        [
            "find", ".", "-name", "pom.xml", "-not", "-path", "*/target/*",
            "-type", "f", "-exec", sys.executable, "tools/bump_pom_version.py",
            "0.3.0-SNAPSHOT", "0.4.0-SNAPSHOT", "{}", "+",
        ],
        cwd=root,
    )

    parent_pom = (root / "java/pom.xml").read_text(encoding="utf-8")
    child_pom = (root / "java/child/pom.xml").read_text(encoding="utf-8")
    assert "<version>0.4.0-SNAPSHOT</version>" in parent_pom
    assert "<version>0.4.0-SNAPSHOT</version>" in child_pom
    # The decoy dependency in each POM keeps the old version: the rewrite is
    # structural, so it never escapes into a dependency element.
    assert parent_pom.count("<version>0.3.0-SNAPSHOT</version>") == 1
    assert child_pom.count("<version>0.3.0-SNAPSHOT</version>") == 1
