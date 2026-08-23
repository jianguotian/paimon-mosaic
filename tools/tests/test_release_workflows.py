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

import os
from pathlib import Path
import subprocess

import yaml


ROOT = Path(__file__).resolve().parents[2]


def load_workflow(name: str) -> dict:
    with (ROOT / ".github/workflows" / name).open(encoding="utf-8") as source:
        return yaml.load(source, Loader=yaml.BaseLoader)


def needs(job: dict) -> set[str]:
    value = job.get("needs", [])
    return {value} if isinstance(value, str) else set(value)


def test_release_jobs_are_blocked_by_tag_and_version_preflight() -> None:
    workflow = load_workflow("release.yml")
    jobs = workflow["jobs"]

    preflight = jobs["release-preflight"]
    assert "if" not in preflight
    assert preflight["uses"] == "./.github/workflows/release-preflight.yml"

    shared_preflight = load_workflow("release-preflight.yml")
    validation = shared_preflight["jobs"]["validate"]
    assert "if" not in validation
    steps = validation["steps"]

    checkout = next(
        step for step in steps if step.get("uses") == "actions/checkout@v6"
    )
    assert checkout["with"]["fetch-depth"] == "0"
    assert checkout["with"]["fetch-tags"] == "true"

    scripts = "\n".join(step.get("run", "") for step in steps)
    for fragment in (
        "https://downloads.apache.org/paimon/KEYS",
        "--retry 3",
        "--retry-connrefused",
        "--connect-timeout 10",
        "--max-time 300",
        "tools/validate_release_tag.py",
        "--expected-commit \"$GITHUB_SHA\"",
        "tools/verify_release_versions.py",
    ):
        assert fragment in scripts

    for name in ("rust", "java", "python-wheels"):
        assert "release-preflight" in needs(jobs[name])
    for name in ("rust", "java"):
        assert jobs[name]["with"]["preflight_completed"] == "true"


def test_direct_release_dispatch_cannot_bypass_preflight() -> None:
    for workflow_name, gated_job_name in (
        ("release-rust.yml", "publish"),
        ("release-java.yml", "build-native"),
    ):
        workflow = load_workflow(workflow_name)
        assert "workflow_dispatch" in workflow["on"]
        assert "inputs" not in (workflow["on"]["workflow_dispatch"] or {})
        call_input = workflow["on"]["workflow_call"]["inputs"][
            "preflight_completed"
        ]
        assert call_input["type"] == "boolean"
        assert call_input["default"] == "false"
        jobs = workflow["jobs"]

        preflight = jobs["release-preflight"]
        assert preflight["uses"] == "./.github/workflows/release-preflight.yml"
        preflight_condition = preflight["if"]
        assert "github.event_name == 'workflow_dispatch'" in preflight_condition
        assert "!inputs.preflight_completed" in preflight_condition

        gated_job = jobs[gated_job_name]
        assert "release-preflight" in needs(gated_job)
        gate_condition = gated_job["if"]
        assert "always()" in gate_condition
        assert "needs.release-preflight.result == 'success'" in gate_condition


def test_python_publish_verifies_the_downloaded_wheel_payloads() -> None:
    workflow = load_workflow("release-python-publish.yml")
    steps = workflow["jobs"]["publish"]["steps"]

    checkout_index = next(
        index
        for index, step in enumerate(steps)
        if step.get("uses") == "actions/checkout@v6"
    )
    download_index = next(
        index
        for index, step in enumerate(steps)
        if step.get("uses") == "actions/download-artifact@v5"
    )
    verification_index = next(
        index
        for index, step in enumerate(steps)
        if "tools/verify_python_wheels.py" in step.get("run", "")
    )
    publish_indices = [
        index
        for index, step in enumerate(steps)
        if step.get("uses", "").startswith("pypa/gh-action-pypi-publish@")
    ]

    assert checkout_index < download_index < verification_index
    assert publish_indices
    assert verification_index < min(publish_indices)

    verification = steps[verification_index]
    assert verification.get("continue-on-error") is None
    script = verification["run"]
    for fragment in (
        "artifacts=(dist/*)",
        '"$artifact" != *.whl',
        "paimon_mosaic-\"${expected_version}\"-*.whl",
        "tools/verify_python_wheels.py",
        "--require-all-targets",
        '"${wheels[@]}"',
    ):
        assert fragment in script


def test_python_publish_shell_rejects_every_unverified_artifact(tmp_path) -> None:
    workflow = load_workflow("release-python-publish.yml")
    script = next(
        step["run"]
        for step in workflow["jobs"]["publish"]["steps"]
        if "tools/verify_python_wheels.py" in step.get("run", "")
    )

    dist = tmp_path / "dist"
    dist.mkdir()
    wheel = dist / "paimon_mosaic-0.3.0-py3-none-manylinux_2_28_x86_64.whl"
    wheel.write_bytes(b"fixture")
    unexpected = dist / "unverified.tar.gz"
    unexpected.write_bytes(b"fixture")

    tools = tmp_path / "tools"
    tools.mkdir()
    marker = tmp_path / "verifier-arguments"
    (tools / "verify_python_wheels.py").write_text(
        "from pathlib import Path\n"
        "import sys\n"
        f"Path({str(marker)!r}).write_text(repr(sys.argv[1:]))\n",
        encoding="utf-8",
    )
    environment = os.environ.copy()
    environment["TAG_NAME"] = "v0.3.0"

    rejected = subprocess.run(
        ["bash", "-c", script],
        cwd=tmp_path,
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )
    assert rejected.returncode != 0
    assert "Unexpected publish artifact" in rejected.stderr
    assert not marker.exists()

    unexpected.unlink()
    accepted = subprocess.run(
        ["bash", "-c", script],
        cwd=tmp_path,
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )
    assert accepted.returncode == 0, accepted.stderr
    arguments = marker.read_text(encoding="utf-8")
    assert "--require-all-targets" in arguments
    assert str(wheel.relative_to(tmp_path)) in arguments
