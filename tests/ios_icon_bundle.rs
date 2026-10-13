//! iOS (`--platform iphoneos`) `.icon` compilation.
//!
//! `tests/fixtures/ios_icon/` holds two synthetic bundles — `props.icon`
//! (two groups exercising per-layer position/blend/opacity/fill and per-group
//! lighting/blur/specular/translucency/shadow) and `plain.icon` (one non-glass
//! group, whose tinted render Apple stores as a GA8 + GA16 gamut pair) — and
//! the structure Xcode 26.6's actool produced for
//! `actool props.icon plain.icon --platform iphoneos
//!  --minimum-deployment-target 17.0 --app-icon props
//!  --alternate-app-icon plain --target-device iphone --target-device ipad`
//! (`apple_props_plain.json`, from `tools/compare_ios_icon_car.py --golden`,
//! and the partial plist). The CAR must match it rendition key for rendition
//! key, with identical TLVs and structural payloads; only the rendered
//! pixels and the random part of the image names differ.

use actool::icon_bundle::ios::{compile_icon_bundles, render_appearances, IosIconOptions};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ios_icon").join(name)
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes(b[o..o + 2].try_into().unwrap())
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

/// Minimal BOM reader: named blocks and fixed-key tree walking.
struct Car {
    data: Vec<u8>,
    blocks: HashMap<u32, (u32, u32)>,
    named: HashMap<String, u32>,
}

impl Car {
    fn open(path: &Path) -> Car {
        let data = fs::read(path).unwrap();
        assert_eq!(&data[..8], b"BOMStore");
        let (io, il) = (be32(&data, 16) as usize, be32(&data, 20) as usize);
        let idx = &data[io..io + il];
        let mut blocks = HashMap::new();
        for i in 0..be32(idx, 0) {
            let o = 4 + i as usize * 8;
            blocks.insert(i, (be32(idx, o), be32(idx, o + 4)));
        }
        let (vo, vl) = (be32(&data, 24) as usize, be32(&data, 28) as usize);
        let vars = &data[vo..vo + vl];
        let mut named = HashMap::new();
        let mut p = 4;
        for _ in 0..be32(vars, 0) {
            let block = be32(vars, p);
            let len = vars[p + 4] as usize;
            named.insert(String::from_utf8_lossy(&vars[p + 5..p + 5 + len]).to_string(), block);
            p += 5 + len;
        }
        Car { data, blocks, named }
    }

    fn block(&self, i: u32) -> &[u8] {
        let (a, l) = self.blocks[&i];
        &self.data[a as usize..(a + l) as usize]
    }

    fn named_block(&self, name: &str) -> &[u8] {
        self.block(self.named[name])
    }

    fn tree(&self, name: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        fn walk(car: &Car, i: u32, out: &mut Vec<(Vec<u8>, Vec<u8>)>) {
            let b = car.block(i);
            let (leaf, n) = (be16(b, 0), be16(b, 2) as usize);
            if leaf != 0 {
                for k in 0..n {
                    let o = 12 + k * 8;
                    out.push((car.block(be32(b, o + 4)).to_vec(), car.block(be32(b, o)).to_vec()));
                }
            } else {
                walk(car, be32(b, 12), out);
                for k in 0..n {
                    walk(car, be32(b, 16 + k * 8 + 4), out);
                }
            }
        }
        let root = be32(self.named_block(name), 8);
        let mut out = Vec::new();
        walk(self, root, &mut out);
        out
    }

    fn keyformat(&self) -> Vec<u16> {
        let kf = self.named_block("KEYFORMAT");
        (0..le32(kf, 8) as usize).map(|i| le32(kf, 12 + i * 4) as u16).collect()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Rendition key (hex) → (layout, pixel format, flags, TLV hex, payload hex).
fn renditions(car: &Car) -> BTreeMap<String, (u16, String, u32, String, String)> {
    car.tree("RENDITIONS")
        .into_iter()
        .map(|(k, v)| {
            let tlv_len = le32(&v, 168) as usize;
            let rend_len = le32(&v, 180) as usize;
            (
                hex(&k),
                (
                    le16(&v, 36),
                    hex(&v[24..28]),
                    le32(&v, 8),
                    hex(&v[184..184 + tlv_len]),
                    hex(&v[184 + tlv_len..184 + tlv_len + rend_len]),
                ),
            )
        })
        .collect()
}

fn options<'a>(
    app_icon: Option<&'a str>,
    alternates: &'a [String],
    devices: &'a [String],
    plist: &'a Path,
) -> IosIconOptions<'a> {
    IosIconOptions {
        platform: "iphoneos",
        min_deploy: "17.0",
        app_icon,
        alternate_app_icons: alternates,
        include_all_app_icons: false,
        target_devices: devices,
        info_plist_path: Some(plist),
    }
}

fn both_devices() -> Vec<String> {
    vec!["iphone".to_string(), "ipad".to_string()]
}

fn bundles() -> Vec<PathBuf> {
    vec![fixture("props.icon"), fixture("plain.icon")]
}

#[test]
fn matches_apple_structure_for_primary_and_alternate() {
    let out = tempfile::tempdir().unwrap();
    let plist = out.path().join("partial.plist");
    let alternates = vec!["plain".to_string()];
    let devices = both_devices();
    let files = compile_icon_bundles(
        &bundles(),
        out.path(),
        &options(Some("props"), &alternates, &devices, &plist),
    )
    .unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names,
        ["Assets.car", "partial.plist", "props60x60@2x.png", "props76x76@2x~ipad.png"]
    );

    let golden: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("apple_props_plain.json")).unwrap())
            .unwrap();
    let car = Car::open(&out.path().join("Assets.car"));

    let kf: Vec<u64> = car.keyformat().iter().map(|&a| a as u64).collect();
    assert_eq!(serde_json::json!(kf), golden["keyformat"]);

    let hdr = car.named_block("CARHEADER");
    assert_eq!(le32(hdr, 4), 975, "CoreUI version");
    assert_eq!(le32(hdr, 432), 2, "iOS key semantics");

    let appearances: BTreeMap<String, u64> = car
        .tree("APPEARANCEKEYS")
        .into_iter()
        .map(|(k, v)| (String::from_utf8_lossy(&k).to_string(), le16(&v, 0) as u64))
        .collect();
    assert_eq!(serde_json::json!(appearances), golden["appearances"]);

    // Facets: (element, part, identifier). Apple's main facet part flips
    // between 220 and 245 from run to run, so either is accepted there.
    let facets: BTreeMap<String, Vec<u16>> = car
        .tree("FACETKEYS")
        .into_iter()
        .map(|(k, v)| {
            let n = le16(&v, 4) as usize;
            let attrs: HashMap<u16, u16> =
                (0..n).map(|i| (le16(&v, 6 + i * 4), le16(&v, 8 + i * 4))).collect();
            (String::from_utf8_lossy(&k).to_string(), vec![attrs[&1], attrs[&2], attrs[&17]])
        })
        .collect();
    let golden_facets = golden["facets"].as_object().unwrap();
    assert_eq!(facets.len(), golden_facets.len());
    for (name, ours) in &facets {
        let apple: Vec<u16> = golden_facets[name]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u16)
            .collect();
        if !name.contains('/') && ours[1] != apple[1] {
            assert!([220, 245].contains(&ours[1]), "main facet {name} part {}", ours[1]);
            assert_eq!((ours[0], ours[2]), (apple[0], apple[2]), "facet {name}");
        } else {
            assert_eq!(ours, &apple, "facet {name}");
        }
    }

    let ours = renditions(&car);
    let apple = golden["renditions"].as_object().unwrap();
    let our_keys: Vec<&String> = ours.keys().collect();
    let apple_keys: Vec<&String> = apple.keys().collect();
    assert_eq!(our_keys, apple_keys, "rendition keys");
    for (key, (layout, pf, flags, tlv, payload)) in &ours {
        let a = &apple[key];
        assert_eq!(*layout as u64, a["layout"].as_u64().unwrap(), "layout of {key}");
        assert_eq!(pf, a["pixel_format"].as_str().unwrap(), "pixel format of {key}");
        assert_eq!(*flags as u64, a["flags"].as_u64().unwrap(), "flags of {key}");
        assert_eq!(tlv, a["tlv"].as_str().unwrap(), "TLVs of {key}");
        if let Some(p) = a.get("payload") {
            assert_eq!(payload, p.as_str().unwrap(), "payload of {key}");
        }
        if let Some(p) = a.get("payload_head") {
            assert_eq!(&payload[..32], p.as_str().unwrap(), "codec header of {key}");
        }
    }

    assert_eq!(
        fs::read_to_string(&plist).unwrap(),
        fs::read_to_string(fixture("apple_props_plain.plist")).unwrap()
    );
    for (file, px) in [("props60x60@2x.png", 120), ("props76x76@2x~ipad.png", 152)] {
        let img = image::open(out.path().join(file)).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (px, px));
        assert!(img.pixels().all(|p| p[3] == 255), "{file} is opaque");
    }
}

#[test]
fn iphone_only_target_drops_ipad_name_and_alternates() {
    let out = tempfile::tempdir().unwrap();
    let plist = out.path().join("p.plist");
    let alternates = vec!["plain".to_string()];
    let devices = vec!["iphone".to_string()];
    compile_icon_bundles(&bundles(), out.path(), &options(Some("props"), &alternates, &devices, &plist))
        .unwrap();
    let text = fs::read_to_string(&plist).unwrap();
    let (phone, pad) = text.split_once("CFBundleIcons~ipad").unwrap();
    assert!(phone.contains("CFBundleAlternateIcons") && phone.contains("<string>plain</string>"));
    assert!(!pad.contains("CFBundleAlternateIcons"));
    assert!(!pad.contains("CFBundleIconName"), "Apple names the iPad icon only for iPad targets");
    assert!(pad.contains("<string>props76x76</string>"));
    // Both idioms are still compiled into the catalog.
    let car = Car::open(&out.path().join("Assets.car"));
    let idiom_pos = car.keyformat().iter().position(|&a| a == 15).unwrap();
    let idioms: std::collections::BTreeSet<u16> = car
        .tree("RENDITIONS")
        .iter()
        .map(|(k, _)| le16(k, idiom_pos * 2))
        .collect();
    assert_eq!(idioms.into_iter().collect::<Vec<_>>(), vec![0, 1, 2]);
}

#[test]
fn selection_rules_follow_apple() {
    let devices = both_devices();
    // No bundle matches --app-icon and none is an alternate: no catalog.
    let out = tempfile::tempdir().unwrap();
    let plist = out.path().join("p.plist");
    let files =
        compile_icon_bundles(&bundles(), out.path(), &options(Some("Other"), &[], &devices, &plist))
            .unwrap();
    assert!(!out.path().join("Assets.car").exists());
    assert_eq!(files.len(), 1);
    assert!(fs::read_to_string(&plist).unwrap().contains("<dict/>"));

    // Only an alternate: it is compiled, but without a primary the plist is
    // empty and no loose PNGs are written.
    let out = tempfile::tempdir().unwrap();
    let plist = out.path().join("p.plist");
    let alternates = vec!["plain".to_string()];
    compile_icon_bundles(&bundles(), out.path(), &options(None, &alternates, &devices, &plist))
        .unwrap();
    let car = Car::open(&out.path().join("Assets.car"));
    let facets: Vec<String> = car
        .tree("FACETKEYS")
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k).to_string())
        .collect();
    assert!(facets.contains(&"plain".to_string()) && !facets.contains(&"props".to_string()));
    assert!(fs::read_to_string(&plist).unwrap().contains("<dict/>"));
    assert!(!out.path().join("plain60x60@2x.png").exists());
}

#[test]
fn renders_differ_per_appearance_and_tinted_is_gray() {
    let renders = render_appearances(&fixture("props.icon")).unwrap();
    let tags: Vec<&str> = renders.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags, ["UIAppearanceAny", "UIAppearanceDark", "ISAppearanceTintable"]);
    let (any, dark, tinted) = (&renders[0].1, &renders[1].1, &renders[2].1);
    assert_eq!(any.len(), 1024 * 1024 * 4);
    let mean = |a: &[u8], b: &[u8]| {
        a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).abs()).sum::<f64>() / a.len() as f64
    };
    assert!(mean(any, dark) > 10.0, "dark background differs from the solid light fill");
    assert!(tinted.chunks(4).all(|p| p[0] == p[1] && p[1] == p[2] && p[3] == 255));
    // The tinted background is black; the solid fill shows in Any.
    let centre_bg = |img: &[u8]| img[(40 * 1024 + 512) * 4..(40 * 1024 + 512) * 4 + 3].to_vec();
    assert_eq!(centre_bg(tinted), vec![0, 0, 0]);
    assert_eq!(centre_bg(any), vec![51, 102, 153]);
}

#[test]
fn cli_compiles_several_icon_documents_for_ios_only() {
    let exe = env!("CARGO_BIN_EXE_actool");
    let out = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(exe)
        .arg(fixture("props.icon"))
        .arg(fixture("plain.icon"))
        .args(["--compile"])
        .arg(out.path())
        .args(["--platform", "iphoneos", "--minimum-deployment-target", "17.0"])
        .args(["--app-icon", "props", "--alternate-app-icon", "plain"])
        .args(["--target-device", "iphone", "--target-device", "ipad"])
        .arg("--output-partial-info-plist")
        .arg(out.path().join("partial.plist"))
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    assert!(out.path().join("Assets.car").exists());
    assert_eq!(
        fs::read_to_string(out.path().join("partial.plist")).unwrap(),
        fs::read_to_string(fixture("apple_props_plain.plist")).unwrap()
    );

    let status = std::process::Command::new(exe)
        .arg(fixture("props.icon"))
        .arg(fixture("plain.icon"))
        .args(["--compile"])
        .arg(out.path().join("mac"))
        .args(["--platform", "macosx", "--app-icon", "props"])
        .output()
        .unwrap();
    assert!(!status.status.success(), "several documents are iOS-only");
}
