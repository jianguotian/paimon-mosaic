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

# Do not let inherited Git variables redirect the release checkout or its
# object/configuration sources.
unset \
  GIT_ALTERNATE_OBJECT_DIRECTORIES \
  GIT_COMMON_DIR \
  GIT_CONFIG_COUNT \
  GIT_CONFIG_GLOBAL \
  GIT_CONFIG_PARAMETERS \
  GIT_CONFIG_SYSTEM \
  GIT_DIR \
  GIT_INDEX_FILE \
  GIT_NAMESPACE \
  GIT_OBJECT_DIRECTORY \
  GIT_WORK_TREE
export GIT_ATTR_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_NOSYSTEM=1
export GIT_NO_REPLACE_OBJECTS=1

MVN=${MVN:-mvn}
GPG=${GPG:-gpg}
CURL=${CURL:-curl}

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd "$SCRIPT_DIR/.." && pwd)

RELEASE_VERSION=
RC_NUMBER=
RUN_ID=
REPOSITORY=apache/paimon-mosaic
MAVEN_SETTINGS=
GPG_KEYNAME=
KEYS_FILE=
DRY_RUN=false
GITHUB_HOST=github.com

usage() {
  cat <<'EOF'
Usage:
  deploy_java_staging.sh --release-version VERSION --rc N --run-id RUN_ID [options]

Validate the exact successful RC-tag Release workflow, download its four JNI
libraries, then build and optionally deploy the signed Java artifacts from the
Release Manager's machine.

Required:
  --release-version VERSION  Release version, for example 0.3.0.
  --rc N                     RC number, for example 1.
  --run-id RUN_ID            Successful RC-tag Release workflow run id.

Options:
  --repo OWNER/REPO          Defaults to apache/paimon-mosaic.
  --maven-settings FILE      Maven settings.xml containing Nexus credentials.
  --gpg-keyname FINGERPRINT  Full OpenPGP signing-key fingerprint.
  --keys-file FILE           ASF Paimon KEYS file; otherwise download it.
  --dry-run                  Verify locally without signing or deploying.
  -h, --help                 Show this help.

Credentials remain local. Configure server id apache.releases.https in Maven
settings and the signing key in the local GPG keyring.
EOF
}

require_option_value() {
  local option=$1
  local value=${2-}
  if [[ $# -lt 2 || -z "$value" || "$value" == -* ]]; then
    echo "$option requires a value that is not another option" >&2
    usage >&2
    exit 1
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release-version)
      require_option_value "$@"
      RELEASE_VERSION=$2
      shift 2
      ;;
    --rc)
      require_option_value "$@"
      RC_NUMBER=$2
      shift 2
      ;;
    --run-id)
      require_option_value "$@"
      RUN_ID=$2
      shift 2
      ;;
    --repo)
      require_option_value "$@"
      REPOSITORY=$2
      shift 2
      ;;
    --maven-settings)
      require_option_value "$@"
      MAVEN_SETTINGS=$2
      shift 2
      ;;
    --gpg-keyname)
      require_option_value "$@"
      GPG_KEYNAME=$2
      shift 2
      ;;
    --keys-file)
      require_option_value "$@"
      KEYS_FILE=$2
      shift 2
      ;;
    --dry-run)
      DRY_RUN=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

require_value() {
  local option=$1
  local value=$2
  if [[ -z "$value" ]]; then
    echo "$option is required" >&2
    usage >&2
    exit 1
  fi
}

require_value "--release-version" "$RELEASE_VERSION"
require_value "--rc" "$RC_NUMBER"
require_value "--run-id" "$RUN_ID"

if [[ ! "$RELEASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "--release-version must use X.Y.Z numeric form: $RELEASE_VERSION" >&2
  exit 1
fi
if [[ ! "$RC_NUMBER" =~ ^[1-9][0-9]*$ ]]; then
  echo "--rc must be a positive integer: $RC_NUMBER" >&2
  exit 1
fi
if [[ ! "$RUN_ID" =~ ^[1-9][0-9]*$ ]]; then
  echo "--run-id must be a positive integer: $RUN_ID" >&2
  exit 1
fi
if [[ ! "$REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "--repo must use OWNER/REPO form: $REPOSITORY" >&2
  exit 1
fi
if [[ "$DRY_RUN" != true && "$REPOSITORY" != apache/paimon-mosaic ]]; then
  echo "Real Nexus deployment requires apache/paimon-mosaic workflow artifacts." >&2
  exit 1
fi
if [[ "$DRY_RUN" != true ]]; then
  require_value "--gpg-keyname" "$GPG_KEYNAME"
fi
if [[ -n "$GPG_KEYNAME" ]]; then
  case "$GPG_KEYNAME" in
    *[!0-9A-Fa-f]*) GPG_KEYNAME_VALID=false ;;
    *) GPG_KEYNAME_VALID=true ;;
  esac
  if [[ "$GPG_KEYNAME_VALID" != true ||
        ( ${#GPG_KEYNAME} -ne 40 && ${#GPG_KEYNAME} -ne 64 ) ]]; then
    echo "--gpg-keyname must be a full 40- or 64-hex OpenPGP fingerprint." >&2
    exit 1
  fi
  GPG_KEYNAME=$(printf '%s' "$GPG_KEYNAME" | tr '[:lower:]' '[:upper:]')
fi

TAG="v${RELEASE_VERSION}-rc${RC_NUMBER}"
STAGING_DESCRIPTION="Apache Paimon Mosaic ${RELEASE_VERSION} RC${RC_NUMBER}"

if [[ -n "$MAVEN_SETTINGS" ]]; then
  if [[ ! -f "$MAVEN_SETTINGS" ]]; then
    echo "--maven-settings does not exist: $MAVEN_SETTINGS" >&2
    exit 1
  fi
  MAVEN_SETTINGS=$(cd "$(dirname "$MAVEN_SETTINGS")" && pwd)/$(basename "$MAVEN_SETTINGS")
fi
if [[ -n "$KEYS_FILE" ]]; then
  if [[ ! -f "$KEYS_FILE" ]]; then
    echo "--keys-file does not exist: $KEYS_FILE" >&2
    exit 1
  fi
  KEYS_FILE=$(cd "$(dirname "$KEYS_FILE")" && pwd)/$(basename "$KEYS_FILE")
fi

for command in git gh tar "$MVN"; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command not found: $command" >&2
    exit 1
  fi
done
if [[ "$DRY_RUN" != true ]] && ! command -v "$GPG" >/dev/null 2>&1; then
  echo "Required command not found: $GPG" >&2
  exit 1
fi
if [[ "$DRY_RUN" != true && -z "$KEYS_FILE" ]] &&
  ! command -v "$CURL" >/dev/null 2>&1; then
  echo "Required command not found: $CURL" >&2
  exit 1
fi

validate_local_signing_key() {
  local local_fingerprints

  if ! local_fingerprints=$(
    "$GPG" --batch --with-colons --list-secret-keys --fingerprint "$GPG_KEYNAME" |
      awk -F: '$1 == "fpr" {print toupper($10)}'
  ); then
    echo "Unable to read local secret key $GPG_KEYNAME." >&2
    exit 1
  fi
  if ! printf '%s\n' "$local_fingerprints" | grep -Fxq "$GPG_KEYNAME"; then
    echo "Local GPG secret keys do not contain $GPG_KEYNAME." >&2
    exit 1
  fi
}

if [[ "$DRY_RUN" != true ]]; then
  validate_local_signing_key
fi

if ! TAG_OBJECT=$(
  git -C "$REPO_DIR" rev-parse -q --verify "refs/tags/$TAG^{tag}"
); then
  echo "Tag $TAG must be an annotated tag." >&2
  echo "Run: git fetch --tags && git checkout $TAG" >&2
  exit 1
fi
if ! TAG_OBJECT_NAME=$(
  git -C "$REPO_DIR" cat-file tag "$TAG_OBJECT" |
    awk '
      /^$/ { exit }
      /^tag / {
        count += 1
        name = substr($0, 5)
      }
      END {
        if (count != 1) {
          exit 1
        }
        print name
      }
    '
); then
  echo "Unable to read the release name from tag object $TAG_OBJECT." >&2
  exit 1
fi
if [[ "$TAG_OBJECT_NAME" != "$TAG" ]]; then
  echo "Tag $TAG signed tag object is for $TAG_OBJECT_NAME, not $TAG." >&2
  exit 1
fi
if ! git -C "$REPO_DIR" -c gpg.program=gpg verify-tag "$TAG_OBJECT"; then
  echo "Tag $TAG signature verification failed." >&2
  exit 1
fi
TAG_COMMIT=$(git -C "$REPO_DIR" rev-parse "$TAG_OBJECT^{commit}")
HEAD_COMMIT=$(git -C "$REPO_DIR" rev-parse HEAD)
if [[ "$HEAD_COMMIT" != "$TAG_COMMIT" ]]; then
  echo "Current checkout does not match $TAG." >&2
  echo "HEAD: $HEAD_COMMIT" >&2
  echo "tag:  $TAG_COMMIT" >&2
  exit 1
fi

REPLACEMENT_REFS=$(
  git -C "$REPO_DIR" for-each-ref --format='%(refname)' refs/replace
)
if [[ -n "$REPLACEMENT_REFS" ]]; then
  echo "Git replacement refs are not allowed during Java staging." >&2
  printf '%s\n' "$REPLACEMENT_REFS" >&2
  exit 1
fi

ARCHIVE_ATTRIBUTES=$(
  git -C "$REPO_DIR" rev-parse --git-path info/attributes
)
case "$ARCHIVE_ATTRIBUTES" in
  /*) ;;
  *) ARCHIVE_ATTRIBUTES="$REPO_DIR/$ARCHIVE_ATTRIBUTES" ;;
esac
if [[ -s "$ARCHIVE_ATTRIBUTES" ]]; then
  echo "Non-empty repository-local Git attributes are not allowed during Java staging." >&2
  echo "$ARCHIVE_ATTRIBUTES" >&2
  exit 1
fi

INDEX_FLAGGED_PATHS=$(
  git -C "$REPO_DIR" ls-files -v |
    awk 'substr($0, 1, 1) == "S" || substr($0, 1, 1) ~ /[a-z]/'
)
if [[ -n "$INDEX_FLAGGED_PATHS" ]]; then
  echo "Git index flags such as assume-unchanged or skip-worktree are not allowed." >&2
  printf '%s\n' "$INDEX_FLAGGED_PATHS" >&2
  exit 1
fi

check_checkout_clean() {
  local status
  local ignored

  status=$(git -C "$REPO_DIR" status --porcelain=v1 --untracked-files=all)
  if [[ -n "$status" ]]; then
    echo "The RC checkout must be clean before Java staging." >&2
    printf '%s\n' "$status" >&2
    exit 1
  fi

  ignored=$(
    git -C "$REPO_DIR" \
      ls-files --others --ignored --exclude-standard -- java |
      sed \
        -e '\#^java/target/#d' \
        -e '\#^java/src/main/resources/native/#d'
  )
  if [[ -n "$ignored" ]]; then
    echo "Ignored Java package inputs must be removed before staging." >&2
    printf '%s\n' "$ignored" >&2
    exit 1
  fi
}

check_checkout_clean

gh_exact() {
  GH_HOST="$GITHUB_HOST" gh "$@"
}

validate_remote_tag_object() {
  local remote_tag_output
  local remote_tag_type
  local remote_tag_object

  if ! remote_tag_output=$(
    gh_exact api \
      "repos/$REPOSITORY/git/ref/tags/$TAG" \
      --jq '.object.type, .object.sha'
  ); then
    echo "Failed to read the current remote tag object for $TAG." >&2
    exit 1
  fi

  remote_tag_type=$(printf '%s\n' "$remote_tag_output" | sed -n '1p')
  remote_tag_object=$(printf '%s\n' "$remote_tag_output" | sed -n '2p')
  if [[ "$remote_tag_type" != tag ]]; then
    echo "Current remote tag $TAG is not an annotated tag." >&2
    exit 1
  fi
  if [[ "$remote_tag_object" != "$TAG_OBJECT" ]]; then
    echo "The current remote tag object does not match the verified local tag." >&2
    echo "remote: $remote_tag_object" >&2
    echo "local:  $TAG_OBJECT" >&2
    exit 1
  fi
}

validate_github_run() {
  local run_output
  local run_status
  local run_conclusion
  local run_head_sha
  local run_head_branch
  local run_workflow_name
  local run_event

  if ! run_output=$(
    gh_exact run view "$RUN_ID" \
      --repo "$REPOSITORY" \
      --json status,conclusion,headSha,headBranch,workflowName,event \
      --template '{{printf "%s\n%s\n%s\n%s\n%s\n%s\n" .status .conclusion .headSha (or .headBranch "") (or .workflowName "") (or .event "")}}'
  ); then
    echo "Failed to read GitHub Actions run: $RUN_ID" >&2
    exit 1
  fi

  run_status=$(printf '%s\n' "$run_output" | sed -n '1p')
  run_conclusion=$(printf '%s\n' "$run_output" | sed -n '2p')
  run_head_sha=$(printf '%s\n' "$run_output" | sed -n '3p')
  run_head_branch=$(printf '%s\n' "$run_output" | sed -n '4p')
  run_workflow_name=$(printf '%s\n' "$run_output" | sed -n '5p')
  run_event=$(printf '%s\n' "$run_output" | sed -n '6p')

  if [[ "$run_status" != completed || "$run_conclusion" != success ]]; then
    echo "GitHub Actions run $RUN_ID is not a successful completed run." >&2
    echo "status=$run_status conclusion=$run_conclusion" >&2
    exit 1
  fi
  if [[ "$run_workflow_name" != Release ]]; then
    echo "GitHub Actions run $RUN_ID is from workflow '$run_workflow_name', expected 'Release'." >&2
    exit 1
  fi
  if [[ "$run_event" != push ]]; then
    echo "GitHub Actions run $RUN_ID was triggered by '$run_event', expected a tag push." >&2
    exit 1
  fi
  if [[ "$run_head_branch" != "$TAG" ]]; then
    echo "GitHub Actions run $RUN_ID is for '$run_head_branch', expected '$TAG'." >&2
    exit 1
  fi
  if [[ "$run_head_sha" != "$TAG_COMMIT" ]]; then
    echo "GitHub Actions run $RUN_ID does not match $TAG." >&2
    echo "run headSha: $run_head_sha" >&2
    echo "tag commit:  $TAG_COMMIT" >&2
    exit 1
  fi
}

validate_remote_tag_object
validate_github_run

STAGING_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/paimon-mosaic-java-staging.XXXXXX")

cleanup() {
  case "$STAGING_ROOT" in
    "${TMPDIR:-/tmp}"/paimon-mosaic-java-staging.*)
      rm -rf -- "$STAGING_ROOT"
      ;;
    *)
      echo "Refusing to remove unexpected staging path: $STAGING_ROOT" >&2
      ;;
  esac
}
trap cleanup EXIT

validate_asf_keys_membership() {
  local curl_status
  local keys_fingerprints

  if [[ -z "$KEYS_FILE" ]]; then
    KEYS_FILE="$STAGING_ROOT/PAIMON_KEYS"
    if "$CURL" \
      --proto '=https' \
      --tlsv1.2 \
      --location \
      --fail \
      --silent \
      --show-error \
      --retry 3 \
      --retry-connrefused \
      --connect-timeout 10 \
      --max-time 300 \
      --output "$KEYS_FILE" \
      https://downloads.apache.org/paimon/KEYS; then
      :
    else
      curl_status=$?
      exit "$curl_status"
    fi
  fi

  if ! keys_fingerprints=$(
    "$GPG" \
      --batch \
      --with-colons \
      --import-options show-only \
      --import "$KEYS_FILE" |
      awk -F: '$1 == "fpr" {print toupper($10)}'
  ); then
    echo "Unable to read OpenPGP fingerprints from $KEYS_FILE." >&2
    exit 1
  fi
  if ! printf '%s\n' "$keys_fingerprints" | grep -Fxq "$GPG_KEYNAME"; then
    echo "Signing key $GPG_KEYNAME is not present in the ASF Paimon KEYS file." >&2
    exit 1
  fi
}

if [[ "$DRY_RUN" != true ]]; then
  validate_asf_keys_membership
fi

ARCHIVE_PREFIX="paimon-mosaic-${RELEASE_VERSION}"
ARCHIVE_PATH="$STAGING_ROOT/source.tar"
git -C "$REPO_DIR" \
  -c core.attributesFile=/dev/null \
  archive \
  --format=tar \
  "--prefix=${ARCHIVE_PREFIX}/" \
  "$TAG_COMMIT" > "$ARCHIVE_PATH"
tar -xf "$ARCHIVE_PATH" -C "$STAGING_ROOT"
BUILD_REPO_DIR="$STAGING_ROOT/$ARCHIVE_PREFIX"

ARTIFACT_VALIDATOR="$BUILD_REPO_DIR/tools/validate_java_staging_artifacts.sh"
if [[ ! -x "$ARTIFACT_VALIDATOR" ]]; then
  echo "Java staging artifact validator is not executable: $ARTIFACT_VALIDATOR" >&2
  exit 1
fi

POM_VERSION=$(
  awk '
    /<\/parent>/ { after_parent = 1; next }
    after_parent && match($0, /<version>[^<]+<\/version>/) {
      value = substr($0, RSTART + 9, RLENGTH - 19)
      print value
      exit
    }
  ' "$BUILD_REPO_DIR/java/pom.xml"
)
if [[ "$POM_VERSION" != "$RELEASE_VERSION" ]]; then
  echo "RC tag java/pom.xml version is $POM_VERSION, expected $RELEASE_VERSION" >&2
  exit 1
fi

NATIVE_RESOURCE_DIR="$BUILD_REPO_DIR/java/src/main/resources/native"

download_native() {
  local artifact=$1
  local file_name=$2
  local resource_path=$3
  local artifact_dir="$STAGING_ROOT/$artifact"
  local source_file="$artifact_dir/$file_name"
  local target_file="$NATIVE_RESOURCE_DIR/$resource_path"
  local download_status

  if gh_exact run download "$RUN_ID" \
    --repo "$REPOSITORY" \
    --name "$artifact" \
    --dir "$artifact_dir"; then
    :
  else
    download_status=$?
    exit "$download_status"
  fi

  if [[ ! -s "$source_file" ]]; then
    echo "Missing native artifact: $artifact/$file_name" >&2
    exit 1
  fi

  mkdir -p "$(dirname "$target_file")"
  cp "$source_file" "$target_file"
}

rm -rf -- "$NATIVE_RESOURCE_DIR"
download_native \
  native-linux-x86_64 \
  libpaimon_mosaic_jni.so \
  linux/x86_64/libpaimon_mosaic_jni.so
download_native \
  native-linux-aarch64 \
  libpaimon_mosaic_jni.so \
  linux/aarch64/libpaimon_mosaic_jni.so
download_native \
  native-macos-aarch64 \
  libpaimon_mosaic_jni.dylib \
  macos/aarch64/libpaimon_mosaic_jni.dylib
download_native \
  native-windows-x86_64 \
  paimon_mosaic_jni.dll \
  windows/x86_64/paimon_mosaic_jni.dll

EXPECTED_NATIVE_FILES=$(cat <<'EOF'
linux/aarch64/libpaimon_mosaic_jni.so
linux/x86_64/libpaimon_mosaic_jni.so
macos/aarch64/libpaimon_mosaic_jni.dylib
windows/x86_64/paimon_mosaic_jni.dll
EOF
)
ACTUAL_NATIVE_FILES=$(
  cd "$NATIVE_RESOURCE_DIR"
  find . -type f -print |
    sed 's#^\./##' |
    LC_ALL=C sort
)
if [[ "$ACTUAL_NATIVE_FILES" != "$EXPECTED_NATIVE_FILES" ]]; then
  echo "Downloaded Java native inputs differ from the four expected files." >&2
  exit 1
fi

check_checkout_clean

MAVEN_CMD=("$MVN")
if [[ -n "$MAVEN_SETTINGS" ]]; then
  MAVEN_CMD+=("-s" "$MAVEN_SETTINGS")
fi
if [[ "$DRY_RUN" == true ]]; then
  MAVEN_CMD+=(clean verify -Prelease -Dgpg.skip=true)
else
  MAVEN_CMD+=(
    clean deploy -Prelease
    -Dgpg.skip=false
    -Dgpg.signer=gpg
    "-Dgpg.executable=$GPG"
    "-Dgpg.keyname=${GPG_KEYNAME}!"
    "-DstagingDescription=$STAGING_DESCRIPTION"
  )
fi
MAVEN_CMD+=(
  -DskipTests
  -Dmaven.main.skip=false
  "-DstagingValidationScript=$ARTIFACT_VALIDATOR"
  -Dexec.skip=false
  -DskipLocalStaging=false
  -DskipNexusStagingDeployMojo=false
  -DskipRemoteStaging=false
  -DskipStaging=false
  -DskipStagingRepositoryClose=false
  -Dmaven.wagon.http.ssl.allowall=false
  -Dmaven.wagon.http.ssl.insecure=false
  -DstagingRepositoryId=
  -DstagingProfileId=
  -DkeepStagingRepositoryOnFailure=false
  -DkeepStagingRepositoryOnCloseRuleFailure=false
)

if [[ "$DRY_RUN" == true ]]; then
  echo "Dry-running Java staging build. No artifacts will be signed or deployed."
else
  echo "Signing and deploying Java artifacts from the local Release Manager machine."
fi

MAVEN_STATUS=0
(
  cd "$BUILD_REPO_DIR/java" || exit $?
  if env \
    MAVEN_SKIP_RC=1 \
    MAVEN_ARGS= \
    MAVEN_OPTS= \
    MAVEN_DEBUG_OPTS= \
    MAVEN_CONFIG= \
    MAVEN_BASEDIR="$PWD" \
    MAVEN_PROJECTBASEDIR= \
    JAVA_TOOL_OPTIONS= \
    JDK_JAVA_OPTIONS= \
    _JAVA_OPTIONS= \
    MAVEN_GPG_KEY= \
    MAVEN_GPG_KEY_FINGERPRINT= \
    MAVEN_GPG_PASSPHRASE= \
    "${MAVEN_CMD[@]}"; then
    :
  else
    MAVEN_STATUS=$?
    exit "$MAVEN_STATUS"
  fi
) || MAVEN_STATUS=$?

if [[ "$MAVEN_STATUS" -ne 0 ]]; then
  exit "$MAVEN_STATUS"
fi

if [[ "$DRY_RUN" == true ]]; then
  echo "Java staging dry run finished successfully."
else
  echo "Java staging deploy finished."
  echo "Leave the resulting orgapachepaimon-XXXX repository staged for the vote."
fi
