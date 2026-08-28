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
RUST_RELEASE_WORKFLOW = ROOT / ".github/workflows/release-rust.yml"
JAVA_RELEASE_WORKFLOW = ROOT / ".github/workflows/release-java.yml"
PYTHON_PUBLISH_WORKFLOW = ROOT / ".github/workflows/release-python-publish.yml"
RELEASE_DOCUMENTATION = ROOT / "docs/creating-a-release.html"
TAG_CONDITION = "startsWith(github.ref, 'refs/tags/')"
RUST_PUBLISH_CONDITION = (
    "github.event_name != 'workflow_dispatch' && "
    "github.repository == 'apache/paimon-mosaic' && "
    "startsWith(github.ref, 'refs/tags/') && "
    "!contains(github.ref_name, '-')"
)
JAVA_DEPLOY_CONDITION = (
    "github.event_name != 'workflow_dispatch' && "
    "github.repository == 'apache/paimon-mosaic' && "
    "startsWith(github.ref, 'refs/tags/') && "
    "contains(github.ref_name, '-rc')"
)
PYTHON_TEST_PUBLISH_CONDITION = (
    "github.event_name != 'workflow_dispatch' && "
    "contains(github.ref_name, '-rc')"
)
PYTHON_PUBLISH_CONDITION = (
    "github.event_name != 'workflow_dispatch' && "
    "!contains(github.ref_name, '-')"
)
REQUIRED_GATE_PATHS = {
    ".github/workflows/release-vote-gate.yml",
    ".github/workflows/release-java.yml",
    ".github/workflows/release-python-publish.yml",
    ".github/workflows/release-python.yml",
    ".github/workflows/release-rust.yml",
    ".github/workflows/release.yml",
    "docs/creating-a-release.html",
    "tools/create_source_release.sh",
    "tools/update_branch_version.sh",
    "tools/verify_release_versions.py",
    "tools/verify_source_archive.py",
    "tools/tests/test_create_source_release.py",
    "tools/tests/test_release_vote_workflow.py",
    "tools/tests/test_update_branch_version.py",
    "tools/tests/test_verify_release_versions.py",
    "tools/tests/test_verify_source_archive.py",
}
RELEASE_WORKFLOW_BY_JOB = {
    "rust": "./.github/workflows/release-rust.yml",
    "java": "./.github/workflows/release-java.yml",
    "python-wheels": "./.github/workflows/release-python.yml",
    "python-publish": "./.github/workflows/release-python-publish.yml",
}
IMPORT_VERIFICATION_KEYS_COMMAND = """set -euo pipefail
keys_file="${RUNNER_TEMP}/KEYS"
curl --fail --location --proto '=https' --tlsv1.2 \\
  --output "${keys_file}" \\
  https://downloads.apache.org/paimon/KEYS
gpg --batch --import "${keys_file}"
"""
VERIFY_RELEASE_COMMAND = (
    'python3 tools/verify_release_versions.py "${TAG_NAME}" '
    "--verify-signature"
)
VERIFY_SOURCE_ARCHIVE_COMMAND = """set -euo pipefail
release_version="${GITHUB_REF_NAME#v}"
release_version="${release_version%-rc*}"
archive="${RUNNER_TEMP}/apache-paimon-mosaic-${release_version}-src.tgz"
prefix="paimon-mosaic-${release_version}/"
commit="$(git rev-parse --verify 'HEAD^{commit}')"
python3 tools/verify_source_archive.py create \\
  --repository "${GITHUB_WORKSPACE}" \\
  --commit "${commit}" \\
  --prefix "${prefix}" \\
  --output "${archive}"
python3 tools/verify_source_archive.py verify \\
  --repository "${GITHUB_WORKSPACE}" \\
  --commit "${commit}" \\
  --prefix "${prefix}" \\
  --archive "${archive}"
"""
GATE_TEST_COMMAND = """python -m pytest -q \\
  tools/tests/test_create_source_release.py \\
  tools/tests/test_release_vote_workflow.py \\
  tools/tests/test_update_branch_version.py \\
  tools/tests/test_verify_release_versions.py \\
  tools/tests/test_verify_source_archive.py
"""
GATE_STATIC_COMMAND = """set -euo pipefail
python -m compileall -q \\
  tools/verify_release_versions.py \\
  tools/verify_source_archive.py \\
  tools/tests/test_create_source_release.py \\
  tools/tests/test_release_vote_workflow.py \\
  tools/tests/test_update_branch_version.py \\
  tools/tests/test_verify_release_versions.py \\
  tools/tests/test_verify_source_archive.py
bash -n tools/create_source_release.sh
bash -n tools/update_branch_version.sh
if [[ -n "${GITHUB_BASE_REF:-}" ]]; then
  comparison_ref="origin/${GITHUB_BASE_REF}"
else
  comparison_ref="HEAD^"
fi
git diff --check "$(git merge-base HEAD "${comparison_ref}")" HEAD
"""


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


def named_step(workflow: dict, name: str) -> dict:
    steps = workflow["jobs"]["release-preflight"]["steps"]
    matches = [step for step in steps if step.get("name") == name]
    assert len(matches) == 1
    return matches[0]


def gate_step(workflow: dict, name: str) -> dict:
    steps = workflow["jobs"]["release-vote-gate"]["steps"]
    matches = [step for step in steps if step.get("name") == name]
    assert len(matches) == 1
    return matches[0]


def assert_gate_contract(workflow: dict) -> None:
    triggers = workflow["on"]
    assert "workflow_dispatch" in triggers
    assert set(triggers["pull_request"]["branches"]) == {"main", "release-*"}
    assert set(triggers["push"]["branches"]) == {"main", "release-*"}
    for event in ("pull_request", "push"):
        assert REQUIRED_GATE_PATHS <= set(triggers[event]["paths"])

    job = workflow["jobs"]["release-vote-gate"]
    assert "if" not in job
    assert "continue-on-error" not in job

    install_step = gate_step(workflow, "Install test dependencies")
    test_step = gate_step(workflow, "Run release vote tests")
    static_step = gate_step(workflow, "Run static checks")
    for step in (install_step, test_step, static_step):
        assert "if" not in step
        assert "continue-on-error" not in step

    assert install_step["run"] == "python -m pip install pytest PyYAML"
    assert test_step["run"] == GATE_TEST_COMMAND
    assert static_step["shell"] == "bash"
    assert static_step["run"] == GATE_STATIC_COMMAND


def assert_release_contract(workflow: dict) -> None:
    assert workflow["concurrency"]["cancel-in-progress"] == "false"

    jobs = workflow["jobs"]
    preflight = jobs["release-preflight"]
    assert "runs-on" in preflight
    assert "uses" not in preflight
    assert "continue-on-error" not in preflight
    assert "continue-on-error" not in release_version_step(workflow)

    checkout = next(
        step
        for step in preflight["steps"]
        if step.get("uses") == "actions/checkout@v6"
    )
    checkout_options = checkout.get("with", {})
    assert checkout_options.get("fetch-depth") == "0"
    assert "ref" not in checkout_options

    import_step = named_step(workflow, "Import release verification keys")
    assert import_step["if"] == TAG_CONDITION
    assert "env" not in import_step
    assert "continue-on-error" not in import_step
    assert import_step["run"] == IMPORT_VERIFICATION_KEYS_COMMAND

    version_step = release_version_step(workflow)
    assert version_step["if"] == TAG_CONDITION
    assert version_step["env"] == {
        "TAG_NAME": "${{ github.ref_name }}",
    }
    assert version_step["run"] == VERIFY_RELEASE_COMMAND

    source_step = named_step(workflow, "Verify source archive")
    assert source_step["if"] == TAG_CONDITION
    assert "continue-on-error" not in source_step
    assert source_step["run"] == VERIFY_SOURCE_ARCHIVE_COMMAND

    for job_name in ("rust", "java", "python-wheels"):
        release_job = jobs[job_name]
        assert "release-preflight" in needs(release_job)
        assert "if" not in release_job
        assert "continue-on-error" not in release_job

    publish_job = jobs["python-publish"]
    assert needs(publish_job) == {"rust", "java", "python-wheels"}
    assert publish_job["if"] == TAG_CONDITION
    assert "continue-on-error" not in publish_job

    for job_name, reusable_workflow in RELEASE_WORKFLOW_BY_JOB.items():
        assert jobs[job_name]["uses"] == reusable_workflow

    source = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    assert "always()" not in source


def test_release_has_signed_exact_tag_preflight() -> None:
    workflow = load_workflow(RELEASE_WORKFLOW)
    assert_release_contract(workflow)

    triggers = workflow["on"]
    assert "workflow_dispatch" in triggers
    version_step = release_version_step(workflow)
    assert version_step["if"] == TAG_CONDITION
    assert "github.ref_name" in version_step["env"]["TAG_NAME"]
    assert "github.ref_name" not in version_step["run"]


def test_release_preflight_does_not_use_private_key_secret() -> None:
    assert "GPG_SECRET_KEY" not in RELEASE_WORKFLOW.read_text(encoding="utf-8")


def test_release_tag_context_is_not_interpolated_into_shell() -> None:
    workflow = load_workflow(RELEASE_WORKFLOW)
    version_step = release_version_step(workflow)

    assert version_step["env"] == {
        "TAG_NAME": "${{ github.ref_name }}",
    }
    assert version_step["run"] == VERIFY_RELEASE_COMMAND
    assert "${{ github.ref_name }}" not in version_step["run"]


def test_manual_rust_dispatch_cannot_publish() -> None:
    workflow = load_workflow(RUST_RELEASE_WORKFLOW)
    publish_steps = [
        step
        for step in workflow["jobs"]["publish"]["steps"]
        if step.get("name") == "Publish paimon-mosaic-core to crates.io"
    ]

    assert len(publish_steps) == 1
    assert publish_steps[0]["if"] == RUST_PUBLISH_CONDITION


def test_manual_java_dispatch_cannot_deploy_staging() -> None:
    workflow = load_workflow(JAVA_RELEASE_WORKFLOW)
    deploy_job = workflow["jobs"]["deploy-staging"]

    assert deploy_job["if"] == JAVA_DEPLOY_CONDITION


def test_manual_release_dispatch_cannot_publish_python() -> None:
    workflow = load_workflow(PYTHON_PUBLISH_WORKFLOW)
    steps = workflow["jobs"]["publish"]["steps"]
    test_publish = [
        step for step in steps if step.get("name") == "Publish to TestPyPI"
    ]
    final_publish = [
        step for step in steps if step.get("name") == "Publish to PyPI"
    ]

    assert len(test_publish) == 1
    assert len(final_publish) == 1
    assert test_publish[0]["if"] == PYTHON_TEST_PUBLISH_CONDITION
    assert final_publish[0]["if"] == PYTHON_PUBLISH_CONDITION


def test_java_release_tag_context_is_not_interpolated_into_shell() -> None:
    workflow = load_workflow(JAVA_RELEASE_WORKFLOW)
    deploy_steps = [
        step
        for step in workflow["jobs"]["deploy-staging"]["steps"]
        if step.get("name") == "Deploy to Apache Nexus staging"
    ]

    assert len(deploy_steps) == 1
    deploy_step = deploy_steps[0]
    assert deploy_step["env"]["TAG_NAME"] == "${{ github.ref_name }}"
    assert 'REF="${TAG_NAME}"' in deploy_step["run"]
    assert "${{ github.ref_name }}" not in deploy_step["run"]


@pytest.mark.parametrize("job_name", ("rust", "java", "python-wheels"))
def test_contract_rejects_deleted_preflight_need(job_name: str) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    workflow["jobs"][job_name].pop("needs")

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


@pytest.mark.parametrize("condition", ("${{ failure() }}", "${{ !cancelled() }}"))
def test_contract_rejects_release_job_status_override(condition: str) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    workflow["jobs"]["rust"]["if"] = condition

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    ("status-override", "missing-dependency", "continue-on-error"),
)
def test_contract_rejects_python_publish_bypass(mutation: str) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    publish_job = workflow["jobs"]["python-publish"]
    if mutation == "status-override":
        publish_job["if"] = "${{ failure() }}"
    elif mutation == "missing-dependency":
        publish_job["needs"] = ["rust"]
    elif mutation == "continue-on-error":
        publish_job["continue-on-error"] = "true"
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


@pytest.mark.parametrize("job_name", RELEASE_WORKFLOW_BY_JOB)
def test_contract_rejects_wrong_reusable_workflow(job_name: str) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    workflow["jobs"][job_name]["uses"] = "./.github/workflows/wrong.yml"

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


def test_contract_rejects_masked_release_verifier_failure() -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    release_version_step(workflow)["run"] += " || true"

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    ("missing-step", "masked-command", "step-continue", "wrong-commit"),
)
def test_contract_rejects_source_archive_preflight_mutations(
    mutation: str,
) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    if mutation == "missing-step":
        steps = workflow["jobs"]["release-preflight"]["steps"]
        steps.remove(named_step(workflow, "Verify source archive"))
    elif mutation == "masked-command":
        named_step(workflow, "Verify source archive")["run"] += " || true"
    elif mutation == "step-continue":
        named_step(workflow, "Verify source archive")[
            "continue-on-error"
        ] = "true"
    elif mutation == "wrong-commit":
        source_step = named_step(workflow, "Verify source archive")
        source_step["run"] = source_step["run"].replace(
            '--commit "${commit}"',
            "--commit HEAD^",
        )
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    (
        "wrong-checkout-ref",
        "missing-signature-verification",
        "masked-key-import",
        "key-import-continue",
        "private-key-secret",
    ),
)
def test_contract_rejects_signed_tag_preflight_mutations(mutation: str) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    if mutation == "wrong-checkout-ref":
        checkout = next(
            step
            for step in workflow["jobs"]["release-preflight"]["steps"]
            if step.get("uses") == "actions/checkout@v6"
        )
        checkout["with"]["ref"] = "main"
    elif mutation == "missing-signature-verification":
        release_version_step(workflow)["run"] = (
            'python3 tools/verify_release_versions.py "${TAG_NAME}"'
        )
    elif mutation == "masked-key-import":
        named_step(workflow, "Import release verification keys")[
            "run"
        ] += " || true"
    elif mutation == "key-import-continue":
        named_step(workflow, "Import release verification keys")[
            "continue-on-error"
        ] = "true"
    elif mutation == "private-key-secret":
        import_step = named_step(workflow, "Import release verification keys")
        import_step["env"] = {
            "GPG_SECRET_KEY": "${{ secrets.GPG_SECRET_KEY }}"
        }
        import_step["run"] = (
            "printf '%s' \"${GPG_SECRET_KEY}\" | gpg --batch --import"
        )
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

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
    elif mutation == "version-step-continue":
        release_version_step(workflow)["continue-on-error"] = "true"
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


def test_source_release_documentation_passes_rc_tag_explicitly() -> None:
    source = RELEASE_DOCUMENTATION.read_text(encoding="utf-8")
    invocation = (
        "RELEASE_VERSION=${RELEASE_VERSION} "
        "RC_TAG=${RC_TAG} ./create_source_release.sh"
    )
    assert invocation in source

    assert "Cargo path dependency constraints and Cargo.lock" in source
    assert (
        "<tr><td><code>GPG_SECRET_KEY</code></td>"
        "<td>Java artifact signing</td></tr>"
    ) in source


def test_release_documentation_describes_source_archive_preflight() -> None:
    source = RELEASE_DOCUMENTATION.read_text(encoding="utf-8")
    assert "temporary source archive" in source


def test_release_vote_gate_matches_blocking_contract() -> None:
    workflow = load_workflow(GATE_WORKFLOW)
    assert_gate_contract(workflow)


@pytest.mark.parametrize("event", ("pull_request", "push"))
@pytest.mark.parametrize(
    "workflow_path",
    (
        ".github/workflows/release.yml",
        *(
            path.removeprefix("./")
            for path in RELEASE_WORKFLOW_BY_JOB.values()
        ),
    ),
)
def test_gate_contract_rejects_missing_release_workflow_path(
    event: str,
    workflow_path: str,
) -> None:
    workflow = copy.deepcopy(load_workflow(GATE_WORKFLOW))
    workflow["on"][event]["paths"].remove(workflow_path)

    with pytest.raises(AssertionError):
        assert_gate_contract(workflow)


@pytest.mark.parametrize("event", ("pull_request", "push"))
def test_gate_contract_rejects_missing_release_branch(event: str) -> None:
    workflow = copy.deepcopy(load_workflow(GATE_WORKFLOW))
    workflow["on"][event]["branches"].remove("release-*")

    with pytest.raises(AssertionError):
        assert_gate_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    ("masked-command", "step-continue", "step-condition", "job-continue"),
)
def test_gate_contract_rejects_non_blocking_test_mutations(mutation: str) -> None:
    workflow = copy.deepcopy(load_workflow(GATE_WORKFLOW))
    if mutation == "masked-command":
        gate_step(workflow, "Run release vote tests")["run"] += " || true"
    elif mutation == "step-continue":
        gate_step(workflow, "Run release vote tests")[
            "continue-on-error"
        ] = "true"
    elif mutation == "step-condition":
        gate_step(workflow, "Run release vote tests")["if"] = "failure()"
    elif mutation == "job-continue":
        workflow["jobs"]["release-vote-gate"]["continue-on-error"] = "true"
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        assert_gate_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    ("masked-command", "step-continue", "step-condition"),
)
def test_gate_contract_rejects_non_blocking_static_checks(mutation: str) -> None:
    workflow = copy.deepcopy(load_workflow(GATE_WORKFLOW))
    static_step = gate_step(workflow, "Run static checks")
    if mutation == "masked-command":
        static_step["run"] += " || true"
    elif mutation == "step-continue":
        static_step["continue-on-error"] = "true"
    elif mutation == "step-condition":
        static_step["if"] = "failure()"
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        assert_gate_contract(workflow)
