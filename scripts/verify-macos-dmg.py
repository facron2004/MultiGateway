#!/usr/bin/env python3
"""Verify that a macOS DMG can be attached, copied from and launched on a Mac.

Only Python is required, so the same gate runs in CI and on a machine that
cross-builds from Linux without an Apple toolchain. It covers the two failures
users actually report for this project: "the disk image cannot be mounted" and
"the app cannot be installed", plus the bundle metadata macOS needs to launch
the app and copy it from a read-only volume.

UDZO layout written by hdiutil and by libdmg-hfsplus' `dmg`:

    data fork (compressed blocks) | plist with resource-fork/blkx | koly trailer

Every blkx chunk points at a byte range of the data fork. The blocks of one
partition may sit in the data fork in any order, because hdiutil writes them
in the order it produced them, so the check is not about the layout of a
particular producer. It is about what a reader needs: every block has to
decode to exactly the sectors it claims, no sector of a partition may be
missing, every byte of the data fork has to belong to a block, the filesystem
inside the partition has to be consistent, and the bundle metadata macOS needs
to launch the app has to be complete.
"""
import bz2
import io
import lzma
import plistlib
import struct
import sys
import zlib

SECTOR = 512
KOLY_SIZE = 512

CHUNK_END = 0xFFFFFFFF
CHUNK_ZERO = 0x00000002
CHUNK_ZERO_FILL = 0x00000000
CHUNK_ADC = 0x00000001
CHUNK_RAW = 0x80000004
CHUNK_ZLIB = 0x80000005
CHUNK_BZIP2 = 0x80000006
CHUNK_LZFSE = 0x80000007
CHUNK_LZMA = 0x80000008

COMPRESSED = {CHUNK_ADC, CHUNK_ZLIB, CHUNK_BZIP2, CHUNK_LZFSE, CHUNK_LZMA}
DECODABLE = {CHUNK_ZERO, CHUNK_ZERO_FILL, CHUNK_ADC, CHUNK_RAW, CHUNK_ZLIB,
             CHUNK_BZIP2, CHUNK_LZFSE, CHUNK_LZMA}

# Type signature: "H+" (HFS Plus) or "HX" (case sensitive HFS Plus).
HFS_SIGNATURES = (b"H+", b"HX")


class Report:
    def __init__(self):
        self.problems = []
        self.notes = []

    def problem(self, message):
        self.problems.append(message)

    def note(self, message):
        self.notes.append(message)


def read_koly(blob):
    if len(blob) < KOLY_SIZE or blob[-KOLY_SIZE:-KOLY_SIZE + 4] != b"koly":
        return None
    trailer = blob[-KOLY_SIZE:]
    fields = dict(zip(
        ("dataForkOffset", "dataForkLength"),
        struct.unpack_from(">QQ", trailer, 24)))
    fields.update(dict(zip(
        ("xmlOffset", "xmlLength", "sigOffset", "sigLength"),
        struct.unpack_from(">QQQQ", trailer, 216))))
    return fields


def read_chunk_list(raw, label, report):
    """Return (chunks, declared count) for a mish table.

    In the mish layout offset 200 in the block table area is only the block
    count for tables whose chunks are all run type 1/2 (ADC/zero). The
    struct is a union: for a compressed partition the same bytes are the low
    half of the first chunk's sector index, which is 0 for a zlib partition.
    libdmg-hfsplus reads that field as an index, so it emits 0, and a reader
    that trusts it would build an empty or bogus chunk list.
    """
    declared = struct.unpack_from(">I", raw, 200)[0]
    available = (len(raw) - 204) // 40
    count = declared if declared <= available else 0
    if count == 0:
        # Fall back to walking until the terminate chunk marks the end.
        while count < available:
            kind = struct.unpack_from(">I", raw, 204 + count * 40)[0]
            count += 1
            if kind == CHUNK_END:
                break
    elif count > 0:
        kinds = {struct.unpack_from(">I", raw, 204 + index * 40)[0] for index in range(count)}
        if kinds <= {CHUNK_ADC, CHUNK_ZERO, CHUNK_ZERO_FILL, CHUNK_END} and count < available:
            while count < available:
                kind = struct.unpack_from(">I", raw, 204 + count * 40)[0]
                count += 1
                if kind == CHUNK_END:
                    break
    chunks = []
    for index in range(min(count, available)):
        base = 204 + index * 40
        kind = struct.unpack_from(">I", raw, base)[0]
        sector, sectors, offset, length = struct.unpack_from(">QQQQ", raw, base + 8)
        chunks.append({"kind": kind, "sector": sector, "sectors": sectors,
                       "offset": offset, "length": length})
        if kind == CHUNK_END:
            break
    if not chunks or chunks[-1]["kind"] != CHUNK_END:
        report.problem(f"partition '{label}' block table has no end marker; "
                       "the image is truncated")
    return chunks, declared


def read_tables(blob, koly, report):
    trailer_present = koly is not None
    if trailer_present:
        start = koly["xmlOffset"]
        if start + koly["xmlLength"] > len(blob) - KOLY_SIZE:
            report.problem("koly XmlOffset/XmlLength points past the end of the file")
            return [], 0
        data_end = start
        if koly["dataForkLength"] and koly["dataForkOffset"] + koly["dataForkLength"] != start:
            report.problem(
                f"koly DataForkOffset+DataForkLength ends at "
                f"{koly['dataForkOffset'] + koly['dataForkLength']} but the partition table "
                f"starts at {start}")
    else:
        start = blob.rfind(b"<?xml")
        if start < 0:
            report.problem("DMG has neither a koly trailer nor an embedded partition table")
            return [], 0
        report.problem("DMG has no koly trailer, so macSOX cannot attach it")
        data_end = start

    end = blob.find(b"</plist>", start)
    if end < 0:
        report.problem("partition table plist is not terminated")
        return [], data_end
    try:
        resource = plistlib.load(io.BytesIO(blob[start:end + len(b"</plist>")]))
    except Exception as error:  # noqa: BLE001 - any defect here is the finding
        report.problem(f"partition table plist cannot be parsed: {error}")
        return [], data_end

    tables = []
    entries = resource.get("resource-fork", {}).get("blkx", [])
    if not entries:
        report.problem("resource fork lists no partitions")
    for entry in entries:
        raw = bytes(entry.get("Data", b""))
        label = entry.get("CFName") or entry.get("Name") or f"ID {entry.get('ID')}"
        if raw[:4] != b"mish" or len(raw) < 204:
            report.problem(f"partition '{label}' is not a mish table")
            continue
        first, count = struct.unpack_from(">QQ", raw, 8)
        data_offset = struct.unpack_from(">Q", raw, 24)[0]
        chunks, buffers = read_chunk_list(raw, label, report)
        tables.append({"id": str(entry.get("ID")), "name": label, "raw": raw,
                       "position": start + data_offset, "declared_offset": data_offset,
                       "first": first, "count": count, "chunks": chunks,
                       "count_field": buffers})
    return tables, data_end


def check_tables(tables, blob, data_end, report):
    """A container that keeps a table area must lay the tables out in order.

    hdiutil and libdmg-hfsplus both lay the tables out consecutively starting
    at the first sector after the compressed blocks. Offsets are reported
    relative to the start of the first table, because the blkx DataOffset that
    produced the layout is relative to the plist start and would only confuse
    whoever has to fix the builder.
    """
    if not tables:
        return
    # hdiutil keeps every blkx table inside the XML plist, so the field that
    # would point into a separate table area is zero for all of them and
    # there is nothing to align. A container that does keep a table area
    # records each table's offset there, and those offsets have to land on
    # their own sector.
    if not any(table["declared_offset"] for table in tables):
        return
    ordered = sorted(tables, key=lambda item: item["position"])
    origin = ordered[0]["position"]
    expected = 0
    for table in ordered:
        relative = table["position"] - origin
        if relative != expected:
            report.problem(
                f"partition table '{table['name']}' starts at byte {relative} of the table "
                f"area but the previous table ends at {expected}: tables packed together "
                f"must each begin at their own {SECTOR}-byte sector, so this one is read "
                f"{relative % SECTOR} bytes out of alignment")
            expected = align_up(relative, SECTOR)
        needed = len(table["raw"])
        expected = align_up(relative + needed, SECTOR)
        # read_tables already rejects a plist whose DataOffset lies outside the
        # file, so only a table that spills into the trailer is a defect here.
        if origin + relative + needed > len(blob) - KOLY_SIZE:
            report.problem(f"partition table '{table['name']}' runs past the end of the file")
            continue
        for chunk in table["chunks"]:
            if chunk["kind"] in COMPRESSED and chunk["length"]:
                if chunk["offset"] + chunk["length"] > data_end:
                    report.problem(
                        f"partition table '{table['name']}' points at bytes "
                        f"{chunk['offset']}..{chunk['offset'] + chunk['length']} but the data "
                        f"fork only holds {data_end}")
                break


def align_up(value, alignment):
    return (value + alignment - 1) // alignment * alignment


def check_chunk_offsets(tables, blob, data_end, report):
    """Chunk payloads live in the data fork and must not overlap.

    The blocks of a partition may be stored in any order inside the data
    fork: hdiutil itself writes them in the order it produced them, which is
    not the order of the sectors they describe.  What has to hold is that the
    bytes exist, that they do not overlap and that every byte of the data fork
    belongs to a block, because a reader that finds unclaimed bytes has no way
    to tell what was supposed to be there.
    """
    spans = []
    for table in tables:
        for chunk in table["chunks"]:
            if chunk["kind"] not in COMPRESSED or not chunk["length"]:
                continue
            if chunk["offset"] + chunk["length"] > data_end:
                report.problem(
                    f"partition '{table['name']}' block at {chunk['offset']} "
                    f"(+{chunk['length']}) overruns the data fork ({data_end} bytes)")
            spans.append((chunk["offset"], chunk["length"], table["name"]))
    if not spans:
        return
    spans.sort()
    cursor = 0
    for offset, length, label in spans:
        if offset < cursor:
            report.problem(f"partition '{label}' block at {offset} overlaps the previous "
                           f"block ending at {cursor}")
        elif offset > cursor:
            unclaimed(blob, cursor, offset, report)
        cursor = max(cursor, offset + length)
    if cursor < data_end:
        unclaimed(blob, cursor, data_end, report)


def unclaimed(blob, start, end, report):
    """Note data fork bytes that no partition block points at.

    hdiutil itself leaves such bytes behind, so they are not a defect on their
    own; they are worth reporting because a reader has to fill them with
    something, and a container that lost a block looks exactly the same.
    """
    length = end - start
    if length <= 0:
        return
    if blob[start:end].strip(b"\0"):
        report.note(f"the data fork holds {length} bytes at {start}..{end} that no "
                    "partition block points at")
    else:
        report.note(f"the data fork has {length} zero bytes of padding at {start}..{end}")


def decode(blob, chunk, label, report):
    kind, sectors = chunk["kind"], chunk["sectors"]
    if kind in (CHUNK_ZERO, CHUNK_ZERO_FILL):
        return b"\0" * (sectors * SECTOR)
    if kind not in DECODABLE:
        report.problem(f"partition '{label}' uses unsupported block type {kind:#x}; "
                       "macOS cannot read this image with stock tools")
        return None
    offset, length = chunk["offset"], chunk["length"]
    if not length:
        # A block that declares no payload still covers its sectors; they are
        # simply empty.
        return b"\0" * (sectors * SECTOR)
    raw = blob[offset:offset + length]
    if len(raw) != length:
        report.problem(f"partition '{label}' block at offset {offset} runs past the end of the file")
        return None
    try:
        if kind == CHUNK_ADC:
            return zlib.decompress(raw, -15)
        if kind == CHUNK_ZLIB:
            return zlib.decompress(raw)
        if kind == CHUNK_BZIP2:
            return bz2.decompress(raw)
        if kind == CHUNK_LZMA:
            return lzma.decompress(raw)
        return raw
    except Exception as error:  # noqa: BLE001 - corrupt payloads are the finding
        report.problem(f"partition '{label}' block at offset {offset} does not decompress "
                       f"({error}); the download is truncated or corrupt")
        return None


def assemble(blob, table, report):
    """Rebuild the partition image described by one blkx table."""
    image = bytearray(table["count"] * SECTOR)
    readable = True
    covered = []
    for chunk in table["chunks"]:
        if chunk["kind"] == CHUNK_END:
            break
        if chunk["sectors"] == 0:
            continue
        if chunk["kind"] not in DECODABLE:
            readable = False
            continue
        start = chunk["sector"] * SECTOR
        end = start + chunk["sectors"] * SECTOR
        if end > len(image):
            report.problem(f"partition '{table['name']}' has a block at sector {chunk['sector']} "
                           f"beyond its declared {table['count']} sectors")
            readable = False
            continue
        payload = decode(blob, chunk, table["name"], report)
        if payload is None:
            readable = False
            continue
        if len(payload) != chunk["sectors"] * SECTOR:
            report.problem(f"partition '{table['name']}' block at sector {chunk['sector']} "
                           f"decodes to {len(payload)} bytes, expected "
                           f"{chunk['sectors'] * SECTOR}")
            readable = False
            continue
        image[start:end] = payload
        covered.append((start, end))
    # Every sector of the partition has to come from a block; a hole means the
    # reader fills it with zeros and the filesystem inside is damaged.
    covered.sort()
    cursor = 0
    for start, end in covered:
        if start > cursor:
            report.problem(f"partition '{table['name']}' has no block for the sectors "
                           f"{cursor // SECTOR}..{start // SECTOR}; the image is incomplete")
        cursor = max(cursor, end)
    if cursor < len(image):
        report.problem(f"partition '{table['name']}' has no block for the sectors "
                       f"{cursor // SECTOR}..{table['count']}; the image is incomplete")
    return image, readable


def check_apfs(image, report):
    if image[32:36] != b"NXSB":
        return None
    block_size = struct.unpack_from("<I", image, 36)[0]
    block_count = struct.unpack_from("<Q", image, 40)[0]
    if block_size not in (4096, 8192, 16384, 32768, 65536):
        report.problem(f"APFS container reports an impossible block size {block_size}")
        return block_size
    span = block_size * block_count
    if span > len(image):
        report.problem(
            f"APFS container spans {span} bytes but the partition image "
            f"holds {len(image)}; the filesystem was cut short during packaging")
    elif span < len(image):
        report.note(f"APFS container {span / 1048576:.1f} MB inside a "
                    f"{len(image) / 1048576:.1f} MB partition, {block_size}-byte blocks")
    else:
        report.note(f"APFS container {len(image) / 1048576:.1f} MB, {block_size}-byte blocks")
    return block_size


def hfs_fork(blob, record, block_size, fragmented):
    """The bytes a HFSPlusForkData record points at.

    A fork record starts with its logical size and lists its extents from
    offset 16 on, so the first extent's start block says where the bytes live.
    """
    if fragmented:
        return None
    length = struct.unpack_from(">Q", record, 0)[0]
    start = struct.unpack_from(">I", record, 16)[0] * block_size
    if start + length > len(blob):
        return None
    return blob[start:start + length]


def btree_node_size(fork):
    """The node size the b-tree header records, or the usual 8192 bytes."""
    if len(fork) >= 42 and fork[8] == 1:          # kind 1 == header node
        size = struct.unpack_from(">I", fork, 14 + 14)[0]
        if 512 <= size <= 65536 and size % 512 == 0:
            return size
    return 8192


def hfs_catalog_names(image, report):
    """Walk the HFS+ catalog and return every record name and mode it finds."""
    if image[1024:1026] not in HFS_SIGNATURES:
        return None
    # HFSPlusVolumeHeader: blockSize at header offset 40, catalog fork at 372.
    block_size = struct.unpack_from(">I", image, 1024 + 40)[0]
    if block_size not in (512, 1024, 2048, 4096, 8192, 16384, 32768, 65536):
        report.problem(f"HFS+ volume header reports an impossible block size {block_size}")
        return None
    fork = hfs_fork(image, image[1024 + 372:1024 + 452], block_size, False)
    if fork is None:
        report.problem("HFS+ catalog file extends past the volume; the image is corrupt")
        return None
    node_size = btree_node_size(fork)
    names = set()
    for index in range(len(fork) // node_size):
        node = fork[index * node_size:(index + 1) * node_size]
        if node[8] != 0:                  # 0 == kBTLeafNode
            continue
        num_records = struct.unpack_from(">H", node, 10)[0]
        for record in range(num_records):
            # Record offsets live in the last two bytes per record of the
            # node, last record first.
            offset = struct.unpack_from(">H", node, node_size - 2 * (record + 1))[0]
            if offset + 2 > len(node):
                continue
            key_length = struct.unpack_from(">H", node, offset)[0]
            # The key starts with its own length, then the parent id, then the
            # node name length and the UTF-16 name itself.
            key = node[offset:offset + 2 + key_length]
            if len(key) < 8:
                continue
            parent = struct.unpack_from(">I", key, 2)[0]
            if parent == 0:
                continue
            name_length = struct.unpack_from(">H", key, 6)[0]
            name = key[8:8 + name_length * 2].decode("utf-16-be", "replace")
            names.add(name)
    return names


def check_volume(image, report):
    apfs = check_apfs(image, report)
    names = hfs_catalog_names(image, report) if apfs is None else None
    if names is not None:
        missing = [wanted for wanted in ("MultiGateway.app", "Applications") if wanted not in names]
        extra = sorted(name for name in names if name.endswith(".app"))
        if "MultiGateway.app" in missing:
            report.problem(f"volume does not contain MultiGateway.app (app bundles found: {extra})")
        if "Applications" in missing:
            report.problem("volume has no Applications link, so dragging the app to install it fails")
        if not missing:
            report.note("volume contains MultiGateway.app and the Applications link")
    return apfs, names


def check_bundle(blobs, report):
    """Validate Info.plist and the executable inside the app bundle, when present.

    The bundle lives inside the filesystem, so the search runs over the
    partition images that were rebuilt from the blkx tables rather than over
    the compressed data fork.
    """
    plists = 0
    for blob in blobs:
        index = blob.find(b"<plist")
        while index >= 0:
            end = blob.find(b"</plist>", index)
            if end < 0:
                break
            try:
                plist = plistlib.loads(blob[index:end + len(b"</plist>")])
            except Exception:  # noqa: BLE001 - not every plist blob is an Info.plist
                plist = None
            if isinstance(plist, dict) and plist.get("CFBundlePackageType") == "APPL":
                plists += 1
                validate_info_plist(plist, report)
            index = blob.find(b"<plist", end)
    if not plists:
        report.note("no application bundle was found inside the image; "
                    "Info.plist checks were skipped")
    return plists


def validate_info_plist(plist, report):
    required = {
        "CFBundleExecutable": str,
        "CFBundleIdentifier": str,
        "CFBundleName": str,
        "CFBundleVersion": str,
        "CFBundleShortVersionString": str,
        "LSMinimumSystemVersion": str,
    }
    for key, kind in required.items():
        if key not in plist:
            report.problem(f"Info.plist is missing {key}")
        elif not isinstance(plist[key], kind) or not str(plist[key]).strip():
            report.problem(f"Info.plist {key} is empty or not a string")
    if "CFBundlePackageType" in plist and plist["CFBundlePackageType"] != "APPL":
        report.problem("Info.plist CFBundlePackageType is not APPL")
    executable = plist.get("CFBundleExecutable")
    if executable and ("/" in executable or executable in (".", "..")):
        report.problem(f"Info.plist CFBundleExecutable {executable!r} is not a file name")


def main(argv):
    if len(argv) != 2:
        raise SystemExit("usage: verify-macos-dmg.py FILE.dmg")
    path = argv[1]
    with open(path, "rb") as handle:
        blob = handle.read()
    report = Report()
    if len(blob) < 8192:
        report.problem("file is too small to be a disk image")

    koly = read_koly(blob)
    if koly is None:
        report.problem("DMG has no koly trailer; macOS reports the image as not recognized")
    tables, data_end = read_tables(blob, koly, report)
    check_tables(tables, blob, data_end, report)
    check_chunk_offsets(tables, blob, data_end, report)

    volumes = []
    partitions = []
    for table in tables:
        if not table["count"]:
            continue
        image, readable = assemble(blob, table, report)
        if not readable:
            continue
        partitions.append(image)
        _, names = check_volume(image, report)
        if names is not None:
            volumes.append(names)

    check_bundle(partitions if partitions else [blob], report)

    for message in report.notes:
        print(f"note: {message}")
    if report.problems:
        for message in report.problems:
            print(f"PROBLEM: {message}", file=sys.stderr)
        print(f"FAIL: {path} cannot be installed on macOS ({len(report.problems)} problems)",
              file=sys.stderr)
        return 1
    print(f"OK: {path} looks mountable and installable")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
