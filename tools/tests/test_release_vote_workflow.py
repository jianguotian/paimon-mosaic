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
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[2]
RELEASE_WORKFLOW = ROOT / ".github/workflows/release.yml"
RELEASE_PREFLIGHT_WORKFLOW = ROOT / ".github/workflows/release-preflight.yml"
GATE_WORKFLOW = ROOT / ".github/workflows/release-vote-gate.yml"
RUST_RELEASE_WORKFLOW = ROOT / ".github/workflows/release-rust.yml"
JAVA_RELEASE_WORKFLOW = ROOT / ".github/workflows/release-java.yml"
JAVA_POM = ROOT / "java/pom.xml"
PYTHON_PUBLISH_WORKFLOW = ROOT / ".github/workflows/release-python-publish.yml"
RELEASE_DOCUMENTATION = ROOT / "docs/creating-a-release.html"
RELEASE_LEAF_WORKFLOWS = (
    (RUST_RELEASE_WORKFLOW, "publish"),
    (JAVA_RELEASE_WORKFLOW, "package-java"),
    (PYTHON_PUBLISH_WORKFLOW, "publish"),
)
TAG_CONDITION = "startsWith(github.ref, 'refs/tags/')"
RUST_PUBLISH_CONDITION = (
    "github.event_name != 'workflow_dispatch' && "
    "github.repository == 'apache/paimon-mosaic' && "
    "startsWith(github.ref, 'refs/tags/') && "
    "!contains(github.ref_name, '-')"
)
JAVA_PACKAGE_CONDITION = (
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
    ".gitattributes",
    ".github/workflows/**",
    "docs/creating-a-release.html",
    "docs/verifying-a-release-candidate.html",
    "java/pom.xml",
    "tools/create_source_release.sh",
    "tools/deploy_java_staging.sh",
    "tools/java-staging-maven-plugins.sha256",
    "tools/prepare_java_staging_maven_plugins.py",
    "tools/update_branch_version.sh",
    "tools/validate_java_staging_artifacts.sh",
    "tools/verify_release_versions.py",
    "tools/verify_source_archive.py",
    "tools/tests/test_create_source_release.py",
    "tools/tests/deploy_java_staging_test.sh",
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
EXPORT_RELEASE_PROVENANCE_COMMAND = """set -euo pipefail
tag_object="$(git rev-parse -q --verify "refs/tags/${TAG_NAME}^{tag}")"
commit="$(git rev-parse "${tag_object}^{commit}")"
if [[ "${commit}" != "${GITHUB_SHA}" ||
      "$(git rev-parse HEAD)" != "${GITHUB_SHA}" ]]; then
  echo "Verified release tag does not match GITHUB_SHA." >&2
  exit 1
fi
printf 'tag_object=%s\\n' "${tag_object}" >> "${GITHUB_OUTPUT}"
printf 'commit=%s\\n' "${commit}" >> "${GITHUB_OUTPUT}"
"""
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
GATE_JAVA_STAGING_COMMAND = "bash tools/tests/deploy_java_staging_test.sh"
JAVA_CANDIDATE_PATHS = """${{ runner.temp }}/java-package/mosaic-${{ steps.java-candidate.outputs.version }}.jar
${{ runner.temp }}/java-package/mosaic-${{ steps.java-candidate.outputs.version }}-sources.jar
${{ runner.temp }}/java-package/mosaic-${{ steps.java-candidate.outputs.version }}-javadoc.jar
${{ runner.temp }}/java-package/java-staging-provenance.txt
"""
GATE_SOURCE_TREE_COMMAND = """set -euo pipefail
output_dir="$(mktemp -d "${RUNNER_TEMP}/source-tree.XXXXXX")"
archive="${output_dir}/source.tgz"
commit="$(git rev-parse --verify 'HEAD^{commit}')"
prefix="paimon-mosaic-source-tree/"
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
GATE_STATIC_COMMAND = """set -euo pipefail
python -m compileall -q \\
  tools/prepare_java_staging_maven_plugins.py \\
  tools/verify_release_versions.py \\
  tools/verify_source_archive.py \\
  tools/tests/test_create_source_release.py \\
  tools/tests/test_release_vote_workflow.py \\
  tools/tests/test_update_branch_version.py \\
  tools/tests/test_verify_release_versions.py \\
  tools/tests/test_verify_source_archive.py
bash -n tools/create_source_release.sh
bash -n tools/deploy_java_staging.sh
bash -n tools/update_branch_version.sh
bash -n tools/validate_java_staging_artifacts.sh
bash -n tools/tests/deploy_java_staging_test.sh
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


def java_package_step(workflow: dict, name: str) -> dict:
    steps = workflow["jobs"]["package-java"]["steps"]
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
    staging_step = gate_step(workflow, "Test local Java staging")
    source_tree_step = gate_step(workflow, "Verify current source tree")
    static_step = gate_step(workflow, "Run static checks")
    for step in (
        install_step,
        test_step,
        staging_step,
        source_tree_step,
        static_step,
    ):
        assert "if" not in step
        assert "continue-on-error" not in step

    assert install_step["run"] == "python -m pip install pytest PyYAML"
    assert test_step["run"] == GATE_TEST_COMMAND
    assert staging_step["shell"] == "bash"
    assert staging_step["run"] == GATE_JAVA_STAGING_COMMAND
    assert source_tree_step["shell"] == "bash"
    assert source_tree_step["run"] == GATE_SOURCE_TREE_COMMAND
    assert static_step["shell"] == "bash"
    assert static_step["run"] == GATE_STATIC_COMMAND


def assert_release_contract(workflow: dict) -> None:
    assert workflow["concurrency"]["cancel-in-progress"] == "false"

    jobs = workflow["jobs"]
    preflight = jobs["release-preflight"]
    assert preflight["uses"] == "./.github/workflows/release-preflight.yml"
    assert "runs-on" not in preflight
    assert "continue-on-error" not in preflight

    for job_name in ("rust", "python-wheels", "python-publish"):
        assert jobs[job_name].get("secrets") == "inherit"
    assert "secrets" not in jobs["java"]

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


def assert_release_preflight_contract(workflow: dict) -> None:
    workflow_call = workflow["on"]["workflow_call"]
    assert workflow_call["outputs"] == {
        "tag_object": {
            "description": "Verified annotated release tag object.",
            "value": "${{ jobs.release-preflight.outputs.tag_object }}",
        },
        "commit": {
            "description": "Commit referenced by the verified release tag.",
            "value": "${{ jobs.release-preflight.outputs.commit }}",
        },
    }
    assert workflow["permissions"]["contents"] == "read"

    preflight = workflow["jobs"]["release-preflight"]
    assert "runs-on" in preflight
    assert "uses" not in preflight
    assert "continue-on-error" not in preflight
    assert preflight["outputs"] == {
        "tag_object": "${{ steps.release-provenance.outputs.tag_object }}",
        "commit": "${{ steps.release-provenance.outputs.commit }}",
    }
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

    provenance_step = named_step(workflow, "Export verified release provenance")
    assert provenance_step["id"] == "release-provenance"
    assert provenance_step["if"] == TAG_CONDITION
    assert provenance_step["env"] == {
        "TAG_NAME": "${{ github.ref_name }}",
    }
    assert "continue-on-error" not in provenance_step
    assert provenance_step["run"] == EXPORT_RELEASE_PROVENANCE_COMMAND

    source_step = named_step(workflow, "Verify source archive")
    assert source_step["if"] == TAG_CONDITION
    assert "continue-on-error" not in source_step
    assert source_step["run"] == VERIFY_SOURCE_ARCHIVE_COMMAND

    steps = preflight["steps"]
    assert steps.index(version_step) < steps.index(provenance_step)
    assert steps.index(provenance_step) < steps.index(source_step)


def assert_leaf_release_contract(
    workflow: dict,
    required_job: str,
) -> None:
    assert "workflow_call" in workflow["on"]
    assert workflow["jobs"]["release-preflight"]["uses"] == (
        "./.github/workflows/release-preflight.yml"
    )
    for job_name, job in workflow["jobs"].items():
        if job_name != "release-preflight":
            assert "release-preflight" in needs(job)
    assert required_job in workflow["jobs"]


def test_release_has_signed_exact_tag_preflight() -> None:
    release = load_workflow(RELEASE_WORKFLOW)
    preflight = load_workflow(RELEASE_PREFLIGHT_WORKFLOW)
    assert_release_contract(release)
    assert_release_preflight_contract(preflight)

    triggers = release["on"]
    assert "workflow_dispatch" in triggers
    version_step = release_version_step(preflight)
    assert version_step["if"] == TAG_CONDITION
    assert "github.ref_name" in version_step["env"]["TAG_NAME"]
    assert "github.ref_name" not in version_step["run"]


def test_release_preflight_is_reusable_and_called_by_orchestrator() -> None:
    preflight = load_workflow(RELEASE_PREFLIGHT_WORKFLOW)
    release = load_workflow(RELEASE_WORKFLOW)

    assert "workflow_call" in preflight["on"]
    assert release["jobs"]["release-preflight"]["uses"] == (
        "./.github/workflows/release-preflight.yml"
    )


def test_release_preflight_does_not_use_private_key_secret() -> None:
    assert "GPG_SECRET_KEY" not in RELEASE_PREFLIGHT_WORKFLOW.read_text(
        encoding="utf-8"
    )


def test_release_tag_context_is_not_interpolated_into_shell() -> None:
    workflow = load_workflow(RELEASE_PREFLIGHT_WORKFLOW)
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


def assert_java_release_contract(workflow: dict) -> None:
    build_native_job = workflow["jobs"]["build-native"]
    package_job = workflow["jobs"]["package-java"]

    assert needs(build_native_job) == {"release-preflight"}
    build_native_checkout_steps = [
        step
        for step in build_native_job["steps"]
        if step.get("uses") == "actions/checkout@v6"
    ]
    assert len(build_native_checkout_steps) == 1
    assert build_native_checkout_steps[0]["with"] == {
        "ref": "${{ needs.release-preflight.outputs.commit }}",
    }

    assert package_job["if"] == JAVA_PACKAGE_CONDITION
    assert needs(package_job) == {"release-preflight", "build-native"}
    assert "continue-on-error" not in package_job

    checkout_steps = [
        step
        for step in package_job["steps"]
        if step.get("uses") == "actions/checkout@v6"
    ]
    assert len(checkout_steps) == 1
    assert checkout_steps[0]["with"] == {
        "ref": "${{ needs.release-preflight.outputs.commit }}",
        "fetch-depth": "0",
    }

    package_step = java_package_step(workflow, "Package Java artifacts")
    assert package_step["working-directory"] == "java"
    assert package_step["run"] == (
        "mvn clean verify -Prelease -Dgpg.skip=true -DskipTests"
    )

    candidate_step = java_package_step(
        workflow,
        "Freeze exact Java candidate",
    )
    assert candidate_step["id"] == "java-candidate"
    assert candidate_step["env"] == {
        "TAG_NAME": "${{ github.ref_name }}",
        "VERIFIED_TAG_OBJECT": (
            "${{ needs.release-preflight.outputs.tag_object }}"
        ),
        "VERIFIED_COMMIT": "${{ needs.release-preflight.outputs.commit }}",
    }
    candidate_run = candidate_step["run"]
    for required in (
        'candidate_dir="${RUNNER_TEMP}/java-package"',
        '"mosaic-${version}.jar"',
        '"mosaic-${version}-sources.jar"',
        '"mosaic-${version}-javadoc.jar"',
        '"${commit}" != "${GITHUB_SHA}"',
        '"$(git rev-parse HEAD)" != "${GITHUB_SHA}"',
        "printf 'repository=%s\\n' \"${GITHUB_REPOSITORY}\"",
        "printf 'tag=%s\\n' \"${TAG_NAME}\"",
        "printf 'tag_object=%s\\n' \"${tag_object}\"",
        "printf 'commit=%s\\n' \"${commit}\"",
        "printf 'run_id=%s\\n' \"${GITHUB_RUN_ID}\"",
        "printf 'run_attempt=%s\\n' \"${GITHUB_RUN_ATTEMPT}\"",
        '} > "${candidate_dir}/java-staging-provenance.txt"',
        "./tools/validate_java_staging_artifacts.sh \\\n"
        '  "${candidate_dir}" \\\n'
        '  "${version}"',
        "printf 'version=%s\\n' \"${version}\" >> \"${GITHUB_OUTPUT}\"",
    ):
        assert required in candidate_run
    assert 'refs/tags/${TAG_NAME}' not in candidate_run

    upload_step = java_package_step(workflow, "Upload Java artifacts")
    assert upload_step["uses"] == "actions/upload-artifact@v5"
    assert upload_step["with"] == {
        "name": "java-package",
        "path": JAVA_CANDIDATE_PATHS,
        "if-no-files-found": "error",
    }
    steps = package_job["steps"]
    assert steps.index(package_step) < steps.index(candidate_step)
    assert steps.index(candidate_step) < steps.index(upload_step)


def test_java_workflow_freezes_exact_unsigned_candidate() -> None:
    workflow = load_workflow(JAVA_RELEASE_WORKFLOW)
    assert_java_release_contract(workflow)


def test_java_release_never_receives_signing_or_nexus_credentials() -> None:
    workflow_text = JAVA_RELEASE_WORKFLOW.read_text(encoding="utf-8")
    assert "secrets." not in workflow_text
    for forbidden in (
        "GPG_SECRET_KEY",
        "GPG_PASSPHRASE",
        "NEXUS_STAGE_DEPLOYER_USER",
        "NEXUS_STAGE_DEPLOYER_PW",
        "mvn clean deploy",
    ):
        assert forbidden not in workflow_text

    release = load_workflow(RELEASE_WORKFLOW)
    assert "secrets" not in release["jobs"]["java"]


def test_java_pom_does_not_control_candidate_validation_or_deploy_order() -> None:
    root = ET.parse(JAVA_POM).getroot()
    namespace = {"m": "http://maven.apache.org/POM/4.0.0"}
    plugin_ids = [
        plugin.findtext("m:artifactId", namespaces=namespace)
        for plugin in root.findall(".//m:plugin", namespace)
    ]
    assert "exec-maven-plugin" not in plugin_ids

    source = JAVA_POM.read_text(encoding="utf-8")
    for forbidden in (
        "staging-artifact-validation",
        "validate-staging-artifacts",
        "stagingValidationScript",
        "stagingReferenceDirectory",
    ):
        assert forbidden not in source


@pytest.mark.parametrize(
    "mutation",
    (
        "native-mutable-checkout",
        "mutable-checkout",
        "shallow-checkout",
        "mutable-tag-object",
        "mutable-commit",
        "missing-run-attempt",
        "missing-validator",
        "wildcard-upload",
        "extra-upload",
        "upload-before-validation",
    ),
)
def test_java_candidate_contract_rejects_mutations(mutation: str) -> None:
    workflow = copy.deepcopy(load_workflow(JAVA_RELEASE_WORKFLOW))
    build_native_job = workflow["jobs"]["build-native"]
    package_job = workflow["jobs"]["package-java"]
    build_native_checkout = next(
        step
        for step in build_native_job["steps"]
        if step.get("uses") == "actions/checkout@v6"
    )
    checkout = next(
        step
        for step in package_job["steps"]
        if step.get("uses") == "actions/checkout@v6"
    )
    candidate = java_package_step(workflow, "Freeze exact Java candidate")
    upload = java_package_step(workflow, "Upload Java artifacts")

    if mutation == "native-mutable-checkout":
        build_native_checkout["with"]["ref"] = "main"
    elif mutation == "mutable-checkout":
        checkout["with"]["ref"] = "${{ github.ref }}"
    elif mutation == "shallow-checkout":
        checkout["with"]["fetch-depth"] = "1"
    elif mutation == "mutable-tag-object":
        candidate["env"]["VERIFIED_TAG_OBJECT"] = "${{ github.sha }}"
    elif mutation == "mutable-commit":
        candidate["env"]["VERIFIED_COMMIT"] = "${{ github.sha }}"
    elif mutation == "missing-run-attempt":
        candidate["run"] = candidate["run"].replace(
            "  printf 'run_attempt=%s\\n' \"${GITHUB_RUN_ATTEMPT}\"\n",
            "",
        )
    elif mutation == "missing-validator":
        candidate["run"] = candidate["run"].replace(
            "./tools/validate_java_staging_artifacts.sh \\\n"
            '  "${candidate_dir}" \\\n'
            '  "${version}"',
            "true",
        )
    elif mutation == "wildcard-upload":
        upload["with"]["path"] = "${{ runner.temp }}/java-package/*"
    elif mutation == "extra-upload":
        upload["with"]["path"] += "java/pom.xml\n"
    elif mutation == "upload-before-validation":
        steps = package_job["steps"]
        steps.remove(upload)
        steps.insert(steps.index(candidate), upload)
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        assert_java_release_contract(workflow)


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


@pytest.mark.parametrize(
    ("workflow_path", "release_job"),
    RELEASE_LEAF_WORKFLOWS,
)
def test_release_leaf_jobs_require_reusable_preflight(
    workflow_path: Path,
    release_job: str,
) -> None:
    workflow = load_workflow(workflow_path)

    assert_leaf_release_contract(workflow, release_job)


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


@pytest.mark.parametrize(
    "job_name",
    ("rust", "python-wheels", "python-publish"),
)
def test_contract_rejects_missing_secret_inheritance(job_name: str) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    workflow["jobs"][job_name].pop("secrets")

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


def test_contract_rejects_java_secret_inheritance() -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
    workflow["jobs"]["java"]["secrets"] = "inherit"

    with pytest.raises(AssertionError):
        assert_release_contract(workflow)


@pytest.mark.parametrize(
    ("workflow_path", "release_job"),
    RELEASE_LEAF_WORKFLOWS,
)
@pytest.mark.parametrize(
    "mutation",
    ("missing-workflow-call", "missing-dependency", "unguarded-extra-job"),
)
def test_release_leaf_contract_rejects_preflight_bypass(
    workflow_path: Path,
    release_job: str,
    mutation: str,
) -> None:
    workflow = copy.deepcopy(load_workflow(workflow_path))
    if mutation == "missing-workflow-call":
        workflow["on"].pop("workflow_call")
    elif mutation == "missing-dependency":
        job = workflow["jobs"][release_job]
        job["needs"] = [
            dependency
            for dependency in job.get("needs", [])
            if dependency != "release-preflight"
        ]
    elif mutation == "unguarded-extra-job":
        workflow["jobs"]["alternate-publish"] = {
            "runs-on": "ubuntu-latest",
            "steps": [{"run": "echo publish"}],
        }
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        assert_leaf_release_contract(workflow, release_job)


def test_contract_rejects_masked_release_verifier_failure() -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_PREFLIGHT_WORKFLOW))
    release_version_step(workflow)["run"] += " || true"

    with pytest.raises(AssertionError):
        assert_release_preflight_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    ("missing-step", "masked-command", "step-continue", "wrong-commit"),
)
def test_contract_rejects_source_archive_preflight_mutations(
    mutation: str,
) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_PREFLIGHT_WORKFLOW))
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
        assert_release_preflight_contract(workflow)


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
    workflow = copy.deepcopy(load_workflow(RELEASE_PREFLIGHT_WORKFLOW))
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
        assert_release_preflight_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    (
        "missing-call-output",
        "wrong-call-output",
        "missing-job-output",
        "wrong-job-output",
        "wrong-step-id",
        "missing-sha-binding",
        "export-before-signature-check",
    ),
)
def test_contract_rejects_release_provenance_output_mutations(
    mutation: str,
) -> None:
    workflow = copy.deepcopy(load_workflow(RELEASE_PREFLIGHT_WORKFLOW))
    workflow_call = workflow["on"]["workflow_call"]
    preflight = workflow["jobs"]["release-preflight"]
    provenance = named_step(workflow, "Export verified release provenance")

    if mutation == "missing-call-output":
        workflow_call["outputs"].pop("tag_object")
    elif mutation == "wrong-call-output":
        workflow_call["outputs"]["tag_object"]["value"] = "${{ github.sha }}"
    elif mutation == "missing-job-output":
        preflight["outputs"].pop("commit")
    elif mutation == "wrong-job-output":
        preflight["outputs"]["commit"] = "${{ github.sha }}"
    elif mutation == "wrong-step-id":
        provenance["id"] = "mutable-provenance"
    elif mutation == "missing-sha-binding":
        provenance["run"] = provenance["run"].replace(
            'if [[ "${commit}" != "${GITHUB_SHA}" ||\n'
            '      "$(git rev-parse HEAD)" != "${GITHUB_SHA}" ]]; then\n',
            "if false; then\n",
        )
    elif mutation == "export-before-signature-check":
        steps = preflight["steps"]
        steps.remove(provenance)
        steps.insert(steps.index(release_version_step(workflow)), provenance)
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        assert_release_preflight_contract(workflow)


@pytest.mark.parametrize(
    "mutation",
    ("cancel-in-progress", "preflight-continue", "version-step-continue"),
)
def test_contract_rejects_non_blocking_preflight_mutations(mutation: str) -> None:
    if mutation == "cancel-in-progress":
        workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
        workflow["concurrency"]["cancel-in-progress"] = "true"
        contract = assert_release_contract
    elif mutation == "preflight-continue":
        workflow = copy.deepcopy(load_workflow(RELEASE_WORKFLOW))
        workflow["jobs"]["release-preflight"]["continue-on-error"] = "true"
        contract = assert_release_contract
    elif mutation == "version-step-continue":
        workflow = copy.deepcopy(load_workflow(RELEASE_PREFLIGHT_WORKFLOW))
        release_version_step(workflow)["continue-on-error"] = "true"
        contract = assert_release_preflight_contract
    else:
        raise AssertionError(f"unknown mutation: {mutation}")

    with pytest.raises(AssertionError):
        contract(workflow)


def test_source_release_documentation_passes_rc_tag_explicitly() -> None:
    source = RELEASE_DOCUMENTATION.read_text(encoding="utf-8")
    invocation = (
        "RELEASE_VERSION=${RELEASE_VERSION} "
        "RC_TAG=${RC_TAG} ./create_source_release.sh"
    )
    assert invocation in source

    assert "Cargo path dependency constraints and Cargo.lock" in source


def test_release_documentation_keeps_java_credentials_local() -> None:
    source = RELEASE_DOCUMENTATION.read_text(encoding="utf-8")
    for forbidden in (
        "NEXUS_STAGE_DEPLOYER_USER",
        "NEXUS_STAGE_DEPLOYER_PW",
        "GPG_SECRET_KEY",
        "GPG_PASSPHRASE",
    ):
        assert forbidden not in source

    staging_command = """./tools/deploy_java_staging.sh \\
  --release-version ${RELEASE_VERSION} \\
  --rc ${RC_NUM} \\
  --run-id ${RELEASE_RUN_ID} \\
  --provenance-manifest ${JAVA_STAGING_PROVENANCE} \\
  --staging-profile-id ${STAGING_PROFILE_ID}"""
    assert f"{staging_command} \\\n  --dry-run" in source
    assert staging_command in source
    assert (
        'JAVA_STAGING_PROVENANCE="../${RC_TAG}-java-staging.provenance"'
        in source
    )
    assert 'STAGING_PROFILE_ID="PAIMON_NEXUS_STAGING_PROFILE_ID"' in source
    assert "https://repository.apache.org/#stagingProfiles" in source
    assert "nexus-staging-maven-plugin:1.7.0:rc-list-profiles" not in source
    assert "maven-gpg-plugin:3.2.8:sign-and-deploy-file" in source
    assert (
        "nexus-staging-maven-plugin:1.7.0:deploy-staged-repository"
        in source
    )
    assert "It does not rebuild the JARs or run a Maven lifecycle." in source
    assert "verifies all four detached signatures" in source
    assert "earlier producer attempt than the final successful run attempt" in source
    assert "local GPG keyring" in source
    assert "apache.releases.https" in source


def test_release_documentation_describes_source_archive_preflight() -> None:
    source = RELEASE_DOCUMENTATION.read_text(encoding="utf-8")
    assert "temporary source archive" in source


def test_release_documentation_preserves_previous_rc_output_before_retry() -> None:
    source = RELEASE_DOCUMENTATION.read_text(encoding="utf-8")
    retry_section = source.partition('<h2 id="fix-any-issues">')[2].partition(
        '<h2 id="finalize-the-release">'
    )[0]
    preserve_command = (
        'mv tools/release "../paimon-mosaic-${RC_TAG}-artifacts"'
    )

    assert preserve_command in retry_section
    assert retry_section.index(preserve_command) < retry_section.index(
        "Increment <code>RC_NUM</code>"
    )


def test_release_vote_gate_matches_blocking_contract() -> None:
    workflow = load_workflow(GATE_WORKFLOW)
    assert_gate_contract(workflow)


@pytest.mark.parametrize("event", ("pull_request", "push"))
def test_release_vote_gate_covers_all_workflows_and_archive_attributes(
    event: str,
) -> None:
    workflow = load_workflow(GATE_WORKFLOW)
    paths = set(workflow["on"][event]["paths"])

    assert ".github/workflows/**" in paths
    assert ".gitattributes" in paths


def test_release_vote_gate_verifies_current_source_tree() -> None:
    workflow = load_workflow(GATE_WORKFLOW)
    step = gate_step(workflow, "Verify current source tree")

    assert "if" not in step
    assert "continue-on-error" not in step
    assert step["shell"] == "bash"
    assert step["run"] == GATE_SOURCE_TREE_COMMAND


@pytest.mark.parametrize("event", ("pull_request", "push"))
@pytest.mark.parametrize("required_path", (".github/workflows/**", ".gitattributes"))
def test_gate_contract_rejects_missing_release_input_path(
    event: str,
    required_path: str,
) -> None:
    workflow = copy.deepcopy(load_workflow(GATE_WORKFLOW))
    workflow["on"][event]["paths"].remove(required_path)

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
    ("masked-command", "step-continue", "step-condition", "wrong-commit"),
)
def test_gate_contract_rejects_source_tree_check_mutations(
    mutation: str,
) -> None:
    workflow = copy.deepcopy(load_workflow(GATE_WORKFLOW))
    source_step = gate_step(workflow, "Verify current source tree")
    if mutation == "masked-command":
        source_step["run"] += " || true"
    elif mutation == "step-continue":
        source_step["continue-on-error"] = "true"
    elif mutation == "step-condition":
        source_step["if"] = "failure()"
    elif mutation == "wrong-commit":
        source_step["run"] = source_step["run"].replace(
            '--commit "${commit}"',
            "--commit HEAD^",
        )
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
