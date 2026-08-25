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
import gzip
import io
import os
from pathlib import Path
import subprocess
import sys
import tarfile

import pytest


TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))

import verify_source_archive as verifier  # noqa: E402


PREFIX = "paimon-mosaic-0.3.0/"


def run(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def commit(repo: Path, message: str) -> str:
    run(["git", "add", "."], cwd=repo)
    run(["git", "commit", "-q", "-m", message], cwd=repo)
    return run(["git", "rev-parse", "HEAD"], cwd=repo).stdout.decode().strip()


def initialize_repo(tmp_path: Path) -> tuple[Path, str]:
    repo = tmp_path / "repo"
    repo.mkdir()
    write(repo / "README.md", "source contents\n")
    write(repo / "bin/tool.sh", "#!/usr/bin/env bash\n")
    (repo / "bin/tool.sh").chmod(0o755)
    write(repo / ".gitignore", "ignored\n")
    write(repo / ".github/workflows/ci.yml", "name: ignored\n")
    write(repo / ".idea/workspace.xml", "ignored\n")
    write(repo / "target/generated.txt", "ignored\n")
    write(repo / "nested/project.iml", "ignored\n")
    write(repo / "nested/.DS_Store", "ignored\n")
    write(repo / "deploysettings.xml", "ignored\n")
    os.symlink("../README.md", repo / "bin/README-link")

    run(["git", "init", "-q"], cwd=repo)
    run(["git", "config", "user.name", "Archive Test"], cwd=repo)
    run(
        ["git", "config", "user.email", "archive-test@example.invalid"],
        cwd=repo,
    )
    return repo, commit(repo, "fixture")


def read_members(path: Path) -> list[tuple[tarfile.TarInfo, bytes | None]]:
    members = []
    with tarfile.open(path, mode="r:gz") as archive:
        for member in archive:
            content = archive.extractfile(member).read() if member.isfile() else None
            members.append((copy.copy(member), content))
    return members


def write_tgz(
    path: Path,
    members: list[tuple[tarfile.TarInfo, bytes | None]],
    *,
    pax_headers: dict[str, str] | None = None,
) -> None:
    raw = io.BytesIO()
    with tarfile.open(
        fileobj=raw,
        mode="w",
        format=tarfile.PAX_FORMAT,
        pax_headers=pax_headers,
    ) as archive:
        for member, content in members:
            archive.addfile(
                member,
                io.BytesIO(content) if content is not None else None,
            )
    with path.open("wb") as output:
        with gzip.GzipFile(
            filename="",
            mode="wb",
            fileobj=output,
            mtime=0,
        ) as compressed:
            compressed.write(raw.getvalue())


def archive_pax_headers(path: Path) -> dict[str, str]:
    with tarfile.open(path, mode="r:gz") as archive:
        return dict(archive.pax_headers)


def test_create_and_verify_uses_one_git_archive_and_raw_git_objects(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo, git_commit = initialize_repo(tmp_path)
    archive = tmp_path / "source.tgz"
    git_archive_calls = 0
    real_run = verifier.subprocess.run

    def recording_run(command, *args, **kwargs):
        nonlocal git_archive_calls
        if "archive" in command:
            git_archive_calls += 1
        return real_run(command, *args, **kwargs)

    monkeypatch.setattr(verifier.subprocess, "run", recording_run)

    assert (
        verifier.create_archive(archive, repo, git_commit, PREFIX) == git_commit
    )
    assert verifier.verify_archive(archive, repo, git_commit, PREFIX) == git_commit
    assert git_archive_calls == 1

    with tarfile.open(archive, mode="r:gz") as source:
        names = {member.name for member in source}
        assert source.getmember(f"{PREFIX}README.md").mode == 0o664
        assert source.getmember(f"{PREFIX}bin/tool.sh").mode == 0o775
        assert source.getmember(f"{PREFIX}bin/README-link").linkname == "../README.md"
    assert f"{PREFIX}README.md" in names
    assert f"{PREFIX}.gitignore" not in names
    assert not any(name.startswith(f"{PREFIX}.github") for name in names)
    assert not any(name.startswith(f"{PREFIX}.idea") for name in names)
    assert not any(name.startswith(f"{PREFIX}target") for name in names)
    assert f"{PREFIX}nested/project.iml" not in names
    assert f"{PREFIX}nested/.DS_Store" not in names
    assert f"{PREFIX}deploysettings.xml" not in names


def test_repository_argument_overrides_foreign_git_environment(
    tmp_path: Path,
) -> None:
    intended_root = tmp_path / "intended"
    intended_root.mkdir()
    intended_repo, intended_commit = initialize_repo(intended_root)
    foreign_root = tmp_path / "foreign"
    foreign_root.mkdir()
    foreign_repo, _ = initialize_repo(foreign_root)
    write(intended_repo / "README.md", "intended repository contents\n")
    intended_commit = commit(intended_repo, "distinguish intended repository")
    archive = tmp_path / "source.tgz"
    env = os.environ.copy()
    env.update(
        {
            "GIT_DIR": str(foreign_repo / ".git"),
            "GIT_WORK_TREE": str(foreign_repo),
        }
    )
    command = [
        sys.executable,
        str(TOOLS / "verify_source_archive.py"),
        "create",
        "--repository",
        str(intended_repo),
        "--commit",
        intended_commit,
        "--prefix",
        PREFIX,
        "--output",
        str(archive),
    ]

    run(command, cwd=tmp_path, env=env)
    command[2] = "verify"
    command[-2:] = ["--archive", str(archive)]
    run(command, cwd=tmp_path, env=env)


def test_committed_export_ignore_is_reported_as_missing(tmp_path: Path) -> None:
    repo, _ = initialize_repo(tmp_path)
    write(repo / "protected.txt", "must be present\n")
    write(repo / ".gitattributes", "protected.txt export-ignore\n")
    git_commit = commit(repo, "export-ignore")

    with pytest.raises(ValueError, match="missing entries.*protected.txt"):
        verifier.create_archive(
            tmp_path / "source.tgz",
            repo,
            git_commit,
            PREFIX,
        )


def test_committed_export_subst_is_reported_as_byte_mismatch(tmp_path: Path) -> None:
    repo, _ = initialize_repo(tmp_path)
    write(repo / "revision.txt", "$Format:%H$\n")
    write(repo / ".gitattributes", "revision.txt export-subst\n")
    git_commit = commit(repo, "export-subst")

    with pytest.raises(ValueError, match="content differs.*revision.txt"):
        verifier.create_archive(
            tmp_path / "source.tgz",
            repo,
            git_commit,
            PREFIX,
        )


def test_same_tree_from_wrong_commit_is_rejected(tmp_path: Path) -> None:
    repo, first_commit = initialize_repo(tmp_path)
    archive = tmp_path / "source.tgz"
    verifier.create_archive(archive, repo, first_commit, PREFIX)
    run(["git", "commit", "-q", "--allow-empty", "-m", "second"], cwd=repo)
    second_commit = run(["git", "rev-parse", "HEAD"], cwd=repo).stdout.decode().strip()

    with pytest.raises(ValueError, match="embedded Git commit"):
        verifier.verify_archive(archive, repo, second_commit, PREFIX)


@pytest.mark.parametrize(
    ("mutation", "expected"),
    (
        ("missing", "missing entries"),
        ("unexpected", "unexpected entries"),
        ("mode", "mode differs"),
        ("content", "content differs"),
        ("symlink", "symbolic-link target differs"),
    ),
)
def test_tree_identity_mutations_are_rejected(
    tmp_path: Path,
    mutation: str,
    expected: str,
) -> None:
    repo, git_commit = initialize_repo(tmp_path)
    archive = tmp_path / "source.tgz"
    verifier.create_archive(archive, repo, git_commit, PREFIX)
    headers = archive_pax_headers(archive)
    members = read_members(archive)

    if mutation == "missing":
        members = [item for item in members if item[0].name != f"{PREFIX}README.md"]
    elif mutation == "unexpected":
        info = tarfile.TarInfo(f"{PREFIX}UNEXPECTED")
        info.mode = 0o664
        info.size = 1
        members.append((info, b"x"))
    elif mutation == "mode":
        for info, _ in members:
            if info.name == f"{PREFIX}README.md":
                info.mode = 0o777
    elif mutation == "content":
        members = [
            (info, b"different\n" if info.name == f"{PREFIX}README.md" else content)
            for info, content in members
        ]
        for info, content in members:
            if info.name == f"{PREFIX}README.md":
                assert content is not None
                info.size = len(content)
    else:
        for info, _ in members:
            if info.name == f"{PREFIX}bin/README-link":
                info.linkname = "tool.sh"

    write_tgz(archive, members, pax_headers=headers)

    with pytest.raises(ValueError, match=expected):
        verifier.verify_archive(archive, repo, git_commit, PREFIX)


def test_rejects_unsafe_and_duplicate_paths(tmp_path: Path) -> None:
    archive = tmp_path / "unsafe.tgz"
    first = tarfile.TarInfo("../escape")
    first.mode = 0o664
    first.size = 1
    write_tgz(archive, [(first, b"x")])
    with pytest.raises(ValueError, match="unsafe archive entry path"):
        verifier.read_source_archive(archive, PREFIX)

    duplicate = tarfile.TarInfo(f"{PREFIX}README.md")
    duplicate.mode = 0o664
    duplicate.size = 1
    write_tgz(archive, [(copy.copy(duplicate), b"x"), (duplicate, b"y")])
    with pytest.raises(ValueError, match="duplicate archive entry"):
        verifier.read_source_archive(archive, PREFIX)


def test_rejects_escaping_symlink(tmp_path: Path) -> None:
    archive = tmp_path / "symlink.tgz"
    root = tarfile.TarInfo(PREFIX.rstrip("/"))
    root.type = tarfile.DIRTYPE
    root.mode = 0o775
    link = tarfile.TarInfo(f"{PREFIX}link")
    link.type = tarfile.SYMTYPE
    link.mode = 0o777
    link.linkname = "../outside"
    write_tgz(archive, [(root, None), (link, None)])

    with pytest.raises(ValueError, match="escapes the archive root"):
        verifier.read_source_archive(archive, PREFIX)


def test_rejects_trailing_gzip_and_tar_data(tmp_path: Path) -> None:
    repo, git_commit = initialize_repo(tmp_path)
    archive = tmp_path / "source.tgz"
    verifier.create_archive(archive, repo, git_commit, PREFIX)

    archive.write_bytes(archive.read_bytes() + b"trailing")
    with pytest.raises(ValueError, match="trailing gzip data"):
        verifier.read_source_archive(archive, PREFIX)

    archive.unlink()
    verifier.create_archive(archive, repo, git_commit, PREFIX)
    raw = gzip.decompress(archive.read_bytes())
    extra = io.BytesIO()
    with tarfile.open(fileobj=extra, mode="w") as trailing:
        info = tarfile.TarInfo(f"{PREFIX}UNVERIFIED")
        info.size = 1
        trailing.addfile(info, io.BytesIO(b"x"))
    archive.write_bytes(gzip.compress(raw + extra.getvalue(), mtime=0))
    with pytest.raises(ValueError, match="trailing tar data"):
        verifier.read_source_archive(archive, PREFIX)


def test_rejects_gzip_stream_with_truncated_final_trailer(tmp_path: Path) -> None:
    repo, git_commit = initialize_repo(tmp_path)
    archive = tmp_path / "source.tgz"
    verifier.create_archive(archive, repo, git_commit, PREFIX)
    complete = archive.read_bytes()
    assert len(complete) > 8
    archive.write_bytes(complete[:-8])

    with pytest.raises(ValueError, match="ended before its trailer"):
        verifier.read_source_archive(archive, PREFIX)


def test_rejects_unreasonably_large_archive(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    archive = tmp_path / "large.tgz"
    archive.write_bytes(gzip.compress(b"x" * 2048, mtime=0))
    monkeypatch.setattr(verifier, "MAX_ARCHIVE_BYTES", 1024)

    with pytest.raises(ValueError, match="size limit"):
        verifier.read_source_archive(archive, PREFIX)


def test_rejects_unreasonably_large_git_tree(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo, git_commit = initialize_repo(tmp_path)
    monkeypatch.setattr(verifier, "MAX_ARCHIVE_BYTES", 8)

    with pytest.raises(ValueError, match="Git source tree exceeds the size limit"):
        verifier.expected_entries(repo, git_commit, PREFIX)
