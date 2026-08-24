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

import copy
from pathlib import Path

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[2]
RELEASE_WORKFLOW = ROOT / ".github/workflows/release.yml"
GATE_WORKFLOW = ROOT / ".github/workflows/release-vote-gate.yml"
RELEASE_DOCUMENTATION = ROOT / "docs/creating-a-release.html"


def load_workflow(path: Path) -> dict:
    return yaml.load(path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)


def needs(job: dict) -> set[str]:
    value = job.get("needs", [])
    return {value} if isinstance(value, str) else set(value)


def release_version_step(workflow: dict) -> dict:
    steps = workflow["jobs"]["release-preflight"]["steps"]
    version_steps = [
        step
        for step in steps
        if "verify_release_versions.py" in step.get("run", "")
    ]
    assert len(version_steps) == 1
    return version_steps[0]


def assert_release_contract(workflow: dict) -> None:
    assert workflow["concurrency"]["cancel-in-progress"] == "false"

    jobs = workflow["jobs"]
    preflight = jobs["release-preflight"]
    assert "runs-on" in preflight
    assert "uses" not in preflight
    assert "continue-on-error" not in preflight
    assert "continue-on-error" not in release_version_step(workflow)

    for job_name in ("rust", "java", "python-wheels"):
        assert "release-preflight" in needs(jobs[job_name])

    source = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    assert "always()" not in source


def test_release_has_inline_tag_only_preflight() -> None:
    workflow = load_workflow(RELEASE_WORKFLOW)
    assert_release_contract(workflow)

    triggers = workflow["on"]
    assert "workflow_dispatch" in triggers
    version_step = release_version_step(workflow)
    assert "github.event_name == 'push'" in version_step["if"]
    assert "github.ref_name" in version_step["run"]


@pytest.mark.parametrize("job_name", ("rust", "java", "python-wheels"))
def test_contract_rejects_deleted_preflight_need(job_name: str) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    workflow["jobs"][job_name].pop("needs")

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    ("cancel-in-progress", "preflight-continue", "version-step-continue"),
)
def test_contract_rejects_non_blocking_preflight_mutations(mutation: str) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    if mutation == "cancel-in-progress":
        workflow["concurrency"]["cancel-in-progress"] = "true"
    elif mutation == "preflight-continue":
        workflow["jobs"]["release-preflight"]["continue-on-error"] = "true"
    else:
        release_version_step(workflow)["continue-on-error"] = "true"

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


def test_source_release_documentation_passes_rc_tag_explicitly() -> None:
    source = RELEASE_DOCUMENTATION.read_text(encoding="utf-8")
    invocation = (
        "RELEASE_VERSION=${RELEASE_VERSION} "
        "RC_TAG=${RC_TAG} ./create_source_release.sh"
    )
    assert invocation in source

    workflow = load_workflow(GATE_WORKFLOW)
    for event in ("pull_request", "push"):
        assert "docs/creating-a-release.html" in workflow["on"][event]["paths"]


def test_release_vote_gate_runs_only_the_pr_a_checks() -> None:
    workflow = load_workflow(GATE_WORKFLOW)
    job = workflow["jobs"]["release-vote-gate"]
    combined_runs = "\n".join(
        step.get("run", "") for step in job["steps"] if "run" in step
    )

    assert "pytest" in combined_runs
    assert "PyYAML" in combined_runs
    for test_file in (
        "tools/tests/test_create_source_release.py",
        "tools/tests/test_release_vote_workflow.py",
        "tools/tests/test_verify_release_versions.py",
        "tools/tests/test_verify_source_archive.py",
    ):
        assert test_file in combined_runs
    assert "compileall" in combined_runs
    assert "bash -n tools/create_source_release.sh" in combined_runs
    assert "git diff --check" in combined_runs
    assert "GITHUB_BASE_REF" in combined_runs
    assert "HEAD^" in combined_runs
