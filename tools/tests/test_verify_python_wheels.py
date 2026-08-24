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

import base64
import csv
import hashlib
import io
from pathlib import Path
import sys
import warnings
from zipfile import ZipFile

import pytest


TOOLS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(TOOLS))

import verify_python_wheels as verifier  # noqa: E402


PE_FIXTURE = bytearray(132)
PE_FIXTURE[:2] = b"MZ"
PE_FIXTURE[0x3C:0x40] = (0x80).to_bytes(4, "little")
PE_FIXTURE[0x80:0x84] = b"PE\0\0"
NATIVE_BYTES = {
    "x86_64-unknown-linux-gnu": b"\x7fELF-x86_64",
    "aarch64-unknown-linux-gnu": b"\x7fELF-aarch64",
    "aarch64-apple-darwin": b"\xcf\xfa\xed\xfe-macos",
    "x86_64-pc-windows-msvc": bytes(PE_FIXTURE),
}


@pytest.fixture
def python_root(tmp_path):
    root = tmp_path
    pyproject = root / "python/pyproject.toml"
    pyproject.parent.mkdir(parents=True)
    pyproject.write_text(
        """
[project]
name = "paimon-mosaic"
version = "0.3.0"
description = "Python bindings for the Mosaic columnar-bucket hybrid file format"
license = "Apache-2.0"
requires-python = ">=3.9"
dependencies = ["pyarrow"]

[project.optional-dependencies]
test = ["pytest"]
""".strip()
        + "\n",
        encoding="utf-8",
    )
    modules = {
        "mosaic/__init__.py": b'"""fixture"""\n',
        "mosaic/_ffi.py": b"def load():\n    pass\n",
        "mosaic/mosaic.py": b"class MosaicReader:\n    pass\n",
    }
    for name, contents in modules.items():
        path = root / "python" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)
    for target in verifier.TARGETS:
        legal = root / "python/licenses" / target
        legal.mkdir(parents=True)
        (legal / "LICENSE").write_bytes(f"LICENSE {target}\n".encode())
        (legal / "NOTICE").write_bytes(f"NOTICE {target}\n".encode())
        (legal / "THIRD-PARTY-LICENSES.html").write_bytes(
            f"REPORT {target}\n".encode()
        )
    return root


def record_bytes(entries, record_path, mutation=None):
    rows = []
    for name, contents in entries.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(contents).digest())
        rows.append(
            [name, f"sha256={digest.rstrip(b'=').decode()}", str(len(contents))]
        )
    rows.append([record_path, "", ""])
    if mutation:
        mutation(rows)
    output = io.StringIO(newline="")
    csv.writer(output, lineterminator="\n").writerows(rows)
    return output.getvalue().encode()


def build_wheel(
    root,
    target="x86_64-unknown-linux-gnu",
    *,
    filename_version="0.3.0",
    dist_info_version=None,
    metadata_version=None,
    wheel_versions=("1.0",),
    wheel_tag=None,
    metadata_dependencies=None,
    native_bytes=None,
    extra_entries=None,
    omitted=(),
    record_mutation=None,
    metadata_mutation=None,
):
    tag = verifier.TARGETS[target]["tag"]
    wheel_tag = wheel_tag or tag
    dist_info_version = dist_info_version or filename_version
    metadata_version = metadata_version or filename_version
    dist_info = f"paimon_mosaic-{dist_info_version}.dist-info"
    record_path = f"{dist_info}/RECORD"
    wheel = root / f"paimon_mosaic-{filename_version}-{tag}.whl"

    dependencies = metadata_dependencies
    if dependencies is None:
        dependencies = (
            "Requires-Dist: pyarrow\n"
            'Requires-Dist: pytest; extra == "test"\n'
        )
    metadata_lines = [
        "Metadata-Version: 2.4",
        "Name: paimon-mosaic",
        f"Version: {metadata_version}",
        (
            "Summary: Python bindings for the Mosaic "
            "columnar-bucket hybrid file format"
        ),
        "License-Expression: Apache-2.0",
        "Requires-Python: >=3.9",
        *(
            f"License-File: licenses/{target}/{name}"
            for name in ("LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.html")
        ),
        *dependencies.rstrip().splitlines(),
        "Provides-Extra: test",
        "Dynamic: license-file",
    ]
    if metadata_mutation is not None:
        metadata_mutation(metadata_lines)
    metadata = ("\n".join(metadata_lines) + "\n\n").encode()
    wheel_metadata = (
        "".join(f"Wheel-Version: {value}\n" for value in wheel_versions)
        + "Generator: fixture\n"
        + "Root-Is-Purelib: false\n"
        + f"Tag: {wheel_tag}\n\n"
    ).encode()

    entries = {
        name: path.read_bytes()
        for name, path in verifier.python_modules(root).items()
    }
    legal = root / "python/licenses" / target
    for legal_name in ("LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.html"):
        contents = (legal / legal_name).read_bytes()
        entries[f"mosaic/{legal_name}"] = contents
        entries[
            f"{dist_info}/licenses/licenses/{target}/{legal_name}"
        ] = contents
    entries[verifier.TARGETS[target]["native"]] = (
        NATIVE_BYTES[target] if native_bytes is None else native_bytes
    )
    entries[f"{dist_info}/METADATA"] = metadata
    entries[f"{dist_info}/WHEEL"] = wheel_metadata
    entries[f"{dist_info}/top_level.txt"] = b"mosaic\n"
    entries.update(extra_entries or {})
    entries = {name: contents for name, contents in entries.items() if name not in omitted}
    entries[record_path] = record_bytes(entries, record_path, record_mutation)

    with ZipFile(wheel, "w") as archive:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            for name, contents in entries.items():
                archive.writestr(name, contents)
    return wheel


@pytest.mark.parametrize("target", tuple(NATIVE_BYTES))
def test_verify_wheel_accepts_each_release_target(python_root, target):
    wheel = build_wheel(python_root, target)

    assert verifier.verify_wheel(wheel, python_root) == target


@pytest.mark.parametrize(
    ("versions", "message"),
    [
        ((), "Wheel-Version"),
        (("1.1",), "Wheel-Version"),
        (("1.0", "1.0"), "Wheel-Version"),
    ],
)
def test_wheel_requires_one_wheel_version_1_0(
    python_root, versions, message
):
    wheel = build_wheel(python_root, wheel_versions=versions)

    with pytest.raises(ValueError, match=message):
        verifier.verify_wheel(wheel, python_root)


@pytest.mark.parametrize(
    ("kwargs", "message"),
    [
        ({"dist_info_version": "9.9.9"}, "dist-info"),
        ({"metadata_version": "9.9.9"}, "METADATA Version"),
        ({"wheel_tag": "py3-none-any"}, "WHEEL tags"),
    ],
)
def test_wheel_identity_is_consistent(python_root, kwargs, message):
    wheel = build_wheel(python_root, **kwargs)

    with pytest.raises(ValueError, match=message):
        verifier.verify_wheel(wheel, python_root)


def test_wheel_dependencies_match_pyproject(python_root):
    wheel = build_wheel(
        python_root, metadata_dependencies="Requires-Dist: different\n"
    )

    with pytest.raises(ValueError, match="Requires-Dist"):
        verifier.verify_wheel(wheel, python_root)


def replace_metadata_header(lines, name, value):
    prefix = f"{name}:"
    index = next(
        index for index, line in enumerate(lines) if line.startswith(prefix)
    )
    lines[index] = f"{name}: {value}"


def remove_metadata_header(lines, name):
    prefix = f"{name}:"
    lines[:] = [line for line in lines if not line.startswith(prefix)]


@pytest.mark.parametrize(
    ("mutation", "message"),
    [
        (
            lambda lines: replace_metadata_header(
                lines, "Metadata-Version", "2.3"
            ),
            "Metadata-Version",
        ),
        (
            lambda lines: replace_metadata_header(
                lines, "Summary", "tampered summary"
            ),
            "Summary",
        ),
        (
            lambda lines: replace_metadata_header(
                lines, "License-Expression", "MIT"
            ),
            "License-Expression",
        ),
        (
            lambda lines: replace_metadata_header(
                lines,
                "License-File",
                "licenses/x86_64-pc-windows-msvc/LICENSE",
            ),
            "License-File",
        ),
        (
            lambda lines: remove_metadata_header(lines, "Dynamic"),
            "Dynamic",
        ),
    ],
)
def test_wheel_metadata_contract_rejects_tampering_with_recomputed_record(
    python_root, mutation, message
):
    wheel = build_wheel(python_root, metadata_mutation=mutation)

    with pytest.raises(ValueError, match=message):
        verifier.verify_wheel(wheel, python_root)


@pytest.mark.parametrize(
    ("mutation", "message"),
    [
        (
            lambda rows: rows.__setitem__(
                0, [rows[0][0], "sha256=AAAAAAAA", rows[0][2]]
            ),
            "hash mismatch",
        ),
        (
            lambda rows: rows.__setitem__(
                0, [rows[0][0], rows[0][1], str(int(rows[0][2]) + 1)]
            ),
            "size mismatch",
        ),
        (
            lambda rows: rows.pop(0),
            "omits wheel entries",
        ),
    ],
)
def test_record_hash_size_and_completeness(
    python_root, mutation, message
):
    wheel = build_wheel(python_root, record_mutation=mutation)

    with pytest.raises(ValueError, match=message):
        verifier.verify_wheel(wheel, python_root)


@pytest.mark.parametrize(
    "extra",
    [
        {"payload.pth": b"import payload\n"},
        {"mosaic/extra.py": b"unexpected = True\n"},
        {"mosaic/extra.so": b"\x7fELFextra"},
    ],
)
def test_wheel_rejects_extra_payload(python_root, extra):
    wheel = build_wheel(python_root, extra_entries=extra)

    with pytest.raises(
        ValueError,
        match="unexpected wheel payload|native payload|Python modules",
    ):
        verifier.verify_wheel(wheel, python_root)


def test_wheel_requires_python_module_bytes(python_root):
    wheel = build_wheel(
        python_root, extra_entries={"mosaic/_ffi.py": b"tampered\n"}
    )

    with pytest.raises(ValueError, match="Python module"):
        verifier.verify_wheel(wheel, python_root)


def test_wheel_requires_target_legal_files(python_root):
    wheel = build_wheel(
        python_root,
        omitted={"mosaic/THIRD-PARTY-LICENSES.html"},
    )

    with pytest.raises(ValueError, match="legal"):
        verifier.verify_wheel(wheel, python_root)


def test_wheel_requires_native_magic(python_root):
    wheel = build_wheel(python_root, native_bytes=b"not native")

    with pytest.raises(ValueError, match="native magic"):
        verifier.verify_wheel(wheel, python_root)


def test_require_all_targets_accepts_exact_matrix(python_root):
    wheels = [build_wheel(python_root, target) for target in verifier.TARGETS]

    assert verifier.verify_wheels(wheels, python_root, require_all_targets=True) == list(
        verifier.TARGETS
    )


@pytest.mark.parametrize("mutation", ["missing", "duplicate"])
def test_require_all_targets_rejects_missing_or_duplicate_target(
    python_root, mutation
):
    wheels = [build_wheel(python_root, target) for target in verifier.TARGETS]
    if mutation == "missing":
        wheels.pop()
    else:
        duplicate = python_root / "duplicate" / wheels[0].name
        duplicate.parent.mkdir()
        duplicate.write_bytes(wheels[0].read_bytes())
        wheels[-1] = duplicate

    with pytest.raises(ValueError, match="four release targets"):
        verifier.verify_wheels(wheels, python_root, require_all_targets=True)


def test_cli_require_all_targets_rejects_a_missing_target(
    python_root, monkeypatch, capsys
):
    wheels = [
        build_wheel(python_root, target)
        for target in list(verifier.TARGETS)[:-1]
    ]
    monkeypatch.setattr(
        sys,
        "argv",
        [
            "verify_python_wheels.py",
            "--require-all-targets",
            *(str(wheel) for wheel in wheels),
        ],
    )
    monkeypatch.setattr(verifier, "repository_root", lambda: python_root)

    assert verifier.main() == 1
    assert "four release targets" in capsys.readouterr().err
