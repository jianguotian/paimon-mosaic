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

import runpy
from pathlib import Path
import sys
import tomllib
import types

import pytest


ROOT = Path(__file__).resolve().parents[2]
SETUP = ROOT / "python/setup.py"


def load_setup(monkeypatch):
    captured = {}

    class Distribution:
        pass

    class BuildPy:
        def run(self):
            self.super_ran = True

    class BdistWheel:
        def finalize_options(self):
            self.root_is_pure = True

        def get_tag(self):
            return "cp312", "cp312", "linux_x86_64"

    setuptools = types.ModuleType("setuptools")
    setuptools.Distribution = Distribution
    setuptools.setup = lambda **kwargs: captured.update(kwargs)
    setuptools_command = types.ModuleType("setuptools.command")
    setuptools_build = types.ModuleType("setuptools.command.build_py")
    setuptools_build.build_py = BuildPy
    wheel = types.ModuleType("wheel")
    wheel_bdist = types.ModuleType("wheel.bdist_wheel")
    wheel_bdist.bdist_wheel = BdistWheel
    monkeypatch.setitem(sys.modules, "setuptools", setuptools)
    monkeypatch.setitem(sys.modules, "setuptools.command", setuptools_command)
    monkeypatch.setitem(sys.modules, "setuptools.command.build_py", setuptools_build)
    monkeypatch.setitem(sys.modules, "wheel", wheel)
    monkeypatch.setitem(sys.modules, "wheel.bdist_wheel", wheel_bdist)
    module = runpy.run_path(str(SETUP))
    return module, captured


@pytest.mark.parametrize(
    ("system", "machine", "target"),
    [
        ("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
        ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
        ("Darwin", "arm64", "aarch64-apple-darwin"),
        ("Windows", "AMD64", "x86_64-pc-windows-msvc"),
        ("Darwin", "x86_64", None),
    ],
)
def test_detects_only_release_targets(monkeypatch, system, machine, target):
    module, _ = load_setup(monkeypatch)
    monkeypatch.setattr(module["platform"], "system", lambda: system)
    monkeypatch.setattr(module["platform"], "machine", lambda: machine)

    assert module["_detect_rust_target"]() == target


@pytest.mark.parametrize(
    ("system", "machine", "expected"),
    [
        (
            "Linux",
            "x86_64",
            [
                "licenses/x86_64-unknown-linux-gnu/LICENSE",
                "licenses/x86_64-unknown-linux-gnu/NOTICE",
                "licenses/x86_64-unknown-linux-gnu/THIRD-PARTY-LICENSES.html",
            ],
        ),
        ("FreeBSD", "amd64", []),
    ],
)
def test_declares_only_current_target_legal_files(
    monkeypatch, system, machine, expected
):
    module, _ = load_setup(monkeypatch)
    monkeypatch.setattr(module["platform"], "system", lambda: system)
    monkeypatch.setattr(module["platform"], "machine", lambda: machine)

    assert module["_license_files"]() == expected


def test_setup_declares_detected_host_legal_files(monkeypatch):
    module, captured = load_setup(monkeypatch)

    assert captured["license_files"] == module["_license_files"]()


def prepare_python_tree(tmp_path, module):
    python = tmp_path / "python"
    package = python / "mosaic"
    package.mkdir(parents=True)
    module["_package_dir"].__globals__["__file__"] = str(python / "setup.py")
    target = "x86_64-unknown-linux-gnu"
    legal = python / "licenses" / target
    legal.mkdir(parents=True)
    for name in ("LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.html"):
        (legal / name).write_bytes(name.encode())
    return python, package, target


def test_find_native_library_precedence(tmp_path, monkeypatch):
    module, _ = load_setup(monkeypatch)
    python, package, _ = prepare_python_tree(tmp_path, module)
    library = "libpaimon_mosaic_ffi.so"
    locations = {
        "packaged": package / library,
        "debug": tmp_path / "target/debug" / library,
        "release": tmp_path / "target/release" / library,
        "environment": tmp_path / "external" / library,
    }
    for name, path in locations.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(name.encode())
    monkeypatch.setenv("MOSAIC_LIB_PATH", str(locations["environment"].parent))

    assert Path(module["_find_native_lib"]()) == locations["environment"]
    monkeypatch.delenv("MOSAIC_LIB_PATH")
    assert (
        Path(module["_find_native_lib"]()).resolve()
        == locations["release"].resolve()
    )
    locations["release"].unlink()
    assert (
        Path(module["_find_native_lib"]()).resolve()
        == locations["debug"].resolve()
    )
    locations["debug"].unlink()
    assert (
        Path(module["_find_native_lib"]()).resolve()
        == locations["packaged"].resolve()
    )


def test_build_copies_one_native_and_target_legal_files(tmp_path, monkeypatch):
    module, _ = load_setup(monkeypatch)
    _, package, _ = prepare_python_tree(tmp_path, module)
    monkeypatch.setattr(module["platform"], "system", lambda: "Linux")
    monkeypatch.setattr(module["platform"], "machine", lambda: "x86_64")
    native = tmp_path / "target/release/libpaimon_mosaic_ffi.so"
    native.parent.mkdir(parents=True)
    native.write_bytes(b"\x7fELF-real")

    command = module["BuildPyWithNativeLib"].__new__(
        module["BuildPyWithNativeLib"]
    )
    command.build_lib = str(tmp_path / "build")
    built_package = Path(command.build_lib) / "mosaic"
    built_package.mkdir(parents=True)
    for stale in (
        "libpaimon_mosaic_ffi.so",
        "libpaimon_mosaic_ffi.dylib",
        "paimon_mosaic_ffi.dll",
    ):
        (built_package / stale).write_bytes(b"stale")

    command.run()

    assert command.super_ran is True
    assert (
        built_package / "libpaimon_mosaic_ffi.so"
    ).read_bytes() == b"\x7fELF-real"
    assert not (built_package / "libpaimon_mosaic_ffi.dylib").exists()
    assert not (built_package / "paimon_mosaic_ffi.dll").exists()
    for name in ("LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.html"):
        assert (built_package / name).read_bytes() == name.encode()
    assert not (package / "libpaimon_mosaic_ffi.so").exists()


def test_build_fails_without_native_library(tmp_path, monkeypatch):
    module, _ = load_setup(monkeypatch)
    prepare_python_tree(tmp_path, module)
    monkeypatch.setattr(module["platform"], "system", lambda: "Linux")
    monkeypatch.setattr(module["platform"], "machine", lambda: "x86_64")
    command = module["BuildPyWithNativeLib"].__new__(
        module["BuildPyWithNativeLib"]
    )
    command.build_lib = str(tmp_path / "build")

    with pytest.raises(RuntimeError, match="native library was not found"):
        command.run()


def test_pyproject_uses_reproducible_explicit_packaging():
    with (ROOT / "python/pyproject.toml").open("rb") as source:
        config = tomllib.load(source)

    assert config["build-system"]["requires"] == [
        "setuptools==79.0.1",
        "wheel==0.45.1",
    ]
    assert config["tool"]["setuptools"]["include-package-data"] is False
    assert "package-data" not in config["tool"]["setuptools"]
