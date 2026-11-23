"""Independent cross-check of BootForge's output images (third implementation,
written separately from the Rust readers): GPT/MBR parsing with zlib's CRC-32,
and a FAT32 walker with long-name support. Usage: crosscheck.py <keep-folder>"""
import struct, sys, zlib, os, hashlib

keep = sys.argv[1]


def gpt(img):
    hdr = img[512:512 + 92]
    assert hdr[:8] == b"EFI PART", "no GPT signature"
    rev, hsize, hcrc, _, my, alt, first, last = struct.unpack_from("<IIIIQQQQ", hdr, 8)
    assert rev == 0x00010000 and hsize == 92
    h2 = bytearray(hdr); h2[16:20] = b"\0\0\0\0"
    assert zlib.crc32(bytes(h2)) == hcrc, "primary header CRC"
    ent_lba, n, esize, ecrc = struct.unpack_from("<QIII", hdr, 72)
    ents = img[ent_lba * 512: ent_lba * 512 + n * esize]
    assert zlib.crc32(ents) == ecrc, "entry array CRC"
    assert my == 1 and alt == len(img) // 512 - 1, "self/alternate LBA"
    # backup header
    b = img[alt * 512: alt * 512 + 92]
    assert b[:8] == b"EFI PART"
    bcrc = struct.unpack_from("<I", b, 16)[0]
    b2 = bytearray(b); b2[16:20] = b"\0\0\0\0"
    assert zlib.crc32(bytes(b2)) == bcrc, "backup header CRC"
    bent = struct.unpack_from("<Q", b, 72)[0]
    assert img[bent * 512: bent * 512 + n * esize] == ents, "backup entries differ"
    # protective MBR
    assert img[510:512] == b"\x55\xaa" and img[446 + 4] == 0xEE
    parts = []
    for i in range(n):
        e = ents[i * esize:(i + 1) * esize]
        if e[:16] != b"\0" * 16:
            s, l = struct.unpack_from("<QQ", e, 32)
            assert first <= s <= l <= last
            parts.append((e[:16], s * 512, (l - s + 1) * 512))
    return parts


def mbr(img):
    assert img[510:512] == b"\x55\xaa"
    parts = []
    for i in range(4):
        e = img[446 + 16 * i: 462 + 16 * i]
        if e[4]:
            start, count = struct.unpack_from("<II", e, 8)
            parts.append((e[0], e[4], start * 512, count * 512))
    return parts


class Fat32:
    def __init__(self, vol):
        self.v = vol
        b = vol[:512]
        assert b[510:512] == b"\x55\xaa"
        self.bps, self.spc, self.rsv, self.nfats = struct.unpack_from("<HBHB", b, 11)
        assert struct.unpack_from("<H", b, 17)[0] == 0, "root entries must be 0 on FAT32"
        assert struct.unpack_from("<H", b, 22)[0] == 0, "FATSz16 must be 0"
        self.total = struct.unpack_from("<I", b, 32)[0]
        self.fatsz = struct.unpack_from("<I", b, 36)[0]
        self.root = struct.unpack_from("<I", b, 44)[0]
        fsinfo, backup = struct.unpack_from("<HH", b, 48)
        assert b[82:90] == b"FAT32   "
        assert b[66] == 0x29
        self.label = b[71:82].decode("ascii").rstrip()
        self.hidden = struct.unpack_from("<I", b, 28)[0]
        assert vol[backup * self.bps:(backup + 1) * self.bps] == b[:self.bps], "backup boot sector"
        fi = vol[fsinfo * self.bps:(fsinfo + 1) * self.bps]
        assert fi[:4] == b"RRaA" and fi[484:488] == b"rrAa" and fi[508:512] == b"\0\0\x55\xaa"
        self.fsinfo_free = struct.unpack_from("<I", fi, 488)[0]
        self.data = self.rsv + self.nfats * self.fatsz
        self.clusters = (self.total - self.data) // self.spc
        assert self.clusters >= 65525, "too few clusters for FAT32"
        f0 = self.rsv * self.bps
        fat_bytes = self.fatsz * self.bps
        fat1 = vol[f0:f0 + fat_bytes]
        for k in range(1, self.nfats):
            assert vol[f0 + k * fat_bytes: f0 + (k + 1) * fat_bytes] == fat1, "FAT copies differ"
        self.fat = struct.unpack_from("<%dI" % (self.clusters + 2), fat1)
        assert self.fat[0] & 0x0FFFFFFF == 0x0FFFFFF8
        free = sum(1 for x in self.fat[2:] if x & 0x0FFFFFFF == 0)
        assert free == self.fsinfo_free, "FSInfo free count %d != %d" % (self.fsinfo_free, free)

    def chain(self, c):
        out = []
        while 2 <= c < 0x0FFFFFF8:
            assert c < self.clusters + 2
            out.append(c)
            assert len(out) <= self.clusters
            c = self.fat[c] & 0x0FFFFFFF
        return out

    def read_chain(self, c):
        cs = self.spc * self.bps
        return b"".join(self.v[(self.data + (k - 2) * self.spc) * self.bps:(self.data + (k - 2) * self.spc) * self.bps + cs] for k in self.chain(c))

    def listdir(self, cluster):
        raw = self.read_chain(cluster)
        lfn = []
        for i in range(0, len(raw), 32):
            e = raw[i:i + 32]
            if e[0] == 0:
                break
            if e[0] == 0xE5:
                lfn = []
                continue
            attr = e[11]
            if attr == 0x0F:
                lfn.append(e)
                continue
            if attr & 0x08:
                lfn = []
                continue
            name = None
            if lfn:
                csum = 0
                for ch in e[:11]:
                    csum = (((csum & 1) << 7) + (csum >> 1) + ch) & 0xFF
                parts = {}
                for l in lfn:
                    assert l[13] == csum, "LFN checksum"
                    chars = l[1:11] + l[14:26] + l[28:32]
                    parts[l[0] & 0x3F] = chars
                data = b"".join(parts[k] for k in sorted(parts))
                name = data.decode("utf-16-le").split("\0")[0]
            else:
                base = e[:8].decode("ascii").rstrip()
                ext = e[8:11].decode("ascii").rstrip()
                if e[12] & 0x08:
                    base = base.lower()
                if e[12] & 0x10:
                    ext = ext.lower()
                name = base + ("." + ext if ext else "")
            lfn = []
            first = (struct.unpack_from("<H", e, 20)[0] << 16) | struct.unpack_from("<H", e, 26)[0]
            size = struct.unpack_from("<I", e, 28)[0]
            yield name, bool(attr & 0x10), first, size

    def walk(self, cluster=None, prefix=""):
        for name, is_dir, first, size in self.listdir(cluster or self.root):
            if name in (".", ".."):
                continue
            path = prefix + name
            if is_dir:
                yield path, None
                yield from self.walk(first, path + "/")
            else:
                yield path, self.read_chain(first)[:size] if size else b""


def summarize(tag, files):
    names = sorted(p for p, d in files if d is not None)
    dirs = sorted(p for p, d in files if d is None)
    h = hashlib.sha256()
    for p, d in sorted((p, d) for p, d in files if d is not None):
        h.update(p.lower().encode()); h.update(d)
    print("%s: %d files, %d dirs, tree digest %s" % (tag, len(names), len(dirs), h.hexdigest()[:16]))
    return names


img = open(os.path.join(keep, "device-files-gpt.img"), "rb").read()
parts = gpt(img)
assert len(parts) == 1
tguid, off, size = parts[0]
assert tguid == bytes.fromhex("a2a0d0ebe5b9334487c068b6b72699c7"), "basic data type GUID (on-disk byte order)"
fs = Fat32(img[off:off + size])
assert fs.hidden == off // 512
files = list(fs.walk())
names = summarize("GPT image, label %r" % fs.label, files)
d = dict(files)
efi = [n for n in names if n.lower() == "efi/boot/bootx64.efi"]
assert efi and d[efi[0]][:2] == b"MZ", "EFI loader present and a PE file"
print("  loader:", efi[0], len(d[efi[0]]), "bytes;", "long names:", [n for n in names if len(n.split('/')[-1]) > 12][:3])

img = open(os.path.join(keep, "device-files-mbr.img"), "rb").read()
parts = mbr(img)
assert len(parts) == 1 and parts[0][0] == 0x80 and parts[0][1] == 0x0C, parts
_, _, off, size = parts[0]
fs = Fat32(img[off:off + size])
files = list(fs.walk())
names = summarize("MBR image, label %r" % fs.label, files)
d = dict(files)
wim = [n for n in names if n.lower() == "sources/install.wim"][0]
assert d[wim][:8] == b"MSWIM\0\0\0", "install.wim starts with the WIM magic"
print("  install.wim", len(d[wim]), "bytes")
# The self-test writes this image with every Windows Setup option on: the
# answer file must be there and be well-formed XML (parsed by Python's own parser).
import xml.etree.ElementTree as ET
answer = [n for n in names if n.lower() == "autounattend.xml"]
assert answer, "autounattend.xml is on the volume"
ns = "{urn:schemas-microsoft-com:unattend}"
root = ET.fromstring(d[answer[0]])
passes = [s.get("pass") for s in root.findall(ns + "settings")]
assert root.tag == ns + "unattend" and passes == ["windowsPE", "specialize", "oobeSystem"], passes
commands = [e.text for e in root.iter(ns + "Path")]
assert len(commands) == 4 and all(c.startswith("reg add HKLM") for c in commands), commands
print("  autounattend.xml", len(d[answer[0]]), "bytes, well-formed, passes:", ", ".join(passes))
whole = d[wim]

img = open(os.path.join(keep, "device-files-split.img"), "rb").read()
parts = gpt(img)
tguid, off, size = parts[0]
fs = Fat32(img[off:off + size])
files = list(fs.walk())
names = summarize("split image, label %r" % fs.label, files)
d = dict(files)
assert not any(n.lower() == "sources/install.wim" for n in names)
joined = b"".join(d["sources/" + n] for n in ["install.swm", "install2.swm", "install3.swm", "install4.swm"])
assert joined == whole, "swm parts do not add up to the wim written in the MBR run"
print("  4 parts add up to the same install.wim")

raw = open(os.path.join(keep, "device-image-mode.img"), "rb").read()
iso = open(os.path.join(keep, "linux-hybrid.iso"), "rb").read()
assert raw[:len(iso)] == iso and not any(raw[-1024 * 1024:]), "raw image mode"
assert raw[510:512] == b"\x55\xaa" and raw[0x8001:0x8006] == b"CD001"
print("image mode: device starts with the ISO (MBR signature + CD001 present), tail zeroed")
print("CROSSCHECK OK")
