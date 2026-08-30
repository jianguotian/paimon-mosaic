#!/usr/bin/env bash

# Licensed to the Apache Software Foundation (ASF) under one or more
# contributor license agreements. See the NOTICE file distributed with
# this work for additional information regarding copyright ownership.
# The ASF licenses this file to You under the Apache License, Version 2.0
# (the "License"); you may not use this file except in compliance with
# the License. You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

set -o errexit
set -o nounset
set -o pipefail

TEST_SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TOOLS_DIR=$(cd "$TEST_SCRIPT_DIR/.." && pwd)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/paimon-mosaic-staging-test.XXXXXX")
TEST_COUNT=0

cleanup() {
  case "$TEST_ROOT" in
    "${TMPDIR:-/tmp}"/paimon-mosaic-staging-test.*)
      rm -rf -- "$TEST_ROOT"
      ;;
    *)
      echo "Refusing to remove unexpected test path: $TEST_ROOT" >&2
      ;;
  esac
}
trap cleanup EXIT

fail() {
  echo "FAIL: $1" >&2
  exit 1
}

assert_contains() {
  local file=$1
  local pattern=$2
  if ! grep -Fq -- "$pattern" "$file"; then
    echo "Expected '$pattern' in $file" >&2
    sed -n '1,200p' "$file" >&2
    fail "missing expected output"
  fi
}

assert_not_contains() {
  local file=$1
  local pattern=$2
  if [[ -f "$file" ]] && grep -Fq -- "$pattern" "$file"; then
    echo "Did not expect '$pattern' in $file" >&2
    sed -n '1,200p' "$file" >&2
    fail "unexpected output"
  fi
}

assert_maven_not_invoked() {
  if [[ -s "$MAVEN_LOG" ]]; then
    sed -n '1,200p' "$MAVEN_LOG" >&2
    fail "Maven must not be invoked"
  fi
}

new_fixture() {
  FIXTURE_DIR=$(mktemp -d "$TEST_ROOT/fixture.XXXXXX")
  MAVEN_LOG="$FIXTURE_DIR/maven.log"
  GH_LOG="$FIXTURE_DIR/gh.log"
  OUTPUT_LOG="$FIXTURE_DIR/output.log"
  export FIXTURE_DIR MAVEN_LOG GH_LOG OUTPUT_LOG

  mkdir -p \
    "$FIXTURE_DIR/fake-bin" \
    "$FIXTURE_DIR/java/src/main/resources" \
    "$FIXTURE_DIR/tools"

  cp "$TOOLS_DIR/deploy_java_staging.sh" "$FIXTURE_DIR/tools/"
  cp "$TOOLS_DIR/validate_java_staging_artifacts.sh" "$FIXTURE_DIR/tools/"
  chmod +x \
    "$FIXTURE_DIR/tools/deploy_java_staging.sh" \
    "$FIXTURE_DIR/tools/validate_java_staging_artifacts.sh"

  cat > "$FIXTURE_DIR/java/pom.xml" <<'EOF'
<project>
  <parent><version>23</version></parent>
  <version>0.3.0</version>
</project>
EOF

  cat > "$FIXTURE_DIR/.gitignore" <<'EOF'
java/target/
java/src/main/resources/native/
*.class
*.log
EOF

  cat > "$FIXTURE_DIR/fake-bin/gh" <<'EOF'
#!/usr/bin/env bash
set -o errexit
set -o nounset
set -o pipefail

printf 'args=%s\n' "$*" >> "$FAKE_GH_LOG"
printf 'host=%s\n' "${GH_HOST:-}" >> "$FAKE_GH_LOG"

if [[ "${1-} ${2-}" == "run view" ]]; then
  printf '%s\n%s\n%s\n%s\n%s\n%s\n' \
    "${FAKE_RUN_STATUS:-completed}" \
    "${FAKE_RUN_CONCLUSION:-success}" \
    "${FAKE_RUN_SHA:-$(git rev-parse HEAD)}" \
    "${FAKE_RUN_REF:-v0.3.0-rc1}" \
    "${FAKE_WORKFLOW_NAME:-Release}" \
    "${FAKE_RUN_EVENT:-push}"
  exit 0
fi

if [[ "${1-} ${2-}" == "run download" ]]; then
  artifact=
  destination=
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --name) artifact=$2; shift 2 ;;
      --dir) destination=$2; shift 2 ;;
      *) shift ;;
    esac
  done
  mkdir -p "$destination"
  if [[ "${FAKE_MISSING_ARTIFACT:-}" == "$artifact" ]]; then
    exit 0
  fi
  case "$artifact" in
    native-linux-x86_64|native-linux-aarch64)
      file=libpaimon_mosaic_jni.so
      ;;
    native-macos-aarch64)
      file=libpaimon_mosaic_jni.dylib
      ;;
    native-windows-x86_64)
      file=paimon_mosaic_jni.dll
      ;;
    *)
      exit 2
      ;;
  esac
  printf 'native %s\n' "$artifact" > "$destination/$file"
  exit 0
fi

exit 2
EOF

  cat > "$FIXTURE_DIR/fake-bin/mvn" <<'EOF'
#!/usr/bin/env bash
set -o errexit
set -o nounset
set -o pipefail

{
  printf '%s\n' 'invocation'
  printf 'pwd=%s\n' "$PWD"
  printf 'args=%s\n' "$*"
  printf 'maven-skip-rc=%s\n' "${MAVEN_SKIP_RC:-}"
  printf 'maven-args=%s\n' "${MAVEN_ARGS:-}"
  printf 'maven-opts=%s\n' "${MAVEN_OPTS:-}"
  printf 'maven-config=%s\n' "${MAVEN_CONFIG:-}"
  printf 'maven-basedir=%s\n' "${MAVEN_BASEDIR:-}"
  printf 'maven-project-basedir=%s\n' "${MAVEN_PROJECTBASEDIR:-}"
  printf 'java-tool-options=%s\n' "${JAVA_TOOL_OPTIONS:-}"
  printf 'jdk-java-options=%s\n' "${JDK_JAVA_OPTIONS:-}"
  printf 'underscore-java-options=%s\n' "${_JAVA_OPTIONS:-}"
} >> "$FAKE_MVN_LOG"

if [[ "${FAKE_MAVEN_EXIT_CODE:-0}" -ne 0 ]]; then
  exit "$FAKE_MAVEN_EXIT_CODE"
fi

validation_script=
for argument in "$@"; do
  case "$argument" in
    -DstagingValidationScript=*) validation_script=${argument#*=} ;;
  esac
done

if [[ -z "$validation_script" ]]; then
  echo "Missing stagingValidationScript Maven property" >&2
  exit 2
fi

mkdir -p target
if [[ "${FAKE_MISSING_JAR:-}" != main ]]; then
  printf 'main jar\n' > target/mosaic-0.3.0.jar
fi
if [[ "${FAKE_MISSING_JAR:-}" != sources ]]; then
  printf 'sources jar\n' > target/mosaic-0.3.0-sources.jar
fi
if [[ "${FAKE_MISSING_JAR:-}" != javadoc ]]; then
  printf 'javadoc jar\n' > target/mosaic-0.3.0-javadoc.jar
fi
"$validation_script" "$PWD/target" 0.3.0
EOF

  cat > "$FIXTURE_DIR/fake-bin/jar" <<'EOF'
#!/usr/bin/env bash
set -o errexit
set -o nounset

if [[ "${1-}" != "tf" ]]; then
  exit 2
fi

for entry in \
  native/linux/x86_64/libpaimon_mosaic_jni.so \
  native/linux/aarch64/libpaimon_mosaic_jni.so \
  native/macos/aarch64/libpaimon_mosaic_jni.dylib \
  native/windows/x86_64/paimon_mosaic_jni.dll
do
  if [[ "${FAKE_MISSING_NATIVE:-}" != "$entry" ]]; then
    printf '%s\n' "$entry"
  fi
done
EOF

  chmod +x \
    "$FIXTURE_DIR/fake-bin/gh" \
    "$FIXTURE_DIR/fake-bin/mvn" \
    "$FIXTURE_DIR/fake-bin/jar"

  git -C "$FIXTURE_DIR" init -q
  git -C "$FIXTURE_DIR" config user.name "Release Script Test"
  git -C "$FIXTURE_DIR" config user.email "release-script-test@example.invalid"
  git -C "$FIXTURE_DIR" add .
  git -C "$FIXTURE_DIR" commit -q -m fixture
  git -C "$FIXTURE_DIR" tag v0.3.0-rc1
  : > "$MAVEN_LOG"
  : > "$GH_LOG"
}

run_script() {
  (
    cd "$FIXTURE_DIR"
    PATH="$FIXTURE_DIR/fake-bin:$(dirname "$BASH"):$PATH" \
      MVN="$FIXTURE_DIR/fake-bin/mvn" \
      FAKE_MVN_LOG="$MAVEN_LOG" \
      FAKE_GH_LOG="$GH_LOG" \
      TMPDIR="$TEST_ROOT" \
      "$BASH" ./tools/deploy_java_staging.sh \
        --release-version 0.3.0 \
        --rc 1 \
        --run-id 42 \
        "$@"
  )
}

test_missing_option_value_never_runs_maven() {
  new_fixture
  if run_script --maven-settings --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "missing Maven settings value was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "requires a value that is not another option"
  assert_maven_not_invoked
}

test_workflow_run_must_be_successful_release_tag_push() {
  new_fixture
  if FAKE_WORKFLOW_NAME=Other run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "run from another workflow was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "expected 'Release'"
  assert_maven_not_invoked

  : > "$GH_LOG"
  if FAKE_RUN_EVENT=workflow_dispatch run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "manually dispatched run was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "expected a tag push"
  assert_maven_not_invoked

  : > "$GH_LOG"
  if FAKE_RUN_CONCLUSION=failure run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "failed workflow run was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "not a successful completed run"
  assert_maven_not_invoked
}

test_workflow_run_tag_and_sha_must_match_checkout() {
  new_fixture
  if FAKE_RUN_REF=v0.3.0-rc2 run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "workflow run from another tag was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "expected 'v0.3.0-rc1'"
  assert_maven_not_invoked

  if FAKE_RUN_SHA=0000000000000000000000000000000000000000 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "workflow run from another commit was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "does not match v0.3.0-rc1"
  assert_maven_not_invoked
}

test_downloads_four_native_artifacts_from_exact_run_and_repo() {
  new_fixture
  run_script --repo example/fork --dry-run > "$OUTPUT_LOG" 2>&1

  assert_contains "$GH_LOG" "args=run view 42 --repo example/fork"
  for artifact in \
    native-linux-x86_64 \
    native-linux-aarch64 \
    native-macos-aarch64 \
    native-windows-x86_64
  do
    assert_contains "$GH_LOG" \
      "args=run download 42 --repo example/fork --name $artifact"
  done
  if [[ $(grep -c '^args=run download ' "$GH_LOG") -ne 4 ]]; then
    fail "staging should download exactly four native artifacts"
  fi
}

test_github_host_is_pinned() {
  new_fixture
  GH_HOST=enterprise.example.invalid \
    run_script --repo example/fork --dry-run > "$OUTPUT_LOG" 2>&1

  if grep -Fq 'host=enterprise.example.invalid' "$GH_LOG"; then
    fail "ambient GH_HOST redirected release provenance"
  fi
  assert_contains "$GH_LOG" "host=github.com"
}

test_missing_native_artifact_stops_before_maven() {
  new_fixture
  if FAKE_MISSING_ARTIFACT=native-macos-aarch64 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "missing macOS artifact was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Missing native artifact"
  assert_maven_not_invoked
}

test_dry_run_uses_one_unsigned_verify_lifecycle() {
  new_fixture
  run_script --dry-run > "$OUTPUT_LOG" 2>&1

  assert_contains "$MAVEN_LOG" "args=clean verify -Prelease"
  assert_contains "$MAVEN_LOG" "-Dgpg.skip=true"
  assert_contains "$MAVEN_LOG" "-DstagingValidationScript="
  assert_not_contains "$MAVEN_LOG" "clean deploy"
  if [[ $(grep -c '^invocation$' "$MAVEN_LOG") -ne 1 ]]; then
    fail "dry-run should invoke Maven exactly once"
  fi
}

test_real_deploy_uses_one_validated_signed_lifecycle() {
  new_fixture
  printf '<settings/>\n' > "$FIXTURE_DIR/settings.xml"
  git -C "$FIXTURE_DIR" add settings.xml
  git -C "$FIXTURE_DIR" commit -q -m settings
  git -C "$FIXTURE_DIR" tag -f v0.3.0-rc1 >/dev/null

  run_script --maven-settings settings.xml > "$OUTPUT_LOG" 2>&1

  assert_contains "$MAVEN_LOG" \
    "args=-s $FIXTURE_DIR/settings.xml clean deploy -Prelease"
  assert_contains "$MAVEN_LOG" "-Dgpg.skip=false"
  assert_contains "$MAVEN_LOG" \
    "-DstagingDescription=Apache Paimon Mosaic 0.3.0 RC1"
  assert_contains "$MAVEN_LOG" "-DstagingRepositoryId="
  assert_contains "$MAVEN_LOG" "-DstagingProfileId="
  assert_contains "$MAVEN_LOG" "-DkeepStagingRepositoryOnFailure=false"
  assert_contains "$MAVEN_LOG" \
    "-DkeepStagingRepositoryOnCloseRuleFailure=false"
  if [[ $(grep -c '^invocation$' "$MAVEN_LOG") -ne 1 ]]; then
    fail "real deploy should invoke Maven exactly once"
  fi
}

test_maven_and_jvm_environment_is_scrubbed() {
  new_fixture
  MAVEN_ARGS='-f /tmp/other-pom.xml' \
    MAVEN_OPTS='-DstagingRepositoryId=unexpected' \
    MAVEN_CONFIG='-f=/tmp/other-pom.xml' \
    MAVEN_BASEDIR='/tmp/other-base' \
    MAVEN_PROJECTBASEDIR='/tmp/other-project' \
    JAVA_TOOL_OPTIONS='-DskipNexusStagingDeployMojo=true' \
    JDK_JAVA_OPTIONS='-DskipStaging=true' \
    _JAVA_OPTIONS='-Dmaven.wagon.http.ssl.insecure=true' \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1

  assert_contains "$MAVEN_LOG" "maven-skip-rc=1"
  assert_contains "$MAVEN_LOG" "maven-args="
  assert_contains "$MAVEN_LOG" "maven-opts="
  assert_contains "$MAVEN_LOG" "maven-config="
  assert_contains "$MAVEN_LOG" "maven-project-basedir="
  assert_contains "$MAVEN_LOG" "java-tool-options="
  assert_contains "$MAVEN_LOG" "jdk-java-options="
  assert_contains "$MAVEN_LOG" "underscore-java-options="
  maven_pwd=$(sed -n 's/^pwd=//p' "$MAVEN_LOG")
  maven_basedir=$(sed -n 's/^maven-basedir=//p' "$MAVEN_LOG")
  if [[ "$maven_basedir" != "$maven_pwd" ]]; then
    fail "Maven base directory was not pinned to the Java project"
  fi
  assert_not_contains "$MAVEN_LOG" "unexpected"
  assert_not_contains "$MAVEN_LOG" "other-pom"
  assert_not_contains "$MAVEN_LOG" "skipNexusStagingDeployMojo=true"
  assert_not_contains "$MAVEN_LOG" "skipStaging=true"
  assert_not_contains "$MAVEN_LOG" "ssl.insecure=true"
}

test_validator_rejects_incomplete_main_jar() {
  new_fixture
  missing=native/windows/x86_64/paimon_mosaic_jni.dll
  if FAKE_MISSING_NATIVE="$missing" \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "JAR missing a Windows native entry was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Packaged JAR is missing native entry: $missing"
}

test_validator_requires_sources_and_javadoc_jars() {
  for missing in sources javadoc; do
    new_fixture
    if FAKE_MISSING_JAR="$missing" \
      run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
      fail "missing $missing JAR was accepted"
    fi
    assert_contains "$OUTPUT_LOG" "Expected Maven artifact is missing or empty"
    assert_contains "$OUTPUT_LOG" "mosaic-0.3.0-$missing.jar"
  done
}

test_maven_failure_status_is_preserved() {
  new_fixture
  set +o errexit
  FAKE_MAVEN_EXIT_CODE=42 run_script --dry-run > "$OUTPUT_LOG" 2>&1
  status=$?
  set -o errexit

  if [[ "$status" -ne 42 ]]; then
    fail "Maven exit 42 was not preserved; got $status"
  fi
  assert_not_contains "$OUTPUT_LOG" "finished successfully"
}

test_real_deploy_requires_official_repository_run() {
  new_fixture
  if run_script --repo example/fork > "$OUTPUT_LOG" 2>&1; then
    fail "real deploy accepted a fork workflow run"
  fi
  assert_contains "$OUTPUT_LOG" "requires apache/paimon-mosaic"
  assert_maven_not_invoked
}

test_dirty_release_input_stops_before_download() {
  new_fixture
  printf '\n<!-- local change -->\n' >> "$FIXTURE_DIR/java/pom.xml"
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "dirty Java package input was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "must be clean"
  assert_not_contains "$GH_LOG" "run download"
  assert_maven_not_invoked
}

test_foreign_git_environment_cannot_redirect_checkout_checks() {
  new_fixture
  foreign_repo=$(mktemp -d "$TEST_ROOT/foreign.XXXXXX")
  git -C "$foreign_repo" init -q
  git -C "$foreign_repo" config user.name "Foreign Repository"
  git -C "$foreign_repo" config user.email "foreign@example.invalid"
  printf 'foreign\n' > "$foreign_repo/input"
  git -C "$foreign_repo" add input
  git -C "$foreign_repo" commit -q -m foreign
  git -C "$foreign_repo" tag v0.3.0-rc1

  printf '\n<!-- local change -->\n' >> "$FIXTURE_DIR/java/pom.xml"
  if GIT_DIR="$foreign_repo/.git" \
    GIT_WORK_TREE="$foreign_repo" \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "foreign Git environment hid dirty Java package input"
  fi
  assert_contains "$OUTPUT_LOG" "must be clean"
  assert_not_contains "$GH_LOG" "run download"
  assert_maven_not_invoked
}

test_git_index_flags_are_rejected() {
  new_fixture
  git -C "$FIXTURE_DIR" update-index --assume-unchanged java/pom.xml

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "assume-unchanged package input was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Git index flags"
  assert_not_contains "$GH_LOG" "run download"
  assert_maven_not_invoked
}

test_git_replacement_refs_are_rejected() {
  new_fixture
  first_blob=$(printf 'first\n' | git -C "$FIXTURE_DIR" hash-object -w --stdin)
  second_blob=$(printf 'second\n' | git -C "$FIXTURE_DIR" hash-object -w --stdin)
  git -C "$FIXTURE_DIR" replace "$first_blob" "$second_blob"

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "Git replacement refs were accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Git replacement refs"
  assert_not_contains "$GH_LOG" "run download"
  assert_maven_not_invoked
}

run_test() {
  local name=$1
  "$name"
  TEST_COUNT=$((TEST_COUNT + 1))
  echo "PASS: $name"
}

run_test test_missing_option_value_never_runs_maven
run_test test_workflow_run_must_be_successful_release_tag_push
run_test test_workflow_run_tag_and_sha_must_match_checkout
run_test test_downloads_four_native_artifacts_from_exact_run_and_repo
run_test test_github_host_is_pinned
run_test test_missing_native_artifact_stops_before_maven
run_test test_dry_run_uses_one_unsigned_verify_lifecycle
run_test test_real_deploy_uses_one_validated_signed_lifecycle
run_test test_maven_and_jvm_environment_is_scrubbed
run_test test_validator_rejects_incomplete_main_jar
run_test test_validator_requires_sources_and_javadoc_jars
run_test test_maven_failure_status_is_preserved
run_test test_real_deploy_requires_official_repository_run
run_test test_dirty_release_input_stops_before_download
run_test test_foreign_git_environment_cannot_redirect_checkout_checks
run_test test_git_index_flags_are_rejected
run_test test_git_replacement_refs_are_rejected

echo "All $TEST_COUNT deploy_java_staging tests passed with Bash $BASH_VERSION."
