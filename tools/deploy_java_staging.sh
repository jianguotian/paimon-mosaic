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
PYTHON=${PYTHON:-python3}

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd "$SCRIPT_DIR/.." && pwd)

RELEASE_VERSION=
RC_NUMBER=
RUN_ID=
REPOSITORY=apache/paimon-mosaic
PROVENANCE_MANIFEST=
STAGING_PROFILE_ID=
MAVEN_SETTINGS=
SOURCE_MAVEN_SETTINGS=
GPG_KEYNAME=
KEYS_FILE=
DRY_RUN=false
GITHUB_HOST=github.com
RELEASE_WORKFLOW_PATH=.github/workflows/release.yml
MAX_JAVA_PACKAGE_SIZE=536870912
NEXUS_URL=https://repository.apache.org/
NEXUS_SERVER_ID=apache.releases.https
GPG_PLUGIN_GOAL=org.apache.maven.plugins:maven-gpg-plugin:3.2.8:sign-and-deploy-file
NEXUS_PLUGIN_GOAL=org.sonatype.plugins:nexus-staging-maven-plugin:1.7.0:deploy-staged-repository
MAVEN_CENTRAL_URL=https://repo.maven.apache.org/maven2
PINNED_MAVEN_MIRROR_ID=paimon-mosaic-pinned-plugins

usage() {
  cat <<'EOF'
Usage:
  deploy_java_staging.sh --release-version VERSION --rc N --run-id RUN_ID \
    --provenance-manifest FILE --staging-profile-id ID [options]

Validate and freeze the exact successful RC-tag Release workflow and its
java-package artifact. A dry-run validates the frozen CI candidate without
Maven, signing, or Nexus. A real deployment signs those exact files into a
private file:// Maven repository, verifies every payload and signature, then
uploads and closes that repository in Apache Nexus staging.

Required:
  --release-version VERSION  Release version, for example 0.3.0.
  --rc N                     RC number, for example 1.
  --run-id RUN_ID            Successful RC-tag Release workflow run id.
  --provenance-manifest FILE Frozen provenance written by a successful dry-run
                             and required unchanged for a real deployment.
  --staging-profile-id ID    Apache Nexus staging profile id. Freeze the same
                             non-secret id in dry-run and real deployment.

Options:
  --repo OWNER/REPO          Defaults to apache/paimon-mosaic.
  --maven-settings FILE      Maven settings.xml containing Nexus credentials.
  --gpg-keyname FINGERPRINT  Full OpenPGP signing-key fingerprint.
  --keys-file FILE           ASF Paimon KEYS file; otherwise download it.
  --dry-run                  Validate and freeze without Maven/GPG/Nexus.
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
    --provenance-manifest)
      require_option_value "$@"
      PROVENANCE_MANIFEST=$2
      shift 2
      ;;
    --staging-profile-id)
      require_option_value "$@"
      STAGING_PROFILE_ID=$2
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
require_value "--provenance-manifest" "$PROVENANCE_MANIFEST"
require_value "--staging-profile-id" "$STAGING_PROFILE_ID"

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
if [[ ! "$STAGING_PROFILE_ID" =~ ^[A-Za-z0-9._-]+$ ||
      ${#STAGING_PROFILE_ID} -gt 128 ]]; then
  echo "--staging-profile-id must be a 1-128 character Nexus profile id." >&2
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

PROVENANCE_PARENT=$(dirname "$PROVENANCE_MANIFEST")
PROVENANCE_NAME=$(basename "$PROVENANCE_MANIFEST")
if [[ ! -d "$PROVENANCE_PARENT" ]]; then
  echo "Provenance manifest parent directory does not exist: $PROVENANCE_PARENT" >&2
  exit 1
fi
if [[ "$PROVENANCE_NAME" == . || "$PROVENANCE_NAME" == .. ]]; then
  echo "--provenance-manifest must name a file" >&2
  exit 1
fi
PROVENANCE_MANIFEST=$(cd "$PROVENANCE_PARENT" && pwd)/$PROVENANCE_NAME
if [[ -L "$PROVENANCE_MANIFEST" ||
      ( -e "$PROVENANCE_MANIFEST" && ! -f "$PROVENANCE_MANIFEST" ) ]]; then
  echo "Provenance manifest must be a regular non-symlink file: $PROVENANCE_MANIFEST" >&2
  exit 1
fi
case "$PROVENANCE_MANIFEST" in
  "$REPO_DIR"/*)
    echo "Provenance manifest must be outside the clean RC checkout." >&2
    exit 1
    ;;
esac
if [[ "$DRY_RUN" != true && ! -f "$PROVENANCE_MANIFEST" ]]; then
  echo "Real deployment requires a provenance manifest from a successful dry-run." >&2
  echo "$PROVENANCE_MANIFEST" >&2
  exit 1
fi

if [[ -n "$MAVEN_SETTINGS" ]]; then
  if [[ -L "$MAVEN_SETTINGS" || ! -f "$MAVEN_SETTINGS" ]]; then
    echo "--maven-settings must be a regular non-symlink file: $MAVEN_SETTINGS" >&2
    exit 1
  fi
  MAVEN_SETTINGS=$(cd "$(dirname "$MAVEN_SETTINGS")" && pwd)/$(basename "$MAVEN_SETTINGS")
fi
if [[ "$DRY_RUN" != true ]]; then
  if [[ -n "$MAVEN_SETTINGS" ]]; then
    SOURCE_MAVEN_SETTINGS=$MAVEN_SETTINGS
  else
    if [[ -z "${HOME:-}" ]]; then
      echo "HOME is required to locate the default Maven settings.xml." >&2
      exit 1
    fi
    SOURCE_MAVEN_SETTINGS="${HOME}/.m2/settings.xml"
    if [[ -L "$SOURCE_MAVEN_SETTINGS" ||
          ! -f "$SOURCE_MAVEN_SETTINGS" ]]; then
      echo "Real deployment requires Maven settings with server $NEXUS_SERVER_ID." >&2
      echo "Use --maven-settings or create $SOURCE_MAVEN_SETTINGS." >&2
      exit 1
    fi
  fi
fi
if [[ -n "$KEYS_FILE" ]]; then
  if [[ -L "$KEYS_FILE" || ! -f "$KEYS_FILE" ]]; then
    echo "--keys-file must be a regular non-symlink file: $KEYS_FILE" >&2
    exit 1
  fi
  KEYS_FILE=$(cd "$(dirname "$KEYS_FILE")" && pwd)/$(basename "$KEYS_FILE")
fi

for command in cmp git gh tar "$PYTHON"; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command not found: $command" >&2
    exit 1
  fi
done
if [[ "$DRY_RUN" != true ]]; then
  for command in "$MVN" "$GPG"; do
    if ! command -v "$command" >/dev/null 2>&1; then
      echo "Required command not found: $command" >&2
      exit 1
    fi
  done
fi
if [[ "$DRY_RUN" != true ]] &&
  ! command -v "$CURL" >/dev/null 2>&1; then
  echo "Required command not found: $CURL" >&2
  exit 1
fi

GIT_TRUSTED=(git -c core.fsmonitor=false)

if ! TAG_OBJECT=$(
  "${GIT_TRUSTED[@]}" \
    -C "$REPO_DIR" rev-parse -q --verify "refs/tags/$TAG^{tag}"
); then
  echo "Tag $TAG must be an annotated tag." >&2
  echo "Run: git fetch --tags && git checkout $TAG" >&2
  exit 1
fi
if ! TAG_OBJECT_NAME=$(
  "${GIT_TRUSTED[@]}" -C "$REPO_DIR" cat-file tag "$TAG_OBJECT" |
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
if ! "${GIT_TRUSTED[@]}" \
  -C "$REPO_DIR" -c gpg.program=gpg verify-tag "$TAG_OBJECT"; then
  echo "Tag $TAG signature verification failed." >&2
  exit 1
fi
TAG_COMMIT=$(
  "${GIT_TRUSTED[@]}" -C "$REPO_DIR" rev-parse "$TAG_OBJECT^{commit}"
)
HEAD_COMMIT=$("${GIT_TRUSTED[@]}" -C "$REPO_DIR" rev-parse HEAD)
if [[ "$HEAD_COMMIT" != "$TAG_COMMIT" ]]; then
  echo "Current checkout does not match $TAG." >&2
  echo "HEAD: $HEAD_COMMIT" >&2
  echo "tag:  $TAG_COMMIT" >&2
  exit 1
fi

REPLACEMENT_REFS=$(
  "${GIT_TRUSTED[@]}" \
    -C "$REPO_DIR" for-each-ref --format='%(refname)' refs/replace
)
if [[ -n "$REPLACEMENT_REFS" ]]; then
  echo "Git replacement refs are not allowed during Java staging." >&2
  printf '%s\n' "$REPLACEMENT_REFS" >&2
  exit 1
fi

ARCHIVE_ATTRIBUTES=$(
  "${GIT_TRUSTED[@]}" \
    -C "$REPO_DIR" rev-parse --git-path info/attributes
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
  "${GIT_TRUSTED[@]}" -C "$REPO_DIR" ls-files -v |
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

  status=$(
    "${GIT_TRUSTED[@]}" -C "$REPO_DIR" \
      status --porcelain=v1 --untracked-files=all
  )
  if [[ -n "$status" ]]; then
    echo "The RC checkout must be clean before Java staging." >&2
    printf '%s\n' "$status" >&2
    exit 1
  fi

  ignored=$(
    "${GIT_TRUSTED[@]}" -C "$REPO_DIR" \
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

validate_gh_transport() {
  local unix_socket

  if ! unix_socket=$(gh_exact config get http_unix_socket); then
    echo "Unable to inspect the GitHub CLI HTTP transport configuration." >&2
    exit 1
  fi
  if [[ -n "$unix_socket" ]]; then
    echo "GitHub CLI Unix socket transport is not allowed during Java staging." >&2
    echo "$unix_socket" >&2
    exit 1
  fi
}

validate_gh_transport

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

umask 077
STAGING_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/paimon-mosaic-java-staging.XXXXXX")
chmod 700 "$STAGING_ROOT"

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

RUN_JSON="$STAGING_ROOT/workflow-run.json"
WORKFLOW_JSON="$STAGING_ROOT/workflow.json"
ARTIFACTS_JSON="$STAGING_ROOT/workflow-artifacts.json"
CURRENT_PROVENANCE="$STAGING_ROOT/current-provenance.txt"
FROZEN_PROVENANCE="$STAGING_ROOT/frozen-provenance.txt"
ARTIFACT_SELECTION="$STAGING_ROOT/artifact-selection.txt"
JAVA_PACKAGE_ARTIFACT_ID=
JAVA_PACKAGE_ARTIFACT_DIGEST=
JAVA_PACKAGE_ARTIFACT_SIZE=
WRITE_PROVENANCE_MANIFEST=false

capture_github_provenance() {
  if ! gh_exact api \
    "repos/$REPOSITORY/actions/runs/$RUN_ID" > "$RUN_JSON"; then
    echo "Failed to read GitHub Actions run: $RUN_ID" >&2
    exit 1
  fi
  if ! gh_exact api \
    "repos/$REPOSITORY/actions/workflows/release.yml" > "$WORKFLOW_JSON"; then
    echo "Failed to read the canonical Release workflow." >&2
    exit 1
  fi
  if ! gh_exact api \
    "repos/$REPOSITORY/actions/runs/$RUN_ID/artifacts?name=java-package&per_page=100" \
    --paginate \
    --slurp > "$ARTIFACTS_JSON"; then
    echo "Failed to list GitHub Actions artifacts for run $RUN_ID." >&2
    exit 1
  fi

  "$PYTHON" - \
    "$RUN_JSON" \
    "$WORKFLOW_JSON" \
    "$ARTIFACTS_JSON" \
    "$REPOSITORY" \
    "$TAG" \
    "$TAG_OBJECT" \
    "$TAG_COMMIT" \
    "$RUN_ID" \
    "$RELEASE_WORKFLOW_PATH" \
    "$MAX_JAVA_PACKAGE_SIZE" \
    "$STAGING_PROFILE_ID" \
    "$CURRENT_PROVENANCE" \
    "$ARTIFACT_SELECTION" <<'PY'
import json
import re
import sys
from pathlib import Path


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


run_path = Path(sys.argv[1])
workflow_path = Path(sys.argv[2])
artifacts_path = Path(sys.argv[3])
repository = sys.argv[4]
tag = sys.argv[5]
tag_object = sys.argv[6]
commit = sys.argv[7]
run_id = sys.argv[8]
canonical_workflow_path = sys.argv[9]
max_artifact_size = int(sys.argv[10])
staging_profile_id = sys.argv[11]
manifest_path = Path(sys.argv[12])
selection_path = Path(sys.argv[13])

try:
    run = json.loads(run_path.read_text(encoding="utf-8"))
    workflow = json.loads(workflow_path.read_text(encoding="utf-8"))
    pages = json.loads(artifacts_path.read_text(encoding="utf-8"))
except (OSError, ValueError) as error:
    fail("Unable to parse GitHub Actions provenance: {}".format(error))
if isinstance(pages, dict):
    pages = [pages]
if (
    not isinstance(run, dict)
    or not isinstance(workflow, dict)
    or not isinstance(pages, list)
):
    fail("GitHub Actions provenance has an unexpected JSON shape")

if str(run.get("id", "")) != run_id:
    fail("GitHub Actions run response does not match requested run id")
if run.get("status") != "completed" or run.get("conclusion") != "success":
    fail(
        "GitHub Actions run {} is not a successful completed run".format(
            run_id
        )
    )
if run.get("name") != "Release":
    fail("GitHub Actions run {} is not named Release".format(run_id))
if run.get("event") != "push":
    fail("GitHub Actions run {} is not a tag-push run".format(run_id))
if run.get("head_branch") != tag or run.get("head_sha") != commit:
    fail(
        "GitHub Actions run {} does not match {} at {}".format(
            run_id, tag, commit
        )
    )
if run.get("path") != canonical_workflow_path:
    fail(
        "GitHub Actions run {} uses workflow path {!r}, expected {!r}".format(
            run_id, run.get("path"), canonical_workflow_path
        )
    )

workflow_id = run.get("workflow_id")
run_attempt = run.get("run_attempt")
if not isinstance(workflow_id, int) or workflow_id <= 0:
    fail("GitHub Actions run does not have a valid workflow id")
if not isinstance(run_attempt, int) or run_attempt <= 0:
    fail("GitHub Actions run does not have a valid run attempt")
if (
    workflow.get("id") != workflow_id
    or workflow.get("name") != "Release"
    or workflow.get("path") != canonical_workflow_path
    or workflow.get("state") != "active"
):
    fail(
        "GitHub Actions run does not use the active canonical Release "
        "workflow id and path"
    )

run_repository = (run.get("repository") or {}).get("full_name")
head_repository = (run.get("head_repository") or {}).get("full_name")
if run_repository != repository or head_repository != repository:
    fail("GitHub Actions run repository does not match {}".format(repository))

artifacts = [
    artifact
    for page in pages
    if isinstance(page, dict)
    for artifact in page.get("artifacts", [])
    if artifact.get("name") == "java-package"
    and not artifact.get("expired", False)
]
if not artifacts:
    fail("No unexpired java-package artifact found")
artifact_ids = [artifact.get("id") for artifact in artifacts]
if any(
    not isinstance(artifact_id, int) or artifact_id <= 0
    for artifact_id in artifact_ids
):
    fail("java-package artifact does not have a valid immutable artifact id")
if len(set(artifact_ids)) != len(artifact_ids):
    fail("Duplicate java-package artifact id")

# Java reruns can retain same-name artifacts; match GitHub's newest-id selection.
artifact = max(artifacts, key=lambda artifact: artifact["id"])
artifact_id = artifact.get("id")
artifact_digest = artifact.get("digest")
artifact_size = artifact.get("size_in_bytes")
artifact_run = artifact.get("workflow_run") or {}
if not isinstance(artifact_digest, str) or not re.fullmatch(
    r"sha256:[0-9a-fA-F]{64}", artifact_digest
):
    fail("java-package artifact does not have a valid SHA-256 digest")
if (
    not isinstance(artifact_size, int)
    or artifact_size <= 0
    or artifact_size > max_artifact_size
):
    fail(
        "java-package artifact size must be between 1 and {} bytes".format(
            max_artifact_size
        )
    )
if str(artifact_run.get("id", "")) != run_id:
    fail("java-package artifact does not belong to the requested run")
if artifact_run.get("head_sha") != commit:
    fail("java-package artifact does not match the signed tag commit")

for value, description in (
    (tag_object, "tag object"),
    (commit, "tag commit"),
):
    if not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", value):
        fail("Invalid {} in Java staging provenance".format(description))

manifest = "\n".join(
    (
        "schema=1",
        "repository={}".format(repository),
        "tag={}".format(tag),
        "tag_object={}".format(tag_object),
        "commit={}".format(commit),
        "workflow_path={}".format(canonical_workflow_path),
        "workflow_id={}".format(workflow_id),
        "run_id={}".format(run_id),
        "run_attempt={}".format(run_attempt),
        "staging_profile_id={}".format(staging_profile_id),
        "artifact_name=java-package",
        "artifact_id={}".format(artifact_id),
        "artifact_digest={}".format(artifact_digest.lower()),
        "artifact_size={}".format(artifact_size),
    )
) + "\n"
manifest_path.write_text(manifest, encoding="utf-8")
selection_path.write_text(
    "artifact_id={}\nartifact_digest={}\nartifact_size={}\n".format(
        artifact_id,
        artifact_digest.lower(),
        artifact_size,
    ),
    encoding="utf-8",
)
PY
}

read_artifact_selection() {
  local key
  local value

  JAVA_PACKAGE_ARTIFACT_ID=
  JAVA_PACKAGE_ARTIFACT_DIGEST=
  JAVA_PACKAGE_ARTIFACT_SIZE=
  while IFS='=' read -r key value; do
    case "$key" in
      artifact_id) JAVA_PACKAGE_ARTIFACT_ID=$value ;;
      artifact_digest) JAVA_PACKAGE_ARTIFACT_DIGEST=$value ;;
      artifact_size) JAVA_PACKAGE_ARTIFACT_SIZE=$value ;;
      *)
        echo "Unexpected java-package artifact field: $key" >&2
        exit 1
        ;;
    esac
  done < "$ARTIFACT_SELECTION"
  if [[ ! "$JAVA_PACKAGE_ARTIFACT_ID" =~ ^[1-9][0-9]*$ ||
        ! "$JAVA_PACKAGE_ARTIFACT_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ||
        ! "$JAVA_PACKAGE_ARTIFACT_SIZE" =~ ^[1-9][0-9]*$ ||
        "$JAVA_PACKAGE_ARTIFACT_SIZE" -gt "$MAX_JAVA_PACKAGE_SIZE" ]]; then
    echo "Invalid java-package artifact selection." >&2
    exit 1
  fi
}

validate_frozen_provenance() {
  validate_remote_tag_object
  capture_github_provenance
  if ! cmp -s "$CURRENT_PROVENANCE" "$FROZEN_PROVENANCE"; then
    echo "GitHub Actions provenance changed after it was frozen." >&2
    echo "frozen:" >&2
    cat "$FROZEN_PROVENANCE" >&2
    echo "current:" >&2
    cat "$CURRENT_PROVENANCE" >&2
    exit 1
  fi
  if [[ ! -f "$PROVENANCE_MANIFEST" ]] ||
    ! cmp -s "$PROVENANCE_MANIFEST" "$FROZEN_PROVENANCE"; then
    echo "Provenance manifest changed after it was validated." >&2
    exit 1
  fi
}

validate_remote_tag_object
capture_github_provenance
read_artifact_selection
cp "$CURRENT_PROVENANCE" "$FROZEN_PROVENANCE"

if [[ -f "$PROVENANCE_MANIFEST" ]]; then
  if ! cmp -s "$PROVENANCE_MANIFEST" "$FROZEN_PROVENANCE"; then
    echo "Provenance manifest does not match the current immutable release inputs." >&2
    echo "provided: $PROVENANCE_MANIFEST" >&2
    echo "current:" >&2
    cat "$FROZEN_PROVENANCE" >&2
    exit 1
  fi
else
  WRITE_PROVENANCE_MANIFEST=true
fi

ARCHIVE_PREFIX="paimon-mosaic-${RELEASE_VERSION}"
ARCHIVE_PATH="$STAGING_ROOT/source.tar"
"${GIT_TRUSTED[@]}" -C "$REPO_DIR" \
  -c core.attributesFile=/dev/null \
  archive \
  --format=tar \
  "--prefix=${ARCHIVE_PREFIX}/" \
  "$TAG_COMMIT" > "$ARCHIVE_PATH"
tar -xf "$ARCHIVE_PATH" -C "$STAGING_ROOT"
SIGNED_REPO_DIR="$STAGING_ROOT/$ARCHIVE_PREFIX"

ARTIFACT_VALIDATOR="$SIGNED_REPO_DIR/tools/validate_java_staging_artifacts.sh"
MAVEN_PLUGIN_PREPARER="$SIGNED_REPO_DIR/tools/prepare_java_staging_maven_plugins.py"
MAVEN_PLUGIN_LOCK="$SIGNED_REPO_DIR/tools/java-staging-maven-plugins.sha256"
SIGNED_POM="$SIGNED_REPO_DIR/java/pom.xml"
if [[ -L "$ARTIFACT_VALIDATOR" || ! -f "$ARTIFACT_VALIDATOR" ||
      ! -x "$ARTIFACT_VALIDATOR" ]]; then
  echo "Java staging artifact validator is not an executable regular file." >&2
  exit 1
fi
if [[ -L "$MAVEN_PLUGIN_PREPARER" || ! -f "$MAVEN_PLUGIN_PREPARER" ||
      ! -s "$MAVEN_PLUGIN_PREPARER" ]]; then
  echo "Signed Maven plugin preparer is missing or unsafe." >&2
  exit 1
fi
if [[ -L "$MAVEN_PLUGIN_LOCK" || ! -f "$MAVEN_PLUGIN_LOCK" ||
      ! -s "$MAVEN_PLUGIN_LOCK" ]]; then
  echo "Signed Maven plugin lock is missing or unsafe." >&2
  exit 1
fi
if [[ -L "$SIGNED_POM" || ! -f "$SIGNED_POM" || ! -s "$SIGNED_POM" ]]; then
  echo "Signed-source Java POM is missing or unsafe: $SIGNED_POM" >&2
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
  ' "$SIGNED_POM"
)
if [[ "$POM_VERSION" != "$RELEASE_VERSION" ]]; then
  echo "RC tag java/pom.xml version is $POM_VERSION, expected $RELEASE_VERSION" >&2
  exit 1
fi

JAVA_PACKAGE_ZIP="$STAGING_ROOT/java-package.zip"
JAVA_PACKAGE_DIR="$STAGING_ROOT/java-package"

if ! gh_exact api \
  "repos/$REPOSITORY/actions/artifacts/$JAVA_PACKAGE_ARTIFACT_ID/zip" \
  > "$JAVA_PACKAGE_ZIP"; then
  echo "Failed to download immutable java-package artifact id $JAVA_PACKAGE_ARTIFACT_ID." >&2
  exit 1
fi

"$PYTHON" - \
  "$JAVA_PACKAGE_ZIP" \
  "$JAVA_PACKAGE_ARTIFACT_DIGEST" \
  "$JAVA_PACKAGE_ARTIFACT_SIZE" \
  "$JAVA_PACKAGE_DIR" \
  "$RELEASE_VERSION" <<'PY'
import hashlib
import os
import stat
import sys
import zipfile
from pathlib import Path, PurePosixPath, PureWindowsPath


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


archive_path = Path(sys.argv[1])
expected_digest = sys.argv[2].split(":", 1)[1]
expected_size = int(sys.argv[3])
destination = Path(sys.argv[4])
version = sys.argv[5]
expected_names = {
    "mosaic-{}.jar".format(version),
    "mosaic-{}-sources.jar".format(version),
    "mosaic-{}-javadoc.jar".format(version),
    "java-staging-provenance.txt",
}

if archive_path.is_symlink() or not archive_path.is_file():
    fail("Downloaded java-package artifact is not a regular file")
actual_size = archive_path.stat().st_size
if actual_size != expected_size:
    fail(
        "Downloaded java-package artifact size mismatch: "
        "found {}, expected {}".format(actual_size, expected_size)
    )

digest = hashlib.sha256()
with archive_path.open("rb") as source:
    while True:
        chunk = source.read(1024 * 1024)
        if not chunk:
            break
        digest.update(chunk)
actual_digest = digest.hexdigest()
if actual_digest != expected_digest:
    fail(
        "Downloaded java-package artifact digest mismatch: "
        "found sha256:{}, expected sha256:{}".format(
            actual_digest, expected_digest
        )
    )

try:
    archive = zipfile.ZipFile(str(archive_path))
except (OSError, zipfile.BadZipFile) as error:
    fail("Invalid java-package artifact ZIP: {}".format(error))

seen = set()
total_size = 0
try:
    infos = archive.infolist()
    for info in infos:
        name = info.orig_filename
        if (
            not name
            or "\x00" in name
            or name != info.filename
            or "\\" in name
            or PurePosixPath(name).is_absolute()
            or PureWindowsPath(name).is_absolute()
            or PureWindowsPath(name).drive
            or ".." in name.split("/")
        ):
            fail("Unsafe java-package artifact path: {!r}".format(name))
        normalized = "/".join(part for part in name.split("/") if part != ".")
        if normalized != name or normalized in seen:
            fail("Duplicate or non-canonical java-package path: {!r}".format(name))
        seen.add(normalized)

        mode = (info.external_attr >> 16) & 0xFFFF
        entry_type = stat.S_IFMT(mode)
        if info.is_dir():
            fail("java-package artifact must not contain directories")
        if entry_type not in (0, stat.S_IFREG):
            fail(
                "Unsupported java-package artifact entry type: {!r}".format(
                    name
                )
            )
        if info.file_size <= 0 or info.file_size > 256 * 1024 * 1024:
            fail(
                "java-package artifact entry has an invalid size: {!r}".format(
                    name
                )
            )
        total_size += info.file_size
        if total_size > 1024 * 1024 * 1024:
            fail("java-package artifact contents are too large")

    if seen != expected_names:
        fail(
            "Downloaded java-package artifact must contain exactly: {}".format(
                ", ".join(sorted(expected_names))
            )
        )

    destination.mkdir(mode=0o700)
    for info in infos:
        output = destination / info.filename
        descriptor = os.open(
            str(output),
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o600,
        )
        try:
            with archive.open(info) as source, os.fdopen(
                descriptor, "wb"
            ) as target:
                descriptor = -1
                while True:
                    chunk = source.read(1024 * 1024)
                    if not chunk:
                        break
                    target.write(chunk)
        finally:
            if descriptor >= 0:
                os.close(descriptor)
except (OSError, RuntimeError, zipfile.BadZipFile) as error:
    fail("Unable to extract java-package artifact: {}".format(error))
finally:
    archive.close()
PY

"$PYTHON" - \
  "$JAVA_PACKAGE_DIR/java-staging-provenance.txt" \
  "$REPOSITORY" \
  "$TAG" \
  "$TAG_OBJECT" \
  "$TAG_COMMIT" \
  "$RUN_ID" \
  "$RUN_JSON" <<'PY'
import json
import re
import sys
from pathlib import Path


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


path = Path(sys.argv[1])
expected = {
    "schema": "1",
    "repository": sys.argv[2],
    "tag": sys.argv[3],
    "tag_object": sys.argv[4],
    "commit": sys.argv[5],
    "run_id": sys.argv[6],
}
run_path = Path(sys.argv[7])
try:
    run = json.loads(run_path.read_text(encoding="utf-8"))
except (OSError, ValueError) as error:
    fail("Unable to re-read workflow run metadata: {}".format(error))
run_attempt = run.get("run_attempt")
if not isinstance(run_attempt, int) or run_attempt <= 0:
    fail("Workflow run has no valid run attempt")
expected_keys = (
    "schema",
    "repository",
    "tag",
    "tag_object",
    "commit",
    "run_id",
    "run_attempt",
)

if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
    fail("Java candidate provenance is missing or unsafe")
try:
    raw = path.read_bytes()
    text = raw.decode("utf-8")
except (OSError, UnicodeDecodeError) as error:
    fail("Unable to read Java candidate provenance: {}".format(error))
if b"\x00" in raw or b"\r" in raw or not raw.endswith(b"\n"):
    fail("Java candidate provenance is not canonical UTF-8 text")
lines = text.splitlines()
if len(lines) != len(expected_keys):
    fail("Java candidate provenance has an unexpected field count")

parsed = {}
for index, (line, expected_key) in enumerate(zip(lines, expected_keys), 1):
    if "=" not in line:
        fail("Invalid Java candidate provenance line {}".format(index))
    key, value = line.split("=", 1)
    if key != expected_key or not value or key in parsed:
        fail("Unexpected Java candidate provenance field: {!r}".format(key))
    if "\n" in value or not re.fullmatch(r"[\x20-\x7e]+", value):
        fail("Invalid Java candidate provenance value for {}".format(key))
    parsed[key] = value

for key in expected_keys:
    if key == "run_attempt":
        if (
            not re.fullmatch(r"[1-9][0-9]*", parsed[key])
            or int(parsed[key]) > run_attempt
        ):
            fail(
                "Java candidate provenance run_attempt {} is newer than "
                "current workflow run attempt {}".format(
                    parsed[key], run_attempt
                )
            )
    elif parsed[key] != expected[key]:
        fail(
            "Java candidate provenance {} mismatch: found {!r}, expected {!r}"
            .format(key, parsed[key], expected[key])
        )
PY

PYTHON="$PYTHON" "$ARTIFACT_VALIDATOR" \
  "$JAVA_PACKAGE_DIR" \
  "$RELEASE_VERSION"

# Re-read the live tag, run, and artifact metadata after candidate validation.
validate_remote_tag_object
capture_github_provenance
if ! cmp -s "$CURRENT_PROVENANCE" "$FROZEN_PROVENANCE"; then
  echo "GitHub Actions provenance changed while validating java-package." >&2
  echo "frozen:" >&2
  cat "$FROZEN_PROVENANCE" >&2
  echo "current:" >&2
  cat "$CURRENT_PROVENANCE" >&2
  exit 1
fi
check_checkout_clean

write_provenance_manifest() {
  "$PYTHON" - "$FROZEN_PROVENANCE" "$PROVENANCE_MANIFEST" <<'PY'
import os
import sys
from pathlib import Path


source = Path(sys.argv[1])
destination = Path(sys.argv[2])
contents = source.read_bytes()
try:
    descriptor = os.open(
        str(destination),
        os.O_WRONLY | os.O_CREAT | os.O_EXCL,
        0o600,
    )
except FileExistsError:
    print(
        "Refusing to overwrite provenance manifest created concurrently: "
        "{}".format(destination),
        file=sys.stderr,
    )
    raise SystemExit(1)
with os.fdopen(descriptor, "wb") as output:
    output.write(contents)
PY
}

if [[ "$DRY_RUN" == true ]]; then
  if [[ "$WRITE_PROVENANCE_MANIFEST" == true ]]; then
    write_provenance_manifest
    echo "Wrote frozen Java staging provenance: $PROVENANCE_MANIFEST"
  fi
  echo "Java staging dry run finished successfully without Maven, GPG, or Nexus."
  exit 0
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

validate_local_signing_key
validate_asf_keys_membership

PINNED_MAVEN_REPOSITORY="$STAGING_ROOT/pinned-maven-plugins"
install -d -m 700 "$PINNED_MAVEN_REPOSITORY"
PINNED_MAVEN_REPOSITORY_URI=$(
  "$PYTHON" \
    "$MAVEN_PLUGIN_PREPARER" \
    prepare \
    "$MAVEN_PLUGIN_LOCK" \
    "$PINNED_MAVEN_REPOSITORY" \
    "$CURL"
)

SIGNING_MAVEN_SETTINGS="$STAGING_ROOT/maven-signing-settings.xml"
NEXUS_MAVEN_SETTINGS="$STAGING_ROOT/maven-nexus-settings.xml"
EMPTY_GLOBAL_MAVEN_SETTINGS="$STAGING_ROOT/maven-global-settings.xml"
"$PYTHON" - \
  "$SOURCE_MAVEN_SETTINGS" \
  "$SIGNING_MAVEN_SETTINGS" \
  "$NEXUS_MAVEN_SETTINGS" \
  "$EMPTY_GLOBAL_MAVEN_SETTINGS" \
  "$NEXUS_SERVER_ID" \
  "$PINNED_MAVEN_MIRROR_ID" \
  "$PINNED_MAVEN_REPOSITORY_URI" <<'PY'
import copy
import os
import sys
import xml.etree.ElementTree as ET
from pathlib import Path


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


source = Path(sys.argv[1])
signing_destination = Path(sys.argv[2])
nexus_destination = Path(sys.argv[3])
global_destination = Path(sys.argv[4])
server_id = sys.argv[5]
mirror_id = sys.argv[6]
mirror_url = sys.argv[7]
if source.is_symlink() or not source.is_file():
    fail("Maven settings source is missing or unsafe: {}".format(source))
try:
    raw = source.read_bytes()
except OSError as error:
    fail("Unable to read Maven settings: {}".format(error))
if b"<!DOCTYPE" in raw.upper() or b"<!ENTITY" in raw.upper():
    fail("Maven settings must not contain a DTD or entity declaration")
try:
    original = ET.fromstring(raw)
except ET.ParseError as error:
    fail("Unable to parse Maven settings: {}".format(error))


def local_name(tag):
    return tag.rsplit("}", 1)[-1]


if local_name(original.tag) != "settings":
    fail("Maven settings root element must be <settings>")
namespace = ""
if original.tag.startswith("{"):
    namespace = original.tag[1:].split("}", 1)[0]
    ET.register_namespace("", namespace)


def qualified(name):
    return "{{{}}}{}".format(namespace, name) if namespace else name


children = {}
for child in original:
    name = local_name(child.tag)
    if name in children:
        fail("Duplicate top-level Maven settings element: {}".format(name))
    children[name] = child

servers = children.get("servers")
if servers is None:
    fail("Maven settings does not define server {}".format(server_id))
matches = []
for server in servers:
    if local_name(server.tag) != "server":
        continue
    ids = [
        element
        for element in server
        if local_name(element.tag) == "id"
    ]
    if len(ids) != 1:
        fail("Each Maven server must contain exactly one id")
    if (ids[0].text or "").strip() == server_id:
        matches.append(server)
if len(matches) != 1:
    fail(
        "Expected exactly one Maven server {}, found {}".format(
            server_id, len(matches)
        )
    )

signing = ET.Element(original.tag, dict(original.attrib))
nexus = ET.Element(original.tag, dict(original.attrib))
nexus_servers = ET.SubElement(
    nexus,
    qualified("servers"),
    dict(servers.attrib),
)
nexus_servers.append(copy.deepcopy(matches[0]))


def add_pinned_mirror(root, mirror_of):
    mirrors = ET.SubElement(root, qualified("mirrors"))
    mirror = ET.SubElement(mirrors, qualified("mirror"))
    ET.SubElement(mirror, qualified("id")).text = mirror_id
    ET.SubElement(mirror, qualified("url")).text = mirror_url
    ET.SubElement(mirror, qualified("mirrorOf")).text = mirror_of


add_pinned_mirror(signing, "*,!local-staging")
add_pinned_mirror(nexus, "*")

if "proxies" in children:
    nexus.append(copy.deepcopy(children["proxies"]))

empty_global = ET.Element(original.tag, dict(original.attrib))
for output, root in (
    (signing_destination, signing),
    (nexus_destination, nexus),
    (global_destination, empty_global),
):
    try:
        descriptor = os.open(
            str(output),
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o600,
        )
        with os.fdopen(descriptor, "wb") as target:
            ET.ElementTree(root).write(
                target,
                encoding="utf-8",
                xml_declaration=True,
                short_empty_elements=False,
            )
    except OSError as error:
        fail("Unable to write sanitized Maven settings: {}".format(error))
PY

MAVEN_TOOL_DIR="$STAGING_ROOT/maven-tool"
LOCAL_MAVEN_REPO="$STAGING_ROOT/file-repository"
SIGNING_MAVEN_RUNTIME_REPO="$STAGING_ROOT/maven-signing-runtime"
NEXUS_MAVEN_RUNTIME_REPO="$STAGING_ROOT/maven-nexus-runtime"
install -d -m 700 \
  "$MAVEN_TOOL_DIR" \
  "$LOCAL_MAVEN_REPO" \
  "$SIGNING_MAVEN_RUNTIME_REPO" \
  "$NEXUS_MAVEN_RUNTIME_REPO"
MAVEN_TOOL_POM="$MAVEN_TOOL_DIR/pom.xml"
cat > "$MAVEN_TOOL_POM" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>local.release</groupId>
  <artifactId>paimon-mosaic-java-staging-tool</artifactId>
  <version>1</version>
</project>
EOF
chmod 600 "$MAVEN_TOOL_POM"

MAIN_JAR="$JAVA_PACKAGE_DIR/mosaic-${RELEASE_VERSION}.jar"
SOURCES_JAR="$JAVA_PACKAGE_DIR/mosaic-${RELEASE_VERSION}-sources.jar"
JAVADOC_JAR="$JAVA_PACKAGE_DIR/mosaic-${RELEASE_VERSION}-javadoc.jar"
FROZEN_JAVA_PAYLOADS=$(
  "$PYTHON" - \
    "$MAIN_JAR" \
    "$SOURCES_JAR" \
    "$JAVADOC_JAR" \
    "$SIGNED_POM" <<'PY'
import hashlib
import sys
from pathlib import Path


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            value.update(chunk)
    return value.hexdigest()


labels = ("main", "sources", "javadoc", "pom")
for label, value in zip(labels, sys.argv[1:5]):
    path = Path(value)
    if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
        fail("Frozen Java staging input is missing or unsafe: {}".format(path))
    print("{}\t{}\t{}".format(label, path.stat().st_size, digest(path)))
PY
)

MAVEN_COMMON=(
  "$MVN"
  -B
  -ntp
  -f "$MAVEN_TOOL_POM"
  -gs "$EMPTY_GLOBAL_MAVEN_SETTINGS"
)
SIGNING_MAVEN_BASE=(
  "${MAVEN_COMMON[@]}"
  -s "$SIGNING_MAVEN_SETTINGS"
  "-Dmaven.repo.local=$SIGNING_MAVEN_RUNTIME_REPO"
)
NEXUS_MAVEN_BASE=(
  "${MAVEN_COMMON[@]}"
  -s "$NEXUS_MAVEN_SETTINGS"
  "-Dmaven.repo.local=$NEXUS_MAVEN_RUNTIME_REPO"
)

run_maven() {
  (
    cd "$MAVEN_TOOL_DIR" || exit $?
    env \
      MAVEN_SKIP_RC=1 \
      MAVEN_ARGS= \
      MAVEN_OPTS= \
      MAVEN_DEBUG_OPTS= \
      MAVEN_CONFIG= \
      MAVEN_BASEDIR="$MAVEN_TOOL_DIR" \
      MAVEN_PROJECTBASEDIR= \
      JAVA_TOOL_OPTIONS= \
      JDK_JAVA_OPTIONS= \
      _JAVA_OPTIONS= \
      MAVEN_GPG_KEY= \
      MAVEN_GPG_KEY_FINGERPRINT= \
      MAVEN_GPG_PASSPHRASE= \
      "$@"
  )
}

echo "Signing the frozen CI Java candidate into a private file:// repository."
run_maven \
  "${SIGNING_MAVEN_BASE[@]}" \
  "$GPG_PLUGIN_GOAL" \
  -DgroupId=org.apache.paimon \
  -DartifactId=mosaic \
  "-Dversion=$RELEASE_VERSION" \
  -Dpackaging=jar \
  -Dclassifier= \
  "-Dfile=$MAIN_JAR" \
  "-Dsources=$SOURCES_JAR" \
  "-Djavadoc=$JAVADOC_JAR" \
  "-DpomFile=$SIGNED_POM" \
  -DgeneratePom=false \
  "-Durl=file://$LOCAL_MAVEN_REPO" \
  -DrepositoryId=local-staging \
  -Dgpg.skip=false \
  -Dgpg.signer=gpg \
  "-Dgpg.executable=$GPG" \
  "-Dgpg.keyname=${GPG_KEYNAME}!" \
  -Dgpg.bestPractices=true

LOCAL_VERSION_DIR="$LOCAL_MAVEN_REPO/org/apache/paimon/mosaic/$RELEASE_VERSION"

verify_frozen_java_payloads() {
  "$PYTHON" - \
    "$FROZEN_JAVA_PAYLOADS" \
    "$LOCAL_MAVEN_REPO" \
    "$RELEASE_VERSION" \
    "$MAIN_JAR" \
    "$SOURCES_JAR" \
    "$JAVADOC_JAR" \
    "$SIGNED_POM" <<'PY'
import hashlib
import sys
from pathlib import Path


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            value.update(chunk)
    return value.hexdigest()


frozen_lines = sys.argv[1].splitlines()
root = Path(sys.argv[2])
version = sys.argv[3]
inputs = tuple(Path(value) for value in sys.argv[4:8])
if root.is_symlink() or not root.is_dir():
    fail("Private Maven repository is missing or unsafe")

labels = ("main", "sources", "javadoc", "pom")
frozen = {}
for line in frozen_lines:
    parts = line.split("\t")
    if len(parts) != 3:
        fail("Frozen Java staging payload manifest is malformed")
    label, size_text, expected_digest = parts
    if (
        label not in labels
        or label in frozen
        or not size_text.isdigit()
        or int(size_text) <= 0
        or len(expected_digest) != 64
        or any(character not in "0123456789abcdef" for character in expected_digest)
    ):
        fail("Frozen Java staging payload manifest is malformed")
    frozen[label] = (int(size_text), expected_digest)
if set(frozen) != set(labels):
    fail("Frozen Java staging payload manifest is incomplete")

artifact_dir = Path("org/apache/paimon/mosaic")
version_dir = artifact_dir / version
allowed_dirs = {
    Path("org"),
    Path("org/apache"),
    Path("org/apache/paimon"),
    artifact_dir,
    version_dir,
}
payload_names = (
    "mosaic-{}.jar".format(version),
    "mosaic-{}-sources.jar".format(version),
    "mosaic-{}-javadoc.jar".format(version),
    "mosaic-{}.pom".format(version),
)
payload_set = set(payload_names)
signature_set = {name + ".asc" for name in payload_names}
checksum_suffixes = (".md5", ".sha1", ".sha256", ".sha512")

seen_payloads = set()
seen_signatures = set()
for path in root.rglob("*"):
    relative = path.relative_to(root)
    if path.is_symlink():
        fail("Private Maven repository contains a symlink: {}".format(relative))
    if path.is_dir():
        if relative not in allowed_dirs:
            fail(
                "Private Maven repository contains an unexpected directory: {}"
                .format(relative)
            )
        continue
    if not path.is_file():
        fail(
            "Private Maven repository contains a non-regular entry: {}".format(
                relative
            )
        )

    parent = relative.parent
    name = relative.name
    if parent == version_dir:
        if name in payload_set:
            seen_payloads.add(name)
            continue
        if name in signature_set:
            seen_signatures.add(name)
            continue
        checksum_base = next(
            (
                name[: -len(suffix)]
                for suffix in checksum_suffixes
                if name.endswith(suffix)
            ),
            None,
        )
        if checksum_base in payload_set or checksum_base in signature_set:
            continue
        fail(
            "Private Maven repository contains an unexpected version file: {}"
            .format(relative)
        )
    elif parent == artifact_dir:
        metadata_base = name
        for suffix in checksum_suffixes:
            if metadata_base.endswith(suffix):
                metadata_base = metadata_base[: -len(suffix)]
                break
        if metadata_base in ("maven-metadata.xml", "maven-metadata-local.xml"):
            continue
        fail(
            "Private Maven repository contains unexpected artifact metadata: {}"
            .format(relative)
        )
    else:
        fail(
            "Private Maven repository contains a file outside the GAV path: {}"
            .format(relative)
        )

if seen_payloads != payload_set or seen_signatures != signature_set:
    fail(
        "Private Maven repository payload/signature set is incomplete: "
        "payloads={}, signatures={}".format(
            sorted(seen_payloads), sorted(seen_signatures)
        )
    )

repository_payloads = tuple(root / version_dir / name for name in payload_names)
for label, source, deployed in zip(labels, inputs, repository_payloads):
    expected_size, expected_digest = frozen[label]
    if (
        source.is_symlink()
        or not source.is_file()
        or source.stat().st_size != expected_size
        or digest(source) != expected_digest
    ):
        fail(
            "Frozen Java staging input changed after signing: {}".format(
                source.name
            )
        )
    if (
        deployed.is_symlink()
        or not deployed.is_file()
        or deployed.stat().st_size != expected_size
        or digest(deployed) != expected_digest
    ):
        fail(
            "Private Maven repository payload differs from frozen input: {}"
            .format(deployed.name)
        )
PY
}

verify_frozen_java_payloads

verify_local_signature() {
  local payload=$1
  local signature="${payload}.asc"
  local name
  local status_file
  local stderr_file

  name=$(basename "$payload")
  status_file="$STAGING_ROOT/gpg-status-${name}.txt"
  stderr_file="$STAGING_ROOT/gpg-stderr-${name}.txt"
  if [[ -L "$signature" || ! -f "$signature" || ! -s "$signature" ]]; then
    echo "Missing detached signature before Nexus upload: $signature" >&2
    exit 1
  fi
  if ! "$GPG" \
    --batch \
    --status-fd 1 \
    --verify "$signature" "$payload" \
    > "$status_file" 2> "$stderr_file"; then
    cat "$stderr_file" >&2
    echo "Detached signature verification failed: $signature" >&2
    exit 1
  fi
  if ! awk -v expected="$GPG_KEYNAME" '
    $1 == "[GNUPG:]" && $2 == "VALIDSIG" {
      count += 1
      signing = toupper($3)
      primary = toupper($NF)
      if (signing == expected || primary == expected) {
        matched += 1
      }
    }
    END {
      if (count != 1 || matched != 1) {
        exit 1
      }
    }
  ' "$status_file"; then
    cat "$status_file" >&2
    echo "Signature was not made by expected fingerprint $GPG_KEYNAME: $signature" >&2
    exit 1
  fi
}

verify_local_signature "$LOCAL_VERSION_DIR/mosaic-${RELEASE_VERSION}.jar"
verify_local_signature "$LOCAL_VERSION_DIR/mosaic-${RELEASE_VERSION}-sources.jar"
verify_local_signature "$LOCAL_VERSION_DIR/mosaic-${RELEASE_VERSION}-javadoc.jar"
verify_local_signature "$LOCAL_VERSION_DIR/mosaic-${RELEASE_VERSION}.pom"

# This is the last read-only boundary before the only remote write. A moved
# tag, rerun workflow, replaced artifact, changed manifest, or dirty checkout
# discards the private repository through the EXIT trap and stops here.
validate_frozen_provenance
check_checkout_clean
verify_frozen_java_payloads
"$PYTHON" \
  "$MAVEN_PLUGIN_PREPARER" \
  verify \
  "$MAVEN_PLUGIN_LOCK" \
  "$PINNED_MAVEN_REPOSITORY"

echo "Uploading the verified private repository to Apache Nexus staging."
run_maven \
  "${NEXUS_MAVEN_BASE[@]}" \
  "$NEXUS_PLUGIN_GOAL" \
  "-DrepositoryDirectory=$LOCAL_MAVEN_REPO" \
  "-DnexusUrl=$NEXUS_URL" \
  "-DserverId=$NEXUS_SERVER_ID" \
  "-DstagingProfileId=$STAGING_PROFILE_ID" \
  -DstagingRepositoryId= \
  "-DstagingDescription=$STAGING_DESCRIPTION" \
  -DautoReleaseAfterClose=false \
  -DautoDropAfterRelease=false \
  -DdetectBuildFailures=true \
  -DkeepStagingRepositoryOnFailure=false \
  -DkeepStagingRepositoryOnCloseRuleFailure=false \
  -DskipStaging=false \
  -DskipStagingRepositoryClose=false \
  -DskipRemoteStaging=false \
  -DskipNexusStagingDeployMojo=false \
  -Dmaven.wagon.http.ssl.allowall=false \
  -Dmaven.wagon.http.ssl.insecure=false

echo "Java staging deploy finished."
echo "Leave the resulting orgapachepaimon-XXXX repository staged for the vote."
