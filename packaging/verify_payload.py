"""Offline release allowlist and PE metadata checks. Never executes an application."""

import argparse
import re
import struct
from pathlib import Path
import zipfile


def payload_names(version):
    return {"微win.exe", "wechat-winh.exe", f"wechat_winh_bridge-{version}.dll",
            "README.md", "LICENSE", "Start.cmd", "Stop.cmd", "OCTICONS-LICENSE.txt"}


class PE:
    def __init__(self, path):
        self.data = Path(path).read_bytes()
        assert self.data[:2] == b"MZ", f"not PE: {path}"
        pe = self.u32(0x3C)
        assert self.data[pe:pe + 4] == b"PE\0\0", "PE signature"
        assert self.u16(pe + 4) == 0x8664, "release payload must be x64"
        count, size = self.u16(pe + 6), self.u16(pe + 20)
        optional = pe + 24
        assert self.u16(optional) == 0x20B, "release payload must be PE32+"
        self.dirs = optional + 112
        self.sections = []
        for index in range(count):
            pos = optional + size + index * 40
            virtual_size, address, raw_size, raw = struct.unpack_from("<IIII", self.data, pos + 8)
            self.sections.append((address, max(virtual_size, raw_size), raw))

    def u16(self, offset):
        return struct.unpack_from("<H", self.data, offset)[0]

    def u32(self, offset):
        return struct.unpack_from("<I", self.data, offset)[0]

    def offset(self, rva):
        for address, size, raw in self.sections:
            if address <= rva < address + size:
                return raw + rva - address
        raise ValueError(f"unmapped RVA {rva:#x}")

    def imports(self):
        imports = []
        for directory, width, name_offset in [(1, 20, 12), (13, 32, 4)]:
            rva, size = struct.unpack_from("<II", self.data, self.dirs + directory * 8)
            if not rva:
                continue
            start = self.offset(rva)
            for index in range(min(size // width, 512)):
                pos = start + index * width
                if not any(self.data[pos:pos + width]):
                    break
                name = self.offset(self.u32(pos + name_offset))
                end = self.data.index(b"\0", name, name + 512)
                imports.append(self.data[name:end].decode("ascii").lower())
        return imports

    def resource(self, kind, identity):
        rva = self.u32(self.dirs + 2 * 8)
        base = self.offset(rva)

        def entries(relative):
            pos = base + relative
            count = self.u16(pos + 12) + self.u16(pos + 14)
            assert count < 4096, "resource table too large"
            return [struct.unpack_from("<II", self.data, pos + 16 + i * 8) for i in range(count)]

        def child(relative, name):
            return next(value for key, value in entries(relative) if key == name)

        entry = child(child(0, kind) & 0x7FFFFFFF, identity)
        if entry & 0x80000000:
            entry = entries(entry & 0x7FFFFFFF)[0][1]
        pos = base + entry
        data_rva, size = struct.unpack_from("<II", self.data, pos)
        start = self.offset(data_rva)
        return self.data[start:start + size]


def check_stage(path, version, static_crt=True):
    expected = payload_names(version)
    actual = {p.name for p in path.iterdir()}
    assert actual == expected, f"payload allowlist mismatch: {actual ^ expected}"
    for name in expected:
        entry = path / name
        assert entry.is_file() and not entry.is_symlink() and entry.stat().st_size > 0, name
    for name in ("微win.exe", "wechat-winh.exe", f"wechat_winh_bridge-{version}.dll"):
        pe = PE(path / name)
        if static_crt:
            forbidden = [dll for dll in pe.imports() if re.match(
                r"^(?:vcruntime|msvcp|msvcr|concrt|ucrtbase|api-ms-win-crt|libgcc|libstdc\+\+|libwinpthread|opus\.)", dll)]
            assert not forbidden, f"{name}: unexpected dynamic runtime {forbidden}"
        if name == "微win.exe":
            assert pe.resource(14, 100) and pe.resource(14, 101), "both GUI icons required"
            manifest = pe.resource(24, 1).decode("utf-8-sig")
            assert 'level="highestAvailable"' in manifest and "PerMonitorV2" in manifest


def check_zip(path, version):
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        assert len(names) == len(set(names)), "duplicate ZIP paths"
        assert set(names) == payload_names(version), "ZIP contains unexpected files or directories"
        assert archive.testzip() is None, "ZIP CRC failure"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", type=Path)
    parser.add_argument("--zip", type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--allow-dynamic-crt", action="store_true", help="local GNU resource regression only")
    args = parser.parse_args()
    if not args.stage and not args.zip:
        parser.error("provide --stage or --zip")
    if args.stage:
        check_stage(args.stage, args.version, not args.allow_dynamic_crt)
    if args.zip:
        check_zip(args.zip, args.version)
    print("payload allowlist / PE metadata checks passed")


if __name__ == "__main__":
    main()
