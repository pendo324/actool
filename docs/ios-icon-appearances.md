# iOS `.icon` catalogs: appearances, renditions and parity

What Apple's actool (Xcode 26.6, `actool` 26.6 / 24765, on macOS 26.6.2)
emits when it compiles IconComposer `.icon` bundles for iOS, and how
`src/icon_bundle/ios.rs` reproduces it. Reference command:

```
xcrun actool primary.icon alt1.icon … --compile out --platform iphoneos \
  --minimum-deployment-target 17.0 --app-icon primary \
  --alternate-app-icon alt1 … --target-device iphone --target-device ipad \
  --output-partial-info-plist out/partial.plist
```

Dumps were taken with `xcrun assetutil --info`, `tools/compare_car.py`'s
parser (raw keys and TLVs) and a CoreUI decoder that writes every icon
rendition by key (`car_dump_keys.m`, a variant of Phoebus's
`new-icons/tools/car_dump.m` naming files `p<part>_a<appearance>_i<idiom>_g<gamut>`).
Inputs: the ten Phoebus bundles plus ~20 synthetic probe bundles that vary
one property at a time (`tests/fixtures/ios_icon/` keeps two of them).

## What Apple emits

### Files

* `Assets.car` holding every selected icon (the `--app-icon` bundle plus the
  `--alternate-app-icon` ones, or all bundles with `--include-all-app-icons`).
  Bundles named by neither are left out; if nothing is selected there is no
  `Assets.car` at all.
* For the primary icon only: `<name>60x60@2x.png` (120 px) and
  `<name>76x76@2x~ipad.png` (152 px), RGBA, the Any look.
* The partial plist:

  ```
  CFBundleIcons
    CFBundleAlternateIcons   {alt: {CFBundleIconName: alt}, …}   (if any; iPhone targeted)
    CFBundlePrimaryIcon      {CFBundleIconFiles: [<name>60x60], CFBundleIconName: <name>}
  CFBundleIcons~ipad
    CFBundleAlternateIcons   (same, only when iPad is a --target-device)
    CFBundlePrimaryIcon      {CFBundleIconFiles: [<name>60x60, <name>76x76],
                              CFBundleIconName: <name> (only when iPad is targeted)}
  ```

  No primary (only alternates) → `<dict/>`, though the alternates are still
  compiled. The phone and pad renditions and both loose PNGs are emitted
  whatever `--target-device` says.

Deployment targets 15.0–26.0 give the same set of renditions (the
EXTENDED_METADATA version string follows the flag); 12.0 adds a deployment-target
key column and duplicate Vector renditions (not implemented, Phoebus targets
17+).

### Catalog header

| field | value |
|---|---|
| CARHEADER CoreUI version / key semantics | 975 / 2 |
| EXTENDED_METADATA platform | `ios` |
| KEYFORMAT | `[7 appearance, 12 scale, 15 idiom, 16 subtype, 9 dimension2, 17 identifier, 1 element, 2 part, 10 state]` |
| … with a gray tinted icon | `24 displayGamut` inserted after 9 |
| APPEARANCEKEYS | `UIAppearanceAny 0`, `UIAppearanceDark 1`, `UIAppearanceLight 4`, `ISAppearanceTintable 10` |

There is no localization column and State (10) is always present, unlike
the appiconset key format. Attribute 24 is `kCRThemeDisplayGamutName`
(assetutil names it); on macOS this repository calls it the "variant axis".

### Renditions per icon (`P` = bundle stem)

| rendition | key | contents |
|---|---|---|
| Vector `image.svg` | element 85, part 181, id `P_Assets/<svg stem>` | the layer SVG, LZFSE-compressed DWAR (same as macOS) |
| Color / Named Gradient | parts 217 / 247 | the palette, same rules as macOS |
| MultiSized Image ×2 | idiom 1 and 2, part 218 | one entry `{1024×1024, index 1}` |
| Icon Image ×6 | appearance 0 / 1 / 10 × idiom 1 / 2, part 220, dim2 1, scale 1 | 1024×1024 opaque BGRA sRGB, KCBC-LZFSE (MLEC v3, 4 chunks of 341/341/341/1 rows), flags 0, name `P1024x1024_<UIAppearanceAny|UIAppearanceDark|ISAppearanceTintable>_<UUID>-<pid>-<hex>.png` |
| IconImageStack ×3 | appearance 1 / 4 / 10, part 245 | `P.iconstack`: background + groups back-to-front |
| IconGroup ×3 per group | appearance 1 / 4 / 10, part 246, id `P/<group>` | the group's layers back-to-front |

So the flattened images exist only at 1024 pt, for Any, Dark and Tintable —
these are what iOS 17/18 home screens use. There is **no** Clear / glass
image: iOS 26 renders every look (including clear and tinted glass) live
from the iconstack, IconGroups, Vectors and palette. The Light appearance
(4) exists only for the stacks; the flattened "light" image is the Any one.

FACETKEYS: `P` (part 245 *or* 220 — this flips between identical Apple runs,
it takes whichever rendition registers first; both work), `P/<group>`
(246), palette and layer facets. BITMAPKEYS follow the usual
per-identifier attribute masks.

#### Tinted image encoding

The tinted render is Rec. 709 luma of the composited icon over black. When
it comes out exactly gray Apple stores it as a pair split on displayGamut:
GA8 (`" 8AG"`, gray gamma 2.2, gamut 0) and GA16 (`"61AG"`, extended gray,
values ×257, gamut 1), and the catalog gains key column 24. That happens for
a single visible group with no glass layer and no shadow in the tinted
look; glass, any shadow, or a second group leave ±2 colour noise in Apple's
renderer and it stays BGRA (all ten Phoebus icons). Translucency, specular
and layer fills don't matter.

### iconstack / IconGroup TLVs

Both use TLVs 0x3F4 / 0x3FC / 0x3FD (+ blend/opacity 0x3EC, orientation
0x3EE, and `public.layeredimage` 0x3ED for the stack), each a count followed
by one entry per child:

* **0x3F4** (48 bytes/entry + 4-byte trailer): `u32 0, u32 0, i32 x, i32 y,
  u32 w, u32 h, u32 CGBlendMode, f32 opacity, u32 16, key{1:85, 2:part,
  17:id}`. For IconGroup layers `w×h` is the layer's point size
  (`native × layer scale × group scale`) and `(x, y)` its top-left in the
  1024 pt canvas, `(1024 − w)/2 + group scale × layer translation + group
  translation`, y down. Stack children have no geometry; groups carry the
  group's blend and opacity.
* **0x3FC** (variable, no trailer): `u32 flags, f32 blur, u32 len, char[len]`
  (NUL-terminated). In an IconGroup `flags` = glass (1/0) and the string is
  the facet of the layer's fill for that appearance (`P_Assets/Color-N`, or
  empty). In the stack `flags` = lighting (2 individual — the default, 0
  combined) and `blur` = blur-material.
* **0x3FD** (20 bytes/entry + 4-byte trailer): `u32 0, u32 specular,
  f32 translucency (0 when disabled), u32 shadow kind (none 0, layer-color 2,
  neutral 3), f32 shadow opacity`; zeros for layers and the background.

Everything is resolved per appearance (Dark / Light / Tintable): layer
opacity and blend specializations, tinted fills, group shadow/translucency/
specular specializations. Phoebus's dark rays are opacity 0 in the Dark
IconGroup, its glow is plus-lighter (27) at 0.9 / 1 / 0.

The stack's first child is the background for that appearance: with
top-level `fill-specializations`, the matching (or default) entry — a
Gradient, or the Color itself for a `solid` entry (cloud's dark fill); with
a plain `solid` fill, the Color for Light/Tintable and the system-dark
Gradient-1 for Dark; with no fill at all, the white Color-1 over the
system-dark gradient.

IconComposer defaults Apple encodes for keys a bundle omits: specular on,
translucency enabled 0.5, shadow neutral 0.5, lighting individual, layers
glass. Plain `opacity` / `blend-mode` (layers, groups) and `lighting`
(groups) are honoured like their `-specializations` lists. A layer with a
plain `fill` ignores its `fill-specializations`.

### Flattened image look (measured)

* Full bleed: no mask or margin (iOS masks it). Layers sit 1:1 on the
  1024 pt canvas; the background gradient's axis spans y ≈ 100…924 (the
  normalized axis inset by 824/1024, like macOS's content box). The
  `system-dark` gradient renders 0.192 → 0.078 (#313131 → #141414).
* Tile rim light: a ~22 pt band inside a square with very round corners,
  like white screened over the tile, strongest at the top-left, then
  bottom-right; the same strength in Any and Dark.
* Glass layers in a translucent group become see-through towards the bottom
  (an S-shaped ramp: 0.5 → 16 % at mid-height, 45 % at the bottom; table in
  `ios_render.rs`), over what lies behind the *group*. Non-glass layers stay
  solid. Glass has an opaque 6–24 pt edge band (darker without specular);
  specular adds a white highlight 6–18 pt inside the edge.
* Groups cast a soft shadow down-right (neutral: ~4 % darkening at the edge
  for opacity 0.5).
* Tintable: the icon composited with the tinted specializations, as luma
  on black.

## Implementation

* `svg_raster.rs`: SVG layers rasterize with CoreSVG on macOS and resvg
  elsewhere.
* `icon_bundle/ios.rs`: selection, palette, renditions, stack/group TLVs
  (`car::build_ios_iconstack_csi` / `build_ios_icongroup_csi`), loose PNGs
  and plist. `actool a.icon b.icon … --platform iphoneos` takes several
  documents.
* `icon_bundle/ios_render.rs`: pure-Rust renderer for the three flattened
  looks (same output on macOS and Linux).
* `examples/ios_icon_previews.rs`: writes those renders as PNGs.
* Size: an iOS catalog stores each distinct rendition once
  (`BomWriter::set_share_identical_values`), so the phone and pad 1024 pt
  images and the layer SVGs repeated across icons share one block; keys
  and rendition bytes are Apple's. Apple writes every copy: for the ten
  Phoebus icons Xcode 26.6 emits 47.8 MB (24.0 MB of identical blocks),
  this writer 12.8 MB. Both store the flattened images as opaque 8-bit
  ARGB, LZFSE; ours compress to about half of Apple's (smoother pixels,
  less glass detail).
* `tools/compare_ios_icon_car.py`: structural diff of two iOS icon CARs, and
  `--golden` for the test reference.

## Parity (ten Phoebus bundles, Linux build vs Xcode 26.6)

Structure: for each bundle, and for the single CAR holding all ten (primary
+ 9 alternates, 425 renditions), every rendition key matches Apple's, with
identical layout, pixel format, flags, TLVs, codec headers and byte-identical
iconstack / IconGroup / Color / Gradient / MultiSized payloads;
`assetutil --info` output is identical apart from sizes, digests and the
random name UUIDs (Vector SHA-1s match too); the partial plist is
byte-identical. The same holds for the synthetic probes. CoreUI decodes our
renditions losslessly (max |decode − render| = 0).

Pixels: mean |difference| per channel (0–255) of the CoreUI-decoded phone
renditions, ours vs Apple's:

| icon | Any | Dark | Tinted |
|---|---|---|---|
| classic | 1.88 | 1.88 | 0.31 |
| cloud | 4.42 | 6.04 | 1.83 |
| dusk | 5.65 | 6.07 | 1.83 |
| graphite | 6.2 | 8.06 | 1.83 |
| lagoon | 5.98 | 6.19 | 1.83 |
| midnight | 6.22 | 8.28 | 1.83 |
| nebula | 5.94 | 6.22 | 1.83 |
| phoebus | 5.65 | 5.9 | 1.83 |
| sky | 6.16 | 6.24 | 1.83 |
| twilight | 6.05 | 8.06 | 1.83 |
| **mean** | **5.41** | **6.29** | **1.68** |

`classic` (no glass, no effects) shows the base geometry, gradients and rim
are right; the rest of the residual is Apple's glass shading — the specular
and refraction detail on the glass layers (laurels, dome, ring, antenna) and
the exact rim profile — which is renderer-bound like the macOS shaders in
`icon-shading.md`.

## Gaps

* **iOS 26 live glass** is rendered by the device from the stack data, which
  matches Apple's encoding; nothing is flattened for it. The iOS 26.5
  simulator in the QEMU VM draws every home-screen icon (Apple's own apps
  included) as a gray placeholder, so it couldn't be checked there; on the
  iOS 18.6 simulator our icon and Apple's look identical in the default
  (light) style. `simctl ui appearance` doesn't switch the iOS 18 icon style,
  so Dark / Tinted weren't exercised on a home screen.
* Flattened pixels are an approximation (table above).
* Mixing `.icon` bundles with an `.xcassets` catalog in one invocation, and
  deployment targets below 13 (extra deployment-target column), aren't
  implemented.
* Layer fills that are gradients resolve to the right Gradient facet but are
  rendered with the stops' average colour.
