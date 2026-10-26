#!/usr/bin/env python3
"""Build small synthetic UDZO disk images used to test verify-macos-dmg.py.

    python scripts/make-test-dmg.py --kind KIND --output PATH [--arch arm64|x64]

Everything is assembled byte by byte in pure Python.  A UDZO image is

    data fork (zlib blocks) | plist embedding one mish table per partition | koly

and the tables inside the plist are the only copies, exactly as hdiutil writes
them; what matters for readers is the DataOffset recorded inside each mish table,
measured from the start of the plist, which is also what hdiutil/real images do.

The `good` fixtures contain a real HFS+ volume: a volume header at offset 1024
with signature "H+", blockSize 4096, valid catalog/extents/overflow b-trees
(8192-byte nodes, header node first) and a catalog holding Applications,
MultiGateway.app, its Contents/MacOS folders, Info.plist and the executable.
`good-apfs` instead contains a minimal but self-consistent APFS container.

Kinds:
    good          valid HFS+ image, the 0.11.1 layout (must pass)
    good-apfs     valid APFS container (must pass; no HFS+ catalog to walk)
    hdiutil-layout the same image with the blkx tables kept inline in the XML
                  plist, which is what hdiutil writes (must pass)
    packed-tables a container that keeps its blkx tables in a table area but
                  packs them back to back, so a table starts mid-sector
    shuffled-blocks the blocks of one partition stored out of order inside the
                  data fork, the defect that shipped as 0.11.2
    short-datafork every zlib payload cut in half (blocks fail to inflate)
    missing-koly  the koly trailer stripped; the plist is still parseable
    no-app        HFS+ image lacking MultiGateway.app
    bad-plist     HFS+ image whose Info.plist misses required keys
"""
import argparse
import io
import os
import plistlib
import struct
import sys
import zlib

KINDS = ("good", "good-apfs", "hdiutil-layout", "packed-tables",
         "shuffled-blocks", "short-datafork", "missing-koly", "no-app",
         "bad-plist")

SECTOR = 512
ENTRY_ARRAY_SECTORS = 32   # GPT keeps its 128-entry array in LBA 2..33
KOLY_SIZE = 512
MISH_HEADER = 204          # "mish" + version + first + count + DataOffset
MISH_COUNT_AT = 200        # union: block count, really the first chunk's sector
CHUNK_SIZE = 40
HFS_NODE = 8192            # b-tree node size (must exceed blockSize in real HFS+)
HFS_BLOCK = 4096           # volume header blockSize
HFS_VOLUME_BYTES = 1 << 20
APFS_BLOCK = 4096
APFS_VOLUME_BYTES = 1 << 20

APP_NAME = "MultiGateway.app"
EXEC_NAME = "MultiGateway"
APP_ID = "com.multigateway.desktop"
VERSION = "0.11.2"

CHUNK_ZLIB = 0x80000005
CHUNK_END = 0xFFFFFFFF

ROOT_CNID = 2              # HFS+ CNID of the volume root folder
APP_CNID = 16
LINK_CNID = 17
WRONG_CNID = 18
CONTENTS_CNID = 19
MACOS_CNID = 20
PLIST_CNID = 21
EXEC_CNID = 22
RESOURCES_CNID = 23
ICON_CNID = 24

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICON_PNG = os.path.join(REPO_ROOT, "crates", "mg-server", "ui", "icons",
                        "multigateway.png")


def align_up(value, alignment):
    return (value + alignment - 1) // alignment * alignment


def write_bytes(path, data):
    directory = os.path.dirname(os.path.abspath(path))
    if directory:
        os.makedirs(directory, exist_ok=True)
    with open(path, "wb") as handle:
        handle.write(data)


# --------------------------------------------------------------------------
# HFS+ structures
# --------------------------------------------------------------------------
def fork_data(logical_size, block_count, extents):
    """HFSPlusForkData: logicalSize u64, clumpSize u32, totalBlocks u32, 8 extents."""
    body = struct.pack(">QII", logical_size, 0, block_count)
    for first, count in list(extents) + [(0, 0)] * (8 - len(extents)):
        body += struct.pack(">II", first, count)
    return body


def hfs_fork(logical_size, start_block, block_count, extents):
    """The 80-byte HFSPlusForkData a volume header stores.

    startBlock is the allocation block the fork begins in; the extent list is
    the authoritative map, so a fork with no extents simply holds nothing.
    """
    extents = list(extents) or [(start_block, 0)]
    body = struct.pack(">QII", logical_size, 0, block_count)
    for first, count in extents + [(0, 0)] * (8 - len(extents)):
        body += struct.pack(">II", first, count)
    return body


def catalog_key(parent_id, name):
    """HFSPlusCatalogKey: u16 keyLength, u32 parentID, u16 nameLength, UTF-16BE."""
    encoded = name.encode("utf-16-be")
    body = struct.pack(">IH", parent_id, len(name)) + encoded
    return struct.pack(">H", len(body)) + body


def catalog_folder(name, parent, cnid, mode):
    """HFSPlusCatalogFolder: recordType 1, folders get mode and a valence."""
    payload = struct.pack(">H", 1)                    # recordType
    payload += struct.pack(">H", 0)                   # flags
    payload += struct.pack(">I", 0)                   # valence
    payload += struct.pack(">I", cnid)                # folderID
    payload += struct.pack(">IIIII", 0, 0, 0, 0, 0)   # create/modify/attr/access/backup
    payload += struct.pack(">II", 0, 0)               # ownerID, groupID
    payload += struct.pack(">H", 0)                   # adminFlags
    payload += struct.pack(">H", 0)                   # ownerFlags
    payload += struct.pack(">H", mode)                # fileMode (POSIX bits)
    payload += struct.pack(">I", 0)                   # special
    payload += struct.pack(">I", 0)                   # textEncoding
    payload += struct.pack(">I", 0)                   # reserved
    payload += struct.pack(">I", 0)                   # folderCount
    payload += struct.pack(">I", 0)                   # writeGenCount
    return catalog_key(parent, name) + payload


def catalog_file(name, parent, cnid, mode, size):
    """HFSPlusCatalogFile: recordType 2 with mode and a data fork."""
    payload = struct.pack(">H", 2)                    # recordType
    payload += struct.pack(">H", 0)                   # flags
    payload += struct.pack(">I", 0)                   # reserved1
    payload += struct.pack(">IIIII", 0, 0, 0, 0, 0)   # create/modify/attr/access/backup
    payload += struct.pack(">II", 0, 0)               # ownerID, groupID
    payload += struct.pack(">H", 0)                   # adminFlags
    payload += struct.pack(">H", 0)                   # ownerFlags
    payload += struct.pack(">H", mode)                # fileMode (POSIX bits)
    payload += struct.pack(">I", 0)                   # special
    payload += hfs_fork(size, 0, 0, [])               # data fork
    payload += hfs_fork(0, 0, 0, [])                  # resource fork
    return catalog_key(parent, name) + payload


def make_btree(records):
    """Serialise catalog-style leaf records into a two-node b-tree.

    Node 0 is the header node, node 1 is the leaf node holding `records`.
    A BTNodeDescriptor is fLink, bLink, kind, height, numRecords and one
    reserved field, so the descriptor is 14 bytes long and the record offsets
    start right behind it.  The offsets themselves live in the last two bytes
    per record of the node, last record first, which is where Apple's readers
    look for them.
    """
    leaf = bytearray(HFS_NODE)
    # fLink, bLink, kind (0 = leaf node), height, numRecords, reserved.
    struct.pack_into(">IIBBHH", leaf, 0, 0, 0, 0, 1, len(records), 0)
    cursor = 14
    offsets = []
    for record in records:
        if cursor + len(record) > HFS_NODE - 2 * (len(records) + 1):
            raise ValueError("test catalog does not fit in one b-tree leaf node")
        offsets.append(cursor)
        leaf[cursor:cursor + len(record)] = record
        cursor += len(record)
    for index, offset in enumerate(offsets):
        struct.pack_into(">H", leaf, HFS_NODE - 2 * (index + 1), offset)

    header = bytearray(HFS_NODE)
    # fLink, bLink, kind (1 = header node), height (0), numRecords (1), reserved.
    struct.pack_into(">IIBBHH", header, 0, 0, 0, 1, 0, 1, 0)
    # The header node's single record is the BTHeaderRec that follows the node
    # descriptor, and the record offset at the end of the node points at it.
    record_at = 14
    struct.pack_into(">H", header, HFS_NODE - 2, record_at)
    struct.pack_into(">IIBBHH", header, record_at, 0, 0, 1, 0, 1, 0)
    info = record_at + 14                             # BTreeInfo inside the record
    struct.pack_into(">I", header, info, HFS_NODE)    # nodeSize
    struct.pack_into(">I", header, info + 4, 40)      # maxKeyLength
    struct.pack_into(">I", header, info + 8, 2)       # totalNodes
    struct.pack_into(">I", header, info + 12, 0)      # freeNodes
    struct.pack_into(">H", header, info + 16, 1)      # firstLeafNode
    struct.pack_into(">H", header, info + 18, 1)      # lastLeafNode
    struct.pack_into(">H", header, info + 20, HFS_NODE)   # nodeSize (deprecated)
    struct.pack_into(">H", header, info + 22, 40)         # maxKeyLength (deprecated)
    struct.pack_into(">I", header, info + 24, 0)      # dataLength
    struct.pack_into(">I", header, info + 28, 0)      # freeLength
    struct.pack_into(">Q", header, info + 32, 0)      # clumpSize
    header[info + 40] = 0                            # btreeType (catalog = 0)
    header[info + 41] = 1                            # keyCompareType (case folded)
    struct.pack_into(">I", header, info + 42, 0)      # attributes
    return bytes(header) + bytes(leaf)


def catalog_records(kind):
    """Build sorted catalog records for the requested fixture flavour."""
    root = ROOT_CNID
    entries = [("Applications", root, LINK_CNID, 0o120755, True)]
    if kind == "no-app":
        entries.append(("Wrong.app", root, WRONG_CNID, 0o040755, True))
    else:
        entries += [
            (APP_NAME, root, APP_CNID, 0o040755, True),
            ("Contents", APP_CNID, CONTENTS_CNID, 0o040755, True),
            ("MacOS", CONTENTS_CNID, MACOS_CNID, 0o040755, True),
            ("Resources", CONTENTS_CNID, RESOURCES_CNID, 0o040755, True),
            ("Info.plist", CONTENTS_CNID, PLIST_CNID, 0o100644, False),
            (EXEC_NAME, MACOS_CNID, EXEC_CNID, 0o100755, False),
            ("AppIcon.icns", RESOURCES_CNID, ICON_CNID, 0o100644, False),
        ]

    records = []
    for name, parent, cnid, mode, is_directory in entries:
        if is_directory:
            records.append(catalog_folder(name, parent, cnid, mode))
        else:
            records.append(catalog_file(name, parent, cnid, mode, 1024))
    order = sorted(entries, key=lambda entry: (entry[1], entry[0].encode("utf-16-be")))
    records.sort(key=_record_sort_key)
    return records, entries


def _record_sort_key(record):
    parent = struct.unpack_from(">I", record, 2)[0]
    name_length = struct.unpack_from(">H", record, 6)[0]
    return (parent, record[8:8 + name_length * 2])


def build_volume_header(file_count, folder_count, catalog, extents, attributes,
                        next_catalog_id, total_blocks):
    """The HFSPlusVolumeHeader that HFS+ places at volume offset 1024.

    The field offsets are Apple's: blockSize sits at header offset 40, the
    catalog fork at 372, and a fork record starts with its logical size and
    lists its extents from offset 16 on.
    """
    header = bytearray(620)
    struct.pack_into(">H", header, 0, 0x482B)              # signature "H+"
    struct.pack_into(">H", header, 2, 4)                   # version
    struct.pack_into(">I", header, 4, 0)                   # attributes
    header[8:12] = b"HFSJ"                                 # lastMountedVersion
    struct.pack_into(">I", header, 12, 0)                  # journalInfoBlock
    struct.pack_into(">IIIII", header, 16, 0, 0, 0, 0, 0)  # the five timestamps
    struct.pack_into(">I", header, 32, file_count)         # fileCount
    struct.pack_into(">I", header, 36, folder_count)       # folderCount
    struct.pack_into(">I", header, 40, HFS_BLOCK)          # blockSize
    struct.pack_into(">I", header, 44, total_blocks)       # totalBlocks
    struct.pack_into(">I", header, 48, 4)                  # freeBlocks
    struct.pack_into(">I", header, 52, 8)                  # nextAllocation
    struct.pack_into(">I", header, 56, 0)                  # rsrcClumpSize
    struct.pack_into(">I", header, 60, next_catalog_id)    # nextCatalogID
    struct.pack_into(">I", header, 64, 0)                  # writeCount
    struct.pack_into(">Q", header, 68, 0)                  # encodingsBitmap
    # finderInfo[8] (76..204) stays zero.
    # Forks, 80 bytes each: allocation file, extents, catalog, attributes, startup.
    header[204:284] = hfs_fork(HFS_VOLUME_BYTES, 0, total_blocks - 4,
                               [(0, total_blocks - 4)])
    header[288:368] = hfs_fork(*extents)
    header[372:452] = hfs_fork(*catalog)
    header[456:536] = hfs_fork(*attributes)
    header[540:620] = hfs_fork(0, 0, 0, [])
    return bytes(header)


def build_hfs_image(kind):
    """A 1 MiB HFS+ volume: header block, catalog b-tree, then the other b-trees."""
    records, entries = catalog_records(kind)
    catalog_blob = make_btree(records)
    # An empty leaf-only tree stands in for the extents and attributes files.
    empty_blob = make_btree([struct.pack(">H", 0)])
    total_blocks = HFS_VOLUME_BYTES // HFS_BLOCK

    def blocks_for(blob):
        return (len(blob) + HFS_BLOCK - 1) // HFS_BLOCK

    catalog_start = 1
    catalog_blocks = blocks_for(catalog_blob)
    extents_start = catalog_start + catalog_blocks
    extents_blocks = blocks_for(empty_blob)
    attributes_start = extents_start + extents_blocks
    attributes_blocks = blocks_for(empty_blob)
    if attributes_start + attributes_blocks >= total_blocks:
        raise ValueError("b-trees do not fit in the test volume")

    image = bytearray(HFS_VOLUME_BYTES)
    for start, blob in ((catalog_start, catalog_blob),
                        (extents_start, empty_blob),
                        (attributes_start, empty_blob)):
        image[start * HFS_BLOCK:start * HFS_BLOCK + len(blob)] = blob

    file_count = sum(1 for entry in entries if not entry[4])
    folder_count = sum(1 for entry in entries if entry[4])
    header = build_volume_header(
        file_count, folder_count,
        (len(catalog_blob), catalog_start, catalog_blocks,
         [(catalog_start, catalog_blocks)]),
        (len(empty_blob), extents_start, extents_blocks,
         [(extents_start, extents_blocks)]),
        (len(empty_blob), attributes_start, attributes_blocks,
         [(attributes_start, attributes_blocks)]),
        max(entry[2] for entry in entries) + 1,
        total_blocks)
    image[1024:1024 + len(header)] = header
    return bytes(image)


def build_apfs_image(payload=b"", total_bytes=None):
    """A self-consistent APFS container: nx_superblock in block 0.

    The container must span the whole partition, so `total_bytes` is the size
    the blkx table declares: block_size * nxs_block_count == len(image).  The
    bundle payload sits behind the superblock, inside that same span.
    """
    if total_bytes is None:
        total_bytes = APFS_VOLUME_BYTES
    total_bytes = align_up(total_bytes, APFS_BLOCK)
    block = bytearray(APFS_BLOCK)
    # obj_header_t (32 bytes): checksum, oid, xid, type, subtype.
    struct.pack_into("<QQQII", block, 0, 0, 1, 1, 1, 0)
    block[32:36] = b"NXSB"                                # nxs_magic
    struct.pack_into("<I", block, 36, APFS_BLOCK)          # nxs_block_size
    struct.pack_into("<Q", block, 40, total_bytes // APFS_BLOCK)   # nxs_block_count
    struct.pack_into("<I", block, 48, 0)                   # features
    struct.pack_into("<I", block, 52, 0)                   # readonly compatible features
    struct.pack_into("<I", block, 56, 0)                   # incompatible features
    struct.pack_into("<Q", block, 60, 0)                   # uuid low
    struct.pack_into("<Q", block, 68, 0)                   # uuid high
    struct.pack_into("<Q", block, 76, 1)                   # next_oid
    struct.pack_into("<Q", block, 84, 1)                   # next_xid
    struct.pack_into("<I", block, 92, 1)                   # xp_desc_blocks
    struct.pack_into("<I", block, 96, 1)                   # xp_data_blocks
    image = bytearray(total_bytes)
    image[0:APFS_BLOCK] = block
    if payload:
        image[APFS_VOLUME_BYTES:APFS_VOLUME_BYTES + len(payload)] = payload
    return bytes(image)


# --------------------------------------------------------------------------
# GPT scaffolding stored ahead of the payload partition
# --------------------------------------------------------------------------
def gpt_boot_blocks(partition_sectors):
    """LBA 0..33: protective MBR, primary GPT header, then the entry array.

    A real image keeps the entry array in LBA 2..33 (32 sectors) even though
    only the first entry is used, so the array is zero padded to full sectors
    here as well.
    """
    mbr = bytearray(SECTOR)
    # A GPT protective MBR entry is 16 bytes: boot flag, CHS start, type 0xEE
    # (GPT protective), CHS end, then the LBA range. Assigning to mbr[446:462]
    # with a longer value would insert bytes and shift the whole data fork.
    entry_mbr = (b"\x00" + b"\x00\x00\x00" + b"\xee" + b"\xff\xff\xff"
                 + struct.pack("<I", 1) + struct.pack("<I", 39 + partition_sectors))
    assert len(entry_mbr) == 16
    mbr[446:462] = entry_mbr
    mbr[510:512] = b"\x55\xAA"

    header = bytearray(SECTOR)
    header[0:8] = b"EFI PART"
    struct.pack_into("<IIII", header, 8, 0x00010000, 92, 0, 0)
    struct.pack_into("<QQQQ", header, 24, 1, 40 + partition_sectors,
                     34, 40 + partition_sectors - 1)
    struct.pack_into("<QQII", header, 56, 2, 40 + partition_sectors + 1, 128, 128)

    entry = bytearray(128)
    entry[0:16] = bytes.fromhex("28732ac11ff8d211ba4b00a0c93ec93b")   # Apple_HFS
    struct.pack_into("<QQ", entry, 32, 40, 40 + partition_sectors)
    entries = bytes(entry) + b"\0" * (ENTRY_ARRAY_SECTORS * SECTOR - len(entry))
    return bytes(mbr) + bytes(header) + entries


# --------------------------------------------------------------------------
# Bundle payloads
# --------------------------------------------------------------------------
def make_info_plist(kind):
    plist = {
        "CFBundleDisplayName": "MultiGateway",
        "CFBundleExecutable": EXEC_NAME,
        "CFBundleIdentifier": APP_ID,
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": "MultiGateway",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": VERSION,
        "CFBundleSignature": "????",
        "CFBundleVersion": VERSION,
        "LSApplicationCategoryType": "public.app-category.developer-tools",
        "LSMinimumSystemVersion": "11.0",
        "NSHighResolutionCapable": True,
    }
    if kind == "bad-plist":
        del plist["LSMinimumSystemVersion"]
        del plist["CFBundleExecutable"]
    return plistlib.dumps(plist, fmt=plistlib.FMT_XML)


def make_icns():
    """Turn the repository icon PNG into a small real .icns when Pillow is there."""
    try:
        from PIL import Image
    except ImportError:                  # the fixture is still valid without it
        return b"icns" + struct.pack(">I", 8)
    if not os.path.exists(ICON_PNG):
        return b"icns" + struct.pack(">I", 8)
    source = Image.open(ICON_PNG).convert("RGBA")
    body = b""
    for ostype, pixels in ((b"icp4", 16), (b"icp5", 32), (b"ic07", 128)):
        buffer = io.BytesIO()
        source.resize((pixels, pixels), Image.Resampling.LANCZOS).save(buffer, format="PNG")
        payload = buffer.getvalue()
        body += ostype + struct.pack(">I", len(payload) + 8) + payload
    return b"icns" + struct.pack(">I", len(body) + 8) + body


def executable_bytes(arch):
    """A tiny but well-formed 64-bit Mach-O header plus filler."""
    cputype = 0x0100000C if arch == "arm64" else 0x01000007
    header = struct.pack("<IIIIIIII", 0xFEEDFACF, cputype, 3, 2, 0x4000, 0, 0, 0)
    return header + bytes(range(256)) * 4


def bundle_bytes(kind, arch):
    """Everything a user reads inside the mounted volume, as one payload blob.

    The installer volume is described by the HFS+ catalog (that is what the
    verifier walks); this blob carries the Info.plist and executable bytes so
    the image also contains them literally, the way a real one does.
    """
    return (make_info_plist(kind) + make_icns() + executable_bytes(arch)
            + ("MultiGateway %s\n" % VERSION).encode())


# --------------------------------------------------------------------------
# mish tables and UDZO assembly
# --------------------------------------------------------------------------
def mish_table(first_sector, sector_count, chunks, data_offset, pad_to_sector=False):
    """One blkx mish table: header, zero padding to 200, then 40-byte records.

    `pad_to_sector` grows the table with trailing zeros until its length is a
    whole number of 512-byte sectors, which is how hdiutil packs the tables:
    the next table then starts on its own sector.  `packed` tables skip it.

    `data_offset` is where the table sits in the table area.  hdiutil keeps its
    tables inline in the XML plist and records zero there, so a container that
    has no table area at all passes 0.
    """
    body = bytearray()
    body += b"mish"
    body += struct.pack(">I", 1)                          # version
    body += struct.pack(">Q", first_sector)               # firstSector
    body += struct.pack(">Q", sector_count)               # sectorCount
    body += struct.pack(">Q", data_offset)                # DataOffset (from the plist)
    body += b"\0" * (MISH_COUNT_AT - len(body))
    # This field is a union: the low half of the first chunk's sector index.
    # Compressed partitions record 0 here, as hdiutil and libdmg write it.
    body += struct.pack(">I", 0)
    for kind, sector, sectors, offset, length in chunks:
        body += struct.pack(">IIQQQQ", kind, 0, sector, sectors, offset, length)
    body += struct.pack(">IIQQQQ", CHUNK_END, 0, 0, 0, 0, 0)
    if pad_to_sector:
        body += b"\0" * (align_up(len(body), SECTOR) - len(body))
    return bytes(body)


class Block:
    """One data-fork payload together with the partition it belongs to."""

    def __init__(self, label, table_id, first_sector, sector_count, payload,
                 chunk_sector, chunk_sectors):
        self.label = label
        self.table_id = table_id
        self.first_sector = first_sector
        self.sector_count = sector_count
        self.payload = payload
        self.chunk_sector = chunk_sector
        self.chunk_sectors = chunk_sectors


def build_dmg(kind, arch="arm64"):
    """Return the bytes of the requested fixture."""
    if kind not in KINDS:
        raise ValueError(f"unknown kind {kind!r}; choose one of {', '.join(KINDS)}")

    apfs = kind in ("good-apfs", "packed-tables")
    payload = bundle_bytes(kind, arch)
    if apfs:
        # The container describes the whole partition, so its size covers the
        # bundle payload that follows the superblock.
        volume = build_apfs_image(payload, APFS_VOLUME_BYTES + len(payload))
    else:
        volume = build_hfs_image(kind) + payload
        volume += b"\0" * (align_up(len(volume), SECTOR) - len(volume))
    partition_sectors = len(volume) // SECTOR
    boot = gpt_boot_blocks(partition_sectors)
    image_label = "disk image (Apple_APFS : 4)" if apfs else "disk image (Apple_HFS : 4)"

    blocks = [
        Block("Protective Master Boot Record (MBR : 0)", -1, 0, 1,
              zlib.compress(boot[0:SECTOR]), 0, 1),
        Block("GPT Header (Primary GPT Header : 1)", 0, 1, 1,
              zlib.compress(boot[SECTOR:2 * SECTOR]), 0, 1),
        # LBA 2..33: the first GPT entry array, zero padded to its 32 sectors.
        Block("GPT Partition Data (Primary GPT Table : 2)", 1, 2, ENTRY_ARRAY_SECTORS,
              zlib.compress(boot[2 * SECTOR:]), 0, ENTRY_ARRAY_SECTORS),
    ]
    if kind == "shuffled-blocks":
        # Split the payload partition into two blocks, so the fixture can hold
        # them in the data fork in the wrong order.
        half = partition_sectors // 2
        blocks.append(Block(image_label, 4, 40, partition_sectors,
                            zlib.compress(volume[:half * SECTOR]), 0, half))
        blocks.append(Block(image_label, 4, 40, partition_sectors,
                            zlib.compress(volume[half * SECTOR:]), half,
                            partition_sectors - half))
    else:
        blocks.append(Block(image_label, 4, 40, partition_sectors,
                            zlib.compress(volume), 0, partition_sectors))
    if kind == "short-datafork":
        # Halving every payload leaves the table geometry alone but makes each
        # zlib stream stop early, so the partition cannot be rebuilt.
        blocks = [Block(item.label, item.table_id, item.first_sector,
                        item.sector_count, item.payload[:max(1, len(item.payload) // 2)],
                        item.chunk_sector, item.chunk_sectors)
                  for item in blocks]
    else:
        blocks = _pad_blocks(blocks)

    return assemble(blocks, packed=(kind == "packed-tables"),
                    inline_tables=(kind == "hdiutil-layout"),
                    shuffled=(kind == "shuffled-blocks"),
                    keep_koly=(kind != "missing-koly"))


def _pad_blocks(blocks):
    """Pad each compressed payload up to a whole sector.

    A DMG's data fork is a run of independently compressed chunks, each one
    starting where the previous one ended, and a chunk has to decode to exactly
    the sectors it declares. hdiutil pads the last block so the next one begins
    on a sector boundary; doing the same here keeps the decoded size equal to
    sectors * 512 and makes the data fork sector aligned, which is what the
    alignment rule for the block tables then builds on.
    """
    padded = []
    for item in blocks:
        payload = item.payload
        target = item.chunk_sectors * SECTOR
        if len(payload) < target:
            payload += b"\0" * (target - len(payload))
        elif len(payload) > target and item.label.startswith("disk image"):
            raise ValueError(
                f"{item.label}: compressed payload is {len(payload)} bytes but the block "
                f"declares only {item.chunk_sectors * SECTOR}; fix the fixture geometry")
        padded.append(Block(item.label, item.table_id, item.first_sector,
                            item.sector_count, payload, item.chunk_sector,
                            item.chunk_sectors))
    return padded


def assemble(blocks, packed=False, inline_tables=False, shuffled=False,
             keep_koly=True):
    """Concatenate payloads, then place the mish tables inside the plist.

    Each table's DataOffset is its position relative to the start of the plist,
    which is how hdiutil labels them (its first table carries 0).  Tables are
    padded to whole sectors so each one begins where the previous ended plus a
    sector, matching what a reader walking the tables expects.  `packed` skips
    that padding, so later tables land mid-sector.

    `inline_tables` models hdiutil, which keeps every table inside the XML
    plist and records no table area at all, so each DataOffset stays zero.

    `shuffled` writes the blocks of one partition into the data fork in the
    wrong order.  The reader then has to look for the block of a sector behind
    the blocks that follow it, which is the defect that shipped as 0.11.2.
    """
    data_fork = bytearray()
    order = list(range(len(blocks)))
    if shuffled:
        # Swap the blocks of the payload partition inside the data fork.
        image_blocks = [index for index, item in enumerate(blocks)
                        if item.label.startswith("disk image")]
        for first, second in zip(image_blocks, image_blocks[1:]):
            order[first], order[second] = order[second], order[first]
    placement = {}
    for index in order:
        item = blocks[index]
        placement[index] = (len(data_fork), len(item.payload))
        data_fork += item.payload
    chunk_records = []
    for index, item in enumerate(blocks):
        offset, length = placement[index]
        chunk_records.append((item, (CHUNK_ZLIB, item.chunk_sector,
                                     item.chunk_sectors, offset, length)))
    # hdiutil rounds the data fork up to a sector before the tables start.
    data_fork += b"\0" * (align_up(len(data_fork), SECTOR) - len(data_fork))

    groups = {}
    for item, record in chunk_records:
        groups.setdefault(item.label, []).append((item, record))

    xml_offset = len(data_fork)
    first_table = xml_offset

    positions = []
    cursor = first_table
    for label in groups:
        positions.append(cursor)
        cursor += MISH_HEADER + CHUNK_SIZE * (len(groups[label]) + 1)
        if not packed:
            cursor = align_up(cursor, SECTOR)

    def render(offset_of):
        entries = []
        for (label, chunks), position in zip(groups.items(), positions):
            item = chunks[0][0]
            data_offset = 0 if inline_tables else position - offset_of
            entries.append({
                "Attributes": "0x0050",
                "CFName": label,
                "Data": mish_table(item.first_sector, item.sector_count,
                                   [record for _, record in chunks],
                                   data_offset, pad_to_sector=not packed),
                "ID": str(item.table_id),
                "Name": label,
            })
        return entries

    # One pass is exact: table lengths depend only on the chunk counts.
    entries = render(xml_offset)
    plist_blob = plistlib.dumps({"resource-fork": {"blkx": entries}},
                                fmt=plistlib.FMT_XML, sort_keys=True)

    blob = bytearray(data_fork)
    blob += plist_blob

    if keep_koly:
        # The trailer is the last 512 bytes of the file, as on a real image.
        gap = align_up(len(blob) + KOLY_SIZE, SECTOR) - len(blob) - KOLY_SIZE
        blob += b"\0" * max(0, gap)
        koly = bytearray(KOLY_SIZE)
        koly[0:4] = b"koly"
        struct.pack_into(">I", koly, 4, 4)                       # version 4
        struct.pack_into(">I", koly, 8, KOLY_SIZE)               # headerSize
        struct.pack_into(">I", koly, 12, 1)                      # flags
        struct.pack_into(">QQ", koly, 24, 0, xml_offset)         # data fork offset/length
        struct.pack_into(">QQ", koly, 216, xml_offset, len(plist_blob))
        blob += bytes(koly)
    return bytes(blob)


# --------------------------------------------------------------------------
# Self-check used by the test suite and by --check
# --------------------------------------------------------------------------
def inspect(path):
    """Re-read a built fixture and describe what the verifier will see.

    Returns a dict with the koly fields, each blkx table's file position, first
    sector and sector count, and the catalog names found by walking the HFS+
    catalog the same way verify-macos-dmg.py does.
    """
    with open(path, "rb") as handle:
        blob = handle.read()
    info = {"size": len(blob), "koly": None, "tables": [], "catalog": None}
    koly = None
    if len(blob) >= KOLY_SIZE and blob[-KOLY_SIZE:-KOLY_SIZE + 4] == b"koly":
        trailer = blob[-KOLY_SIZE:]
        koly = {
            "version": struct.unpack_from(">I", trailer, 4)[0],
            "headerSize": struct.unpack_from(">I", trailer, 8)[0],
            "flags": struct.unpack_from(">I", trailer, 12)[0],
            "dataForkOffset": struct.unpack_from(">Q", trailer, 24)[0],
            "dataForkLength": struct.unpack_from(">Q", trailer, 32)[0],
            "xmlOffset": struct.unpack_from(">Q", trailer, 216)[0],
            "xmlLength": struct.unpack_from(">Q", trailer, 224)[0],
        }
        info["koly"] = koly
        start = koly["xmlOffset"]
    else:
        start = blob.rfind(b"<?xml")
    end = blob.find(b"</plist>", start)
    if start < 0 or end < 0:
        return info
    resource = plistlib.loads(blob[start:end + len(b"</plist>")])
    for entry in resource.get("resource-fork", {}).get("blkx", []):
        raw = bytes(entry.get("Data", b""))
        first, count = struct.unpack_from(">QQ", raw, 8)
        data_offset = struct.unpack_from(">Q", raw, 24)[0]
        info["tables"].append({
            "name": entry.get("CFName"),
            "position": start + data_offset,
            "first": first,
            "count": count,
            "end_marker": struct.unpack_from(">I", raw, len(raw) - 40)[0] == CHUNK_END,
        })
    info["data_start"] = koly["xmlOffset"] if koly else start
    image = payload_image(blob, info["tables"])
    if image is not None:
        info["catalog"] = hfs_catalog_names(image)
    return info


def payload_image(blob, tables):
    """Decode the payload partition (the one with the most sectors)."""
    if not tables:
        return None
    biggest = max(tables, key=lambda table: table["count"])
    if not biggest["count"]:
        return None
    start, resource = _load_plist(blob)
    entry = next((item for item in resource["resource-fork"]["blkx"]
                  if item.get("CFName") == biggest["name"]), None)
    if entry is None:
        return None
    raw = bytes(entry["Data"])
    image = bytearray(biggest["count"] * SECTOR)
    cursor = MISH_HEADER
    while cursor + CHUNK_SIZE <= len(raw):
        kind, _, sector, sectors, offset, length = struct.unpack_from(">IIQQQQ", raw, cursor)
        cursor += CHUNK_SIZE
        if kind == CHUNK_END:
            break
        if not sectors:
            continue
        try:
            payload = zlib.decompress(blob[offset:offset + length])
        except zlib.error:
            return None
        begin = sector * SECTOR
        image[begin:begin + len(payload)] = payload
    return bytes(image)


def _load_plist(blob):
    """Return (plist start, parsed plist) for a DMG that still has its tables."""
    if len(blob) >= KOLY_SIZE and blob[-KOLY_SIZE:-KOLY_SIZE + 4] == b"koly":
        start = struct.unpack_from(">Q", blob, len(blob) - KOLY_SIZE + 216)[0]
    else:
        start = blob.rfind(b"<?xml")
    end = blob.find(b"</plist>", start)
    return start, plistlib.loads(blob[start:end + len(b"</plist>")])


def hfs_catalog_names(image):
    """Walk the catalog the way verify-macos-dmg.py does; return the names seen."""
    if image[1024:1026] != b"H+":
        return None
    block_size = struct.unpack_from(">I", image, 1024 + 40)[0]
    record = image[1024 + 372:1024 + 452]
    length = struct.unpack_from(">Q", record, 0)[0]
    start = struct.unpack_from(">I", record, 16)[0] * block_size
    fork = image[start:start + length]
    node_size = HFS_NODE
    if len(fork) >= 42 and fork[8] == 1:
        node_size = struct.unpack_from(">I", fork, 28)[0] or HFS_NODE
    names = set()
    for index in range(len(fork) // node_size):
        node = fork[index * node_size:(index + 1) * node_size]
        if node[8] != 0:                      # 0 == kBTLeafNode
            continue
        num_records = struct.unpack_from(">H", node, 10)[0]
        for record_index in range(num_records):
            offset = struct.unpack_from(">H", node, node_size - 2 * (record_index + 1))[0]
            if offset + 2 > len(node):
                continue
            key_length = struct.unpack_from(">H", node, offset)[0]
            key = node[offset:offset + 2 + key_length]
            if len(key) < 8:
                continue
            name_length = struct.unpack_from(">H", key, 6)[0]
            names.add(key[8:8 + name_length * 2].decode("utf-16-be", "replace"))
    return names


def build(kind, output, arch="arm64"):
    """Build one fixture at `output`; raises ValueError for an unknown kind."""
    write_bytes(output, build_dmg(kind, arch))
    return output


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Build a synthetic UDZO disk image for verify-macos-dmg.py.")
    parser.add_argument("--kind", required=True, choices=KINDS)
    parser.add_argument("--output", required=True, help="path of the .dmg to write")
    parser.add_argument("--arch", default="arm64", choices=("arm64", "x64"))
    parser.add_argument("--check", action="store_true",
                        help="print the structure the verifier will read")
    args = parser.parse_args(argv)
    try:
        build(args.kind, args.output, args.arch)
    except Exception as error:                 # noqa: BLE001 - surface the reason
        print(f"make-test-dmg: {error}", file=sys.stderr)
        return 1
    print(f"wrote {args.output} ({args.kind}, {args.arch})")
    if args.check:
        print(inspect(args.output))
    return 0


if __name__ == "__main__":
    sys.exit(main())
