#!/usr/bin/env bash

#
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
#

# Create ASF source release artifacts under tools/release/:
#   apache-paimon-mosaic-{version}-src.tgz
#   apache-paimon-mosaic-{version}-src.tgz.asc
#   apache-paimon-mosaic-{version}-src.tgz.sha512
#
# Usage:
#   cd tools
#   RELEASE_VERSION=0.3.0 RC_TAG=v0.3.0-rc1 ./create_source_release.sh

# fail immediately
set -o errexit
set -o nounset
set -o pipefail

# Do not let inherited Git selection variables redirect any check to another
# repository or worktree.
unset \
  GIT_ALTERNATE_OBJECT_DIRECTORIES \
  GIT_COMMON_DIR \
  GIT_DIR \
  GIT_INDEX_FILE \
  GIT_NAMESPACE \
  GIT_OBJECT_DIRECTORY \
  GIT_WORK_TREE
export GIT_ATTR_NOSYSTEM=1
export GIT_NO_REPLACE_OBJECTS=1

CURR_DIR=$(pwd -P)
if [[ $(basename "${CURR_DIR}") != "tools" ]] ; then
  echo "You have to call the script from the tools/ dir"
  exit 1
fi

RELEASE_VERSION=${RELEASE_VERSION:-}
RC_TAG=${RC_TAG:-}
if [[ -z "${RELEASE_VERSION}" ]]; then
  echo "RELEASE_VERSION is unset" >&2
  exit 1
fi
if [[ -z "${RC_TAG}" ]]; then
  echo "RC_TAG is unset" >&2
  exit 1
fi
if [[ ! "${RC_TAG}" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)-rc[0-9]+$ ]]; then
  echo "RC_TAG must have the form vX.Y.Z-rcN: ${RC_TAG}" >&2
  exit 1
fi
if [[ "${BASH_REMATCH[1]}" != "${RELEASE_VERSION}" ]]; then
  echo \
    "RC_TAG ${RC_TAG} does not match RELEASE_VERSION ${RELEASE_VERSION}" \
    >&2
  exit 1
fi

cd ..
REPOSITORY=$(pwd -P)

if [[ -n $(git status --porcelain --untracked-files=all) ]]; then
  echo "The source release must be created from a clean Git worktree" >&2
  git status --short >&2
  exit 1
fi

HEAD_COMMIT=$(git rev-parse --verify 'HEAD^{commit}')
TAG_COMMIT=$(git rev-parse --verify "${RC_TAG}^{commit}")
if [[ "${TAG_COMMIT}" != "${HEAD_COMMIT}" ]]; then
  echo \
    "RC_TAG ${RC_TAG} does not resolve to current HEAD ${HEAD_COMMIT}" \
    >&2
  exit 1
fi
TAG_OBJECT=$(git cat-file tag "${RC_TAG}" 2>/dev/null || true)
if ! grep -q '^-----BEGIN PGP SIGNATURE-----$' <<< "${TAG_OBJECT}" ||
  ! git verify-tag "${RC_TAG}"
then
  echo \
    "RC_TAG ${RC_TAG} is not a locally verifiable GPG-signed tag" \
    >&2
  exit 1
fi

ARCHIVE="apache-paimon-mosaic-${RELEASE_VERSION}-src.tgz"
SIGNATURE="${ARCHIVE}.asc"
CHECKSUM="${ARCHIVE}.sha512"
ARCHIVE_ROOT="paimon-mosaic-${RELEASE_VERSION}"
RELEASE_DIR="${CURR_DIR}/release"

if [[ -e "${RELEASE_DIR}" || -L "${RELEASE_DIR}" ]]; then
  echo "Release output already exists and will not be overwritten: ${RELEASE_DIR}" >&2
  exit 1
fi

STAGING_DIR=$(mktemp -d "${CURR_DIR}/.source-release.XXXXXX")
ARTIFACT_DIR="${STAGING_DIR}/release"
mkdir "${ARTIFACT_DIR}"
cleanup() {
  status=$?
  rm -rf "${STAGING_DIR}"
  exit "${status}"
}
trap cleanup EXIT

echo "Creating source package from signed tag ${RC_TAG}"
python3 tools/verify_source_archive.py create \
  --repository "${REPOSITORY}" \
  --commit "${RC_TAG}" \
  --prefix "${ARCHIVE_ROOT}/" \
  --output "${ARTIFACT_DIR}/${ARCHIVE}"

mkdir "${STAGING_DIR}/extracted"
tar xzf "${ARTIFACT_DIR}/${ARCHIVE}" -C "${STAGING_DIR}/extracted"
(
  cd "${STAGING_DIR}/extracted/${ARCHIVE_ROOT}"
  python3 tools/verify_release_versions.py "${RC_TAG}"
)

gpg \
  --armor \
  --detach-sign \
  --output "${ARTIFACT_DIR}/${SIGNATURE}" \
  "${ARTIFACT_DIR}/${ARCHIVE}"

(
  cd "${ARTIFACT_DIR}"
  if [[ $(uname) == "Darwin" ]]; then
    shasum -a 512 "${ARCHIVE}" > "${CHECKSUM}"
    shasum -a 512 -c "${CHECKSUM}"
  else
    sha512sum "${ARCHIVE}" > "${CHECKSUM}"
    sha512sum -c "${CHECKSUM}"
  fi
  gpg --verify "${SIGNATURE}" "${ARCHIVE}"
)

python3 tools/verify_source_archive.py verify \
  --repository "${REPOSITORY}" \
  --commit "${RC_TAG}" \
  --prefix "${ARCHIVE_ROOT}/" \
  --archive "${ARTIFACT_DIR}/${ARCHIVE}"

# The staging and release directories share a filesystem, so this publishes
# the fully verified artifact set with one atomic directory rename.
python3 -c \
  'import os, sys; os.rename(sys.argv[1], sys.argv[2])' \
  "${ARTIFACT_DIR}" \
  "${RELEASE_DIR}"

trap - EXIT
rm -rf "${STAGING_DIR}"

echo ""
echo "Source release created successfully. Artifacts in tools/release/:"
ls -la "${RELEASE_DIR}"/apache-paimon-mosaic-*
echo ""
echo "Next: upload contents to SVN (see docs/creating-a-release.html)."
