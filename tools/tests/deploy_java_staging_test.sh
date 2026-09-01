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
REPO_ROOT=$(cd "$TOOLS_DIR/.." && pwd)
REAL_GIT=$(command -v git)
PYTHON=${PYTHON:-python3}
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
    sed -n '1,240p' "$file" >&2
    fail "missing expected output"
  fi
}

assert_not_contains() {
  local file=$1
  local pattern=$2
  if [[ -f "$file" ]] && grep -Fq -- "$pattern" "$file"; then
    echo "Did not expect '$pattern' in $file" >&2
    sed -n '1,240p' "$file" >&2
    fail "unexpected output"
  fi
}

assert_maven_not_invoked() {
  if [[ -s "$MAVEN_LOG" ]]; then
    sed -n '1,240p' "$MAVEN_LOG" >&2
    fail "Maven must not be invoked"
  fi
}

create_candidate_artifacts() {
  local tag_object
  local commit
  local candidate_run_attempt

  tag_object=$(git -C "$FIXTURE_DIR" rev-parse "v0.3.0-rc1^{tag}")
  commit=$(git -C "$FIXTURE_DIR" rev-parse "v0.3.0-rc1^{commit}")
  candidate_run_attempt=${FAKE_CANDIDATE_RUN_ATTEMPT:-$FAKE_RUN_ATTEMPT}
  "$PYTHON" - \
    "$FIXTURE_DIR" \
    "$CANDIDATE_DIR" \
    "$ARTIFACT_ZIP" \
    "$tag_object" \
    "$commit" \
    "$candidate_run_attempt" <<'PY'
import re
import struct
import sys
import zipfile
from pathlib import Path


fixture = Path(sys.argv[1])
candidate = Path(sys.argv[2])
artifact_zip = Path(sys.argv[3])
tag_object = sys.argv[4]
commit = sys.argv[5]
run_attempt = sys.argv[6]
candidate.mkdir(parents=True, exist_ok=True)

version = "0.3.0"
main_name = "mosaic-{}.jar".format(version)
sources_name = "mosaic-{}-sources.jar".format(version)
javadoc_name = "mosaic-{}-javadoc.jar".format(version)
pom = (fixture / "java/pom.xml").read_bytes()
source_path = fixture / (
    "java/src/main/java/org/apache/paimon/mosaic/MosaicReader.java"
)
source_root = fixture / "java/src/main/java"
source_files = sorted(source_root.rglob("*.java"))
license_text = b"\n" + (fixture / "LICENSE").read_bytes()
notice_text = (
    b"\nMosaic\nCopyright 2026-2025 The Apache Software Foundation\n\n"
    b"This product includes software developed at\n"
    b"The Apache Software Foundation (http://www.apache.org/).\n\n\n"
)
dependencies_text = (
    b"// ------------------------------------------------------------------\n"
    b"// Transitive dependencies of this project determined from the\n"
    b"// maven pom organized by organization.\n"
    b"// ------------------------------------------------------------------\n\n"
    b"Mosaic\n\n\n\n\n\n"
)
class_bytes = b"\xca\xfe\xba\xbe\x00\x00\x00\x34\x00\x01" + b"\x00" * 54
class_entries = (
    "org/apache/paimon/mosaic/ColumnStatistics.class",
    "org/apache/paimon/mosaic/InputFile.class",
    "org/apache/paimon/mosaic/MosaicReader.class",
    "org/apache/paimon/mosaic/MosaicWriter$1.class",
    "org/apache/paimon/mosaic/MosaicWriter$RootArrayExporter.class",
    "org/apache/paimon/mosaic/MosaicWriter$RootArrayPrivateData.class",
    "org/apache/paimon/mosaic/MosaicWriter.class",
    "org/apache/paimon/mosaic/NativeLib.class",
    "org/apache/paimon/mosaic/WriterOptions.class",
)

native_source = (
    source_root / "org/apache/paimon/mosaic/NativeLib.java"
).read_text(encoding="utf-8")
native_methods = re.findall(
    r"\bnative\s+[A-Za-z0-9_.$<>\[\]?]+\s+"
    r"([A-Za-z_$][A-Za-z0-9_$]*)\s*\(",
    native_source,
)
symbol_blob = b"\x00".join(
    (
        "Java_org_apache_paimon_mosaic_NativeLib_" + method
    ).encode("ascii")
    for method in native_methods
)


def elf(machine):
    data = bytearray(64 * 1024)
    data[:4] = b"\x7fELF"
    data[4] = 2
    data[5] = 1
    data[6] = 1
    struct.pack_into("<H", data, 16, 3)
    struct.pack_into("<H", data, 18, machine)
    struct.pack_into("<I", data, 20, 1)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<H", data, 52, 64)
    struct.pack_into("<H", data, 54, 56)
    struct.pack_into("<H", data, 56, 2)
    struct.pack_into("<I", data, 64, 1)
    struct.pack_into("<I", data, 68, 5)
    struct.pack_into("<Q", data, 72, 0)
    struct.pack_into("<Q", data, 96, len(data))
    struct.pack_into("<Q", data, 104, len(data))
    struct.pack_into("<Q", data, 112, 4096)
    struct.pack_into("<I", data, 120, 2)
    struct.pack_into("<I", data, 124, 6)
    struct.pack_into("<Q", data, 128, 4096)
    struct.pack_into("<Q", data, 152, 512)
    struct.pack_into("<Q", data, 160, 512)
    struct.pack_into("<Q", data, 168, 8)
    data[8192 : 8192 + len(symbol_blob)] = symbol_blob
    return bytes(data)


def macho():
    data = bytearray(64 * 1024)
    data[:4] = b"\xcf\xfa\xed\xfe"
    struct.pack_into("<I", data, 4, 0x0100000C)
    struct.pack_into("<I", data, 12, 6)
    struct.pack_into("<I", data, 16, 2)
    struct.pack_into("<I", data, 20, 104)
    struct.pack_into("<I", data, 32, 0x19)
    struct.pack_into("<I", data, 36, 72)
    data[40:56] = b"__TEXT\x00" + b"\x00" * 9
    struct.pack_into("<Q", data, 64, len(data))
    struct.pack_into("<Q", data, 72, 0)
    struct.pack_into("<Q", data, 80, len(data))
    struct.pack_into("<I", data, 88, 7)
    struct.pack_into("<I", data, 92, 5)
    struct.pack_into("<I", data, 104, 0xD)
    struct.pack_into("<I", data, 108, 32)
    struct.pack_into("<I", data, 112, 24)
    data[128:135] = b"mosaic\x00"
    data[4096 : 4096 + len(symbol_blob)] = symbol_blob
    return bytes(data)


def pe():
    data = bytearray(64 * 1024)
    data[:2] = b"MZ"
    pe_offset = 0x80
    struct.pack_into("<I", data, 0x3C, pe_offset)
    data[pe_offset : pe_offset + 4] = b"PE\x00\x00"
    struct.pack_into("<H", data, pe_offset + 4, 0x8664)
    struct.pack_into("<H", data, pe_offset + 6, 1)
    struct.pack_into("<H", data, pe_offset + 20, 240)
    struct.pack_into("<H", data, pe_offset + 22, 0x2022)
    optional = pe_offset + 24
    struct.pack_into("<H", data, optional, 0x20B)
    struct.pack_into("<I", data, optional + 60, 0x200)
    struct.pack_into("<I", data, optional + 108, 16)
    struct.pack_into("<I", data, optional + 112, 0x1000)
    struct.pack_into("<I", data, optional + 116, 0x400)
    section = optional + 240
    data[section : section + 8] = b".text\x00\x00\x00"
    struct.pack_into("<I", data, section + 8, len(data) - 0x200)
    struct.pack_into("<I", data, section + 12, 0x1000)
    struct.pack_into("<I", data, section + 16, len(data) - 0x200)
    struct.pack_into("<I", data, section + 20, 0x200)
    struct.pack_into("<I", data, section + 36, 0x60000020)
    data[4096 : 4096 + len(symbol_blob)] = symbol_blob
    return bytes(data)


def write_jar(path, entries):
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, contents in entries:
            info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, contents)


legal = [
    ("META-INF/LICENSE", license_text),
    ("META-INF/NOTICE", notice_text),
    ("META-INF/DEPENDENCIES", dependencies_text),
]
write_jar(
    candidate / main_name,
    [
        *legal,
        *((entry, class_bytes) for entry in class_entries),
        (
            "META-INF/maven/org.apache.paimon/mosaic/pom.xml",
            pom,
        ),
        (
            "META-INF/maven/org.apache.paimon/mosaic/pom.properties",
            b"artifactId=mosaic\n"
            b"groupId=org.apache.paimon\n"
            b"version=0.3.0\n",
        ),
        (
            "native/linux/x86_64/libpaimon_mosaic_jni.so",
            elf(62),
        ),
        (
            "native/linux/aarch64/libpaimon_mosaic_jni.so",
            elf(183),
        ),
        (
            "native/macos/aarch64/libpaimon_mosaic_jni.dylib",
            macho(),
        ),
        (
            "native/windows/x86_64/paimon_mosaic_jni.dll",
            pe(),
        ),
    ],
)
write_jar(
    candidate / sources_name,
    [
        *legal,
        *(
            (
                path.relative_to(source_root).as_posix(),
                path.read_bytes(),
            )
            for path in source_files
        ),
    ],
)
javadoc_entries = {
    "allclasses-frame.html": b"<html>All Classes</html>\n",
    "allclasses-noframe.html": b"<html>All Classes</html>\n",
    "index-all.html": b"<html>Index</html>\n",
    "index.html": b"<html><body>Mosaic</body></html>\n",
    "overview-tree.html": b"<html>Overview Tree</html>\n",
    "package-list": b"org.apache.paimon.mosaic\n",
    "script.js": b"function loadFrames() {}\n",
    "stylesheet.css": b"body { color: black; }\n",
    "org/apache/paimon/mosaic/package-frame.html": (
        b"<html>org.apache.paimon.mosaic</html>\n"
    ),
    "org/apache/paimon/mosaic/package-summary.html": (
        b"<html>org.apache.paimon.mosaic</html>\n"
    ),
    "org/apache/paimon/mosaic/package-tree.html": (
        b"<html>org.apache.paimon.mosaic</html>\n"
    ),
    "org/apache/paimon/mosaic/package-use.html": (
        b"<html>org.apache.paimon.mosaic</html>\n"
    ),
}
for path in source_files:
    text = path.read_text(encoding="utf-8")
    matches = re.findall(
        r"(?m)^\s*public\s+(?:(?:abstract|final)\s+)?"
        r"(?:class|interface|enum)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        text,
    )
    if matches:
        package = path.relative_to(source_root).parent.as_posix()
        type_name = matches[0]
        javadoc_entries["{}/{}.html".format(package, type_name)] = (
            "<html>{}</html>\n".format(type_name).encode("utf-8")
        )
        javadoc_entries[
            "{}/class-use/{}.html".format(package, type_name)
        ] = "<html>{}</html>\n".format(type_name).encode("utf-8")
write_jar(
    candidate / javadoc_name,
    [
        *legal,
        *sorted(javadoc_entries.items()),
    ],
)

provenance_name = "java-staging-provenance.txt"
(candidate / provenance_name).write_text(
    "\n".join(
        (
            "schema=1",
            "repository=apache/paimon-mosaic",
            "tag=v0.3.0-rc1",
            "tag_object={}".format(tag_object),
            "commit={}".format(commit),
            "run_id=42",
            "run_attempt={}".format(run_attempt),
        )
    )
    + "\n",
    encoding="utf-8",
)

with zipfile.ZipFile(artifact_zip, "w", zipfile.ZIP_DEFLATED) as archive:
    for name in (main_name, sources_name, javadoc_name, provenance_name):
        info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        archive.writestr(info, (candidate / name).read_bytes())
PY
}

write_expected_provenance() {
  local tag_object
  local tag_commit

  tag_object=$(git -C "$FIXTURE_DIR" rev-parse "v0.3.0-rc1^{tag}")
  tag_commit=$(git -C "$FIXTURE_DIR" rev-parse "v0.3.0-rc1^{commit}")
  cat > "$EXPECTED_PROVENANCE" <<EOF
schema=1
repository=apache/paimon-mosaic
tag=v0.3.0-rc1
tag_object=$tag_object
commit=$tag_commit
workflow_path=.github/workflows/release.yml
workflow_id=$FAKE_WORKFLOW_ID
run_id=42
run_attempt=$FAKE_RUN_ATTEMPT
staging_profile_id=$STAGING_PROFILE_ID
artifact_name=java-package
artifact_id=$FAKE_ARTIFACT_ID
artifact_digest=$FAKE_ARTIFACT_DIGEST
artifact_size=$FAKE_ARTIFACT_SIZE
EOF
  cp "$EXPECTED_PROVENANCE" "$PROVENANCE_PATH"
}

repack_candidate_and_refresh_provenance() {
  "$PYTHON" - "$CANDIDATE_DIR" "$ARTIFACT_ZIP" <<'PY'
import os
import sys
import zipfile
from pathlib import Path


candidate = Path(sys.argv[1])
artifact = Path(sys.argv[2])
temporary = artifact.with_suffix(".tmp")
with zipfile.ZipFile(temporary, "w", zipfile.ZIP_DEFLATED) as archive:
    for path in sorted(candidate.iterdir()):
        if not path.is_file():
            continue
        info = zipfile.ZipInfo(
            path.name,
            date_time=(2026, 1, 1, 0, 0, 0),
        )
        info.compress_type = zipfile.ZIP_DEFLATED
        archive.writestr(info, path.read_bytes())
os.replace(temporary, artifact)
PY
  FAKE_ARTIFACT_DIGEST="sha256:$(sha256sum "$ARTIFACT_ZIP" | awk '{print $1}')"
  FAKE_ARTIFACT_SIZE=$(wc -c < "$ARTIFACT_ZIP")
  export FAKE_ARTIFACT_DIGEST FAKE_ARTIFACT_SIZE
  write_expected_provenance
}

rewrite_candidate_jar_entry() {
  local jar_name=$1
  local entry_name=$2
  local mode=$3
  local replacement=${4-}

  "$PYTHON" - \
    "$CANDIDATE_DIR/$jar_name" \
    "$entry_name" \
    "$mode" \
    "$replacement" <<'PY'
import os
import sys
import zipfile
from pathlib import Path


path = Path(sys.argv[1])
entry_name = sys.argv[2]
mode = sys.argv[3]
replacement = sys.argv[4].encode()
temporary = path.with_suffix(".tmp")
found = False
with zipfile.ZipFile(path) as source, zipfile.ZipFile(
    temporary, "w", zipfile.ZIP_DEFLATED
) as target:
    for info in source.infolist():
        contents = source.read(info)
        if info.filename == entry_name:
            found = True
            if mode == "remove":
                continue
            if mode == "replace":
                contents = replacement
            if mode == "truncate":
                contents = contents[: int(sys.argv[4])]
        output_info = zipfile.ZipInfo(
            info.filename,
            date_time=(2026, 1, 1, 0, 0, 0),
        )
        output_info.compress_type = zipfile.ZIP_DEFLATED
        target.writestr(output_info, contents)
if not found:
    raise SystemExit("entry not found: {}".format(entry_name))
os.replace(temporary, path)
PY
  repack_candidate_and_refresh_provenance
}

add_candidate_jar_entry() {
  local jar_name=$1
  local entry_name=$2
  local contents=$3

  "$PYTHON" - \
    "$CANDIDATE_DIR/$jar_name" \
    "$entry_name" \
    "$contents" <<'PY'
import os
import sys
import zipfile
from pathlib import Path


path = Path(sys.argv[1])
entry_name = sys.argv[2]
contents = sys.argv[3].encode()
temporary = path.with_suffix(".tmp")
with zipfile.ZipFile(path) as source, zipfile.ZipFile(
    temporary, "w", zipfile.ZIP_DEFLATED
) as target:
    for info in source.infolist():
        output_info = zipfile.ZipInfo(
            info.filename,
            date_time=(2026, 1, 1, 0, 0, 0),
        )
        output_info.compress_type = zipfile.ZIP_DEFLATED
        target.writestr(output_info, source.read(info))
    output_info = zipfile.ZipInfo(
        entry_name,
        date_time=(2026, 1, 1, 0, 0, 0),
    )
    output_info.compress_type = zipfile.ZIP_DEFLATED
    target.writestr(output_info, contents)
os.replace(temporary, path)
PY
  repack_candidate_and_refresh_provenance
}

new_fixture() {
  FIXTURE_DIR=$(mktemp -d "$TEST_ROOT/fixture.XXXXXX")
  CANDIDATE_DIR=$(mktemp -d "$TEST_ROOT/candidate.XXXXXX")
  ARTIFACT_ZIP=$(mktemp "$TEST_ROOT/java-package.XXXXXX.zip")
  EXPECTED_PROVENANCE=$(mktemp "$TEST_ROOT/expected-provenance.XXXXXX")
  PROVENANCE_PATH=$(mktemp "$TEST_ROOT/provenance.XXXXXX")
  GIT_LOG="$FIXTURE_DIR/git.log"
  MAVEN_LOG="$FIXTURE_DIR/maven.log"
  GPG_LOG="$FIXTURE_DIR/gpg.log"
  GH_LOG="$FIXTURE_DIR/gh.log"
  GH_RUN_COUNT="$FIXTURE_DIR/gh-run-count.log"
  GH_TAG_COUNT="$FIXTURE_DIR/gh-tag-count.log"
  CURL_LOG="$FIXTURE_DIR/curl.log"
  NEXUS_CALLED_LOG="$FIXTURE_DIR/nexus-called.log"
  SIGNING_SETTINGS_COPY=$(mktemp "$TEST_ROOT/signing-settings.XXXXXX.xml")
  NEXUS_SETTINGS_COPY=$(mktemp "$TEST_ROOT/nexus-settings.XXXXXX.xml")
  EMPTY_GLOBAL_SETTINGS_COPY=$(
    mktemp "$TEST_ROOT/empty-global-settings.XXXXXX.xml"
  )
  PLUGIN_SOURCE_DIR="$FIXTURE_DIR/plugin-source"
  OUTPUT_LOG="$FIXTURE_DIR/output.log"
  STAGING_PROFILE_ID=paimon-profile-123
  FAKE_WORKFLOW_ID=7001
  FAKE_RUN_ATTEMPT=1
  FAKE_ARTIFACT_ID=9001
  export \
    FIXTURE_DIR \
    CANDIDATE_DIR \
    ARTIFACT_ZIP \
    EXPECTED_PROVENANCE \
    PROVENANCE_PATH \
    GIT_LOG \
    MAVEN_LOG \
    GPG_LOG \
    GH_LOG \
    GH_RUN_COUNT \
    GH_TAG_COUNT \
    CURL_LOG \
    NEXUS_CALLED_LOG \
    SIGNING_SETTINGS_COPY \
    NEXUS_SETTINGS_COPY \
    EMPTY_GLOBAL_SETTINGS_COPY \
    PLUGIN_SOURCE_DIR \
    OUTPUT_LOG \
    STAGING_PROFILE_ID \
    FAKE_WORKFLOW_ID \
    FAKE_RUN_ATTEMPT \
    FAKE_ARTIFACT_ID

  mkdir -p \
    "$FIXTURE_DIR/fake-bin" \
    "$FIXTURE_DIR/home/.m2" \
    "$FIXTURE_DIR/java/src/main/java/org/apache/paimon/mosaic" \
    "$PLUGIN_SOURCE_DIR" \
    "$FIXTURE_DIR/tools"

  cp "$REPO_ROOT/LICENSE" "$FIXTURE_DIR/LICENSE"
  cp -a \
    "$REPO_ROOT/java/src/main/java/." \
    "$FIXTURE_DIR/java/src/main/java/"
  cp "$TOOLS_DIR/deploy_java_staging.sh" "$FIXTURE_DIR/tools/"
  cp "$TOOLS_DIR/prepare_java_staging_maven_plugins.py" "$FIXTURE_DIR/tools/"
  cp "$TOOLS_DIR/validate_java_staging_artifacts.sh" "$FIXTURE_DIR/tools/"
  chmod +x \
    "$FIXTURE_DIR/tools/deploy_java_staging.sh" \
    "$FIXTURE_DIR/tools/validate_java_staging_artifacts.sh"

  "$PYTHON" - "$PLUGIN_SOURCE_DIR" "$FIXTURE_DIR/tools" <<'PY'
import hashlib
import sys
from pathlib import Path


source_root = Path(sys.argv[1])
tools = Path(sys.argv[2])
artifacts = tuple(sorted((
    "org/apache/maven/plugins/maven-gpg-plugin/3.2.8/"
    "maven-gpg-plugin-3.2.8.jar",
    "org/apache/maven/plugins/maven-gpg-plugin/3.2.8/"
    "maven-gpg-plugin-3.2.8.pom",
    "org/sonatype/plugins/nexus-staging-maven-plugin/1.7.0/"
    "nexus-staging-maven-plugin-1.7.0.jar",
    "org/sonatype/plugins/nexus-staging-maven-plugin/1.7.0/"
    "nexus-staging-maven-plugin-1.7.0.pom",
    "org/example/pinned-dependency/1.0/pinned-dependency-1.0.jar",
    "org/example/pinned-dependency/1.0/pinned-dependency-1.0.pom",
)))
lines = [
    "# Licensed to the Apache Software Foundation (ASF) under one or more",
    "# contributor license agreements. See the NOTICE file distributed with",
    "# this work for additional information regarding copyright ownership.",
    "# The ASF licenses this file to You under the Apache License, Version 2.0",
    '# (the "License"); you may not use this file except in compliance with',
    "# the License. You may obtain a copy of the License at",
    "#",
    "# https://www.apache.org/licenses/LICENSE-2.0",
    "#",
    "# Unless required by applicable law or agreed to in writing, software",
    '# distributed under the License is distributed on an "AS IS" BASIS,',
    "# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.",
    "# See the License for the specific language governing permissions and",
    "# limitations under the License.",
    "",
]
for relative in artifacts:
    path = source_root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    contents = ("fixture Maven artifact: {}\n".format(relative)).encode()
    path.write_bytes(contents)
    lines.append(
        "{} {} {}".format(
            hashlib.sha256(contents).hexdigest(),
            len(contents),
            relative,
        )
    )
(tools / "java-staging-maven-plugins.sha256").write_text(
    "\n".join(lines) + "\n",
    encoding="utf-8",
)
PY

  cat > "$FIXTURE_DIR/java/pom.xml" <<'EOF'
<project>
  <parent><version>23</version></parent>
  <groupId>org.apache.paimon</groupId>
  <artifactId>mosaic</artifactId>
  <version>0.3.0</version>
  <name>Mosaic</name>
  <inceptionYear>2026</inceptionYear>
</project>
EOF
  cat > "$FIXTURE_DIR/.gitignore" <<'EOF'
java/target/
java/src/main/resources/native/
*.class
*.log
EOF
  cat > "$FIXTURE_DIR/home/.m2/settings.xml" <<'EOF'
<settings xmlns="http://maven.apache.org/SETTINGS/1.0.0">
  <servers>
    <server>
      <id>apache.releases.https</id>
      <username>release-manager</username>
      <password>{encrypted-password}</password>
    </server>
  </servers>
</settings>
EOF

  cat > "$FIXTURE_DIR/fake-bin/git" <<'EOF'
#!/usr/bin/env bash
set -o errexit
set -o nounset
set -o pipefail

for argument in "$@"; do
  if [[ "$argument" == verify-tag ]]; then
    printf 'args=%s\n' "$*" >> "$FAKE_GIT_LOG"
    if [[ "${FAKE_VERIFY_TAG_STATUS:-0}" -ne 0 ]]; then
      echo "fake tag signature verification failure" >&2
      exit "$FAKE_VERIFY_TAG_STATUS"
    fi
    exit 0
  fi
done

exec "$REAL_GIT" "$@"
EOF

  cat > "$FIXTURE_DIR/fake-bin/gh" <<'EOF'
#!/usr/bin/env bash
set -o errexit
set -o nounset
set -o pipefail

printf 'args=%s\n' "$*" >> "$FAKE_GH_LOG"
printf 'host=%s\n' "${GH_HOST:-}" >> "$FAKE_GH_LOG"

if [[ "${1-}" != api ]]; then
  exit 2
fi

case "${2-}" in
  repos/*/git/ref/tags/*)
    count=0
    if [[ -s "$FAKE_GH_TAG_COUNT" ]]; then
      count=$(cat "$FAKE_GH_TAG_COUNT")
    fi
    count=$((count + 1))
    printf '%s\n' "$count" > "$FAKE_GH_TAG_COUNT"
    tag_object=$(
      printf '%s' \
        "${FAKE_REMOTE_TAG_OBJECT:-$(git rev-parse "v0.3.0-rc1^{tag}")}"
    )
    change_after=${FAKE_REMOTE_TAG_CHANGE_AFTER_COUNT:-1}
    if [[ "$count" -gt "$change_after" &&
          -n "${FAKE_SECOND_REMOTE_TAG_OBJECT:-}" ]]; then
      tag_object=$FAKE_SECOND_REMOTE_TAG_OBJECT
    fi
    printf '%s\n%s\n' \
      "${FAKE_REMOTE_TAG_TYPE:-tag}" \
      "$tag_object"
    ;;
  repos/*/actions/runs/42)
    repository=${2#repos/}
    repository=${repository%/actions/runs/42}
    count=0
    if [[ -s "$FAKE_GH_RUN_COUNT" ]]; then
      count=$(cat "$FAKE_GH_RUN_COUNT")
    fi
    count=$((count + 1))
    printf '%s\n' "$count" > "$FAKE_GH_RUN_COUNT"
    run_attempt=${FAKE_RUN_ATTEMPT:-1}
    change_after=${FAKE_RUN_CHANGE_AFTER_COUNT:-1}
    if [[ "$count" -gt "$change_after" &&
          -n "${FAKE_SECOND_RUN_ATTEMPT:-}" ]]; then
      run_attempt=$FAKE_SECOND_RUN_ATTEMPT
    fi
    cat <<JSON
{
  "id": ${FAKE_RUN_ID:-42},
  "status": "${FAKE_RUN_STATUS:-completed}",
  "conclusion": "${FAKE_RUN_CONCLUSION:-success}",
  "name": "${FAKE_WORKFLOW_NAME:-Release}",
  "event": "${FAKE_RUN_EVENT:-push}",
  "head_branch": "${FAKE_RUN_REF:-v0.3.0-rc1}",
  "head_sha": "${FAKE_RUN_SHA:-$(git rev-parse HEAD)}",
  "path": "${FAKE_WORKFLOW_PATH:-.github/workflows/release.yml}",
  "workflow_id": ${FAKE_WORKFLOW_ID:-7001},
  "run_attempt": ${run_attempt},
  "repository": {
    "full_name": "${FAKE_RUN_REPOSITORY:-$repository}"
  },
  "head_repository": {
    "full_name": "${FAKE_HEAD_REPOSITORY:-$repository}"
  }
}
JSON
    ;;
  repos/*/actions/workflows/release.yml)
    cat <<JSON
{
  "id": ${FAKE_CANONICAL_WORKFLOW_ID:-7001},
  "name": "${FAKE_CANONICAL_WORKFLOW_NAME:-Release}",
  "path": "${FAKE_CANONICAL_WORKFLOW_PATH:-.github/workflows/release.yml}",
  "state": "${FAKE_CANONICAL_WORKFLOW_STATE:-active}"
}
JSON
    ;;
  repos/*/actions/runs/42/artifacts\?*)
    artifact_count=1
    duplicate=
    if [[ "${FAKE_DUPLICATE_ARTIFACT:-false}" == true ]]; then
      artifact_count=2
      duplicate=',
        {
          "id": 9002,
          "name": "java-package",
          "expired": false,
          "digest": "'"${FAKE_ARTIFACT_DIGEST}"'",
          "workflow_run": {
            "id": 42,
            "head_sha": "'"${FAKE_ARTIFACT_SHA:-$(git rev-parse HEAD)}"'"
          }
        }'
    fi
    cat <<JSON
[
  {
    "total_count": $artifact_count,
    "artifacts": [
      {
        "id": ${FAKE_ARTIFACT_ID:-9001},
        "name": "${FAKE_ARTIFACT_NAME:-java-package}",
        "expired": ${FAKE_ARTIFACT_EXPIRED:-false},
        "digest": "${FAKE_ARTIFACT_DIGEST:-}",
        "size_in_bytes": ${FAKE_ARTIFACT_SIZE:-0},
        "workflow_run": {
          "id": ${FAKE_ARTIFACT_RUN_ID:-42},
          "head_sha": "${FAKE_ARTIFACT_SHA:-$(git rev-parse HEAD)}"
        }
      }
      $duplicate
    ]
  }
]
JSON
    ;;
  repos/*/actions/artifacts/*/zip)
    expected="repos/${FAKE_EXPECTED_REPOSITORY:-apache/paimon-mosaic}/actions/artifacts/${FAKE_ARTIFACT_ID:-9001}/zip"
    if [[ "$2" != "$expected" ]]; then
      echo "unexpected artifact download endpoint: $2" >&2
      exit 2
    fi
    cat "$FAKE_ARTIFACT_ZIP"
    ;;
  *)
    exit 2
    ;;
esac
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
  printf 'maven-gpg-key=%s\n' "${MAVEN_GPG_KEY:-}"
  printf 'maven-gpg-key-fingerprint=%s\n' "${MAVEN_GPG_KEY_FINGERPRINT:-}"
  printf 'maven-gpg-passphrase=%s\n' "${MAVEN_GPG_PASSPHRASE:-}"
} >> "$FAKE_MVN_LOG"

goal=
settings=
global_settings=
file=
sources=
javadoc=
pom_file=
url=
group_id=
artifact_id=
version=
packaging=
classifier=unset
staging_profile_id=
staging_repository_id=unset
maven_repo_local=

while [[ $# -gt 0 ]]; do
  case "$1" in
    -s)
      settings=$2
      shift 2
      ;;
    -gs)
      global_settings=$2
      shift 2
      ;;
    org.apache.maven.plugins:maven-gpg-plugin:3.2.8:sign-and-deploy-file)
      goal=sign
      shift
      ;;
    org.sonatype.plugins:nexus-staging-maven-plugin:1.7.0:deploy-staged-repository)
      goal=nexus
      shift
      ;;
    -Dfile=*) file=${1#*=}; shift ;;
    -Dsources=*) sources=${1#*=}; shift ;;
    -Djavadoc=*) javadoc=${1#*=}; shift ;;
    -DpomFile=*) pom_file=${1#*=}; shift ;;
    -Durl=*) url=${1#*=}; shift ;;
    -DgroupId=*) group_id=${1#*=}; shift ;;
    -DartifactId=*) artifact_id=${1#*=}; shift ;;
    -Dversion=*) version=${1#*=}; shift ;;
    -Dpackaging=*) packaging=${1#*=}; shift ;;
    -Dclassifier=*) classifier=${1#*=}; shift ;;
    -DstagingProfileId=*) staging_profile_id=${1#*=}; shift ;;
    -DstagingRepositoryId=*) staging_repository_id=${1#*=}; shift ;;
    -Dmaven.repo.local=*) maven_repo_local=${1#*=}; shift ;;
    *) shift ;;
  esac
done
printf 'resolved-goal=%s maven-repo-local=%s\n' \
  "$goal" "$maven_repo_local" >> "$FAKE_MVN_LOG"

if [[ -n "$settings" ]]; then
  case "$goal" in
    sign) cp "$settings" "$FAKE_SIGNING_SETTINGS_COPY" ;;
    nexus) cp "$settings" "$FAKE_NEXUS_SETTINGS_COPY" ;;
  esac
fi
if [[ -n "$global_settings" ]]; then
  cp "$global_settings" "$FAKE_EMPTY_GLOBAL_SETTINGS_COPY"
fi

case "$goal" in
  sign)
    if [[ "${FAKE_SIGN_MAVEN_EXIT_CODE:-0}" -ne 0 ]]; then
      exit "$FAKE_SIGN_MAVEN_EXIT_CODE"
    fi
    if [[ "$group_id" != org.apache.paimon ||
          "$artifact_id" != mosaic ||
          "$version" != 0.3.0 ||
          "$packaging" != jar ||
          "$classifier" != "" ||
          -z "$file" ||
          -z "$sources" ||
          -z "$javadoc" ||
          -z "$pom_file" ||
          "$url" != file:///* ]]; then
      echo "sign-and-deploy-file inputs are not pinned" >&2
      exit 2
    fi
    repository=${url#file://}
    version_dir="$repository/org/apache/paimon/mosaic/0.3.0"
    artifact_dir="$repository/org/apache/paimon/mosaic"
    mkdir -p "$version_dir"
    cp "$file" "$version_dir/mosaic-0.3.0.jar"
    cp "$sources" "$version_dir/mosaic-0.3.0-sources.jar"
    cp "$javadoc" "$version_dir/mosaic-0.3.0-javadoc.jar"
    cp "$pom_file" "$version_dir/mosaic-0.3.0.pom"
    for payload in \
      mosaic-0.3.0.jar \
      mosaic-0.3.0-sources.jar \
      mosaic-0.3.0-javadoc.jar \
      mosaic-0.3.0.pom
    do
      printf 'fake signature for %s\n' "$payload" > "$version_dir/$payload.asc"
      printf 'fake-md5\n' > "$version_dir/$payload.md5"
      printf 'fake-sha1\n' > "$version_dir/$payload.sha1"
    done
    printf '<metadata/>\n' > "$artifact_dir/maven-metadata.xml"
    printf 'fake-md5\n' > "$artifact_dir/maven-metadata.xml.md5"
    printf 'fake-sha1\n' > "$artifact_dir/maven-metadata.xml.sha1"

    case "${FAKE_REPO_PAYLOAD_MUTATION:-}" in
      "") ;;
      main) printf 'changed\n' >> "$version_dir/mosaic-0.3.0.jar" ;;
      sources) printf 'changed\n' >> "$version_dir/mosaic-0.3.0-sources.jar" ;;
      javadoc) printf 'changed\n' >> "$version_dir/mosaic-0.3.0-javadoc.jar" ;;
      pom) printf 'changed\n' >> "$version_dir/mosaic-0.3.0.pom" ;;
      *) exit 2 ;;
    esac
    case "${FAKE_MISSING_SIGNATURE:-}" in
      "") ;;
      main) rm -f -- "$version_dir/mosaic-0.3.0.jar.asc" ;;
      sources) rm -f -- "$version_dir/mosaic-0.3.0-sources.jar.asc" ;;
      javadoc) rm -f -- "$version_dir/mosaic-0.3.0-javadoc.jar.asc" ;;
      pom) rm -f -- "$version_dir/mosaic-0.3.0.pom.asc" ;;
      *) exit 2 ;;
    esac
    if [[ "${FAKE_EXTRA_REPO_PAYLOAD:-false}" == true ]]; then
      printf 'extra\n' > "$version_dir/extra.jar"
    fi
    if [[ "${FAKE_EXTRA_REPO_SYMLINK:-false}" == true ]]; then
      ln -s mosaic-0.3.0.jar "$version_dir/alias.jar"
    fi
    case "${FAKE_MUTATE_INPUT_AND_REPOSITORY:-}" in
      "") ;;
      main)
        "$PYTHON" - "$file" <<'PY'
import sys
from pathlib import Path


path = Path(sys.argv[1])
contents = bytearray(path.read_bytes())
contents[0] ^= 1
path.write_bytes(contents)
PY
        cp "$file" "$version_dir/mosaic-0.3.0.jar"
        ;;
      *) exit 2 ;;
    esac
    if [[ -n "${FAKE_MUTATE_PINNED_PLUGIN_AFTER_SIGN:-}" ]]; then
      "$PYTHON" - \
        "$settings" \
        "$FAKE_MUTATE_PINNED_PLUGIN_AFTER_SIGN" <<'PY'
import sys
import hashlib
import xml.etree.ElementTree as ET
from pathlib import Path
from urllib.parse import urlparse


settings = ET.parse(sys.argv[1]).getroot()
relative = sys.argv[2]
mirror_url = None
for element in settings.iter():
    if element.tag.rsplit("}", 1)[-1] == "url":
        value = (element.text or "").strip()
        if value.startswith("file://"):
            mirror_url = value
            break
if mirror_url is None:
    raise SystemExit("pinned plugin mirror was not found")
repository = Path(urlparse(mirror_url).path)
path = repository / relative
contents = bytearray(path.read_bytes())
contents[0] ^= 1
path.write_bytes(contents)
(Path(str(path) + ".sha1")).write_text(
    hashlib.sha1(contents).hexdigest() + "\n",
    encoding="utf-8",
)
PY
    fi
    if [[ "${FAKE_MUTATE_PROVENANCE_AFTER_SIGN:-false}" == true ]]; then
      printf 'changed\n' >> "$FAKE_PROVENANCE_PATH"
    fi
    ;;
  nexus)
    if [[ -n "${FAKE_NEXUS_CALLED_LOG:-}" ]]; then
      printf 'called\n' >> "$FAKE_NEXUS_CALLED_LOG"
    fi
    if [[ "$staging_profile_id" != "$FAKE_STAGING_PROFILE_ID" ||
          "$staging_repository_id" != "" ]]; then
      echo "Nexus staging ids are not pinned" >&2
      exit 2
    fi
    if [[ "${FAKE_NEXUS_MAVEN_EXIT_CODE:-0}" -ne 0 ]]; then
      exit "$FAKE_NEXUS_MAVEN_EXIT_CODE"
    fi
    ;;
  *)
    echo "Unexpected Maven invocation" >&2
    exit 2
    ;;
esac
EOF

  cat > "$FIXTURE_DIR/fake-bin/javap" <<'EOF'
#!/usr/bin/env bash
set -o errexit
set -o nounset

if [[ "${FAKE_JAVAP_STATUS:-0}" -ne 0 ]]; then
  echo "invalid class" >&2
  exit "$FAKE_JAVAP_STATUS"
fi
eval "class_name=\${$#}"
if [[ "$class_name" != org.apache.paimon.mosaic.MosaicReader ]]; then
  printf 'public class %s {\n}\n' "$class_name"
  exit 0
fi
if [[ "${FAKE_JAVAP_MODE:-}" == empty-reader ]]; then
  cat <<'OUT'
Compiled from "MosaicReader.java"
public class org.apache.paimon.mosaic.MosaicReader {
}
OUT
  exit 0
fi
cat <<'OUT'
Compiled from "MosaicReader.java"
public class org.apache.paimon.mosaic.MosaicReader implements java.lang.AutoCloseable {
  public static org.apache.paimon.mosaic.MosaicReader open(org.apache.paimon.mosaic.InputFile, long, org.apache.arrow.memory.BufferAllocator) throws java.io.IOException;
  public org.apache.arrow.vector.types.pojo.Schema getSchema();
  public int numRowGroups();
  public void project(java.lang.String[]);
  public org.apache.arrow.vector.VectorSchemaRoot readRowGroup(int, org.apache.arrow.memory.BufferAllocator) throws java.io.IOException;
  public int rowGroupNumRows(int);
  public java.util.Map<java.lang.String, org.apache.paimon.mosaic.ColumnStatistics> getRowGroupStatistics(int);
  public void close();
}
OUT
EOF

  cat > "$FIXTURE_DIR/fake-bin/gpg" <<'EOF'
#!/usr/bin/env bash
set -o errexit
set -o nounset
set -o pipefail

printf 'args=%s\n' "$*" >> "$FAKE_GPG_LOG"

for argument in "$@"; do
  if [[ "$argument" == --list-secret-keys ]]; then
    printf 'fpr:::::::::%s:\n' \
      "${FAKE_LOCAL_GPG_FINGERPRINT:-0123456789ABCDEF0123456789ABCDEF01234567}"
    exit 0
  fi
  if [[ "$argument" == --import ]]; then
    printf 'fpr:::::::::%s:\n' \
      "${FAKE_KEYS_FINGERPRINT:-0123456789ABCDEF0123456789ABCDEF01234567}"
    exit 0
  fi
  if [[ "$argument" == --verify ]]; then
    if [[ "${FAKE_GPG_VERIFY_STATUS:-0}" -ne 0 ]]; then
      echo "fake bad signature" >&2
      exit "$FAKE_GPG_VERIFY_STATUS"
    fi
    fingerprint=$(
      printf '%s' \
        "${FAKE_SIGNATURE_FINGERPRINT:-0123456789ABCDEF0123456789ABCDEF01234567}"
    )
    primary=$(
      printf '%s' \
        "${FAKE_SIGNATURE_PRIMARY_FINGERPRINT:-$fingerprint}"
    )
    printf '[GNUPG:] VALIDSIG %s 2026-08-30 0 0 0 1 10 00 %s\n' \
      "$fingerprint" \
      "$primary"
    exit 0
  fi
done

exit 2
EOF

  cat > "$FIXTURE_DIR/fake-bin/curl" <<'EOF'
#!/usr/bin/env bash
set -o errexit
set -o nounset
set -o pipefail

printf 'args=%s\n' "$*" >> "$FAKE_CURL_LOG"
output=
config=
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      output=$2
      shift 2
      ;;
    --config)
      config=$2
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
if [[ -n "$config" ]]; then
  "$PYTHON" - \
    "$config" \
    "$FAKE_PLUGIN_SOURCE_DIR" \
    "${FAKE_PLUGIN_DOWNLOAD_MUTATION:-}" <<'PY'
import shutil
import sys
from pathlib import Path
from urllib.parse import urlparse


config = Path(sys.argv[1])
source_root = Path(sys.argv[2])
mutation = sys.argv[3]
urls = []
outputs = []
for raw_line in config.read_text(encoding="utf-8").splitlines():
    line = raw_line.strip()
    if not line or line.startswith("#"):
        continue
    key, value = line.split("=", 1)
    value = value.strip().strip('"')
    if key.strip() == "url":
        urls.append(value)
    elif key.strip() == "output":
        outputs.append(value)
if len(urls) != len(outputs) or not urls:
    raise SystemExit("invalid fake curl config")
for url, output in zip(urls, outputs):
    marker = "/maven2/"
    parsed = urlparse(url)
    if parsed.scheme != "https" or marker not in parsed.path:
        raise SystemExit("unexpected plugin URL: {}".format(url))
    relative = parsed.path.split(marker, 1)[1]
    source = source_root / relative
    destination = Path(output)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    if relative == mutation:
        contents = bytearray(destination.read_bytes())
        contents[0] ^= 1
        destination.write_bytes(contents)
PY
  exit 0
fi
if [[ -z "$output" ]]; then
  exit 2
fi
printf 'fake KEYS\n' > "$output"
EOF

  chmod +x \
    "$FIXTURE_DIR/fake-bin/git" \
    "$FIXTURE_DIR/fake-bin/gh" \
    "$FIXTURE_DIR/fake-bin/mvn" \
    "$FIXTURE_DIR/fake-bin/javap" \
    "$FIXTURE_DIR/fake-bin/gpg" \
    "$FIXTURE_DIR/fake-bin/curl"

  git -C "$FIXTURE_DIR" init -q
  git -C "$FIXTURE_DIR" config user.name "Release Script Test"
  git -C "$FIXTURE_DIR" config user.email "release-script-test@example.invalid"
  git -C "$FIXTURE_DIR" add .
  git -C "$FIXTURE_DIR" commit -q -m fixture
  git -C "$FIXTURE_DIR" tag -a v0.3.0-rc1 -m v0.3.0-rc1

  export PYTHON
  create_candidate_artifacts
  FAKE_ARTIFACT_ZIP=$ARTIFACT_ZIP
  FAKE_CANDIDATE_DIR=$CANDIDATE_DIR
  FAKE_ARTIFACT_DIGEST="sha256:$(sha256sum "$ARTIFACT_ZIP" | awk '{print $1}')"
  FAKE_ARTIFACT_SIZE=$(wc -c < "$ARTIFACT_ZIP")
  export \
    FAKE_ARTIFACT_ZIP \
    FAKE_CANDIDATE_DIR \
    FAKE_ARTIFACT_DIGEST \
    FAKE_ARTIFACT_SIZE
  write_expected_provenance

  : > "$GIT_LOG"
  : > "$MAVEN_LOG"
  : > "$GPG_LOG"
  : > "$GH_LOG"
  : > "$GH_RUN_COUNT"
  : > "$GH_TAG_COUNT"
  : > "$CURL_LOG"
  : > "$NEXUS_CALLED_LOG"
}

run_script() {
  (
    cd "$FIXTURE_DIR"
    HOME="$FIXTURE_DIR/home" \
      PATH="$FIXTURE_DIR/fake-bin:$(dirname "$BASH"):$PATH" \
      REAL_GIT="$REAL_GIT" \
      FAKE_GIT_LOG="$GIT_LOG" \
      MVN="$FIXTURE_DIR/fake-bin/mvn" \
      GPG="$FIXTURE_DIR/fake-bin/gpg" \
      CURL="$FIXTURE_DIR/fake-bin/curl" \
      PYTHON="$PYTHON" \
      FAKE_MVN_LOG="$MAVEN_LOG" \
      FAKE_GPG_LOG="$GPG_LOG" \
      FAKE_GH_LOG="$GH_LOG" \
      FAKE_GH_RUN_COUNT="$GH_RUN_COUNT" \
      FAKE_GH_TAG_COUNT="$GH_TAG_COUNT" \
      FAKE_CURL_LOG="$CURL_LOG" \
      FAKE_SIGNING_SETTINGS_COPY="$SIGNING_SETTINGS_COPY" \
      FAKE_NEXUS_SETTINGS_COPY="$NEXUS_SETTINGS_COPY" \
      FAKE_EMPTY_GLOBAL_SETTINGS_COPY="$EMPTY_GLOBAL_SETTINGS_COPY" \
      FAKE_PLUGIN_SOURCE_DIR="$PLUGIN_SOURCE_DIR" \
      FAKE_NEXUS_CALLED_LOG="$NEXUS_CALLED_LOG" \
      FAKE_PROVENANCE_PATH="$PROVENANCE_PATH" \
      FAKE_STAGING_PROFILE_ID="$STAGING_PROFILE_ID" \
      TMPDIR="$TEST_ROOT" \
      "$BASH" ./tools/deploy_java_staging.sh \
        --release-version 0.3.0 \
        --rc 1 \
        --run-id 42 \
        --provenance-manifest "$PROVENANCE_PATH" \
        --staging-profile-id "$STAGING_PROFILE_ID" \
        "$@"
  )
}

run_script_without_manifest() {
  (
    cd "$FIXTURE_DIR"
    HOME="$FIXTURE_DIR/home" \
      PATH="$FIXTURE_DIR/fake-bin:$(dirname "$BASH"):$PATH" \
      REAL_GIT="$REAL_GIT" \
      FAKE_GIT_LOG="$GIT_LOG" \
      MVN="$FIXTURE_DIR/fake-bin/mvn" \
      GPG="$FIXTURE_DIR/fake-bin/gpg" \
      CURL="$FIXTURE_DIR/fake-bin/curl" \
      PYTHON="$PYTHON" \
      FAKE_MVN_LOG="$MAVEN_LOG" \
      FAKE_GPG_LOG="$GPG_LOG" \
      FAKE_GH_LOG="$GH_LOG" \
      FAKE_GH_RUN_COUNT="$GH_RUN_COUNT" \
      FAKE_GH_TAG_COUNT="$GH_TAG_COUNT" \
      FAKE_CURL_LOG="$CURL_LOG" \
      FAKE_SIGNING_SETTINGS_COPY="$SIGNING_SETTINGS_COPY" \
      FAKE_NEXUS_SETTINGS_COPY="$NEXUS_SETTINGS_COPY" \
      FAKE_EMPTY_GLOBAL_SETTINGS_COPY="$EMPTY_GLOBAL_SETTINGS_COPY" \
      FAKE_PLUGIN_SOURCE_DIR="$PLUGIN_SOURCE_DIR" \
      FAKE_NEXUS_CALLED_LOG="$NEXUS_CALLED_LOG" \
      FAKE_PROVENANCE_PATH="$PROVENANCE_PATH" \
      FAKE_STAGING_PROFILE_ID="$STAGING_PROFILE_ID" \
      TMPDIR="$TEST_ROOT" \
      "$BASH" ./tools/deploy_java_staging.sh \
        --release-version 0.3.0 \
        --rc 1 \
        --run-id 42 \
        --staging-profile-id "$STAGING_PROFILE_ID" \
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

test_provenance_manifest_option_is_required() {
  new_fixture
  if run_script_without_manifest --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "missing provenance manifest option was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "--provenance-manifest is required"
  assert_maven_not_invoked
}

test_dry_run_writes_frozen_provenance_without_overwrite() {
  new_fixture
  rm -f -- "$PROVENANCE_PATH"
  if ! run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    sed -n '1,240p' "$OUTPUT_LOG" >&2
    fail "valid dry-run failed"
  fi
  cmp "$EXPECTED_PROVENANCE" "$PROVENANCE_PATH"
  assert_contains "$OUTPUT_LOG" "Wrote frozen Java staging provenance"

  printf 'changed\n' >> "$PROVENANCE_PATH"
  : > "$MAVEN_LOG"
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "mismatched existing provenance manifest was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Provenance manifest does not match"
  assert_maven_not_invoked
}

test_real_deploy_requires_manifest_from_dry_run() {
  new_fixture
  rm -f -- "$PROVENANCE_PATH"
  if run_script \
    --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
    > "$OUTPUT_LOG" 2>&1; then
    fail "real deploy without dry-run provenance was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "requires a provenance manifest from a successful dry-run"
  assert_maven_not_invoked
}

test_workflow_run_must_be_successful_release_tag_push() {
  new_fixture
  if FAKE_WORKFLOW_NAME=Other \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "run from another workflow was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "is not named Release"
  assert_maven_not_invoked

  : > "$GH_RUN_COUNT"
  if FAKE_RUN_EVENT=workflow_dispatch \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "manually dispatched run was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "is not a tag-push run"
  assert_maven_not_invoked

  : > "$GH_RUN_COUNT"
  if FAKE_RUN_CONCLUSION=failure \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "failed workflow run was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "is not a successful completed run"
  assert_maven_not_invoked
}

test_workflow_path_id_and_attempt_are_frozen() {
  new_fixture
  if FAKE_WORKFLOW_PATH=.github/workflows/other.yml \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "run from another workflow path was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "uses workflow path"
  assert_maven_not_invoked

  : > "$GH_RUN_COUNT"
  rm -f -- "$PROVENANCE_PATH"
  if FAKE_WORKFLOW_ID=7002 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "changed workflow id was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "does not use the active canonical Release workflow id and path"
  assert_maven_not_invoked

  : > "$GH_RUN_COUNT"
  cp "$EXPECTED_PROVENANCE" "$PROVENANCE_PATH"
  if FAKE_RUN_ATTEMPT=2 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "changed workflow attempt was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Provenance manifest does not match"
  assert_maven_not_invoked
}

test_failed_job_rerun_can_reuse_earlier_java_candidate() {
  new_fixture
  FAKE_RUN_ATTEMPT=2
  write_expected_provenance
  if ! run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    sed -n '1,240p' "$OUTPUT_LOG" >&2
    fail "final run attempt rejected Java candidate from an earlier attempt"
  fi
  assert_contains "$OUTPUT_LOG" "dry run finished successfully"
  assert_maven_not_invoked

  FAKE_CANDIDATE_RUN_ATTEMPT=2 new_fixture
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "candidate claiming a future workflow attempt was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Java candidate provenance run_attempt 2 is newer than current workflow run attempt 1"
  assert_maven_not_invoked
}

test_rerun_during_candidate_validation_stops_before_maven() {
  new_fixture
  if FAKE_SECOND_RUN_ATTEMPT=2 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "workflow rerun during validation was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "GitHub Actions provenance changed while validating java-package"
  assert_maven_not_invoked
}

test_retag_during_candidate_validation_stops_before_maven() {
  new_fixture
  if FAKE_SECOND_REMOTE_TAG_OBJECT=0000000000000000000000000000000000000000 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "release tag replacement during validation was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "current remote tag object does not match the verified local tag"
  assert_maven_not_invoked
}

test_workflow_run_tag_and_sha_must_match_checkout() {
  new_fixture
  if FAKE_RUN_REF=v0.3.0-rc2 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "workflow run from another tag was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "does not match v0.3.0-rc1"
  assert_maven_not_invoked

  : > "$GH_RUN_COUNT"
  if FAKE_RUN_SHA=0000000000000000000000000000000000000000 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "workflow run from another commit was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "does not match v0.3.0-rc1"
  assert_maven_not_invoked
}

test_release_tag_must_be_annotated() {
  new_fixture
  git -C "$FIXTURE_DIR" tag -d v0.3.0-rc1 >/dev/null
  git -C "$FIXTURE_DIR" tag v0.3.0-rc1

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "lightweight release tag was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "must be an annotated tag"
  assert_not_contains "$GH_LOG" "actions/runs"
  assert_maven_not_invoked
}

test_release_tag_signature_must_verify() {
  new_fixture
  tag_object=$(git -C "$FIXTURE_DIR" rev-parse "v0.3.0-rc1^{tag}")

  if FAKE_VERIFY_TAG_STATUS=7 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "release tag with an invalid signature was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "signature verification failed"
  assert_contains "$GIT_LOG" "verify-tag $tag_object"
  assert_not_contains "$GH_LOG" "actions/runs"
  assert_maven_not_invoked
}

test_signed_tag_object_name_must_match_release_tag() {
  new_fixture
  git -C "$FIXTURE_DIR" tag -a v0.3.0-rc2 -m v0.3.0-rc2
  mismatched_object=$(
    git -C "$FIXTURE_DIR" rev-parse "v0.3.0-rc2^{tag}"
  )
  git -C "$FIXTURE_DIR" update-ref refs/tags/v0.3.0-rc1 "$mismatched_object"

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "signed tag object with a different embedded name was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "signed tag object is for v0.3.0-rc2, not v0.3.0-rc1"
  assert_not_contains "$GH_LOG" "actions/runs"
  assert_maven_not_invoked
}

test_current_remote_tag_object_must_match_local_tag() {
  new_fixture

  if FAKE_REMOTE_TAG_OBJECT=0000000000000000000000000000000000000000 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "stale local tag object was accepted after the remote tag moved"
  fi
  assert_contains "$OUTPUT_LOG" "current remote tag object does not match"
  assert_not_contains "$GH_LOG" "actions/runs"
  assert_maven_not_invoked
}

test_downloads_java_candidate_by_immutable_artifact_id() {
  new_fixture
  run_script --dry-run > "$OUTPUT_LOG" 2>&1

  assert_contains "$GH_LOG" \
    "args=api repos/apache/paimon-mosaic/actions/artifacts/9001/zip"
  assert_not_contains "$GH_LOG" "run download"
  if [[ $(grep -c '/actions/artifacts/9001/zip' "$GH_LOG") -ne 1 ]]; then
    fail "java-package should be downloaded exactly once by immutable id"
  fi
}

test_dry_run_can_validate_a_fork_without_enabling_real_deploy() {
  new_fixture
  rm -f -- "$PROVENANCE_PATH"
  sed -i \
    's#^repository=apache/paimon-mosaic$#repository=example/fork#' \
    "$CANDIDATE_DIR/java-staging-provenance.txt"
  repack_candidate_and_refresh_provenance
  rm -f -- "$PROVENANCE_PATH"
  FAKE_EXPECTED_REPOSITORY=example/fork \
    run_script --repo example/fork --dry-run > "$OUTPUT_LOG" 2>&1

  assert_contains "$PROVENANCE_PATH" "repository=example/fork"
  assert_contains "$GH_LOG" \
    "args=api repos/example/fork/actions/artifacts/9001/zip"
}

test_github_host_is_pinned() {
  new_fixture
  GH_HOST=enterprise.example.invalid \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1

  if grep -Fq 'host=enterprise.example.invalid' "$GH_LOG"; then
    fail "ambient GH_HOST redirected release provenance"
  fi
  assert_contains "$GH_LOG" "host=github.com"
}

test_java_package_metadata_must_be_unique_and_complete() {
  new_fixture
  if FAKE_DUPLICATE_ARTIFACT=true \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "duplicate java-package artifacts were accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Expected exactly one unexpired java-package artifact"
  assert_maven_not_invoked

  : > "$GH_RUN_COUNT"
  if FAKE_ARTIFACT_EXPIRED=true \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "expired java-package artifact was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Expected exactly one unexpired java-package artifact"
  assert_maven_not_invoked

  : > "$GH_RUN_COUNT"
  if FAKE_ARTIFACT_DIGEST= \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "java-package without digest was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "does not have a valid SHA-256 digest"
  assert_maven_not_invoked

  new_fixture
  if FAKE_ARTIFACT_SIZE=536870913 \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "oversized java-package artifact was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "artifact size must be between 1 and 536870912 bytes"
  assert_not_contains "$GH_LOG" "/actions/artifacts/9001/zip"
  assert_maven_not_invoked
}

test_downloaded_java_package_digest_must_match_metadata() {
  new_fixture
  printf 'changed zip bytes\n' >> "$ARTIFACT_ZIP"
  FAKE_ARTIFACT_SIZE=$(wc -c < "$ARTIFACT_ZIP")
  export FAKE_ARTIFACT_SIZE
  sed -i \
    "s/^artifact_size=.*/artifact_size=$FAKE_ARTIFACT_SIZE/" \
    "$EXPECTED_PROVENANCE" \
    "$PROVENANCE_PATH"

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "downloaded java-package with wrong digest was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "artifact digest mismatch"
  assert_maven_not_invoked
}

test_java_package_zip_rejects_unsafe_paths() {
  new_fixture
  "$PYTHON" - "$ARTIFACT_ZIP" <<'PY'
import sys
import zipfile


with zipfile.ZipFile(sys.argv[1], "a") as archive:
    archive.writestr("../escape", b"x")
PY
  FAKE_ARTIFACT_DIGEST="sha256:$(sha256sum "$ARTIFACT_ZIP" | awk '{print $1}')"
  FAKE_ARTIFACT_SIZE=$(wc -c < "$ARTIFACT_ZIP")
  export FAKE_ARTIFACT_DIGEST FAKE_ARTIFACT_SIZE
  write_expected_provenance

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "java-package with unsafe path was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Unsafe java-package artifact path"
  assert_maven_not_invoked
}

test_java_package_requires_exact_four_candidate_files() {
  new_fixture
  printf 'extra\n' > "$CANDIDATE_DIR/extra.jar"
  repack_candidate_and_refresh_provenance
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "java-package with an extra file was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Downloaded java-package artifact must contain exactly"
  assert_maven_not_invoked

  new_fixture
  rm -f -- "$CANDIDATE_DIR/mosaic-0.3.0-javadoc.jar"
  repack_candidate_and_refresh_provenance
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "java-package missing javadoc JAR was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Downloaded java-package artifact must contain exactly"
  assert_maven_not_invoked
}

test_validator_rejects_one_byte_classifier_jars() {
  for classifier in sources javadoc; do
    new_fixture
    printf 'x' > "$CANDIDATE_DIR/mosaic-0.3.0-$classifier.jar"
    repack_candidate_and_refresh_provenance
    if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
      fail "one-byte $classifier JAR was accepted"
    fi
    assert_contains "$OUTPUT_LOG" "Invalid JAR"
    assert_maven_not_invoked
  done
}

test_validator_rejects_invalid_java_class_and_maven_metadata() {
  new_fixture
  rewrite_candidate_jar_entry \
    mosaic-0.3.0.jar \
    org/apache/paimon/mosaic/MosaicReader.class \
    replace \
    x
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "one-byte Java class was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Packaged Java class is invalid"
  assert_maven_not_invoked

  new_fixture
  rewrite_candidate_jar_entry \
    mosaic-0.3.0.jar \
    org/apache/paimon/mosaic/ColumnStatistics.class \
    remove
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "main JAR missing a required class was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Packaged Java class set is invalid"
  assert_maven_not_invoked

  new_fixture
  if FAKE_JAVAP_MODE=empty-reader \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "empty MosaicReader public API was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Packaged MosaicReader public API is incomplete"
  assert_maven_not_invoked

  new_fixture
  rewrite_candidate_jar_entry \
    mosaic-0.3.0.jar \
    META-INF/maven/org.apache.paimon/mosaic/pom.xml \
    replace \
    x
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "wrong embedded POM was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Packaged Maven pom.xml does not match the signed source tree"
  assert_maven_not_invoked

  new_fixture
  rewrite_candidate_jar_entry \
    mosaic-0.3.0.jar \
    META-INF/maven/org.apache.paimon/mosaic/pom.properties \
    replace \
    x
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "wrong pom.properties was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Invalid Maven pom.properties line"
  assert_maven_not_invoked
}

test_validator_rejects_invalid_legal_and_native_contents() {
  new_fixture
  rewrite_candidate_jar_entry \
    mosaic-0.3.0.jar \
    META-INF/LICENSE \
    replace \
    x
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "one-byte LICENSE was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "differs across main, sources, and javadoc"
  assert_maven_not_invoked

  for native in \
    native/linux/x86_64/libpaimon_mosaic_jni.so \
    native/linux/aarch64/libpaimon_mosaic_jni.so \
    native/macos/aarch64/libpaimon_mosaic_jni.dylib \
    native/windows/x86_64/paimon_mosaic_jni.dll
  do
    new_fixture
    rewrite_candidate_jar_entry \
      mosaic-0.3.0.jar \
      "$native" \
      replace \
      x
    if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
      fail "one-byte native entry was accepted: $native"
    fi
    assert_contains "$OUTPUT_LOG" "Packaged native entry is not"
    assert_maven_not_invoked
  done

  for native in \
    native/linux/x86_64/libpaimon_mosaic_jni.so \
    native/linux/aarch64/libpaimon_mosaic_jni.so \
    native/macos/aarch64/libpaimon_mosaic_jni.dylib \
    native/windows/x86_64/paimon_mosaic_jni.dll
  do
    case "$native" in
      *.so) header_size=64 ;;
      *.dylib) header_size=32 ;;
      *.dll) header_size=128 ;;
      *) fail "unknown native fixture: $native" ;;
    esac
    new_fixture
    rewrite_candidate_jar_entry \
      mosaic-0.3.0.jar \
      "$native" \
      truncate \
      "$header_size"
    if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
      fail "header-only native entry was accepted: $native"
    fi
    assert_contains "$OUTPUT_LOG" "Packaged native entry is not"
    assert_maven_not_invoked
  done

  new_fixture
  fake_license=$(
    printf '\nApache License\nVersion 2.0, January 2004\n'
    printf 'fake legal text\n%.0s' 1 2 3 4 5 6 7 8 9 10
  )
  for jar in \
    mosaic-0.3.0.jar \
    mosaic-0.3.0-sources.jar \
    mosaic-0.3.0-javadoc.jar
  do
    rewrite_candidate_jar_entry \
      "$jar" \
      META-INF/LICENSE \
      replace \
      "$fake_license"
  done
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "colluding fake LICENSE files were accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Packaged META-INF/LICENSE does not match the signed source tree"
  assert_maven_not_invoked
}

test_validator_rejects_changed_source_and_javadoc_contents() {
  new_fixture
  rewrite_candidate_jar_entry \
    mosaic-0.3.0-sources.jar \
    org/apache/paimon/mosaic/MosaicReader.java \
    replace \
    x
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "changed Java source was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Packaged Java source differs from signed source"
  assert_maven_not_invoked

  new_fixture
  rewrite_candidate_jar_entry \
    mosaic-0.3.0-javadoc.jar \
    org/apache/paimon/mosaic/MosaicReader.html \
    replace \
    x
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "changed MosaicReader javadoc was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Packaged javadoc page is invalid"
  assert_maven_not_invoked

  new_fixture
  rewrite_candidate_jar_entry \
    mosaic-0.3.0-javadoc.jar \
    org/apache/paimon/mosaic/ColumnStatistics.html \
    remove
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "incomplete javadoc JAR was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Packaged javadoc is missing required entries"
  assert_maven_not_invoked
}

test_validator_rejects_windows_drive_relative_jar_entries() {
  new_fixture
  add_candidate_jar_entry \
    mosaic-0.3.0.jar \
    'C:../escape.class' \
    x
  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "Windows drive-relative JAR entry was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Unsafe JAR entry path"
  assert_maven_not_invoked
}

test_same_commit_retag_candidate_provenance_mismatch() {
  new_fixture
  old_tag_object=$(git -C "$FIXTURE_DIR" rev-parse "v0.3.0-rc1^{tag}")
  git -C "$FIXTURE_DIR" tag -fa v0.3.0-rc1 -m same-commit-retag >/dev/null
  new_tag_object=$(git -C "$FIXTURE_DIR" rev-parse "v0.3.0-rc1^{tag}")
  if [[ "$old_tag_object" == "$new_tag_object" ]]; then
    fail "same-commit retag did not create a new tag object"
  fi
  rm -f -- "$PROVENANCE_PATH"

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "candidate provenance from the old same-commit tag was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Java candidate provenance tag_object mismatch"
  assert_maven_not_invoked
}

test_staging_profile_id_is_required_and_frozen() {
  new_fixture
  if run_script --staging-profile-id "" --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "empty staging profile id was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "--staging-profile-id requires a value"
  assert_maven_not_invoked

  new_fixture
  if STAGING_PROFILE_ID=other-profile \
    run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "changed staging profile id was accepted with frozen provenance"
  fi
  assert_contains "$OUTPUT_LOG" "Provenance manifest does not match"
  assert_maven_not_invoked
}

test_dry_run_invokes_no_maven_or_gpg() {
  new_fixture
  run_script --dry-run > "$OUTPUT_LOG" 2>&1

  assert_maven_not_invoked
  if [[ -s "$GPG_LOG" ]]; then
    sed -n '1,240p' "$GPG_LOG" >&2
    fail "dry-run must not invoke GPG directly"
  fi
  assert_not_contains "$NEXUS_CALLED_LOG" "called"
  assert_contains "$OUTPUT_LOG" "without Maven, GPG, or Nexus"
}

test_real_deploy_requires_full_and_explicit_signing_fingerprint() {
  new_fixture
  if run_script --gpg-keyname ABCDEF > "$OUTPUT_LOG" 2>&1; then
    fail "real deployment accepted a short signing key id"
  fi
  assert_contains "$OUTPUT_LOG" "full 40- or 64-hex OpenPGP fingerprint"
  assert_maven_not_invoked

  if run_script > "$OUTPUT_LOG" 2>&1; then
    fail "real deployment accepted an ambient default signing key"
  fi
  assert_contains "$OUTPUT_LOG" "--gpg-keyname is required"
  assert_maven_not_invoked
}

test_real_deploy_requires_local_and_asf_signing_key() {
  new_fixture
  if FAKE_LOCAL_GPG_FINGERPRINT=89ABCDEF0123456789ABCDEF0123456789ABCDEF \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      > "$OUTPUT_LOG" 2>&1; then
    fail "real deployment accepted a missing local secret key"
  fi
  assert_contains "$OUTPUT_LOG" "Local GPG secret keys do not contain"
  assert_maven_not_invoked

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_KEYS_FINGERPRINT=89ABCDEF0123456789ABCDEF0123456789ABCDEF \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "real deployment accepted a key absent from ASF KEYS"
  fi
  assert_contains "$OUTPUT_LOG" "is not present in the ASF Paimon KEYS file"
  assert_maven_not_invoked
}

test_real_deploy_downloads_pinned_asf_keys_by_default() {
  new_fixture
  if ! run_script \
    --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
    > "$OUTPUT_LOG" 2>&1; then
    sed -n '1,240p' "$OUTPUT_LOG" >&2
    fail "real deployment with downloaded ASF KEYS failed"
  fi

  assert_contains "$CURL_LOG" \
    "args=--proto =https --tlsv1.2 --location --fail --silent --show-error --retry 3 --retry-connrefused --connect-timeout 10 --max-time 300"
  assert_contains "$CURL_LOG" "https://downloads.apache.org/paimon/KEYS"
}

test_settings_and_environment_cannot_control_plugin_resolution_or_signing_inputs() {
  local nexus_runtime_repository
  local signing_runtime_repository

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  settings=$(mktemp "$TEST_ROOT/settings.XXXXXX.xml")
  printf 'fake KEYS\n' > "$keys"
  cat > "$settings" <<'EOF'
<settings xmlns="http://maven.apache.org/SETTINGS/1.0.0">
  <localRepository>/tmp/release-local-repository</localRepository>
  <pluginGroups><pluginGroup>org.example.plugins</pluginGroup></pluginGroups>
  <servers>
    <server>
      <id>apache.releases.https</id>
      <username>release-manager</username>
      <password>{encrypted-password}</password>
    </server>
    <server><id>other-server</id><username>other</username></server>
  </servers>
  <mirrors>
    <mirror><id>corp</id><url>https://mirror.invalid/</url><mirrorOf>central</mirrorOf></mirror>
  </mirrors>
  <proxies>
    <proxy><id>corp-proxy</id><active>true</active><protocol>https</protocol><host>proxy.invalid</host><port>443</port></proxy>
  </proxies>
  <profiles>
    <profile>
      <id>hostile-release-overrides</id>
      <properties>
        <groupId>evil.group</groupId>
        <artifactId>evil-artifact</artifactId>
        <version>9.9.9</version>
        <packaging>pom</packaging>
        <file>/tmp/evil.jar</file>
        <sources>/tmp/evil-sources.jar</sources>
        <javadoc>/tmp/evil-javadoc.jar</javadoc>
        <pomFile>/tmp/evil.pom</pomFile>
        <files>/tmp/extra.jar</files>
        <classifiers>evil</classifiers>
        <url>file:///tmp/evil-repo</url>
        <gpg.signer>bc</gpg.signer>
        <gpg.executable>/tmp/ambient-gpg</gpg.executable>
      </properties>
    </profile>
  </profiles>
  <activeProfiles><activeProfile>hostile-release-overrides</activeProfile></activeProfiles>
</settings>
EOF

  if ! MAVEN_GPG_KEY='ambient-bc-secret-key' \
    MAVEN_GPG_KEY_FINGERPRINT=89ABCDEF0123456789ABCDEF0123456789ABCDEF \
    MAVEN_GPG_PASSPHRASE='ambient-passphrase' \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      --maven-settings "$settings" \
      > "$OUTPUT_LOG" 2>&1; then
    sed -n '1,240p' "$OUTPUT_LOG" >&2
    fail "sanitized settings deployment failed"
  fi

  assert_contains "$SIGNING_SETTINGS_COPY" "paimon-mosaic-pinned-plugins"
  assert_contains "$SIGNING_SETTINGS_COPY" "file://$TEST_ROOT/"
  assert_contains "$SIGNING_SETTINGS_COPY" \
    "<mirrorOf>*,!local-staging</mirrorOf>"
  assert_not_contains "$SIGNING_SETTINGS_COPY" "apache.releases.https"
  assert_not_contains "$SIGNING_SETTINGS_COPY" "release-manager"
  assert_not_contains "$SIGNING_SETTINGS_COPY" "release-local-repository"
  assert_not_contains "$SIGNING_SETTINGS_COPY" "org.example.plugins"
  assert_not_contains "$SIGNING_SETTINGS_COPY" "mirror.invalid"
  assert_not_contains "$SIGNING_SETTINGS_COPY" "proxy.invalid"

  assert_contains "$NEXUS_SETTINGS_COPY" "paimon-mosaic-pinned-plugins"
  assert_contains "$NEXUS_SETTINGS_COPY" "file://$TEST_ROOT/"
  assert_contains "$NEXUS_SETTINGS_COPY" "<mirrorOf>*</mirrorOf>"
  assert_contains "$NEXUS_SETTINGS_COPY" "apache.releases.https"
  assert_contains "$NEXUS_SETTINGS_COPY" "release-manager"
  assert_contains "$NEXUS_SETTINGS_COPY" "proxy.invalid"
  assert_not_contains "$NEXUS_SETTINGS_COPY" "other-server"
  assert_not_contains "$NEXUS_SETTINGS_COPY" "release-local-repository"
  assert_not_contains "$NEXUS_SETTINGS_COPY" "org.example.plugins"
  assert_not_contains "$NEXUS_SETTINGS_COPY" "mirror.invalid"
  assert_not_contains "$NEXUS_SETTINGS_COPY" "<profiles"
  assert_not_contains "$NEXUS_SETTINGS_COPY" "<activeProfiles"
  assert_not_contains "$NEXUS_SETTINGS_COPY" "evil.group"
  assert_not_contains "$EMPTY_GLOBAL_SETTINGS_COPY" "<server"
  assert_not_contains "$EMPTY_GLOBAL_SETTINGS_COPY" "<profile"

  assert_contains "$MAVEN_LOG" "-DgroupId=org.apache.paimon"
  assert_contains "$MAVEN_LOG" "-DartifactId=mosaic"
  assert_contains "$MAVEN_LOG" "-Dversion=0.3.0"
  assert_contains "$MAVEN_LOG" "-Dpackaging=jar"
  assert_contains "$MAVEN_LOG" "-Dclassifier="
  assert_contains "$MAVEN_LOG" "-Dgpg.signer=gpg"
  assert_contains "$MAVEN_LOG" \
    "-Dgpg.executable=$FIXTURE_DIR/fake-bin/gpg"
  assert_not_contains "$MAVEN_LOG" "evil.group"
  assert_not_contains "$MAVEN_LOG" "/tmp/evil"
  assert_contains "$MAVEN_LOG" "maven-gpg-key="
  assert_not_contains "$MAVEN_LOG" "maven-gpg-key=ambient-bc-secret-key"
  assert_contains "$MAVEN_LOG" "maven-gpg-key-fingerprint="
  assert_not_contains "$MAVEN_LOG" \
    "maven-gpg-key-fingerprint=89ABCDEF0123456789ABCDEF0123456789ABCDEF"
  assert_contains "$MAVEN_LOG" "maven-gpg-passphrase="
  assert_not_contains "$MAVEN_LOG" "maven-gpg-passphrase=ambient-passphrase"

  signing_runtime_repository=$(
    sed -n \
      's/^resolved-goal=sign maven-repo-local=//p' \
      "$MAVEN_LOG"
  )
  nexus_runtime_repository=$(
    sed -n \
      's/^resolved-goal=nexus maven-repo-local=//p' \
      "$MAVEN_LOG"
  )
  case "$signing_runtime_repository" in
    "$TEST_ROOT"/paimon-mosaic-java-staging.*/maven-signing-runtime) ;;
    *)
      fail "signing Maven did not use its isolated runtime repository: \
$signing_runtime_repository"
      ;;
  esac
  case "$nexus_runtime_repository" in
    "$TEST_ROOT"/paimon-mosaic-java-staging.*/maven-nexus-runtime) ;;
    *)
      fail "Nexus Maven did not use its isolated runtime repository: \
$nexus_runtime_repository"
      ;;
  esac
  if [[ "$signing_runtime_repository" == "$nexus_runtime_repository" ]]; then
    fail "signing and Nexus must use distinct isolated Maven repositories"
  fi
}

test_pinned_plugin_download_digest_mismatch_stops_before_maven() {
  local mutation

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  mutation=org/example/pinned-dependency/1.0/pinned-dependency-1.0.jar
  if FAKE_PLUGIN_DOWNLOAD_MUTATION="$mutation" \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "tampered pinned Maven dependency was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Pinned Maven plugin artifact digest mismatch"
  assert_maven_not_invoked
  assert_not_contains "$NEXUS_CALLED_LOG" "called"
}

test_real_deploy_uses_only_two_direct_plugin_goals() {
  local nexus_args_log

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  run_script \
    --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
    --keys-file "$keys" \
    > "$OUTPUT_LOG" 2>&1

  if [[ $(grep -c '^invocation$' "$MAVEN_LOG") -ne 2 ]]; then
    fail "real deploy must invoke Maven exactly twice"
  fi
  assert_contains "$MAVEN_LOG" \
    "org.apache.maven.plugins:maven-gpg-plugin:3.2.8:sign-and-deploy-file"
  assert_contains "$MAVEN_LOG" \
    "org.sonatype.plugins:nexus-staging-maven-plugin:1.7.0:deploy-staged-repository"
  nexus_args_log=$(mktemp "$TEST_ROOT/nexus-args.XXXXXX")
  grep -F \
    "org.sonatype.plugins:nexus-staging-maven-plugin:1.7.0:deploy-staged-repository" \
    "$MAVEN_LOG" > "$nexus_args_log"
  if [[ $(wc -l < "$nexus_args_log") -ne 1 ]]; then
    fail "real deploy must invoke the Nexus plugin exactly once"
  fi
  assert_contains "$nexus_args_log" \
    "-DstagingProfileId=$STAGING_PROFILE_ID"
  assert_contains "$nexus_args_log" "-DstagingRepositoryId="
  assert_contains "$nexus_args_log" "-DserverId=apache.releases.https"
  assert_contains "$nexus_args_log" "-DautoReleaseAfterClose=false"
  assert_contains "$nexus_args_log" \
    "-DkeepStagingRepositoryOnFailure=false"
  assert_contains "$nexus_args_log" \
    "-DkeepStagingRepositoryOnCloseRuleFailure=false"
  assert_contains "$nexus_args_log" "-DskipStaging=false"
  assert_contains "$nexus_args_log" "-Dmaven.wagon.http.ssl.insecure=false"
  if sed -n 's/^args=//p' "$MAVEN_LOG" |
    tr ' ' '\n' |
    grep -Eq '^(clean|package|verify|install|deploy)$'; then
    fail "real deploy invoked a Maven lifecycle phase"
  fi
  assert_contains "$NEXUS_CALLED_LOG" "called"
}

test_file_repository_payload_and_allowlist_gate_nexus() {
  for mutation in main sources javadoc pom; do
    new_fixture
    keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
    printf 'fake KEYS\n' > "$keys"
    if FAKE_REPO_PAYLOAD_MUTATION=$mutation \
      run_script \
        --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
        --keys-file "$keys" \
        > "$OUTPUT_LOG" 2>&1; then
      fail "changed file-repository $mutation payload was accepted"
    fi
    assert_contains "$OUTPUT_LOG" "payload differs from frozen input"
    assert_not_contains "$NEXUS_CALLED_LOG" "called"
  done

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_EXTRA_REPO_PAYLOAD=true \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "extra file-repository payload was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "unexpected version file"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_EXTRA_REPO_SYMLINK=true \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "file-repository symlink was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "contains a symlink"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"
}

test_signing_cannot_mutate_frozen_input_and_repository_together() {
  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_MUTATE_INPUT_AND_REPOSITORY=main \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "matching input and repository mutation after signing was accepted"
  fi
  assert_contains "$OUTPUT_LOG" \
    "Frozen Java staging input changed after signing"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"
}

test_signing_cannot_mutate_pinned_plugin_closure_before_upload() {
  local mutation

  for mutation in \
    org/sonatype/plugins/nexus-staging-maven-plugin/1.7.0/nexus-staging-maven-plugin-1.7.0.jar \
    org/example/pinned-dependency/1.0/pinned-dependency-1.0.jar; do
    new_fixture
    keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
    printf 'fake KEYS\n' > "$keys"
    if FAKE_MUTATE_PINNED_PLUGIN_AFTER_SIGN="$mutation" \
      run_script \
        --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
        --keys-file "$keys" \
        > "$OUTPUT_LOG" 2>&1; then
      fail "changed pinned Maven plugin closure was accepted after signing: \
$mutation"
    fi
    assert_contains "$OUTPUT_LOG" \
      "Pinned Maven plugin artifact digest mismatch"
    assert_not_contains "$NEXUS_CALLED_LOG" "called"
  done
}

test_missing_bad_or_wrong_key_signature_blocks_nexus() {
  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_MISSING_SIGNATURE=main \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "missing signature was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "payload/signature set is incomplete"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_GPG_VERIFY_STATUS=7 \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "bad signature was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Detached signature verification failed"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_SIGNATURE_FINGERPRINT=89ABCDEF0123456789ABCDEF0123456789ABCDEF \
    FAKE_SIGNATURE_PRIMARY_FINGERPRINT=89ABCDEF0123456789ABCDEF0123456789ABCDEF \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "signature from the wrong fingerprint was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "was not made by expected fingerprint"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"
}

test_final_provenance_boundary_runs_after_signatures() {
  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_SECOND_RUN_ATTEMPT=2 \
    FAKE_RUN_CHANGE_AFTER_COUNT=2 \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "workflow rerun during local signing was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "provenance changed after it was frozen"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_SECOND_REMOTE_TAG_OBJECT=0000000000000000000000000000000000000000 \
    FAKE_REMOTE_TAG_CHANGE_AFTER_COUNT=2 \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "retag during local signing was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "current remote tag object does not match"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"

  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  if FAKE_MUTATE_PROVENANCE_AFTER_SIGN=true \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1; then
    fail "changed frozen manifest during local signing was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Provenance manifest changed"
  assert_not_contains "$NEXUS_CALLED_LOG" "called"
}

test_maven_and_jvm_environment_is_scrubbed() {
  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  MAVEN_ARGS='-f /tmp/other-pom.xml' \
    MAVEN_OPTS='-DstagingRepositoryId=unexpected' \
    MAVEN_CONFIG='-f=/tmp/other-pom.xml' \
    MAVEN_BASEDIR='/tmp/other-base' \
    MAVEN_PROJECTBASEDIR='/tmp/other-project' \
    JAVA_TOOL_OPTIONS='-DskipNexusStagingDeployMojo=true' \
    JDK_JAVA_OPTIONS='-DskipStaging=true' \
    _JAVA_OPTIONS='-Dmaven.wagon.http.ssl.insecure=true' \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1

  assert_contains "$MAVEN_LOG" "maven-skip-rc=1"
  assert_contains "$MAVEN_LOG" "maven-args="
  assert_contains "$MAVEN_LOG" "maven-opts="
  assert_contains "$MAVEN_LOG" "maven-config="
  assert_contains "$MAVEN_LOG" "maven-project-basedir="
  assert_contains "$MAVEN_LOG" "java-tool-options="
  assert_contains "$MAVEN_LOG" "jdk-java-options="
  assert_contains "$MAVEN_LOG" "underscore-java-options="
  assert_not_contains "$MAVEN_LOG" "unexpected"
  assert_not_contains "$MAVEN_LOG" "other-pom"
  assert_not_contains "$MAVEN_LOG" "skipNexusStagingDeployMojo=true"
  assert_not_contains "$MAVEN_LOG" "skipStaging=true"
  assert_not_contains "$MAVEN_LOG" "ssl.insecure=true"
}

test_maven_failure_status_is_preserved() {
  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  set +o errexit
  FAKE_SIGN_MAVEN_EXIT_CODE=42 \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1
  status=$?
  set -o errexit

  if [[ "$status" -ne 42 ]]; then
    fail "Maven exit 42 was not preserved; got $status"
  fi
  assert_not_contains "$NEXUS_CALLED_LOG" "called"
  assert_not_contains "$OUTPUT_LOG" "deploy finished"
}

test_nexus_maven_failure_status_is_preserved() {
  new_fixture
  keys=$(mktemp "$TEST_ROOT/keys.XXXXXX")
  printf 'fake KEYS\n' > "$keys"
  set +o errexit
  FAKE_NEXUS_MAVEN_EXIT_CODE=43 \
    run_script \
      --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
      --keys-file "$keys" \
      > "$OUTPUT_LOG" 2>&1
  status=$?
  set -o errexit

  if [[ "$status" -ne 43 ]]; then
    fail "Nexus Maven exit 43 was not preserved; got $status"
  fi
  assert_contains "$NEXUS_CALLED_LOG" "called"
  assert_not_contains "$OUTPUT_LOG" "deploy finished"
}

test_real_deploy_requires_official_repository_run() {
  new_fixture
  if run_script \
    --repo example/fork \
    --gpg-keyname 0123456789ABCDEF0123456789ABCDEF01234567 \
    > "$OUTPUT_LOG" 2>&1; then
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
  assert_not_contains "$GH_LOG" "actions/artifacts"
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
  assert_not_contains "$GH_LOG" "actions/artifacts"
  assert_maven_not_invoked
}

test_git_index_flags_are_rejected() {
  new_fixture
  git -C "$FIXTURE_DIR" update-index --assume-unchanged java/pom.xml

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "assume-unchanged package input was accepted"
  fi
  assert_contains "$OUTPUT_LOG" "Git index flags"
  assert_not_contains "$GH_LOG" "actions/artifacts"
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
  assert_not_contains "$GH_LOG" "actions/artifacts"
  assert_maven_not_invoked
}

test_repository_local_attributes_are_rejected() {
  new_fixture
  mkdir -p "$FIXTURE_DIR/.git/info"
  printf 'java/pom.xml export-ignore\n' > "$FIXTURE_DIR/.git/info/attributes"

  if run_script --dry-run > "$OUTPUT_LOG" 2>&1; then
    fail "repository-local Git attributes were accepted"
  fi
  assert_contains "$OUTPUT_LOG" "repository-local Git attributes"
  assert_not_contains "$GH_LOG" "actions/artifacts"
  assert_maven_not_invoked
}

run_test() {
  local name=$1
  "$name"
  TEST_COUNT=$((TEST_COUNT + 1))
  echo "PASS: $name"
}

run_test test_missing_option_value_never_runs_maven
run_test test_provenance_manifest_option_is_required
run_test test_dry_run_writes_frozen_provenance_without_overwrite
run_test test_real_deploy_requires_manifest_from_dry_run
run_test test_workflow_run_must_be_successful_release_tag_push
run_test test_workflow_path_id_and_attempt_are_frozen
run_test test_failed_job_rerun_can_reuse_earlier_java_candidate
run_test test_rerun_during_candidate_validation_stops_before_maven
run_test test_retag_during_candidate_validation_stops_before_maven
run_test test_workflow_run_tag_and_sha_must_match_checkout
run_test test_release_tag_must_be_annotated
run_test test_release_tag_signature_must_verify
run_test test_signed_tag_object_name_must_match_release_tag
run_test test_current_remote_tag_object_must_match_local_tag
run_test test_downloads_java_candidate_by_immutable_artifact_id
run_test test_dry_run_can_validate_a_fork_without_enabling_real_deploy
run_test test_github_host_is_pinned
run_test test_java_package_metadata_must_be_unique_and_complete
run_test test_downloaded_java_package_digest_must_match_metadata
run_test test_java_package_zip_rejects_unsafe_paths
run_test test_java_package_requires_exact_four_candidate_files
run_test test_validator_rejects_one_byte_classifier_jars
run_test test_validator_rejects_invalid_java_class_and_maven_metadata
run_test test_validator_rejects_invalid_legal_and_native_contents
run_test test_validator_rejects_changed_source_and_javadoc_contents
run_test test_validator_rejects_windows_drive_relative_jar_entries
run_test test_same_commit_retag_candidate_provenance_mismatch
run_test test_staging_profile_id_is_required_and_frozen
run_test test_dry_run_invokes_no_maven_or_gpg
run_test test_real_deploy_requires_full_and_explicit_signing_fingerprint
run_test test_real_deploy_requires_local_and_asf_signing_key
run_test test_real_deploy_downloads_pinned_asf_keys_by_default
run_test test_settings_and_environment_cannot_control_plugin_resolution_or_signing_inputs
run_test test_pinned_plugin_download_digest_mismatch_stops_before_maven
run_test test_real_deploy_uses_only_two_direct_plugin_goals
run_test test_file_repository_payload_and_allowlist_gate_nexus
run_test test_signing_cannot_mutate_frozen_input_and_repository_together
run_test test_signing_cannot_mutate_pinned_plugin_closure_before_upload
run_test test_missing_bad_or_wrong_key_signature_blocks_nexus
run_test test_final_provenance_boundary_runs_after_signatures
run_test test_maven_and_jvm_environment_is_scrubbed
run_test test_maven_failure_status_is_preserved
run_test test_nexus_maven_failure_status_is_preserved
run_test test_real_deploy_requires_official_repository_run
run_test test_dirty_release_input_stops_before_download
run_test test_foreign_git_environment_cannot_redirect_checkout_checks
run_test test_git_index_flags_are_rejected
run_test test_git_replacement_refs_are_rejected
run_test test_repository_local_attributes_are_rejected

echo "All $TEST_COUNT deploy_java_staging tests passed with Bash $BASH_VERSION."
