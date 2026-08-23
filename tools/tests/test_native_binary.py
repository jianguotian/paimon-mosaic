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

import struct
import sys
from pathlib import Path

import pytest


TESTS = Path(__file__).resolve().parent
TOOLS = TESTS.parent
sys.path.insert(0, str(TOOLS))
sys.path.insert(0, str(TESTS))

import native_binary as verifier  # noqa: E402
from native_binary_fixtures import (  # noqa: E402
    FFI_SYMBOLS,
    JNI_SYMBOLS,
    align,
    build_elf,
)


def verify_jni_target(data, target, path):
    verifier.verify_native_target(
        data,
        target,
        path,
        symbol_family="JNI",
    )


def build_pe(
    symbols=JNI_SYMBOLS,
    dll=True,
    optional_magic=0x20B,
    function_rva=None,
    section_characteristics=0x60000020,
    forwarder=None,
    symbol_rvas=None,
    symbol_forwarders=None,
    export_name_order=None,
):
    pe_offset = 0x80
    optional_size = 240
    section_table_offset = pe_offset + 24 + optional_size
    headers_size = 0x200
    section_rva = 0x1000
    section_offset = headers_size
    section_size = 0x4000
    text_rva = 0x5000
    text_offset = section_offset + section_size
    text_size = 0x200
    data = bytearray(text_offset + text_size)

    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, pe_offset)
    data[pe_offset : pe_offset + 4] = b"PE\0\0"
    characteristics = 0x0022 | (0x2000 if dll else 0)
    struct.pack_into(
        "<HHIIIHH",
        data,
        pe_offset + 4,
        0x8664,
        2,
        0,
        0,
        0,
        optional_size,
        characteristics,
    )

    optional_offset = pe_offset + 24
    struct.pack_into("<H", data, optional_offset, optional_magic)
    struct.pack_into("<I", data, optional_offset + 32, 0x1000)
    struct.pack_into("<I", data, optional_offset + 36, 0x200)
    struct.pack_into("<I", data, optional_offset + 56, 0x6000)
    struct.pack_into("<I", data, optional_offset + 60, headers_size)
    struct.pack_into("<I", data, optional_offset + 108, 16)
    struct.pack_into(
        "<8sIIIIIIHHI",
        data,
        section_table_offset,
        b".rdata\0\0",
        section_size,
        section_rva,
        section_size,
        section_offset,
        0,
        0,
        0,
        0,
        0x40000040,
    )
    struct.pack_into(
        "<8sIIIIIIHHI",
        data,
        section_table_offset + 40,
        b".text\0\0\0",
        text_size,
        text_rva,
        text_size,
        text_offset,
        0,
        0,
        0,
        0,
        section_characteristics,
    )

    symbol_list = sorted(symbols)
    export_names = (
        symbol_list
        if export_name_order is None
        else list(export_name_order)
    )
    if len(export_names) != len(symbol_list):
        raise ValueError("export_name_order must contain one entry per symbol")
    if any(symbol not in symbols for symbol in export_names):
        raise ValueError("export_name_order contains an unknown symbol")

    export_offset = section_offset
    module_offset = section_offset + 0x40

    def rva(offset):
        return section_rva + offset - section_offset

    if function_rva is None:
        function_rva = text_rva

    module_name = b"paimon_mosaic_test.dll\0"
    data[module_offset : module_offset + len(module_name)] = module_name
    functions_offset = align(module_offset + len(module_name), 4)
    names_offset = functions_offset + len(symbol_list) * 4
    ordinals_offset = names_offset + len(export_names) * 4
    string_offset = ordinals_offset + len(export_names) * 2

    name_offsets = {}
    for symbol in symbol_list:
        encoded = symbol.encode() + b"\0"
        name_offsets[symbol] = string_offset
        data[string_offset : string_offset + len(encoded)] = encoded
        string_offset += len(encoded)

    forwarder_offset = string_offset
    function_rvas = {}
    for symbol in symbol_list:
        symbol_forwarder = (
            (symbol_forwarders or {}).get(symbol)
            if forwarder is None
            else forwarder
        )
        if symbol_forwarder is not None:
            encoded_forwarder = symbol_forwarder.encode() + b"\0"
            function_rvas[symbol] = rva(forwarder_offset)
            data[
                forwarder_offset : forwarder_offset + len(encoded_forwarder)
            ] = encoded_forwarder
            forwarder_offset += len(encoded_forwarder)
        else:
            function_rvas[symbol] = (symbol_rvas or {}).get(
                symbol, function_rva
            )

    export_size = forwarder_offset - export_offset
    struct.pack_into(
        "<II", data, optional_offset + 112, section_rva, export_size
    )
    struct.pack_into(
        "<IIHHIIIIIII",
        data,
        export_offset,
        0,
        0,
        0,
        0,
        rva(module_offset),
        1,
        len(symbol_list),
        len(export_names),
        rva(functions_offset),
        rva(names_offset),
        rva(ordinals_offset),
    )
    symbol_ordinals = {
        symbol: index for index, symbol in enumerate(symbol_list)
    }
    for index, symbol in enumerate(symbol_list):
        struct.pack_into(
            "<I",
            data,
            functions_offset + index * 4,
            function_rvas[symbol],
        )
    for index, symbol in enumerate(export_names):
        struct.pack_into(
            "<I", data, names_offset + index * 4, rva(name_offsets[symbol])
        )
        struct.pack_into(
            "<H",
            data,
            ordinals_offset + index * 2,
            symbol_ordinals[symbol],
        )
    return bytes(data)


def encode_uleb128(value):
    encoded = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            encoded.append(byte | 0x80)
        else:
            encoded.append(byte)
            return bytes(encoded)


def build_macho_export_trie(
    symbols,
    *,
    flags=0,
    address=0x1000,
    resolver=None,
    reexport_name=b"",
):
    names = [b"_" + symbol.encode() for symbol in sorted(symbols)]
    if len(names) > 255:
        raise ValueError("test export trie has too many root children")

    payload = encode_uleb128(flags)
    if flags & 0x08:
        payload += encode_uleb128(1) + reexport_name + b"\0"
    else:
        payload += encode_uleb128(address)
        if flags & 0x10:
            payload += encode_uleb128(
                address if resolver is None else resolver
            )
    leaf = encode_uleb128(len(payload)) + payload + b"\0"
    root_size = 2
    for _ in range(10):
        child_offsets = [
            root_size + index * len(leaf) for index in range(len(names))
        ]
        root = b"".join(
            (
                b"\0",
                bytes((len(names),)),
                *(
                    name + b"\0" + encode_uleb128(child_offset)
                    for name, child_offset in zip(names, child_offsets)
                ),
            )
        )
        if len(root) == root_size:
            return root + leaf * len(names)
        root_size = len(root)
    raise AssertionError("test export trie layout did not converge")


def build_id_dylib_command(mode):
    name = b"@rpath/libpaimon_mosaic_jni.dylib\0"
    command_size = align(24 + len(name), 8)
    if mode == "invalid_cmdsize":
        return struct.pack("<IIII", 0x0D, 16, 8, 0)
    if mode == "invalid_name_offset":
        name_offset = 16
    elif mode == "out_of_bounds_name_offset":
        name_offset = command_size
    else:
        name_offset = 24
    command = bytearray(command_size)
    struct.pack_into(
        "<IIIIII",
        command,
        0,
        0x0D,
        command_size,
        name_offset,
        0,
        0x10000,
        0x10000,
    )
    if mode == "unterminated_name":
        command[24:] = b"x" * (command_size - 24)
    elif mode == "empty_name":
        pass
    else:
        command[24 : 24 + len(name)] = name
    return bytes(command)


def build_macho(
    cpu_type=0x0100000C,
    symbols=JNI_SYMBOLS,
    file_type=6,
    id_dylib="valid",
    export_trie_symbols=None,
    export_trie_data=None,
    export_trie_command="exports_trie",
    export_trie_offset_override=None,
    export_trie_size_override=None,
    dyld_info_export_trie_symbols=None,
    dyld_info_export_trie_data=None,
    symbol_type=0x0F,
    symbol_value=0x1000,
    section_flags=0x80000400,
    segment_initial_protection=5,
    export_trie_flags=0,
    export_trie_address=0x1000,
    export_trie_resolver=None,
    export_trie_reexport_name=b"",
    extra_sections=0,
    extra_segments=(),
):
    symbol_list = sorted(symbols)
    section_count = 1 + extra_sections
    segment_size = 72 + 80 * section_count
    # Additional __LINKEDIT-style segments, each declaring its own section
    # count, so a test can exceed the cap only in aggregate.
    extra_segment_sizes = [72 + 80 * count for count in extra_segments]
    if id_dylib == "missing":
        id_dylib_commands = []
    elif id_dylib == "duplicate":
        id_dylib_commands = [
            build_id_dylib_command("valid"),
            build_id_dylib_command("valid"),
        ]
    else:
        id_dylib_commands = [build_id_dylib_command(id_dylib)]

    if export_trie_symbols is not None and export_trie_data is not None:
        raise ValueError(
            "provide export_trie_symbols or export_trie_data, not both"
        )
    if export_trie_data is not None:
        export_trie = bytes(export_trie_data)
    elif export_trie_symbols is not None:
        export_trie = build_macho_export_trie(
            export_trie_symbols,
            flags=export_trie_flags,
            address=export_trie_address,
            resolver=export_trie_resolver,
            reexport_name=export_trie_reexport_name,
        )
    else:
        export_trie = None
    if (
        dyld_info_export_trie_symbols is not None
        and dyld_info_export_trie_data is not None
    ):
        raise ValueError(
            "provide dyld_info_export_trie_symbols or "
            "dyld_info_export_trie_data, not both"
        )
    if dyld_info_export_trie_data is not None:
        dyld_info_export_trie = bytes(dyld_info_export_trie_data)
    elif dyld_info_export_trie_symbols is not None:
        dyld_info_export_trie = build_macho_export_trie(
            dyld_info_export_trie_symbols
        )
    else:
        dyld_info_export_trie = None
    if export_trie is None:
        export_command_size = 0
    elif export_trie_command == "exports_trie":
        export_command_size = 16
    elif export_trie_command in ("dyld_info", "dyld_info_only"):
        export_command_size = 48
    else:
        raise ValueError(
            f"unsupported test export trie command {export_trie_command}"
        )

    commands_size = (
        segment_size
        + sum(extra_segment_sizes)
        + sum(len(command) for command in id_dylib_commands)
        + 24
        + export_command_size
        + (48 if dyld_info_export_trie is not None else 0)
    )
    code_offset = 32 + commands_size
    symbols_offset = align(code_offset + 1, 8)

    strings = bytearray(b"\0")
    name_offsets = {}
    for symbol in symbol_list:
        name_offsets[symbol] = len(strings)
        strings.extend(b"_" + symbol.encode() + b"\0")
    strings_offset = symbols_offset + len(symbol_list) * 16
    export_trie_offset = align(strings_offset + len(strings), 8)
    dyld_info_export_trie_offset = align(
        export_trie_offset + len(export_trie or b""), 8
    )
    file_size = strings_offset + len(strings)
    if export_trie:
        file_size = export_trie_offset + len(export_trie)
    if dyld_info_export_trie:
        file_size = (
            dyld_info_export_trie_offset + len(dyld_info_export_trie)
        )
    command_export_offset = (
        export_trie_offset
        if export_trie
        else 0
    )
    if export_trie_offset_override is not None:
        command_export_offset = export_trie_offset_override
    command_export_size = (
        0 if export_trie is None else len(export_trie)
    )
    if export_trie_size_override is not None:
        command_export_size = export_trie_size_override
    data = bytearray(file_size)

    command_count = 2 + len(extra_segments) + len(id_dylib_commands)
    if export_trie is not None:
        command_count += 1
    if dyld_info_export_trie is not None:
        command_count += 1
    struct.pack_into(
        "<IiiIIIII",
        data,
        0,
        0xFEEDFACF,
        cpu_type,
        0,
        file_type,
        command_count,
        commands_size,
        0x80,
        0,
    )
    struct.pack_into(
        "<II16sQQQQiiII",
        data,
        32,
        0x19,
        segment_size,
        b"__TEXT\0" + b"\0" * 9,
        0,
        file_size,
        0,
        file_size,
        7,
        segment_initial_protection,
        section_count,
        0,
    )
    struct.pack_into(
        "<16s16sQQIIIIIIII",
        data,
        32 + 72,
        b"__text\0" + b"\0" * 9,
        b"__TEXT\0" + b"\0" * 9,
        0x1000,
        1,
        code_offset,
        0,
        0,
        0,
        section_flags,
        0,
        0,
        0,
    )
    command_offset = 32 + segment_size
    for extra_index, (extra_count, extra_size) in enumerate(
        zip(extra_segments, extra_segment_sizes)
    ):
        struct.pack_into(
            "<II16sQQQQiiII",
            data,
            command_offset,
            0x19,
            extra_size,
            f"__EXTRA{extra_index}\0".encode().ljust(16, b"\0"),
            0,
            0,
            0,
            0,
            7,
            segment_initial_protection,
            extra_count,
            0,
        )
        command_offset += extra_size
    for command in id_dylib_commands:
        data[command_offset : command_offset + len(command)] = command
        command_offset += len(command)
    struct.pack_into(
        "<IIIIII",
        data,
        command_offset,
        0x02,
        24,
        symbols_offset,
        len(symbol_list),
        strings_offset,
        len(strings),
    )
    command_offset += 24
    if export_trie is not None:
        if export_trie_command == "exports_trie":
            struct.pack_into(
                "<IIII",
                data,
                command_offset,
                0x80000033,
                16,
                command_export_offset,
                command_export_size,
            )
        else:
            command = (
                0x22
                if export_trie_command == "dyld_info"
                else 0x80000022
            )
            struct.pack_into(
                "<12I",
                data,
                command_offset,
                command,
                48,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                command_export_offset,
                command_export_size,
            )
        command_offset += export_command_size
    if dyld_info_export_trie is not None:
        dyld_info_export_offset = (
            dyld_info_export_trie_offset
            if dyld_info_export_trie
            else 0
        )
        struct.pack_into(
            "<12I",
            data,
            command_offset,
            0x80000022,
            48,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            dyld_info_export_offset,
            len(dyld_info_export_trie),
        )
    data[code_offset] = 0xC3
    for index, symbol in enumerate(symbol_list):
        struct.pack_into(
            "<IBBHQ",
            data,
            symbols_offset + index * 16,
            name_offsets[symbol],
            symbol_type,
            1,
            0,
            symbol_value,
        )
    data[strings_offset : strings_offset + len(strings)] = strings
    if export_trie:
        data[
            export_trie_offset : export_trie_offset + len(export_trie)
        ] = export_trie
    if dyld_info_export_trie:
        data[
            dyld_info_export_trie_offset :
            dyld_info_export_trie_offset + len(dyld_info_export_trie)
        ] = dyld_info_export_trie
    return bytes(data)


def build_fat_macho(slices, magic=0xCAFEBABE):
    entry_size = 20
    table_end = 8 + len(slices) * entry_size
    offsets = []
    offset = align(table_end, 0x1000)
    for cpu_type, image in slices:
        offsets.append(offset)
        offset = align(offset + len(image), 0x1000)
    data = bytearray(offset)
    struct.pack_into(">II", data, 0, magic, len(slices))
    for index, ((cpu_type, image), slice_offset) in enumerate(
        zip(slices, offsets)
    ):
        struct.pack_into(
            ">IIIII",
            data,
            8 + index * entry_size,
            cpu_type,
            0,
            slice_offset,
            len(image),
            12,
        )
        data[slice_offset : slice_offset + len(image)] = image
    return bytes(data)


@pytest.mark.parametrize(
    "target,path,data,symbol_family",
    (
        (
            "x86_64-unknown-linux-gnu",
            "native/linux/x86_64/libpaimon_mosaic_jni.so",
            build_elf(machine=62, symbols=JNI_SYMBOLS),
            "JNI",
        ),
        (
            "aarch64-unknown-linux-gnu",
            "mosaic/libpaimon_mosaic_ffi.so",
            build_elf(machine=183, symbols=FFI_SYMBOLS),
            "FFI",
        ),
        (
            "aarch64-apple-darwin",
            "native/macos/aarch64/libpaimon_mosaic_jni.dylib",
            build_macho(symbols=JNI_SYMBOLS),
            "JNI",
        ),
        (
            "x86_64-pc-windows-msvc",
            "mosaic/paimon_mosaic_ffi.dll",
            build_pe(symbols=FFI_SYMBOLS),
            "FFI",
        ),
    ),
)
def test_verify_native_target_accepts_four_release_targets(
    target, path, data, symbol_family
):
    verifier.verify_native_target(
        data,
        target,
        path,
        symbol_family=symbol_family,
    )


@pytest.mark.parametrize(
    "target,path,data,symbol_family,expected_format",
    (
        (
            "x86_64-unknown-linux-gnu",
            "native/linux/x86_64/libpaimon_mosaic_jni.so",
            build_pe(symbols=JNI_SYMBOLS),
            "JNI",
            "ELF",
        ),
        (
            "aarch64-unknown-linux-gnu",
            "mosaic/libpaimon_mosaic_ffi.so",
            build_macho(symbols=FFI_SYMBOLS),
            "FFI",
            "ELF",
        ),
        (
            "aarch64-apple-darwin",
            "native/macos/aarch64/libpaimon_mosaic_jni.dylib",
            build_elf(machine=183, symbols=JNI_SYMBOLS),
            "JNI",
            "Mach-O",
        ),
        (
            "x86_64-pc-windows-msvc",
            "mosaic/paimon_mosaic_ffi.dll",
            build_elf(machine=62, symbols=FFI_SYMBOLS),
            "FFI",
            "PE",
        ),
    ),
)
def test_verify_native_target_rejects_wrong_format_with_matching_arch_and_symbols(
    target, path, data, symbol_family, expected_format
):
    # The binary carries the right architecture and the right Mosaic symbols
    # for the target; only the container format is wrong. If the format check
    # is dropped, the architecture and symbol checks both pass and the call
    # would wrongly succeed, so this is what pins that check in place.
    with pytest.raises(ValueError, match=f"expected {expected_format}"):
        verifier.verify_native_target(
            data,
            target,
            path,
            symbol_family=symbol_family,
        )


def test_verify_native_target_uses_explicit_symbol_family_for_renamed_library():
    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verifier.verify_native_target(
            build_elf(machine=62, symbols={"unrelated_export"}),
            "x86_64-unknown-linux-gnu",
            "renamed-native-library.so",
            symbol_family="JNI",
        )


def test_verify_native_target_requires_keyword_only_symbol_family():
    arguments = (
        build_elf(machine=62, symbols=JNI_SYMBOLS),
        "x86_64-unknown-linux-gnu",
        "libpaimon_mosaic_jni.so",
    )

    with pytest.raises(TypeError):
        verifier.verify_native_target(*arguments)
    with pytest.raises(TypeError):
        verifier.verify_native_target(*arguments, "JNI")


@pytest.mark.parametrize(
    "data,error",
    (
        (b"\x7fELF", "truncated ELF header"),
        (build_elf(file_type=2), "not ET_DYN"),
        (build_elf(interpreter=True), "PT_INTERP"),
    ),
)
def test_elf_rejects_truncated_and_executable_images(data, error):
    with pytest.raises(ValueError, match=error):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_rejects_header_only_image():
    data = bytearray(64)
    data[:16] = b"\x7fELF\x02\x01\x01" + b"\0" * 9
    struct.pack_into(
        "<HHIQQQIHHHHHH",
        data,
        16,
        3,
        62,
        1,
        0,
        64,
        0,
        0,
        64,
        56,
        0,
        64,
        0,
        0,
    )

    with pytest.raises(ValueError, match="program header count"):
        verify_jni_target(
            bytes(data),
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_rejects_dynsym_not_referenced_by_pt_dynamic():
    data = build_elf(loader_symbols=False)

    with pytest.raises(ValueError, match="DT_SYMTAB"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_rejects_dynsym_entries_beyond_dt_hash_symbol_count():
    data = build_elf(hash_symbol_count=1)

    with pytest.raises(ValueError, match="DT_HASH.*symbol count"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_rejects_an_unbounded_dynamic_symbol_table(monkeypatch):
    # Keep a hard cap as defense in depth even though hash membership is O(1).
    monkeypatch.setattr(verifier, "MAX_DYNAMIC_SYMBOLS", 2)
    data = build_elf()

    with pytest.raises(ValueError, match="more than 2 symbols"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


@pytest.mark.parametrize(
    ("format_name", "data"),
    (
        ("PE", build_pe()),
        ("Mach-O", build_macho()),
    ),
    ids=("pe", "macho"),
)
def test_pe_and_macho_reject_unbounded_symbol_tables(
    monkeypatch, format_name, data
):
    monkeypatch.setattr(verifier, "MAX_DYNAMIC_SYMBOLS", 2)

    with pytest.raises(
        ValueError, match=rf"{format_name}.*more than 2 symbols"
    ):
        verifier.native_binary(data)


@pytest.mark.parametrize(
    ("format_name", "data"),
    (
        ("ELF", build_elf()),
        ("PE", build_pe()),
        ("Mach-O", build_macho()),
    ),
    ids=("elf", "pe", "macho"),
)
def test_native_symbol_name_scans_obey_a_common_budget(
    monkeypatch, format_name, data
):
    # Every fixture name fits individually, but their cumulative scan does not.
    # This kills implementations that cap one name without charging the loop.
    monkeypatch.setattr(verifier, "MAX_SYMBOL_STRING_BYTES", 100)

    with pytest.raises(
        ValueError,
        match=rf"{format_name}.*symbol names.*string scan budget",
    ):
        verifier.native_binary(data)


def test_elf_does_not_accept_exports_unreachable_from_dt_hash():
    data = build_elf(hash_reachable=False)

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_accepts_exports_reachable_from_dt_gnu_hash():
    verify_jni_target(
        build_elf(hash_style="gnu"),
        "x86_64-unknown-linux-gnu",
        "libpaimon_mosaic_jni.so",
    )


def test_elf_accepts_gnu_hash_with_only_unhashed_dynamic_symbols():
    parsed = verifier.native_binary(
        build_elf(symbols=set(), hash_style="gnu", unhashed_symbols=4)
    )

    assert parsed.binary_format == "ELF"
    assert parsed.exported_symbols == frozenset()


@pytest.mark.parametrize(
    ("name", "sysv_hash", "gnu_hash"),
    (
        (b"printf", 0x077905A6, 0x156B2BB8),
        (b"mosaic_writer_open", 0x0C6E4E1E, 0xFD2D0CCE),
        (
            b"Java_org_apache_paimon_mosaic_NativeLib_nativeOpen",
            0x064C35DE,
            0x440D1BC2,
        ),
    ),
)
def test_elf_hash_functions_match_known_vectors(name, sysv_hash, gnu_hash):
    assert verifier.elf_sysv_hash(name) == sysv_hash
    assert verifier.elf_gnu_hash(name) == gnu_hash


@pytest.mark.parametrize(
    ("parser", "section_type", "header_format", "header", "table_size"),
    (
        (
            verifier.parse_elf_sysv_hash,
            5,
            "<II",
            (32, 64),
            8 + (32 + 64) * 4,
        ),
        (
            verifier.parse_elf_gnu_hash,
            0x6FFFFFF6,
            "<IIII",
            (32, 1, 1, 5),
            16 + 8 + 32 * 4 + 64 * 4,
        ),
    ),
    ids=("sysv", "gnu"),
)
def test_elf_hash_validates_shared_chain_once(
    monkeypatch, parser, section_type, header_format, header, table_size
):
    bucket_count = 32
    symbol_count = 64
    # A symbol's hash selects exactly one bucket, so only one bucket may head the
    # chain. The walk is still 63 nodes deep, which is what the read count below
    # measures.
    buckets = (1,) + (0,) * (bucket_count - 1)

    class CountingChains:
        def __init__(self):
            if parser is verifier.parse_elf_gnu_hash:
                self.values = (0,) * (symbol_count - 1) + (1,)
            else:
                self.values = (
                    0,
                    *range(2, symbol_count),
                    0,
                )
            self.reads = 0

        def __iter__(self):
            return iter(self.values)

        def __getitem__(self, index):
            self.reads += 1
            return self.values[index]

    chains = CountingChains()

    def unpack_from(format_string, _data, _offset):
        if format_string == header_format:
            return header
        if format_string == "<1Q":
            return (0xFFFFFFFFFFFFFFFF,)
        if format_string == f"<{bucket_count}I":
            return buckets
        if format_string == f"<{symbol_count}I":
            return chains
        raise AssertionError(f"unexpected format: {format_string}")

    monkeypatch.setattr(verifier.struct, "unpack_from", unpack_from)
    section = verifier.ElfSection(
        section_type=section_type,
        flags=0,
        address=0,
        offset=0,
        size=table_size,
        link=0,
        entry_size=4,
    )

    parser(b"", section)

    assert chains.reads <= symbol_count * 2


def test_elf_sysv_hash_rejects_buckets_that_alias_a_chain():
    bucket_count, symbol_count = 2, 3
    # Both buckets head the chain at index 1. A symbol's hash selects exactly one
    # bucket, and contains() compares only indices while walking, so an aliased
    # chain would let a symbol resolve under a name that hashes elsewhere.
    buckets = (1, 1)
    chains = (0, 2, 0)
    table = struct.pack("<II", bucket_count, symbol_count)
    table += struct.pack(f"<{bucket_count}I", *buckets)
    table += struct.pack(f"<{symbol_count}I", *chains)
    section = verifier.ElfSection(
        section_type=5,
        flags=0,
        address=0,
        offset=0,
        size=len(table),
        link=0,
        entry_size=4,
    )

    with pytest.raises(ValueError, match="bucket chains alias"):
        verifier.parse_elf_sysv_hash(table, section)


# The same three guards exist in parse_elf_gnu_hash, where commit 964df48 added
# them without coverage: all three could be neutered with the suite still green.


def gnu_hash_section(table):
    return verifier.ElfSection(
        section_type=0x6FFFFFF6,
        flags=0,
        address=0,
        offset=0,
        size=len(table),
        link=0,
        entry_size=4,
    )


def build_gnu_hash_table(symbol_offset, buckets, chains):
    table = struct.pack("<IIII", len(buckets), symbol_offset, 1, 0)
    table += struct.pack("<Q", 0)
    table += struct.pack(f"<{len(buckets)}I", *buckets)
    table += struct.pack(f"<{len(chains)}I", *chains)
    return table


def test_elf_gnu_hash_rejects_buckets_that_alias_a_chain():
    # Both buckets head the chain entry at index 1; contains() compares only
    # indices, so an aliased chain would resolve a symbol under a foreign bucket.
    table = build_gnu_hash_table(0, (1, 1), (0, 1))

    with pytest.raises(ValueError, match="bucket chains alias"):
        verifier.parse_elf_gnu_hash(table, gnu_hash_section(table))


def test_elf_gnu_hash_rejects_a_bucket_preceding_the_symbol_offset():
    # chain_index = bucket - symbol_offset, so a bucket below symbol_offset
    # would index the chain table from before its start.
    table = build_gnu_hash_table(2, (1,), (1,))

    with pytest.raises(ValueError, match="bucket precedes the symbol offset"):
        verifier.parse_elf_gnu_hash(table, gnu_hash_section(table))


def test_elf_gnu_hash_rejects_a_chain_that_is_not_terminated():
    # No chain entry sets the terminator bit, so the walk runs off the table.
    # Turning this raise into a break is a fail-closed to fail-open change.
    table = build_gnu_hash_table(0, (1,), (0, 0))

    with pytest.raises(ValueError, match="chain is not terminated"):
        verifier.parse_elf_gnu_hash(table, gnu_hash_section(table))


# The loader hash tables declare their own sizes, and the checks that reconcile
# them with the already-capped SHT_DYNSYM size run only after the tables are
# materialized. Every table below would parse successfully without the cap, so
# deleting a cap makes the over-limit test fail rather than merely change the
# message, and each at-limit sibling forbids flipping the comparison.


def sysv_hash_section(table):
    return verifier.ElfSection(
        section_type=5,
        flags=0,
        address=0,
        offset=0,
        size=len(table),
        link=0,
        entry_size=4,
    )


def build_sysv_hash_table(buckets, chains):
    table = struct.pack("<II", len(buckets), len(chains))
    table += struct.pack(f"<{len(buckets)}I", *buckets)
    table += struct.pack(f"<{len(chains)}I", *chains)
    return table


def build_gnu_hash_table_with_bloom(bloom_count, buckets, chains):
    table = struct.pack("<IIII", len(buckets), 0, bloom_count, 0)
    table += struct.pack(f"<{bloom_count}Q", *([0] * bloom_count))
    table += struct.pack(f"<{len(buckets)}I", *buckets)
    table += struct.pack(f"<{len(chains)}I", *chains)
    return table


def test_elf_sysv_hash_rejects_counts_above_the_symbol_cap(monkeypatch):
    monkeypatch.setattr(verifier, "MAX_DYNAMIC_SYMBOLS", 4)
    table = build_sysv_hash_table((1, 2), (0, 0, 3, 0, 0))

    with pytest.raises(ValueError, match="DT_HASH declares more than 4"):
        verifier.parse_elf_sysv_hash(table, sysv_hash_section(table))


def test_elf_sysv_hash_accepts_counts_at_the_symbol_cap(monkeypatch):
    monkeypatch.setattr(verifier, "MAX_DYNAMIC_SYMBOLS", 5)
    table = build_sysv_hash_table((1, 2), (0, 0, 3, 0, 0))

    parsed = verifier.parse_elf_sysv_hash(table, sysv_hash_section(table))

    assert parsed.chains == (0, 0, 3, 0, 0)


def test_elf_gnu_hash_rejects_a_bucket_count_above_the_symbol_cap(monkeypatch):
    monkeypatch.setattr(verifier, "MAX_DYNAMIC_SYMBOLS", 4)
    table = build_gnu_hash_table(0, (0, 0, 0, 0, 0), (1,))

    with pytest.raises(ValueError, match="DT_GNU_HASH declares more than 4"):
        verifier.parse_elf_gnu_hash(table, gnu_hash_section(table))


def test_elf_gnu_hash_rejects_a_bloom_count_above_the_symbol_cap(monkeypatch):
    monkeypatch.setattr(verifier, "MAX_DYNAMIC_SYMBOLS", 4)
    table = build_gnu_hash_table_with_bloom(5, (0,), (1,))

    with pytest.raises(ValueError, match="DT_GNU_HASH declares more than 4"):
        verifier.parse_elf_gnu_hash(table, gnu_hash_section(table))


def test_elf_gnu_hash_rejects_a_chain_count_above_the_symbol_cap(monkeypatch):
    monkeypatch.setattr(verifier, "MAX_DYNAMIC_SYMBOLS", 4)
    table = build_gnu_hash_table(0, (0,), (0, 0, 0, 0, 1))

    with pytest.raises(ValueError, match="chain entries: 5"):
        verifier.parse_elf_gnu_hash(table, gnu_hash_section(table))


def test_elf_gnu_hash_accepts_counts_at_the_symbol_cap(monkeypatch):
    monkeypatch.setattr(verifier, "MAX_DYNAMIC_SYMBOLS", 5)
    table = build_gnu_hash_table_with_bloom(5, (0, 0, 0, 0, 0), (0, 0, 0, 0, 1))

    parsed = verifier.parse_elf_gnu_hash(table, gnu_hash_section(table))

    assert parsed.chains == (0, 0, 0, 0, 1)


@pytest.mark.parametrize("hash_style", ("sysv", "gnu"))
def test_elf_hash_membership_accepts_each_reachable_symbol(hash_style):
    parsed = verifier.native_binary(build_elf(hash_style=hash_style))

    assert parsed.exported_symbols == JNI_SYMBOLS


def test_elf_sysv_hash_membership_does_not_walk_the_chain():
    class UnreadableChains:
        def __getitem__(self, _index):
            raise AssertionError("SysV membership walked the hash chain")

    table = verifier.ElfSysvHash(
        buckets=(1,),
        chains=UnreadableChains(),
        owners=(-1, 0),
    )

    assert table.contains(1, b"mosaic_writer_open")


def test_elf_gnu_hash_membership_reads_only_the_requested_chain_entry():
    name = b"mosaic_writer_open"
    name_hash = verifier.elf_gnu_hash(name)

    class CountingChains:
        def __init__(self):
            self.reads = 0

        def __len__(self):
            return 1

        def __getitem__(self, index):
            assert index == 0
            self.reads += 1
            return name_hash | 1

    chains = CountingChains()
    table = verifier.ElfGnuHash(
        symbol_offset=1,
        bloom_shift=5,
        bloom=(
            (1 << (name_hash % 64))
            | (1 << ((name_hash >> 5) % 64)),
        ),
        buckets=(1,),
        chains=chains,
        owners=(0,),
        symbol_count=2,
    )

    assert table.contains(1, name)
    assert chains.reads == 1


def test_elf_does_not_accept_exports_unreachable_from_dt_gnu_hash():
    data = build_elf(hash_style="gnu", hash_reachable=False)

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_requires_exports_reachable_from_each_loader_hash():
    data = build_elf(
        hash_style="both",
        hash_reachable=True,
        gnu_hash_reachable=False,
    )

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_does_not_accept_object_symbols_as_function_exports():
    data = build_elf(symbol_info=0x11)

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_rejects_function_export_outside_load_segments():
    data = build_elf(symbol_value=0xFFFFFFFF)

    with pytest.raises(ValueError, match="function.*not mapped"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


def test_elf_rejects_function_export_in_non_executable_segment():
    data = build_elf(load_flags=4)

    with pytest.raises(ValueError, match="function.*not mapped"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


@pytest.mark.parametrize(
    "data,error",
    (
        (b"MZ", "truncated DOS header"),
        (build_pe(dll=False), "DLL characteristic"),
        (build_pe(optional_magic=0x10B), "not PE32\\+"),
    ),
)
def test_pe_rejects_truncated_executable_and_pe32_images(data, error):
    with pytest.raises(ValueError, match=error):
        verify_jni_target(
            data,
            "x86_64-pc-windows-msvc",
            "paimon_mosaic_jni.dll",
        )


def test_pe_rejects_named_export_with_unmapped_function_rva():
    data = build_pe(function_rva=0xFFFFFFFF)

    with pytest.raises(ValueError, match="function RVA.*not mapped"):
        verify_jni_target(
            data,
            "x86_64-pc-windows-msvc",
            "paimon_mosaic_jni.dll",
        )


def test_pe_rejects_named_export_in_non_executable_section():
    data = build_pe(section_characteristics=0x40000040)

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "x86_64-pc-windows-msvc",
            "paimon_mosaic_jni.dll",
        )


def test_pe_rejects_named_forwarded_export():
    data = build_pe(forwarder="other_module.mosaic_writer_open")

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "x86_64-pc-windows-msvc",
            "paimon_mosaic_jni.dll",
        )


def test_pe_accepts_required_functions_with_unrelated_data_export():
    unrelated = "unrelated_data"
    # Inside non-executable .rdata (0x1000-0x5000) but past the export directory,
    # which ends at 0x178d; an RVA inside that directory is a forwarder instead.
    data = build_pe(
        symbols=JNI_SYMBOLS | {unrelated},
        symbol_rvas={unrelated: 0x2000},
    )

    verify_jni_target(
        data,
        "x86_64-pc-windows-msvc",
        "paimon_mosaic_jni.dll",
    )
    # verify_jni_target only fails on missing symbols, so assert the data export
    # was actually filtered rather than merely tolerated.
    assert verifier.native_binary(data).exported_symbols == frozenset(JNI_SYMBOLS)


def test_pe_accepts_required_functions_with_unrelated_forwarder():
    unrelated = "unrelated_forwarder"
    data = build_pe(
        symbols=JNI_SYMBOLS | {unrelated},
        symbol_forwarders={unrelated: "KERNEL32.Sleep"},
    )

    verify_jni_target(
        data,
        "x86_64-pc-windows-msvc",
        "paimon_mosaic_jni.dll",
    )
    # A forwarder resolves elsewhere, so it must not appear as a local export.
    assert verifier.native_binary(data).exported_symbols == frozenset(JNI_SYMBOLS)


def test_pe_forwarder_shares_export_name_string_scan_budget(monkeypatch):
    unrelated = "zz_unrelated_forwarder"
    forwarder = "KERNEL32.Sleep"
    symbols = JNI_SYMBOLS | {unrelated}
    ordinary_name_bytes = sum(len(symbol.encode()) + 1 for symbol in symbols)
    forwarder_bytes = len(forwarder.encode()) + 1
    monkeypatch.setattr(
        verifier,
        "MAX_SYMBOL_STRING_BYTES",
        ordinary_name_bytes + forwarder_bytes - 1,
    )
    data = build_pe(
        symbols=symbols,
        symbol_forwarders={unrelated: forwarder},
    )

    with pytest.raises(ValueError, match="PE export symbol names.*budget"):
        verifier.native_binary(data)


def test_pe_forwarder_accepts_exact_string_scan_budget(monkeypatch):
    unrelated = "zz_unrelated_forwarder"
    forwarder = "KERNEL32.Sleep"
    symbols = JNI_SYMBOLS | {unrelated}
    exact_budget = sum(len(symbol.encode()) + 1 for symbol in symbols)
    exact_budget += len(forwarder.encode()) + 1
    monkeypatch.setattr(verifier, "MAX_SYMBOL_STRING_BYTES", exact_budget)
    data = build_pe(
        symbols=symbols,
        symbol_forwarders={unrelated: forwarder},
    )

    assert verifier.native_binary(data).exported_symbols == frozenset(JNI_SYMBOLS)


def test_pe_rejects_unsorted_export_name_pointer_table():
    data = build_pe(
        export_name_order=list(reversed(sorted(JNI_SYMBOLS)))
    )

    with pytest.raises(ValueError, match="strictly increasing"):
        verifier.native_binary(data)


def test_pe_rejects_duplicate_export_name_pointer():
    export_names = sorted(JNI_SYMBOLS)
    export_names[1] = export_names[0]
    data = build_pe(export_name_order=export_names)

    with pytest.raises(ValueError, match="strictly increasing"):
        verifier.native_binary(data)


@pytest.mark.parametrize(
    "target,path,data",
    (
        (
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
            build_elf(symbols={f"_{symbol}" for symbol in JNI_SYMBOLS}),
        ),
        (
            "x86_64-pc-windows-msvc",
            "paimon_mosaic_jni.dll",
            build_pe(symbols={f"_{symbol}" for symbol in JNI_SYMBOLS}),
        ),
    ),
)
def test_non_macho_targets_do_not_strip_leading_underscores(
    target, path, data
):
    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(data, target, path)


@pytest.mark.parametrize(
    "data,error",
    (
        (b"\xcf\xfa\xed\xfe", "truncated Mach-O header"),
        (build_macho(file_type=2), "not MH_DYLIB"),
    ),
)
def test_macho_rejects_truncated_and_executable_images(data, error):
    with pytest.raises(ValueError, match=error):
        verify_jni_target(
            data,
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


def test_macho_rejects_truncated_load_commands():
    data = bytearray(build_macho())
    struct.pack_into("<I", data, 32 + 4, len(data))

    with pytest.raises(ValueError, match="load command"):
        verify_jni_target(
            bytes(data),
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


def test_macho_rejects_missing_id_dylib():
    with pytest.raises(ValueError, match="missing LC_ID_DYLIB"):
        verifier.native_binary(build_macho(id_dylib="missing"))


def test_macho_rejects_duplicate_id_dylib():
    with pytest.raises(ValueError, match="multiple LC_ID_DYLIB"):
        verifier.native_binary(build_macho(id_dylib="duplicate"))


@pytest.mark.parametrize(
    "mode,error",
    (
        ("invalid_cmdsize", "LC_ID_DYLIB.*invalid size"),
        ("invalid_name_offset", "LC_ID_DYLIB.*name offset"),
        ("out_of_bounds_name_offset", "LC_ID_DYLIB.*name offset"),
        ("unterminated_name", "LC_ID_DYLIB.*not null-terminated"),
        ("empty_name", "LC_ID_DYLIB.*empty name"),
    ),
)
def test_macho_rejects_malformed_id_dylib(mode, error):
    with pytest.raises(ValueError, match=error):
        verifier.native_binary(build_macho(id_dylib=mode))


@pytest.mark.parametrize(
    "export_trie_command",
    ("dyld_info", "dyld_info_only", "exports_trie"),
)
def test_macho_reads_exports_from_loader_export_trie(
    export_trie_command,
):
    data = build_macho(
        symbols={"unrelated_export"},
        export_trie_symbols=JNI_SYMBOLS,
        export_trie_command=export_trie_command,
    )

    verify_jni_target(
        data,
        "aarch64-apple-darwin",
        "libpaimon_mosaic_jni.dylib",
    )


def test_macho_export_trie_accumulates_multilevel_prefixes():
    # root -> "_mosaic_" -> {"open", "free"}
    root = b"\x00\x01_mosaic_\x00\x0c"
    shared_prefix = b"\x00\x02open\x00\x1afree\x00\x1f"
    export_leaf = b"\x03\x00\x80\x20\x00"
    trie = root + shared_prefix + export_leaf + export_leaf

    assert {
        export.name
        for export in verifier.parse_macho_export_trie(trie, 0, len(trie))
    } == {"_mosaic_open", "_mosaic_free"}


def test_macho_export_trie_rejects_node_visits_over_budget(monkeypatch):
    monkeypatch.setattr(
        verifier, "MAX_MACHO_EXPORT_TRIE_NODES", 1, raising=False
    )
    trie = b"\x00\x01a\x00\x05\x00\x00"

    with pytest.raises(ValueError, match="node visits.*budget"):
        verifier.parse_macho_export_trie(trie, 0, len(trie))


def test_macho_export_trie_accepts_node_visits_at_exact_budget(monkeypatch):
    monkeypatch.setattr(
        verifier, "MAX_MACHO_EXPORT_TRIE_NODES", 2, raising=False
    )
    trie = b"\x00\x01a\x00\x05\x00\x00"

    assert verifier.parse_macho_export_trie(trie, 0, len(trie)) == ()


def test_macho_export_trie_shares_edge_and_reexport_byte_budget(monkeypatch):
    monkeypatch.setattr(
        verifier, "MAX_MACHO_EXPORT_TRIE_STRING_BYTES", 3, raising=False
    )
    trie = b"\x00\x01a\x00\x05\x04\x08\x01b\x00\x00"

    with pytest.raises(ValueError, match="edge and re-export names.*budget"):
        verifier.parse_macho_export_trie(trie, 0, len(trie))


def test_macho_export_trie_accepts_strings_at_exact_budget(monkeypatch):
    monkeypatch.setattr(
        verifier, "MAX_MACHO_EXPORT_TRIE_STRING_BYTES", 4, raising=False
    )
    trie = b"\x00\x01a\x00\x05\x04\x08\x01b\x00\x00"

    assert verifier.parse_macho_export_trie(trie, 0, len(trie)) == (
        verifier.MachoExport("a", 0x08, None, None),
    )


def test_macho_export_trie_rejects_prefix_work_over_budget(monkeypatch):
    monkeypatch.setattr(
        verifier, "MAX_MACHO_EXPORT_TRIE_PREFIX_BYTES", 9, raising=False
    )
    trie = (
        b"\x00\x01a\x00\x05"
        b"\x00\x01b\x00\x0a"
        b"\x00\x01c\x00\x0f"
        b"\x00\x01d\x00\x14"
        b"\x00\x00"
    )

    with pytest.raises(ValueError, match="prefix construction.*budget"):
        verifier.parse_macho_export_trie(trie, 0, len(trie))


def test_macho_export_trie_accepts_prefix_work_at_exact_budget(monkeypatch):
    monkeypatch.setattr(
        verifier, "MAX_MACHO_EXPORT_TRIE_PREFIX_BYTES", 10, raising=False
    )
    trie = (
        b"\x00\x01a\x00\x05"
        b"\x00\x01b\x00\x0a"
        b"\x00\x01c\x00\x0f"
        b"\x00\x01d\x00\x14"
        b"\x00\x00"
    )

    assert verifier.parse_macho_export_trie(trie, 0, len(trie)) == ()


def test_macho_empty_export_trie_is_authoritative():
    data = build_macho(export_trie_symbols=set())

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


def test_macho_zero_sized_dyld_info_only_export_trie_is_authoritative():
    data = build_macho(
        export_trie_data=b"",
        export_trie_command="dyld_info_only",
    )

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


def test_macho_exports_trie_command_precedes_zero_sized_dyld_info():
    data = build_macho(
        symbols={"unrelated_export"},
        export_trie_symbols=JNI_SYMBOLS,
        export_trie_command="exports_trie",
        dyld_info_export_trie_data=b"",
    )

    verify_jni_target(
        data,
        "aarch64-apple-darwin",
        "libpaimon_mosaic_jni.dylib",
    )


def test_macho_exports_trie_command_precedes_conflicting_dyld_info():
    missing_symbol = min(JNI_SYMBOLS)
    data = build_macho(
        symbols={"unrelated_export"},
        export_trie_symbols=JNI_SYMBOLS - {missing_symbol},
        export_trie_command="exports_trie",
        dyld_info_export_trie_symbols=JNI_SYMBOLS,
    )

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


def test_macho_does_not_fill_export_trie_gaps_from_symtab():
    missing_symbol = min(JNI_SYMBOLS)
    data = build_macho(
        export_trie_symbols=JNI_SYMBOLS - {missing_symbol}
    )

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


@pytest.mark.parametrize(
    "options",
    (
        {
            "export_trie_flags": 0x08,
            "export_trie_reexport_name": b"",
        },
        {"export_trie_flags": 0x02},
        {"export_trie_flags": 0x01},
        {"export_trie_address": 0x2000},
        {"section_flags": 0},
        {"section_flags": 0x80000401},
        {"segment_initial_protection": 3},
    ),
    ids=(
        "re-export",
        "absolute",
        "thread-local",
        "non-executable-address",
        "non-instruction-section",
        "zero-fill-instruction-section",
        "non-executable-segment",
    ),
)
def test_macho_export_trie_only_accepts_direct_executable_functions(options):
    data = build_macho(export_trie_symbols=JNI_SYMBOLS, **options)

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


@pytest.mark.parametrize(
    "export_trie,error",
    (
        (b"\x7f", "terminal.*out of bounds"),
        (b"\x00", "child count.*out of bounds"),
        (b"\x00\x01edge", "edge.*not null-terminated"),
        (b"\x00\x01edge\0\x80", "ULEB128.*truncated"),
        (b"\x00\x01edge\0\x7f", "child offset.*out of bounds"),
        (b"\x00\x01edge\0\x00", "cycle"),
        (b"\x80" * 10 + b"\x02", "ULEB128.*overflow"),
        (b"\x01\x00\x00", "terminal address.*truncated"),
    ),
)
def test_macho_rejects_malformed_export_trie(export_trie, error):
    data = build_macho(export_trie_data=export_trie)

    with pytest.raises(ValueError, match=error):
        verifier.native_binary(data)


@pytest.mark.parametrize(
    "offset_override,size_override",
    (
        (0xFFFFFFFF, None),
        (None, 0xFFFFFFFF),
    ),
)
def test_macho_rejects_out_of_bounds_export_trie_metadata(
    offset_override, size_override
):
    data = build_macho(
        export_trie_data=b"\0\0",
        export_trie_offset_override=offset_override,
        export_trie_size_override=size_override,
    )

    with pytest.raises(ValueError, match="export trie.*out of bounds"):
        verifier.native_binary(data)


@pytest.mark.parametrize(
    "options",
    (
        {"symbol_type": 0x0E},
        {"symbol_type": 0x1F},
        {"symbol_type": 0x0B},
        {"symbol_type": 0x0D},
        {"symbol_type": 0x03},
        {"section_flags": 0},
        {"section_flags": 0x80000401},
        {"segment_initial_protection": 3},
        {"symbol_value": 0x2000},
    ),
    ids=(
        "local",
        "private-extern",
        "indirect",
        "prebound-undefined",
        "absolute",
        "non-instruction-section",
        "zero-fill-instruction-section",
        "non-executable-segment",
        "outside-section",
    ),
)
def test_macho_symtab_only_accepts_symbols_the_dylib_defines(options):
    data = build_macho(**options)

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


@pytest.mark.parametrize(
    "magic",
    (0xCAFEBABE, 0xBEBAFECA, 0xCAFEBABF, 0xBFBAFECA),
)
def test_rejects_macho_universal_binaries(magic):
    # The release builds the single Mach-O target thin, so a universal image is
    # not a release artifact shape; it is refused before any slice is parsed.
    data = build_fat_macho(
        ((0x0100000C, build_macho(cpu_type=0x0100000C)),), magic=magic
    )

    with pytest.raises(ValueError, match="universal binaries are not"):
        verify_jni_target(
            data,
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


@pytest.mark.parametrize(
    "target,path,data,symbol_family",
    (
        (
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
            build_elf(symbols={"unrelated_export"}),
            "JNI",
        ),
        (
            "x86_64-pc-windows-msvc",
            "paimon_mosaic_ffi.dll",
            build_pe(symbols={"unrelated_export"}),
            "FFI",
        ),
        (
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
            build_macho(symbols={"unrelated_export"}),
            "JNI",
        ),
    ),
)
def test_rejects_binary_without_expected_mosaic_exports(
    target, path, data, symbol_family
):
    with pytest.raises(ValueError, match="missing expected Mosaic"):
        verifier.verify_native_target(
            data,
            target,
            path,
            symbol_family=symbol_family,
        )


def test_raw_symbol_strings_do_not_count_as_elf_exports():
    raw_names = b"\0".join(symbol.encode() for symbol in sorted(JNI_SYMBOLS))
    data = build_elf(symbols={"unrelated_export"}) + raw_names

    with pytest.raises(ValueError, match="missing expected Mosaic JNI exports"):
        verify_jni_target(
            data,
            "x86_64-unknown-linux-gnu",
            "libpaimon_mosaic_jni.so",
        )


# Symbol counts are bounded, but every accepted symbol is range-checked against
# the section or load-segment list, so the structural counts need their own cap.


def test_rejects_elf_declaring_too_many_program_headers():
    data = bytearray(build_elf())
    struct.pack_into("<H", data, 56, verifier.MAX_NATIVE_SECTIONS + 1)

    with pytest.raises(ValueError, match=rf"more than {verifier.MAX_NATIVE_SECTIONS} program headers: "
        rf"{verifier.MAX_NATIVE_SECTIONS + 1}"):
        verify_jni_target(
            bytes(data), "x86_64-unknown-linux-gnu", "libpaimon_mosaic_jni.so"
        )


def test_accepts_elf_program_header_count_at_the_limit():
    # The cap must reject 513 and not 512; require_range then rejects this
    # image for the table it cannot actually hold.
    data = bytearray(build_elf())
    struct.pack_into("<H", data, 56, verifier.MAX_NATIVE_SECTIONS)

    with pytest.raises(ValueError, match="program header table"):
        verify_jni_target(
            bytes(data), "x86_64-unknown-linux-gnu", "libpaimon_mosaic_jni.so"
        )


def test_rejects_elf_declaring_too_many_sections():
    data = bytearray(build_elf())
    struct.pack_into("<H", data, 60, verifier.MAX_NATIVE_SECTIONS + 1)

    with pytest.raises(ValueError, match=rf"more than {verifier.MAX_NATIVE_SECTIONS} sections: "
        rf"{verifier.MAX_NATIVE_SECTIONS + 1}"):
        verify_jni_target(
            bytes(data), "x86_64-unknown-linux-gnu", "libpaimon_mosaic_jni.so"
        )


def test_rejects_pe_declaring_too_many_sections():
    data = bytearray(build_pe())
    struct.pack_into("<H", data, 0x80 + 6, verifier.MAX_NATIVE_SECTIONS + 1)

    with pytest.raises(ValueError, match=rf"more than {verifier.MAX_NATIVE_SECTIONS} sections: "
        rf"{verifier.MAX_NATIVE_SECTIONS + 1}"):
        verify_jni_target(
            bytes(data), "x86_64-pc-windows-msvc", "paimon_mosaic_jni.dll"
        )


def test_rejects_macho_declaring_too_many_load_commands():
    data = bytearray(build_macho())
    struct.pack_into("<I", data, 16, verifier.MAX_NATIVE_SECTIONS + 1)

    with pytest.raises(ValueError, match=rf"more than {verifier.MAX_NATIVE_SECTIONS} load commands: "
        rf"{verifier.MAX_NATIVE_SECTIONS + 1}"):
        verify_jni_target(
            bytes(data), "aarch64-apple-darwin", "libpaimon_mosaic_jni.dylib"
        )


def test_rejects_macho_whose_single_segment_exceeds_the_section_cap():
    # Zero-filled section headers are legal (size 0 skips the range check), so
    # one segment can inflate the list the per-export scan walks.
    with pytest.raises(ValueError, match=rf"more than {verifier.MAX_NATIVE_SECTIONS} sections"):
        verify_jni_target(
            build_macho(extra_sections=verifier.MAX_NATIVE_SECTIONS),
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


def test_rejects_macho_whose_segments_exceed_the_cap_only_in_aggregate():
    # Each segment stays under the cap on its own, so only the running total can
    # reject this; a per-command check cannot see the aggregate.
    half = verifier.MAX_NATIVE_SECTIONS // 2
    with pytest.raises(ValueError, match=rf"more than {verifier.MAX_NATIVE_SECTIONS} sections"):
        verify_jni_target(
            build_macho(extra_sections=half, extra_segments=(half, half)),
            "aarch64-apple-darwin",
            "libpaimon_mosaic_jni.dylib",
        )


def test_accepts_macho_section_count_at_the_cumulative_cap():
    half = verifier.MAX_NATIVE_SECTIONS // 2
    verify_jni_target(
        build_macho(extra_sections=half - 1, extra_segments=(half,)),
        "aarch64-apple-darwin",
        "libpaimon_mosaic_jni.dylib",
    )
