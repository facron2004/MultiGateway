#!/usr/bin/env python3
"""Make a cross-compiled macOS executable launch on Apple Silicon.

Zig 0.17 stamps the binary as macOS 15 and leaves the ad-hoc signature off a
16KB boundary. Apple Silicon refuses to map that file, which shows up as the
app being damaged or impossible to install. This rewrites the deployment
target to macOS 11 and moves the signature onto a 16KB boundary before
rcodesign fills it in. The bundle signer must run afterwards.
"""
import struct
import sys

PAGE = 16384
LC_SEGMENT_64 = 0x19
LC_BUILD_VERSION = 0x32
LC_CODE_SIGNATURE = 0x1D
MINOS = (11 << 16) | (0 << 8) | 0
SDK = (15 << 16) | (5 << 8) | 0


def align_up(value, page):
    return (value + page - 1) & ~(page - 1)


def load_commands(data):
    if struct.unpack_from("<I", data, 0)[0] != 0xFEEDFACF:
        raise SystemExit("not a 64-bit Mach-O")
    ncmds, sizeofcmds = struct.unpack_from("<II", data, 16)
    off = 32
    commands = []
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from("<II", data, off)
        commands.append((cmd, cmdsize, off))
        off += cmdsize
    return sizeofcmds, commands


def fix(path):
    data = bytearray(open(path, "rb").read())
    sizeofcmds, commands = load_commands(data)
    sig_off = None
    linkedit = None
    sig_cmd = None
    for cmd, cmdsize, off in commands:
        if cmd == LC_BUILD_VERSION:
            struct.pack_into("<II", data, off + 12, MINOS, SDK)
        elif cmd == LC_CODE_SIGNATURE:
            sig_off, _sig_size = struct.unpack_from("<II", data, off + 8)
            sig_cmd = (cmdsize, off)
        elif cmd == LC_SEGMENT_64 and data[off + 8:off + 18] == b"__LINKEDIT":
            linkedit = off
    if linkedit is None:
        raise SystemExit(f"{path} has no __LINKEDIT segment")
    if sig_off is not None:
        del data[sig_off:]
    elif sig_cmd is not None:
        raise SystemExit(f"{path} has a code signature command without an offset")
    new_sig = align_up(len(data), PAGE)
    data.extend(b"\0" * (new_sig - len(data)))
    _vmaddr, vmsize, fileoff, _filesize = struct.unpack_from("<QQQQ", data, linkedit + 24)
    new_filesize = new_sig - fileoff
    new_vmsize = align_up(max(vmsize, new_filesize + 0x20000), PAGE)
    struct.pack_into("<Q", data, linkedit + 32, new_vmsize)
    struct.pack_into("<Q", data, linkedit + 48, new_filesize)
    if sig_cmd is not None:
        cmdsize, off = sig_cmd
        commands_end = 32 + sizeofcmds
        tail = data[off + cmdsize:commands_end]
        data[off:off + len(tail)] = tail
        data[commands_end - cmdsize:commands_end] = b"\0" * cmdsize
        ncmds = struct.unpack_from("<I", data, 16)[0]
        struct.pack_into("<II", data, 16, ncmds - 1, sizeofcmds - cmdsize)
    open(path, "wb").write(data)


def check(path, require_arm_alignment):
    data = open(path, "rb").read()
    _sizeofcmds, commands = load_commands(data)
    minos = None
    sig_off = None
    for cmd, _cmdsize, off in commands:
        if cmd == LC_BUILD_VERSION:
            minos = struct.unpack_from("<I", data, off + 12)[0]
        elif cmd == LC_CODE_SIGNATURE:
            sig_off = struct.unpack_from("<I", data, off + 8)[0]
    if minos is None or (minos >> 16) > 11:
        raise SystemExit(f"{path} still requires macOS {minos >> 16 if minos else '?'}")
    if sig_off is None:
        raise SystemExit(f"{path} has no code signature")
    if require_arm_alignment and sig_off % PAGE != 0:
        raise SystemExit(f"{path} code signature is not 16KB aligned ({sig_off:#x})")
    print(f"ok {path} minos={minos >> 16}.{(minos >> 8) & 0xff}.{minos & 0xff} sig={sig_off:#x}")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--check":
        check(sys.argv[2], require_arm_alignment=True)
        return
    if len(sys.argv) != 2:
        raise SystemExit("usage: fix-macos-macho.py BINARY | --check BINARY")
    fix(sys.argv[1])


if __name__ == "__main__":
    main()
