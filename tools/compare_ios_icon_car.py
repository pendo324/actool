#!/usr/bin/env python3
"""Structural diff of two iOS IconComposer .car files (ours vs Apple's).

Compares what CoreUI keys lookups on: KEYFORMAT, APPEARANCEKEYS, FACETKEYS,
BITMAPKEYS, the full set of rendition keys, and per rendition its layout,
pixel format, flags, size and (for iconstack / IconGroup / Color / Gradient /
MultiSized) the exact TLV and payload bytes. Pre-rendered pixels and the
random UUID Apple embeds in sized-icon names are ignored.

Usage:
    python3 tools/compare_ios_icon_car.py ours.car apple.car [--json]
    python3 tools/compare_ios_icon_car.py --golden apple.car > golden.json
Exit status is 0 when the structure matches. `--golden` writes the reference
consumed by tests/ios_icon_bundle.rs: rendition keys (hex) with their layout,
pixel format, flags and TLVs, plus payloads for the structural renditions.
"""

import json
import os
import re
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import compare_car as cc  # noqa: E402

UUID_RE = re.compile(r"_[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}-[0-9]+-[0-9A-F]+\.png$")
STRUCTURAL_LAYOUTS = {"iconstack", "icon_group", "color", "gradient", "multisize"}


def load(path):
    data = open(path, "rb").read()
    _blocks, named, read_block = cc._read_bom(data)
    kf = cc._parse_keyformat(read_block, named)
    car = cc.parse_car(path)
    rends = {}
    tree = read_block(named["RENDITIONS"])
    root = struct.unpack(">I", tree[8:12])[0]
    for kb, vb in cc._walk_tree(read_block, root):
        csi = cc._parse_csi(vb)
        if csi is None:
            continue
        n = len(kb) // 2
        key = tuple(zip(kf, struct.unpack_from("<%dH" % n, kb)))
        name = UUID_RE.sub("_<uuid>.png", csi["name"])
        info = {
            "name": name,
            "layout": csi["layout_name"],
            "pixel_format": csi["pixel_format"].hex(),
            "flags": csi["flags"],
            "size": [csi["width"], csi["height"]],
            "scale": csi["scale"],
            "tlv_tags": csi["tlv_tags"],
            "celm": None if csi["celm"] is None else [csi["celm"]["ver"], csi["celm"]["comp"], csi["celm"]["payload_len"]],
        }
        if csi["layout_name"] in STRUCTURAL_LAYOUTS:
            info["tlv"] = vb[184:184 + csi["tlv_len"]].hex()
            info["payload"] = csi["rend_raw"].hex()
        rends[key] = info
    return {
        "keyformat": kf,
        "appearances": car["appearance_keys"],
        "facets": {k: v for k, v in car["facets"].items()},
        "bitmapkeys": car["bitmap_keys"],
        "carheader": {k: car["carheader"].get(k) for k in ("coreuiVersion", "keySemantics", "schemaVersion", "renditionCount")},
        "metadata": {k: car["metadata"].get(k) for k in ("deploymentPlatform", "deploymentPlatformVersion")},
        "renditions": rends,
    }


def key_str(key):
    return ",".join("%d=%d" % kv for kv in key)


def compare(a, b):
    problems = []
    for field in ("keyformat", "appearances", "carheader", "metadata"):
        if a[field] != b[field]:
            problems.append(f"{field}: ours {a[field]} vs apple {b[field]}")
    fa, fb = a["facets"], b["facets"]
    for name in sorted(set(fa) | set(fb)):
        x, y = fa.get(name), fb.get(name)
        # Apple's main-facet part flips between 220 and 245 across identical
        # runs (it takes whichever rendition it registers first), so either
        # is a match.
        if x and y and "/" not in name and {x.get(2), y.get(2)} <= {220, 245} \
                and {k: v for k, v in x.items() if k != 2} == {k: v for k, v in y.items() if k != 2}:
            continue
        if x != y:
            problems.append(f"facet {name}: ours {fa.get(name)} vs apple {fb.get(name)}")
    ba, bb = a["bitmapkeys"], b["bitmapkeys"]
    for ident in sorted(set(ba) | set(bb)):
        if ba.get(ident) != bb.get(ident):
            problems.append(f"bitmapkey {ident}: ours {ba.get(ident)} vs apple {bb.get(ident)}")
    ra, rb = a["renditions"], b["renditions"]
    only_a = sorted(set(ra) - set(rb))
    only_b = sorted(set(rb) - set(ra))
    for k in only_a:
        problems.append(f"rendition only in ours: {key_str(k)} {ra[k]['name']} {ra[k]['layout']}")
    for k in only_b:
        problems.append(f"rendition only in apple: {key_str(k)} {rb[k]['name']} {rb[k]['layout']}")
    for k in sorted(set(ra) & set(rb)):
        x, y = ra[k], rb[k]
        for f in sorted(set(x) | set(y)):
            if x.get(f) != y.get(f):
                problems.append(f"rendition {key_str(k)} {y['name']}: {f} ours {x.get(f)} vs apple {y.get(f)}")
    return {
        "renditions_ours": len(ra),
        "renditions_apple": len(rb),
        "matching_keys": len(set(ra) & set(rb)),
        "problems": problems,
    }


def golden(path):
    data = open(path, "rb").read()
    _blocks, named, read_block = cc._read_bom(data)
    car = cc.parse_car(path)
    tree = read_block(named["RENDITIONS"])
    root = struct.unpack(">I", tree[8:12])[0]
    rends = {}
    for kb, vb in cc._walk_tree(read_block, root):
        csi = cc._parse_csi(vb)
        if csi is None:
            continue
        entry = {
            "layout": struct.unpack_from("<H", vb, 36)[0],
            "pixel_format": vb[24:28].hex(),
            "flags": csi["flags"],
            "tlv": vb[184:184 + csi["tlv_len"]].hex(),
        }
        if csi["layout_name"] in STRUCTURAL_LAYOUTS:
            entry["payload"] = csi["rend_raw"].hex()
        elif csi["rend_raw"][:4] == b"MLEC":
            # Bitmap codec header: version, compression, KCBC chunk count.
            entry["payload_head"] = csi["rend_raw"][:16].hex()
        rends[kb.hex()] = entry
    return {
        "keyformat": car["keyformat"],
        "appearances": car["appearance_keys"],
        "facets": {k: [v.get(1), v.get(2), v.get(17)] for k, v in car["facets"].items()},
        "renditions": rends,
    }


def main():
    if sys.argv[1:2] == ["--golden"]:
        print(json.dumps(golden(sys.argv[2]), indent=1, sort_keys=True))
        return
    args = [x for x in sys.argv[1:] if not x.startswith("--")]
    if len(args) != 2:
        print(__doc__)
        sys.exit(2)
    report = compare(load(args[0]), load(args[1]))
    if "--json" in sys.argv:
        print(json.dumps(report, indent=2))
    else:
        print(f"renditions: ours {report['renditions_ours']}, apple {report['renditions_apple']}, "
              f"same keys {report['matching_keys']}")
        for p in report["problems"]:
            print("  " + p)
        print("STRUCTURE MATCHES" if not report["problems"] else f"{len(report['problems'])} differences")
    sys.exit(0 if not report["problems"] else 1)


if __name__ == "__main__":
    main()
