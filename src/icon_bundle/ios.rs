//! iOS (`--platform iphoneos`) IconComposer catalogs.
//!
//! For each `.icon` selected by `--app-icon` / `--alternate-app-icon` /
//! `--include-all-app-icons`, Apple's actool emits into one `Assets.car`:
//!
//! * the layer sources (Vector), Color and Gradient palette, as on macOS;
//! * an `iconstack` and one `IconGroup` per group for each of the Dark (1),
//!   Light (4) and Tintable (10) appearances — the live iOS 26 recipe, with
//!   every layer/group attribute resolved for that appearance;
//! * flattened 1024 pt renders for Any (0), Dark (1) and Tintable (10) for
//!   both the phone and pad idioms (the iOS 17/18 home-screen images), plus a
//!   MultiSized rendition per idiom;
//!
//! and, for the primary icon only, loose `<name>60x60@2x.png` /
//! `<name>76x76@2x~ipad.png` files and the `CFBundleIcons` partial plist.
//! See `docs/ios-icon-appearances.md` for the reference dumps.

use super::ios_render::{self, Background, Blend, IosAppearance, Scene, SceneGroup, SceneLayer};
use super::*;
use crate::icon_effects::{
    resolve_icon_effects_with, Appearance, IconEffects, Lighting, ShadowKind,
    ICON_COMPOSER_DEFAULTS,
};
use crate::icon_json::Group;

const ICON_PT: u32 = 1024;
/// Phone and pad: Apple emits both regardless of `--target-device`.
const IDIOMS: [u16; 2] = [1, 2];

/// Flattened renditions: (appearance attribute, rendition-name tag, look).
const FLAT_APPEARANCES: [(u16, &str, IosAppearance); 3] = [
    (0, "UIAppearanceAny", IosAppearance::Any),
    (1, "UIAppearanceDark", IosAppearance::Dark),
    (10, "ISAppearanceTintable", IosAppearance::Tinted),
];

/// iconstack / IconGroup renditions: (appearance attribute, icon.json look).
const STACK_APPEARANCES: [(u16, Appearance); 3] =
    [(1, Appearance::Dark), (4, Appearance::Light), (10, Appearance::Tinted)];

pub struct IosIconOptions<'a> {
    pub platform: &'a str,
    pub min_deploy: &'a str,
    pub app_icon: Option<&'a str>,
    pub alternate_app_icons: &'a [String],
    pub include_all_app_icons: bool,
    /// `--target-device` values; empty means iPhone and iPad.
    pub target_devices: &'a [String],
    pub info_plist_path: Option<&'a Path>,
}

fn stem(path: &Path) -> String {
    path.file_stem().unwrap_or_default().to_string_lossy().to_string()
}

/// Compile one or more `.icon` bundles into an iOS `Assets.car`.
pub fn compile_icon_bundles(
    bundles: &[PathBuf],
    output_dir: &Path,
    opts: &IosIconOptions,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(output_dir)?;
    let primary = opts
        .app_icon
        .and_then(|name| bundles.iter().find(|b| stem(b) == name));
    let mut alternates: Vec<&PathBuf> = bundles
        .iter()
        .filter(|b| Some(*b) != primary)
        .filter(|b| {
            opts.include_all_app_icons || opts.alternate_app_icons.iter().any(|a| *a == stem(b))
        })
        .collect();
    alternates.sort_by_key(|b| stem(b));

    let mut outputs: Vec<PathBuf> = Vec::new();
    let selected: Vec<&PathBuf> = primary.into_iter().chain(alternates.iter().copied()).collect();
    if !selected.is_empty() {
        let mut renditions: Vec<Rendition> = Vec::new();
        let mut facets: Vec<FacetEntry> = Vec::new();
        for bundle in &selected {
            let built = build_bundle(bundle, opts)?;
            renditions.extend(built.renditions);
            facets.extend(built.facets);
            if Some(*bundle) == primary {
                let name = stem(bundle);
                for (file, px) in [
                    (format!("{name}60x60@2x.png"), 120),
                    (format!("{name}76x76@2x~ipad.png"), 152),
                ] {
                    let path = output_dir.join(file);
                    let img = image::RgbaImage::from_raw(ICON_PT, ICON_PT, built.any_rgba.clone())
                        .ok_or_else(|| anyhow::anyhow!("icon render size mismatch"))?;
                    image::imageops::resize(&img, px, px, FilterType::Lanczos3).save(&path)?;
                    outputs.push(path);
                }
            }
        }
        let keyformat = if renditions.iter().any(|r| r.variant != 0) {
            car::KEYFORMAT_IOS_ICON_GAMUT.to_vec()
        } else {
            car::KEYFORMAT_IOS_ICON.to_vec()
        };
        for r in &mut renditions {
            r.keyformat = keyformat.clone();
        }
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = renditions
            .iter()
            .map(|r| (r.build_rendition_key(), r.build_csi()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let car_path = output_dir.join("Assets.car");
        write_icon_car_with(
            &car_path,
            &facets,
            &keyformat,
            &entries,
            opts.platform,
            opts.min_deploy,
            2,
            car::make_ios_appearancekeys_entries(),
        )?;
        outputs.push(car_path);
    }

    if let Some(path) = opts.info_plist_path {
        match primary {
            Some(p) => {
                let alt_names: Vec<String> = alternates.iter().map(|b| stem(b)).collect();
                let wants = |d: &str| {
                    opts.target_devices.is_empty() || opts.target_devices.iter().any(|t| t == d)
                };
                write_ios_plist(path, &stem(p), &alt_names, wants("iphone"), wants("ipad"))?;
            }
            None => write_empty_partial_plist(path)?,
        }
        outputs.push(path.to_path_buf());
    }
    outputs.sort();
    Ok(outputs
        .into_iter()
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
        .collect())
}

/// Render a bundle's flattened iOS appearances — the pixels that go into its
/// Any / Dark / Tintable 1024 pt renditions — as straight RGBA, for previews
/// and parity checks without a CoreUI decode.
pub fn render_appearances(bundle: &Path) -> Result<Vec<(&'static str, Vec<u8>)>> {
    let text = fs::read_to_string(bundle.join("icon.json"))?;
    let parsed = IconJson::parse(&text)?;
    let prefix = bundle_facet_prefix(bundle);
    let (colors, gradients) = ios_palette(&prefix, &parsed);
    let mut cache = RasterCache::default();
    let mut out = Vec::new();
    for (_, tag, look) in FLAT_APPEARANCES {
        let scene = build_scene(bundle, &parsed, &colors, &gradients, look, &mut cache)?;
        out.push((tag, ios_render::render(&scene)));
    }
    Ok(out)
}

struct BuiltBundle {
    renditions: Vec<Rendition>,
    facets: Vec<FacetEntry>,
    /// Straight RGBA of the Any render, for the loose home-screen PNGs.
    any_rgba: Vec<u8>,
}

fn build_bundle(bundle: &Path, opts: &IosIconOptions) -> Result<BuiltBundle> {
    let json_path = bundle.join("icon.json");
    let text = fs::read_to_string(&json_path)
        .map_err(|e| anyhow::anyhow!("could not read {}: {e}", json_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    if value.get("groups").is_none() {
        anyhow::bail!("icon.json missing required `groups` field");
    }
    validate_layer_image_refs(bundle, &value)?;
    let parsed = IconJson::parse(&text)?;

    let prefix = bundle_facet_prefix(bundle);
    let ident = hash_name(&prefix);
    let layer_assets = collect_layer_assets(bundle, &parsed, &prefix);
    let group_names = group_facet_names(&prefix, &parsed);
    let (colors, gradients) = ios_palette(&prefix, &parsed);
    let kf = car::KEYFORMAT_IOS_ICON.to_vec();
    let platform = opts.platform;
    let min_deploy = opts.min_deploy;

    let (mut renditions, asset_facets) =
        asset_renditions(&layer_assets, &colors, &gradients, &kf, platform, min_deploy)?;
    // On iOS the main facet carries the iconstack's part (245), not the
    // sized-icon part (220) macOS uses.
    let mut facets: Vec<FacetEntry> = vec![(
        prefix.clone(),
        car::ELEMENT_UNIVERSAL,
        Some(car::PART_ICON_COMPOSER),
        ident,
    )];
    for g in &group_names {
        facets.push((g.clone(), car::ELEMENT_UNIVERSAL, Some(car::PART_ICON_GROUP), hash_name(g)));
    }
    facets.extend(asset_facets);

    for idiom in IDIOMS {
        let mut ms = car::build_multisize_rendition(
            &prefix,
            ident,
            &[MultisizeImageEntry { width: ICON_PT, height: ICON_PT, index: 1 }],
        );
        ms.idiom = idiom;
        ms.keyformat = kf.clone();
        ms.platform = platform.to_string();
        ms.min_deploy = min_deploy.to_string();
        renditions.push(ms);
    }

    let mut raster_cache = RasterCache::default();
    let mut any_rgba = Vec::new();
    for (appearance_id, tag, look) in FLAT_APPEARANCES {
        let scene = build_scene(bundle, &parsed, &colors, &gradients, look, &mut raster_cache)?;
        let rgba = ios_render::render(&scene);
        let mut bgra = rgba.clone();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        if look == IosAppearance::Any {
            any_rgba = rgba;
        }
        // A tinted render with no glass and no shadow is pure gray in Apple's
        // output, which it stores as an sRGB GA8 + P3 GA16 pair split on the
        // DisplayGamut attribute; anything else stays BGRA.
        let variants: Vec<([u8; 4], u32, u16, Vec<u8>)> =
            if look == IosAppearance::Tinted && tinted_is_monochrome(&parsed) {
                vec![
                    (*b" 8AG", 2, 0, crate::catalog::bgra_to_ga8_force(&bgra)),
                    (*b"61AG", 6, 1, crate::catalog::bgra_to_ga16_force(&bgra)),
                ]
            } else {
                vec![(*b"BGRA", 1, 0, bgra)]
            };
        for (pixel_format, colorspace_id, gamut, pixels) in variants {
        let base = Rendition {
            name: pre_rendered_name(&prefix, ICON_PT, 1, tag),
            identifier: ident,
            element: car::ELEMENT_UNIVERSAL,
            part: car::PART_ICON,
            scale: 1,
            width: ICON_PT,
            height: ICON_PT,
            pixel_data: pixels,
            pixel_format,
            layout: car::LAYOUT_ONE_PART_SCALE,
            dim2: 1,
            appearance: appearance_id,
            variant: gamut,
            keyformat: kf.clone(),
            min_deploy: min_deploy.to_string(),
            platform: platform.to_string(),
            colorspace_id,
            template_rendering_intent: 0,
            ..Rendition::default()
        };
        // Phone and pad share the rendition bytes; compress once.
        let csi = base.build_csi();
        for idiom in IDIOMS {
            renditions.push(Rendition {
                idiom,
                pixel_data: Vec::new(),
                csi_override: Some(csi.clone()),
                ..base.clone()
            });
        }
        }
    }

    if !layer_assets.is_empty() {
        for (appearance_id, appearance) in STACK_APPEARANCES {
            renditions.extend(stack_renditions(
                bundle,
                &prefix,
                &parsed,
                &group_names,
                &colors,
                &gradients,
                appearance_id,
                appearance,
                &kf,
                opts,
            ));
        }
    }

    Ok(BuiltBundle { renditions, facets, any_rgba })
}

/// The bundle's Color/Gradient palette. A bundle without any fill gets
/// IconComposer's default: a white light background (the Color-1 anchor)
/// over the system-dark gradient.
fn ios_palette(prefix: &str, parsed: &IconJson) -> (Vec<ColorAsset>, Vec<GradientAsset>) {
    let has_specs = parsed.fill_specializations.as_ref().is_some_and(|v| !v.is_empty());
    if parsed.fill.is_none() && !has_specs {
        let g = |v: f32| v as f64;
        let n = |s: &str| format!("{prefix}_Assets/{s}");
        let mut colors = vec![
            ColorAsset { facet_name: n("Color-1"), colorspace_id: 6, components: vec![g(1.0), g(1.0)] },
            ColorAsset { facet_name: n("Color-2"), colorspace_id: 2, components: vec![g(0.192), g(1.0)] },
            ColorAsset { facet_name: n("Color-3"), colorspace_id: 2, components: vec![g(0.078), g(1.0)] },
        ];
        let mut gradients = vec![GradientAsset {
            facet_name: n("Gradient-1"),
            geometry: [0.5, 0.0, 0.5, 1.0],
            stops: vec![(0.0, n("Color-2")), (1.0, n("Color-3"))],
            kind: 1,
        }];
        let base = colors.len();
        append_layer_fills(prefix, parsed, &mut colors, &mut gradients, base);
        return (colors, gradients);
    }
    fill_assets(prefix, parsed.fill.as_ref(), parsed).unwrap_or_default()
}

/// The palette entry an appearance paints as its background.
enum BackgroundRef<'a> {
    Color(&'a ColorAsset),
    Gradient(&'a GradientAsset),
}

impl BackgroundRef<'_> {
    fn stack_ref(&self) -> (u16, String) {
        match self {
            BackgroundRef::Color(c) => (car::PART_COLOR, c.facet_name.clone()),
            BackgroundRef::Gradient(g) => (car::PART_ICON_GRADIENT, g.facet_name.clone()),
        }
    }
}

fn color_for_spec<'a>(spec: &str, colors: &'a [ColorAsset]) -> Option<&'a ColorAsset> {
    let (cs, comps) = parse_color_spec(spec)?;
    colors.iter().find(|c| c.colorspace_id == cs && c.components == comps)
}

fn gradient_with_stops<'a>(
    names: &[&str],
    gradients: &'a [GradientAsset],
) -> Option<&'a GradientAsset> {
    gradients
        .iter()
        .find(|g| g.stops.iter().map(|(_, n)| n.as_str()).eq(names.iter().copied()))
}

/// Resolve the background for `appearance`. With top-level
/// fill-specializations each appearance paints its own entry (a `solid` one
/// references its Color directly — cloud's dark background); with a plain
/// fill, a solid colour (or no fill: the white Color-1) is the light/tinted
/// background over the system-dark Gradient-1, and gradient fills use
/// Gradient-1 (light) / Gradient-2 (dark).
fn background_ref<'a>(
    parsed: &IconJson,
    appearance: Appearance,
    colors: &'a [ColorAsset],
    gradients: &'a [GradientAsset],
) -> Option<BackgroundRef<'a>> {
    if let Some(specs) = parsed.fill_specializations.as_ref().filter(|v| !v.is_empty()) {
        let name = appearance_name(appearance);
        let entry = specs
            .iter()
            .find(|s| s.get("appearance").and_then(|a| a.as_str()) == Some(name))
            .or_else(|| specs.iter().find(|s| s.get("appearance").is_none()));
        let value = entry.and_then(|e| e.get("value"));
        let resolved = value.and_then(|v| {
            if let Some(solid) = v.get("solid").and_then(|x| x.as_str()) {
                return color_for_spec(solid, colors).map(BackgroundRef::Color);
            }
            if let Some(arr) = v.get("linear-gradient").and_then(|x| x.as_array()) {
                let names: Option<Vec<&str>> = arr
                    .iter()
                    .map(|x| x.as_str().and_then(|s| color_for_spec(s, colors)))
                    .map(|c| c.map(|c| c.facet_name.as_str()))
                    .collect();
                return gradient_with_stops(&names?, gradients).map(BackgroundRef::Gradient);
            }
            let keyword = v.as_str()?;
            let entry_appearance = entry.and_then(|e| e.get("appearance")).and_then(|a| a.as_str());
            let (top, bottom) = keyword_bg_pair(keyword, entry_appearance)?;
            let gray = |v: f64| {
                colors
                    .iter()
                    .find(|c| c.colorspace_id == 2 && c.components == vec![v as f32 as f64, 1.0])
                    .map(|c| c.facet_name.as_str())
            };
            gradient_with_stops(&[gray(top)?, gray(bottom)?], gradients)
                .map(BackgroundRef::Gradient)
        });
        return resolved.or_else(|| gradients.first().map(BackgroundRef::Gradient));
    }
    let solid = match parsed.fill.as_ref() {
        None => colors.first(),
        Some(Fill::Structured(v)) if v.get("solid").is_some() => colors.get(1),
        _ => None,
    };
    match (appearance, solid) {
        (Appearance::Dark, Some(_)) => gradients.first().map(BackgroundRef::Gradient),
        (_, Some(c)) => Some(BackgroundRef::Color(c)),
        (Appearance::Dark, None) => {
            gradients.get(1).or(gradients.first()).map(BackgroundRef::Gradient)
        }
        (_, None) => gradients.first().map(BackgroundRef::Gradient),
    }
}

/// A palette colour as sRGB 0..1. Display-P3 components are converted (the
/// Phoebus backgrounds are P3); gray spaces expand to a neutral triple.
fn color_srgb(c: &ColorAsset) -> [f32; 3] {
    let comps = &c.components;
    let rgb = match comps.len() {
        2 => [comps[0], comps[0], comps[0]],
        n if n >= 3 => [comps[0], comps[1], comps[2]],
        _ => [0.0; 3],
    };
    if c.colorspace_id != 3 {
        return rgb.map(|v| v as f32);
    }
    let lin = |v: f64| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    let enc = |v: f64| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
    };
    let (r, g, b) = (lin(rgb[0]), lin(rgb[1]), lin(rgb[2]));
    [
        enc(1.224_940 * r - 0.224_940 * g) as f32,
        enc(-0.042_057 * r + 1.042_057 * g) as f32,
        enc(-0.019_638 * r - 0.078_636 * g + 1.098_274 * b) as f32,
    ]
}

fn find_color<'a>(colors: &'a [ColorAsset], facet: &str) -> Option<&'a ColorAsset> {
    colors.iter().find(|c| c.facet_name == facet)
}

/// Background for a gradient asset, in canvas-normalized coordinates: like
/// macOS the first stop is drawn at the top and the axis is inset by
/// `LAYER_BASE_SCALE` about the centre — on the full-bleed iOS canvas that
/// spans y ≈ 100…924 (black→white probe).
fn gradient_background(grad: &GradientAsset, colors: &[ColorAsset]) -> Option<Background> {
    let first = color_srgb(find_color(colors, &grad.stops.first()?.1)?);
    let last = color_srgb(find_color(colors, &grad.stops.last()?.1)?);
    let inset = |p: f32| 0.5 + (p - 0.5) * LAYER_BASE_SCALE;
    let g = grad.geometry;
    Some(Background {
        start_rgb: first,
        stop_rgb: last,
        start: [inset(g[0]), inset(0.0)],
        stop: [inset(g[2]), inset(1.0)],
    })
}

fn flat_background(rgb: [f32; 3]) -> Background {
    Background { start_rgb: rgb, stop_rgb: rgb, start: [0.5, 0.0], stop: [0.5, 1.0] }
}

fn to_effects_appearance(look: IosAppearance) -> Appearance {
    match look {
        IosAppearance::Any => Appearance::Light,
        IosAppearance::Dark => Appearance::Dark,
        IosAppearance::Tinted => Appearance::Tinted,
    }
}

fn appearance_name(a: Appearance) -> &'static str {
    match a {
        Appearance::Light => "light",
        Appearance::Dark => "dark",
        Appearance::Tinted => "tinted",
    }
}

/// The layer's fill value for `appearance`: its plain `fill` (which Apple
/// applies to every appearance), else the matching / default entry of its
/// `fill-specializations`.
fn layer_fill_value<'a>(
    layer: &'a crate::icon_json::Layer,
    appearance: Appearance,
) -> Option<&'a serde_json::Value> {
    if let Some(Fill::Structured(v)) = layer.fill.as_ref() {
        return Some(v);
    }
    let specs = layer.fill_specializations.as_ref()?;
    let name = appearance_name(appearance);
    specs
        .iter()
        .find(|s| s.get("appearance").and_then(|a| a.as_str()) == Some(name))
        .or_else(|| specs.iter().find(|s| s.get("appearance").is_none()))
        .and_then(|s| s.get("value"))
}

/// Facet name of the palette entry a layer fill resolved to (a Color for a
/// solid, the Gradient for a linear gradient), and its render colour.
fn resolve_layer_fill(
    value: &serde_json::Value,
    colors: &[ColorAsset],
    gradients: &[GradientAsset],
) -> Option<(String, [f32; 3])> {
    let spec_color = |spec: &str| -> Option<&ColorAsset> {
        let (cs, comps) = parse_color_spec(spec)?;
        colors.iter().find(|c| c.colorspace_id == cs && c.components == comps)
    };
    if let Some(solid) = value.get("solid").and_then(|s| s.as_str()) {
        let c = spec_color(solid)?;
        return Some((c.facet_name.clone(), color_srgb(c)));
    }
    if let Some(arr) = value.get("linear-gradient").and_then(|x| x.as_array()) {
        let stops: Vec<&ColorAsset> =
            arr.iter().filter_map(|s| s.as_str()).filter_map(spec_color).collect();
        if stops.is_empty() {
            return None;
        }
        let names: Vec<&str> = stops.iter().map(|c| c.facet_name.as_str()).collect();
        let grad = gradients.iter().find(|g| {
            g.stops.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>() == names
        })?;
        let mut avg = [0.0f32; 3];
        for c in &stops {
            let rgb = color_srgb(c);
            for i in 0..3 {
                avg[i] += rgb[i] / stops.len() as f32;
            }
        }
        return Some((grad.facet_name.clone(), avg));
    }
    None
}

fn group_position(group: &Group) -> (f32, f32, f32) {
    group
        .position
        .as_ref()
        .map(|p| {
            let t = p.translation_in_points.unwrap_or([0.0, 0.0]);
            (p.scale.unwrap_or(1.0), t[0], t[1])
        })
        .unwrap_or((1.0, 0.0, 0.0))
}

fn layer_source(bundle: &Path, image_name: &str) -> Option<PathBuf> {
    let assets = bundle.join("Assets").join(image_name);
    if assets.exists() {
        return Some(assets);
    }
    let root = bundle.join(image_name);
    root.exists().then_some(root)
}

/// A visible layer with its geometry in 1024-pt icon space: size and
/// top-left offset after composing group and layer `position`.
struct PlacedLayer<'a> {
    layer: &'a crate::icon_json::Layer,
    index: usize,
    source: PathBuf,
    size: [f32; 2],
    offset: [f32; 2],
}

fn placed_layers<'a>(bundle: &Path, group: &'a Group) -> Vec<PlacedLayer<'a>> {
    let (gscale, gtx, gty) = group_position(group);
    let mut out: Vec<PlacedLayer> = group
        .layers
        .iter()
        .enumerate()
        .filter(|(_, l)| l.hidden != Some(true))
        .filter_map(|(index, layer)| {
            let source = layer_source(bundle, layer.image_name.as_deref()?)?;
            let native = layer_native_size(&source);
            let (lscale, ltx, lty) = layer
                .position
                .as_ref()
                .map(|p| {
                    let t = p.translation_in_points.unwrap_or([0.0, 0.0]);
                    (p.scale.unwrap_or(1.0), t[0], t[1])
                })
                .unwrap_or((1.0, 0.0, 0.0));
            let scale = gscale * lscale;
            let size = [native.0 as f32 * scale, native.1 as f32 * scale];
            let offset = [
                (ICON_PT as f32 - size[0]) / 2.0 + gscale * ltx + gtx,
                (ICON_PT as f32 - size[1]) / 2.0 + gscale * lty + gty,
            ];
            Some(PlacedLayer { layer, index, source, size, offset })
        })
        .collect();
    // icon.json lists layers front-to-back; renditions and painting go
    // back-to-front.
    out.reverse();
    out
}

/// Whether Apple's tinted render comes out exactly gray: a single group that
/// casts no shadow and has no glass layer in the tinted appearance. Glass,
/// shadows and multi-group compositing each leave ±2 colour noise in its
/// renderer; translucency, specular and layer fills don't.
fn tinted_is_monochrome(parsed: &IconJson) -> bool {
    let visible: Vec<&Group> = parsed
        .groups
        .iter()
        .filter(|g| g.hidden != Some(true) && !g.layers.is_empty())
        .collect();
    visible.len() == 1 && visible.iter().all(|g| {
        let eff = group_effects(g, Appearance::Tinted);
        let casts_shadow = eff.shadow.kind != ShadowKind::None && eff.shadow.opacity > 0.0;
        !casts_shadow && !eff.layers.iter().any(|l| l.glass && !l.hidden)
    })
}

fn shadow_code(kind: ShadowKind) -> u32 {
    match kind {
        ShadowKind::None => 0,
        ShadowKind::LayerColor => 2,
        ShadowKind::Neutral => 3,
    }
}

fn group_effects(group: &Group, appearance: Appearance) -> IconEffects {
    resolve_icon_effects_with(group, appearance, &ICON_COMPOSER_DEFAULTS)
}

#[allow(clippy::too_many_arguments)]
fn stack_renditions(
    bundle: &Path,
    prefix: &str,
    parsed: &IconJson,
    group_names: &[String],
    colors: &[ColorAsset],
    gradients: &[GradientAsset],
    appearance_id: u16,
    appearance: Appearance,
    kf: &[u16],
    opts: &IosIconOptions,
) -> Vec<Rendition> {
    let rendition = |name: &str, ident: u16, part: u16, layout: u16, dim: u32, csi: Vec<u8>| {
        Rendition {
            name: name.to_string(),
            identifier: ident,
            element: car::ELEMENT_UNIVERSAL,
            part,
            scale: 1,
            appearance: appearance_id,
            width: dim,
            height: dim,
            pixel_format: *car::PIXELFMT_DATA,
            layout,
            keyformat: kf.to_vec(),
            min_deploy: opts.min_deploy.to_string(),
            platform: opts.platform.to_string(),
            colorspace_id: 0,
            csi_override: Some(csi),
            ..Rendition::default()
        }
    };

    let mut stack: Vec<car::StackEntry> = Vec::new();
    if let Some((part, facet)) =
        background_ref(parsed, appearance, colors, gradients).map(|b| b.stack_ref())
    {
        stack.push(car::StackEntry {
            part,
            identifier: hash_name(&facet),
            opacity: 1.0,
            ..Default::default()
        });
    }

    let mut out = Vec::new();
    for (group, gname) in parsed.groups.iter().zip(group_names).rev() {
        let layers = placed_layers(bundle, group);
        if layers.is_empty() {
            continue;
        }
        let eff = group_effects(group, appearance);
        let entries: Vec<car::StackEntry> = layers
            .iter()
            .map(|pl| {
                let leff = eff.layers.get(pl.index);
                let stem = Path::new(pl.layer.image_name.as_deref().unwrap_or_default())
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let fill_name = layer_fill_value(pl.layer, appearance)
                    .and_then(|v| resolve_layer_fill(v, colors, gradients))
                    .map(|(name, _)| name)
                    .unwrap_or_default();
                car::StackEntry {
                    part: car::PART_REGULAR,
                    identifier: hash_name(&format!("{prefix}_Assets/{stem}")),
                    offset: [pl.offset[0].round() as i32, pl.offset[1].round() as i32],
                    size: [pl.size[0].round() as u32, pl.size[1].round() as u32],
                    blend: leff.map_or(0, |l| Blend::parse(&l.blend_mode).cg_value()),
                    opacity: leff.map_or(1.0, |l| l.opacity),
                    flags: leff.is_some_and(|l| l.glass) as u32,
                    fill_name,
                    ..Default::default()
                }
            })
            .collect();
        let gident = hash_name(gname);
        out.push(rendition(
            "IconGroup",
            gident,
            car::PART_ICON_GROUP,
            car::LAYOUT_ICON_GROUP,
            0,
            car::build_ios_icongroup_csi("IconGroup", &entries),
        ));
        stack.push(car::StackEntry {
            part: car::PART_ICON_GROUP,
            identifier: gident,
            blend: Blend::parse(&eff.blend_mode).cg_value(),
            opacity: eff.opacity,
            flags: if eff.lighting == Lighting::Combined { 0 } else { 2 },
            blur: eff.blur_material.unwrap_or(0.0),
            specular: eff.specular,
            translucency: if eff.translucency.enabled { eff.translucency.value } else { 0.0 },
            shadow_kind: shadow_code(eff.shadow.kind),
            shadow_opacity: eff.shadow.opacity,
            ..Default::default()
        });
    }
    let name = format!("{prefix}.iconstack");
    out.push(rendition(
        &name,
        hash_name(prefix),
        car::PART_ICON_COMPOSER,
        car::LAYOUT_ICONSTACK,
        ICON_PT,
        car::build_ios_iconstack_csi(&name, ICON_PT, &stack),
    ));
    out
}

type RasterCache = std::collections::HashMap<(PathBuf, u32, u32), Vec<u8>>;

/// Resolve the bundle into a renderable scene for one flattened appearance.
fn build_scene(
    bundle: &Path,
    parsed: &IconJson,
    colors: &[ColorAsset],
    gradients: &[GradientAsset],
    look: IosAppearance,
    cache: &mut RasterCache,
) -> Result<Scene> {
    let appearance = to_effects_appearance(look);
    let background = match look {
        IosAppearance::Tinted => None,
        _ => match background_ref(parsed, appearance, colors, gradients) {
            Some(BackgroundRef::Color(c)) => Some(flat_background(color_srgb(c))),
            Some(BackgroundRef::Gradient(g)) => gradient_background(g, colors),
            None => None,
        },
    };
    let mut groups = Vec::new();
    for group in parsed.groups.iter().rev() {
        if group.hidden == Some(true) {
            continue;
        }
        let eff = group_effects(group, appearance);
        let mut layers = Vec::new();
        for pl in placed_layers(bundle, group) {
            let leff = eff.layers.get(pl.index);
            if leff.is_some_and(|l| l.hidden || l.opacity <= 0.0) {
                continue;
            }
            let w = (pl.size[0].round() as u32).max(1);
            let h = (pl.size[1].round() as u32).max(1);
            let key = (pl.source.clone(), w, h);
            if !cache.contains_key(&key) {
                cache.insert(key.clone(), rasterize_layer(&pl.source, w, h)?);
            }
            let fill = layer_fill_value(pl.layer, appearance)
                .and_then(|v| resolve_layer_fill(v, colors, gradients))
                .map(|(_, rgb)| rgb);
            layers.push(SceneLayer {
                rgba: cache[&key].clone(),
                w: w as usize,
                h: h as usize,
                x: pl.offset[0].round() as i32,
                y: pl.offset[1].round() as i32,
                opacity: leff.map_or(1.0, |l| l.opacity),
                blend: leff.map_or(Blend::Normal, |l| Blend::parse(&l.blend_mode)),
                glass: leff.is_some_and(|l| l.glass),
                fill,
            });
        }
        if layers.is_empty() {
            continue;
        }
        groups.push(SceneGroup {
            layers,
            opacity: eff.opacity,
            blend: Blend::parse(&eff.blend_mode),
            shadow_kind: shadow_code(eff.shadow.kind),
            shadow_opacity: eff.shadow.opacity,
            specular: eff.specular,
            translucency: eff.translucency.enabled.then_some(eff.translucency.value),
            combined_lighting: eff.lighting == Lighting::Combined,
        });
    }
    Ok(Scene { size: ICON_PT as usize, appearance: look, background, groups })
}

/// The iOS partial Info.plist for a primary `.icon` plus alternates, laid out
/// like Apple's: iPad repeats the iPhone file and only names the icon (and
/// lists alternates) when iPad is a target device.
fn write_ios_plist(
    path: &Path,
    name: &str,
    alternates: &[String],
    iphone: bool,
    ipad: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut lines = vec![
        r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_string(),
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#.to_string(),
        r#"<plist version="1.0">"#.to_string(),
        "<dict>".to_string(),
    ];
    let mut section = |key: &str, files: &[String], with_alternates: bool, with_name: bool| {
        lines.push(format!("\t<key>{key}</key>"));
        lines.push("\t<dict>".to_string());
        if with_alternates && !alternates.is_empty() {
            lines.push("\t\t<key>CFBundleAlternateIcons</key>".to_string());
            lines.push("\t\t<dict>".to_string());
            for alt in alternates {
                lines.push(format!("\t\t\t<key>{alt}</key>"));
                lines.push("\t\t\t<dict>".to_string());
                lines.push("\t\t\t\t<key>CFBundleIconName</key>".to_string());
                lines.push(format!("\t\t\t\t<string>{alt}</string>"));
                lines.push("\t\t\t</dict>".to_string());
            }
            lines.push("\t\t</dict>".to_string());
        }
        lines.push("\t\t<key>CFBundlePrimaryIcon</key>".to_string());
        lines.push("\t\t<dict>".to_string());
        lines.push("\t\t\t<key>CFBundleIconFiles</key>".to_string());
        lines.push("\t\t\t<array>".to_string());
        for f in files {
            lines.push(format!("\t\t\t\t<string>{f}</string>"));
        }
        lines.push("\t\t\t</array>".to_string());
        if with_name {
            lines.push("\t\t\t<key>CFBundleIconName</key>".to_string());
            lines.push(format!("\t\t\t<string>{name}</string>"));
        }
        lines.push("\t\t</dict>".to_string());
        lines.push("\t</dict>".to_string());
    };
    let phone = format!("{name}60x60");
    section("CFBundleIcons", &[phone.clone()], iphone, true);
    section("CFBundleIcons~ipad", &[phone, format!("{name}76x76")], ipad, ipad);
    lines.push("</dict>".to_string());
    lines.push("</plist>".to_string());
    lines.push(String::new());
    fs::write(path, lines.join("\n"))?;
    Ok(())
}
