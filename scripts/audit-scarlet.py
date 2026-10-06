#!/usr/bin/env python3
"""Fail closed before staging a native Scarlet Cadence executable.

The four SGFX/CLAP loader imports are supplied by /bin/scarlet-ld. No other unresolved
symbol, startup DSO, or unsupported dynamic-linker feature is admitted here.
Use after any link that allows these deliberately unresolved interpreter APIs.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import struct
import subprocess

IMPORTS = {"dlopen", "dlsym", "dlclose", "dlerror"}
ARCHES = {
    "aarch64": (183, {"R_AARCH64_RELATIVE", "R_AARCH64_ABS64", "R_AARCH64_GLOB_DAT", "R_AARCH64_JUMP_SLOT"}),
    "riscv64": (243, {"R_RISCV_RELATIVE", "R_RISCV_64", "R_RISCV_JUMP_SLOT"}),
}


def readelf(path, *args):
    tool = shutil.which("readelf") or shutil.which("llvm-readelf") or "readelf"
    return subprocess.check_output([tool, "-W", *args, str(path)], text=True)


def undefined(text):
    result = set()
    for line in text.splitlines():
        fields = line.split()
        if len(fields) >= 8 and fields[6] == "UND":
            # readelf's null symbol line has no final name and is excluded.
            if fields[4] != "GLOBAL" or fields[5] != "DEFAULT":
                raise ValueError("unexpected undefined binding/visibility: " + line)
            result.add(fields[7])
    return result


def audit(path, arch):
    data = path.read_bytes()
    machine, allowed_relocations = ARCHES[arch]
    if len(data) < 64 or data[:8] != b"\x7fELF\x02\x01\x01\x53":
        raise ValueError("expected a native Scarlet little-endian ELF64 executable")
    if struct.unpack_from("<HH", data, 16) != (3, machine):
        raise ValueError("expected a position-independent executable for " + arch)
    phoff = struct.unpack_from("<Q", data, 32)[0]
    phsize, phnum = struct.unpack_from("<HH", data, 54)
    if phsize != 56 or not phnum or phoff + phsize * phnum > len(data):
        raise ValueError("invalid ELF program headers")
    interpreters = []
    for index in range(phnum):
        kind, _, offset, _, _, size, _, _ = struct.unpack_from("<IIQQQQQQ", data, phoff + index * phsize)
        if kind == 3:  # PT_INTERP
            if offset + size > len(data):
                raise ValueError("truncated interpreter")
            interpreters.append(data[offset:offset + size])
        if kind == 7:
            raise ValueError("native ELF TLS is unsupported")
    if interpreters != [b"/bin/scarlet-ld\0"]:
        raise ValueError("expected exactly /bin/scarlet-ld as interpreter")
    dynamic = readelf(path, "--dynamic")
    dynsym = readelf(path, "--dyn-syms")
    symbols = readelf(path, "--symbols")
    relocations = readelf(path, "--relocs")
    for tag in ["NEEDED", "RPATH", "RUNPATH", "VERNEED", "VERSYM", "VERDEF", "TEXTREL", "RELR", "RELRSZ", "RELRENT"]:
        if "(" + tag + ")" in dynamic:
            raise ValueError("unsupported dynamic tag " + tag)
    if not re.search(r"\(FLAGS(?:_1)?\).*\b(?:BIND_NOW|NOW)\b", dynamic):
        raise ValueError("eager binding (-z now) is required")
    if not re.search(r"\(FLAGS_1\).*\bPIE\b", dynamic):
        raise ValueError("DF_1_PIE is required")
    if re.search(r"\b(?:TLS|IFUNC)\b", symbols):
        raise ValueError("TLS/IFUNC symbols are unsupported")
    if "Symbol table '.symtab'" not in symbols:
        raise ValueError("audit before stripping .symtab so all unresolved symbols can be checked")
    for label, text in [("dynamic", dynsym), ("all", symbols)]:
        found = undefined(text)
        if found != IMPORTS:
            raise ValueError(f"{label} unresolved imports must be exactly {sorted(IMPORTS)}, got {sorted(found)}")
    kinds = set(re.findall(r"\bR_[A-Z0-9_]+\b", relocations))
    if not kinds or not kinds.issubset(allowed_relocations):
        raise ValueError("unsupported relocation kinds: " + repr(sorted(kinds - allowed_relocations)))
    for name in IMPORTS:
        matches = [line for line in relocations.splitlines() if re.search(r"\b" + name + r"\s*\+", line)]
        if len(matches) != 1:
            raise ValueError("expected exactly one dynamic relocation for " + name)
    return {"path": str(path.resolve()), "arch": arch, "machine": machine,
            "interpreter": "/bin/scarlet-ld", "undefined_imports": sorted(IMPORTS),
            "needed_libraries": [], "relocation_types": sorted(kinds), "tls": False,
            "size_bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument("--arch", choices=ARCHES, required=True)
    parser.add_argument("--json", type=Path, help="also save audit/provenance JSON")
    args = parser.parse_args()
    try:
        result = audit(args.path, args.arch)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, "Cadence Scarlet audit failed: " + str(error) + "\n")
    text = json.dumps(result, indent=2) + "\n"
    if args.json:
        args.json.write_text(text)
    print(text, end="")


if __name__ == "__main__":
    main()
