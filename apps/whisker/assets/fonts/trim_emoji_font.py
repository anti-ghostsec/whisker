"""Makes "Whisker Emoji" from Noto Color Emoji: the same colour glyphs, but
without ordinary text characters (space, digits, # * © ® ™ and the rest of
Latin-1), so it can come first in a font list without taking over text, and
with the line-height metrics of a reference text font.

Usage: trim_emoji_font.py NotoColorEmoji.ttf DejaVuSans.ttf out.ttf
Standard library only (no fontTools)."""
import struct, sys

def read_tables(data):
    num = struct.unpack(">H", data[4:6])[0]
    tables = {}
    for i in range(num):
        tag, chk, off, ln = struct.unpack(">4sIII", data[12 + 16 * i: 28 + 16 * i])
        tables[tag.decode("latin-1")] = data[off:off + ln]
    return data[:4], tables

def checksum(b):
    b = b + b"\0" * (-len(b) % 4)
    return sum(struct.unpack(">%dI" % (len(b) // 4), b)) & 0xFFFFFFFF

def cmap_mappings(cmap):
    """All code point -> glyph mappings from format 4 and 12 subtables."""
    out = {}
    n = struct.unpack(">H", cmap[2:4])[0]
    for i in range(n):
        plat, enc, off = struct.unpack(">HHI", cmap[4 + 8 * i: 12 + 8 * i])
        fmt = struct.unpack(">H", cmap[off:off + 2])[0]
        if fmt == 12:
            ngroups = struct.unpack(">I", cmap[off + 12: off + 16])[0]
            for g in range(ngroups):
                s, e, gid = struct.unpack(">III", cmap[off + 16 + 12 * g: off + 28 + 12 * g])
                for cp in range(s, e + 1):
                    out.setdefault(cp, gid + cp - s)
        elif fmt == 4:
            segx2 = struct.unpack(">H", cmap[off + 6: off + 8])[0]
            seg = segx2 // 2
            ends = struct.unpack(">%dH" % seg, cmap[off + 14: off + 14 + segx2])
            starts = struct.unpack(">%dH" % seg, cmap[off + 16 + segx2: off + 16 + 2 * segx2])
            deltas = struct.unpack(">%dh" % seg, cmap[off + 16 + 2 * segx2: off + 16 + 3 * segx2])
            ro_off = off + 16 + 3 * segx2
            ranges = struct.unpack(">%dH" % seg, cmap[ro_off: ro_off + segx2])
            for k in range(seg):
                for cp in range(starts[k], ends[k] + 1):
                    if cp == 0xFFFF:
                        continue
                    if ranges[k] == 0:
                        gid = (cp + deltas[k]) & 0xFFFF
                    else:
                        p = ro_off + 2 * k + ranges[k] + 2 * (cp - starts[k])
                        gid = struct.unpack(">H", cmap[p:p + 2])[0]
                        if gid:
                            gid = (gid + deltas[k]) & 0xFFFF
                    if gid:
                        out.setdefault(cp, gid)
    return out

def build_cmap(mapping):
    """One format 12 subtable, listed for Unicode (0,4) and Windows (3,10)."""
    items = sorted(mapping.items())
    groups = []
    for cp, gid in items:
        if groups and cp == groups[-1][1] + 1 and gid == groups[-1][2] + (cp - groups[-1][0]):
            groups[-1][1] = cp
        else:
            groups.append([cp, cp, gid])
    sub = struct.pack(">HHIII", 12, 0, 16 + 12 * len(groups), 0, len(groups))
    sub += b"".join(struct.pack(">III", *g) for g in groups)
    head = struct.pack(">HH", 0, 2) + struct.pack(">HHI", 0, 4, 20) + struct.pack(">HHI", 3, 10, 20)
    return head + sub

def build_name(family):
    """A minimal name table (Windows, English) for `family`."""
    names = {0: "Copyright 2013 Google LLC; trimmed for Whisker", 1: family, 2: "Regular",
             3: family.replace(" ", "") + "-Regular", 4: family, 5: "Version 1.0",
             6: family.replace(" ", "") + "-Regular", 13: "SIL Open Font License, Version 1.1"}
    recs, strings = [], b""
    for nid, s in sorted(names.items()):
        enc = s.encode("utf-16-be")
        recs.append(struct.pack(">HHHHHH", 3, 1, 0x409, nid, len(enc), len(strings)))
        strings += enc
    return struct.pack(">HHH", 0, len(recs), 6 + 12 * len(recs)) + b"".join(recs) + strings

def metrics(tables):
    upem = struct.unpack(">H", tables["head"][18:20])[0]
    asc, desc, gap = struct.unpack(">hhh", tables["hhea"][4:10])
    return upem, asc, desc, gap

def main(src, ref, dst):
    sfnt, t = read_tables(open(src, "rb").read())
    _, rt = read_tables(open(ref, "rb").read())
    mapping = cmap_mappings(t["cmap"])
    before = len(mapping)
    # Text characters stay with the text font: everything below U+2000 except
    # the joiners emoji sequences need, plus ™ and the like.
    # Also left to the text font: symbols Whisker draws as UI icons
    # (⚙ settings, ▶ ◀ play, ↩ ↪ forward/back).
    text_symbols = (0x2122, 0x2139, 0x2194, 0x2195, 0x2699, 0x25B6, 0x25C0, 0x21A9, 0x21AA)
    keep = lambda cp: cp >= 0x2000 and cp not in text_symbols or cp in (0x200D, 0xFE0F, 0x20E3)
    mapping = {cp: g for cp, g in mapping.items() if keep(cp)}
    t["cmap"] = build_cmap(mapping)
    t["name"] = build_name("Whisker Emoji")
    # Line height like the text font: same ascent/descent/gap per em.
    upem, *_ = metrics(t)
    rupem, rasc, rdesc, rgap = metrics(rt)
    asc, desc, gap = (round(v * upem / rupem) for v in (rasc, rdesc, rgap))
    hhea = bytearray(t["hhea"])
    hhea[4:10] = struct.pack(">hhh", asc, desc, gap)
    t["hhea"] = bytes(hhea)
    if "OS/2" in t:
        os2 = bytearray(t["OS/2"])
        os2[68:74] = struct.pack(">hhh", asc, desc, gap)        # sTypo*
        os2[74:78] = struct.pack(">HH", asc, -desc)              # usWin*
        os2[62:64] = struct.pack(">H", struct.unpack(">H", os2[62:64])[0] & ~0x80)  # USE_TYPO_METRICS off
        t["OS/2"] = bytes(os2)
    # Write the font back out, with fresh checksums.
    head = bytearray(t["head"])
    head[8:12] = b"\0\0\0\0"
    t["head"] = bytes(head)
    tags = sorted(t)
    n = len(tags)
    es = max(1 << (n.bit_length() - 1), 1)
    hdr = sfnt + struct.pack(">HHHH", n, es * 16, es.bit_length() - 1, n * 16 - es * 16)
    off = 12 + 16 * n
    dir_, body = b"", b""
    for tag in tags:
        b = t[tag]
        dir_ += struct.pack(">4sIII", tag.encode("latin-1"), checksum(b), off + len(body), len(b))
        body += b + b"\0" * (-len(b) % 4)
    font = bytearray(hdr + dir_ + body)
    adj = (0xB1B0AFBA - checksum(bytes(font))) & 0xFFFFFFFF
    hoff = off + body.find(t["head"]) if False else None
    # head is at its directory offset:
    for i in range(n):
        tag, _, o, _ = struct.unpack(">4sIII", font[12 + 16 * i: 28 + 16 * i])
        if tag == b"head":
            font[o + 8: o + 12] = struct.pack(">I", adj)
    open(dst, "wb").write(font)
    print(f"{before} -> {len(mapping)} mapped characters; line metrics {asc}/{desc}/{gap} per {upem}")

if __name__ == "__main__":
    main(*sys.argv[1:4])
