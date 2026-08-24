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

from pathlib import Path
import xml.etree.ElementTree as ET

import yaml


ROOT = Path(__file__).resolve().parents[2]
ARTIFACT_WORKFLOW = ROOT / ".github/workflows/artifact-verification.yml"
PYTHON_PUBLISH = ROOT / ".github/workflows/release-python-publish.yml"
RELEASE_WORKFLOW = ROOT / ".github/workflows/release.yml"
RELEASE_JAVA_WORKFLOW = ROOT / ".github/workflows/release-java.yml"
POM = ROOT / "java/pom.xml"
NS = {"m": "http://maven.apache.org/POM/4.0.0"}


def test_artifact_workflow_runs_dedicated_tests_and_real_builds():
    text = ARTIFACT_WORKFLOW.read_text(encoding="utf-8")
    yaml.safe_load(text)

    for test_file in (
        "tools/tests/test_artifact_release_workflow.py",
        "tools/tests/test_python_setup.py",
        "tools/tests/test_verify_java_jars.py",
        "tools/tests/test_verify_python_wheels.py",
    ):
        assert test_file in text
    for required in (
        "pytest",
        "pyyaml",
        "python -m compileall",
        "git diff --check",
        "python -m build --wheel",
        "tools/verify_python_wheels.py",
        "mvn",
        "-Prelease",
        "verify",
        "tools/verify_java_jars.py",
    ):
        assert required in text
    assert "native_binary.py" not in text


def test_publish_verifier_is_before_every_publish_action():
    text = PYTHON_PUBLISH.read_text(encoding="utf-8")
    verifier = text.index("python3 tools/verify_python_wheels.py")
    publishers = [
        index
        for index in (
            text.find("pypa/gh-action-pypi-publish", verifier + 1),
            text.rfind("pypa/gh-action-pypi-publish"),
        )
        if index >= 0
    ]

    assert "--require-all-targets" in text
    assert "Unexpected publish artifact" in text
    assert len(publishers) == 2
    assert all(verifier < publisher for publisher in publishers)


def test_release_profile_verifies_three_real_jars_during_verify():
    document = ET.parse(POM).getroot()
    release = next(
        profile
        for profile in document.findall("m:profiles/m:profile", NS)
        if profile.findtext("m:id", namespaces=NS) == "release"
    )
    plugins = release.findall("m:build/m:plugins/m:plugin", NS)
    artifact_ids = [
        plugin.findtext("m:artifactId", namespaces=NS) for plugin in plugins
    ]
    exec_plugin = plugins[artifact_ids.index("exec-maven-plugin")]
    verifier_execution = next(
        item
        for item in exec_plugin.findall("m:executions/m:execution", NS)
        if item.findtext("m:id", namespaces=NS) == "verify-release-jars"
    )
    gpg_plugin = plugins[artifact_ids.index("maven-gpg-plugin")]
    signer_execution = next(
        item
        for item in gpg_plugin.findall("m:executions/m:execution", NS)
        if item.findtext("m:id", namespaces=NS) == "sign-artifacts"
    )
    arguments = [
        argument.text
        for argument in verifier_execution.findall(
            "m:configuration/m:arguments/m:argument", NS
        )
    ]

    assert verifier_execution.findtext("m:phase", namespaces=NS) == "verify"
    assert signer_execution.findtext("m:phase", namespaces=NS) == "verify"
    assert artifact_ids.index("exec-maven-plugin") < artifact_ids.index(
        "maven-gpg-plugin"
    )
    assert arguments == [
        "tools/verify_java_jars.py",
        "--main",
        "${project.build.directory}/${project.build.finalName}.jar",
        "--sources",
        "${project.build.directory}/${project.build.finalName}-sources.jar",
        "--javadoc",
        "${project.build.directory}/${project.build.finalName}-javadoc.jar",
    ]


def test_release_workflow_keeps_publish_and_java_release_consumers():
    release = yaml.safe_load(RELEASE_WORKFLOW.read_text(encoding="utf-8"))
    python_publish = release["jobs"]["python-publish"]

    assert python_publish["needs"] == ["rust", "java", "python-wheels"]
    assert python_publish["if"] == "startsWith(github.ref, 'refs/tags/')"

    release_java = yaml.safe_load(
        RELEASE_JAVA_WORKFLOW.read_text(encoding="utf-8")
    )
    deploy_steps = release_java["jobs"]["deploy-staging"]["steps"]
    deploy_command = next(
        step["run"]
        for step in deploy_steps
        if "run" in step and "mvn clean deploy" in step["run"]
    )
    assert "-Prelease" in deploy_command


def test_pom_packages_binary_and_classifier_legal_resources():
    text = POM.read_text(encoding="utf-8")

    assert "copy-classifier-legal-resources" in text
    assert "maven-shared-archive-resources/META-INF" in text
    assert "copy-binary-only-legal-resources" in text
    assert "src/main/binary-resources" in text
    assert "${project.build.outputDirectory}" in text


def test_general_native_parser_is_not_reintroduced():
    assert not (ROOT / "tools/native_binary.py").exists()
