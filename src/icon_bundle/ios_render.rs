//! Pure-Rust renderer for the flattened iOS app-icon renditions (the 1024 pt
//! Any / Dark / Tintable images Apple's actool bakes for iOS 17/18 home
//! screens; iOS 26 re-renders live from the iconstack instead).
//!
//! Unlike the macOS renditions these are full-bleed and opaque: no squircle
//! clip or margin (iOS applies its own mask), the layers sit 1:1 on the
//! 1024 pt canvas and the background gradient spans the canvas inset by
//! `LAYER_BASE_SCALE` about its centre (y ≈ 100…924). Everything else is an
//! approximation of Apple's proprietary glass renderer, fitted against
//! synthetic fixtures compiled with `xcrun actool --platform iphoneos` (see
//! `docs/ios-icon-appearances.md`).

use super::{box_blur_h, box_blur_v};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IosAppearance {
    Any,
    Dark,
    Tinted,
}

/// CGBlendMode-compatible compositing modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    SoftLight,
    HardLight,
    Difference,
    Exclusion,
    PlusDarker,
    PlusLighter,
}

impl Blend {
    pub fn parse(s: &str) -> Blend {
        match s {
            "multiply" => Blend::Multiply,
            "screen" => Blend::Screen,
            "overlay" => Blend::Overlay,
            "darken" => Blend::Darken,
            "lighten" => Blend::Lighten,
            "color-dodge" => Blend::ColorDodge,
            "color-burn" => Blend::ColorBurn,
            "soft-light" => Blend::SoftLight,
            "hard-light" => Blend::HardLight,
            "difference" => Blend::Difference,
            "exclusion" => Blend::Exclusion,
            "plus-darker" => Blend::PlusDarker,
            "plus-lighter" => Blend::PlusLighter,
            _ => Blend::Normal,
        }
    }

    /// The `CGBlendMode` value Apple stores in iconstack / IconGroup TLVs.
    pub fn cg_value(self) -> u32 {
        match self {
            Blend::Normal => 0,
            Blend::Multiply => 1,
            Blend::Screen => 2,
            Blend::Overlay => 3,
            Blend::Darken => 4,
            Blend::Lighten => 5,
            Blend::ColorDodge => 6,
            Blend::ColorBurn => 7,
            Blend::SoftLight => 8,
            Blend::HardLight => 9,
            Blend::Difference => 10,
            Blend::Exclusion => 11,
            Blend::PlusDarker => 26,
            Blend::PlusLighter => 27,
        }
    }
}

/// Two-stop linear background in canvas-normalized coordinates (0..1, y
/// down), colours in sRGB 0..1.
#[derive(Debug, Clone)]
pub struct Background {
    pub start_rgb: [f32; 3],
    pub stop_rgb: [f32; 3],
    pub start: [f32; 2],
    pub stop: [f32; 2],
}

impl Background {
    fn sample(&self, x: usize, y: usize, size: usize) -> [f32; 3] {
        let nx = (x as f32 + 0.5) / size as f32;
        let ny = (y as f32 + 0.5) / size as f32;
        let (dx, dy) = (self.stop[0] - self.start[0], self.stop[1] - self.start[1]);
        let len2 = dx * dx + dy * dy;
        let t = if len2 <= 0.0 {
            0.0
        } else {
            (((nx - self.start[0]) * dx + (ny - self.start[1]) * dy) / len2).clamp(0.0, 1.0)
        };
        let mut out = [0.0; 3];
        for c in 0..3 {
            out[c] = self.start_rgb[c] + (self.stop_rgb[c] - self.start_rgb[c]) * t;
        }
        out
    }
}

/// A rasterized layer placed on the canvas.
pub struct SceneLayer {
    /// Straight RGBA, `w × h`.
    pub rgba: Vec<u8>,
    pub w: usize,
    pub h: usize,
    /// Top-left canvas pixel.
    pub x: i32,
    pub y: i32,
    pub opacity: f32,
    pub blend: Blend,
    /// Solid fill replacing the layer's own colours (keeps its alpha).
    pub fill: Option<[f32; 3]>,
}

/// A group's layers (back-to-front) and its appearance-resolved effects. In
/// the flattened renditions the group's translucency, specular rim and
/// shadow apply to every layer, glass or not (a non-glass disc in a default
/// group gets all three); the per-layer glass flag only matters live.
pub struct SceneGroup {
    pub layers: Vec<SceneLayer>,
    pub opacity: f32,
    pub blend: Blend,
    /// 0 = none, 2 = layer-color, 3 = neutral (the TLV encoding).
    pub shadow_kind: u32,
    pub shadow_opacity: f32,
    pub specular: bool,
    /// Translucency value when enabled.
    pub translucency: Option<f32>,
    pub combined_lighting: bool,
}

pub struct Scene {
    pub size: usize,
    pub appearance: IosAppearance,
    /// `None` paints black (the tinted background).
    pub background: Option<Background>,
    /// Back-to-front.
    pub groups: Vec<SceneGroup>,
}

/// Bottom-of-layer see-through exponent: a translucent group's layer is opaque
/// at its top and lets `value · s^1.6` of the backdrop through at fraction `s`
/// of its height (glass disc probe: t ≈ 0.16 / 0.38 / 0.46 at s = 0.5 / 0.8 /
/// 1.0 for value 0.5).
const TRANSLUCENCY_EXP: f32 = 1.6;

/// Specular rim: a white highlight in a band 6…18 pt inside the
/// layer edge, strongest (0.41 toward white) at its outer side.
const SPECULAR_INNER: f32 = 6.0;
const SPECULAR_WIDTH: f32 = 12.0;
const SPECULAR_PEAK: f32 = 0.41;

/// Group drop shadow: offset down-right, blurred, and darkening the backdrop
/// by `peak · opacity` at the edge (glass disc probe: −4.4 % beside the edge
/// at neutral 0.5).
const SHADOW_OFFSET: [f32; 2] = [3.0, 8.0];
const SHADOW_SIGMA: f32 = 14.0;
const SHADOW_NEUTRAL_PEAK: f32 = 0.18;
const SHADOW_LAYERCOLOR_PEAK: f32 = 0.49;

/// Tile rim light: depth of the band inside the iOS icon shape (pt).
const RIM_DEPTH: f32 = 22.0;
/// Corner shape of the rim band: superellipse corners of this radius and
/// exponent, fitted to the band's outer boundary on a flat fill (it reaches
/// the canvas edge ~140 pt from each corner and the diagonal at 51 pt).
const RIM_CORNER_RADIUS: f32 = 138.0;
const RIM_CORNER_EXP: f32 = 1.55;

pub fn render(scene: &Scene) -> Vec<u8> {
    let size = scene.size;
    let n = size * size;
    let mut canvas = vec![[0.0f32; 3]; n];
    if let Some(bg) = &scene.background {
        for y in 0..size {
            for x in 0..size {
                canvas[y * size + x] = bg.sample(x, y, size);
            }
        }
    }
    let k = size as f32 / 1024.0;
    for group in &scene.groups {
        draw_group(&mut canvas, size, k, group);
    }
    if scene.appearance != IosAppearance::Tinted {
        apply_rim_light(&mut canvas, size, scene.appearance);
    }
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        let c = canvas[i];
        if scene.appearance == IosAppearance::Tinted {
            let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            let v = to_u8(l);
            out[i * 4..i * 4 + 3].copy_from_slice(&[v, v, v]);
        } else {
            out[i * 4] = to_u8(c[0]);
            out[i * 4 + 1] = to_u8(c[1]);
            out[i * 4 + 2] = to_u8(c[2]);
        }
        out[i * 4 + 3] = 255;
    }
    out
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Per-layer straight colour + coverage on the full canvas.
struct Placed {
    rgb: Vec<[f32; 3]>,
    alpha: Vec<f32>,
    bbox: Option<(usize, usize, usize, usize)>,
}

fn place(layer: &SceneLayer, size: usize, alpha_scale: f32) -> Placed {
    let n = size * size;
    let mut rgb = vec![[0.0f32; 3]; n];
    let mut alpha = vec![0.0f32; n];
    let mut bbox: Option<(usize, usize, usize, usize)> = None;
    for ly in 0..layer.h {
        let cy = layer.y + ly as i32;
        if cy < 0 || cy >= size as i32 {
            continue;
        }
        for lx in 0..layer.w {
            let cx = layer.x + lx as i32;
            if cx < 0 || cx >= size as i32 {
                continue;
            }
            let si = (ly * layer.w + lx) * 4;
            let a = layer.rgba[si + 3] as f32 / 255.0 * alpha_scale;
            if a <= 0.0 {
                continue;
            }
            let ci = cy as usize * size + cx as usize;
            rgb[ci] = match layer.fill {
                Some(f) => f,
                None => [
                    layer.rgba[si] as f32 / 255.0,
                    layer.rgba[si + 1] as f32 / 255.0,
                    layer.rgba[si + 2] as f32 / 255.0,
                ],
            };
            alpha[ci] = a;
            let (cx, cy) = (cx as usize, cy as usize);
            bbox = Some(match bbox {
                None => (cx, cy, cx, cy),
                Some((x0, y0, x1, y1)) => (x0.min(cx), y0.min(cy), x1.max(cx), y1.max(cy)),
            });
        }
    }
    Placed { rgb, alpha, bbox }
}

fn blur(buf: &mut [f32], size: usize, sigma: f32) {
    let r = sigma.round() as usize;
    if r < 1 {
        return;
    }
    let mut tmp = vec![0.0f32; buf.len()];
    for _ in 0..3 {
        box_blur_h(buf, &mut tmp, size, r);
        box_blur_v(buf, &mut tmp, size, r);
    }
}

/// Approximate depth (pt) inside the shape described by `alpha`: a
/// box-blurred alpha of radius R rises linearly from 0.5 at a straight edge to
/// 1 at depth R, so `(blur − 0.5) · 2R` recovers the distance there.
fn inner_depth(alpha: &[f32], size: usize, k: f32) -> Vec<f32> {
    let r = ((SPECULAR_INNER + SPECULAR_WIDTH + 6.0) * k).round().max(1.0) as usize;
    let mut b = alpha.to_vec();
    let mut tmp = vec![0.0f32; b.len()];
    box_blur_h(&mut b, &mut tmp, size, r);
    box_blur_v(&mut b, &mut tmp, size, r);
    b.iter()
        .map(|v| ((v - 0.5) * 2.0 * r as f32 / k).max(0.0))
        .collect()
}

fn specular_amount(depth: f32) -> f32 {
    if depth < SPECULAR_INNER || depth > SPECULAR_INNER + SPECULAR_WIDTH {
        return 0.0;
    }
    SPECULAR_PEAK * (1.0 - (depth - SPECULAR_INNER) / SPECULAR_WIDTH)
}

fn draw_group(canvas: &mut [[f32; 3]], size: usize, k: f32, group: &SceneGroup) {
    let n = size * size;
    let isolated = group.blend != Blend::Normal;
    let layer_alpha_scale = if isolated { 1.0 } else { group.opacity };
    let placed: Vec<Placed> = group
        .layers
        .iter()
        .map(|l| place(l, size, l.opacity * layer_alpha_scale))
        .collect();
    if placed.iter().all(|p| p.bbox.is_none()) {
        return;
    }

    let mut union = vec![0.0f32; n];
    for p in &placed {
        for i in 0..n {
            union[i] = union[i] + p.alpha[i] * (1.0 - union[i]);
        }
    }

    if group.shadow_kind != 0 && group.shadow_opacity > 0.0 {
        draw_shadow(canvas, size, k, group, &placed, &union);
    }

    let group_depth = if group.specular && group.combined_lighting {
        Some(inner_depth(&union, size, k))
    } else {
        None
    };
    let group_bbox = placed.iter().filter_map(|p| p.bbox).reduce(|a, b| {
        (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
    });

    // Isolated groups composite into their own premultiplied buffer first.
    let mut iso_rgb = vec![[0.0f32; 3]; if isolated { n } else { 0 }];
    let mut iso_a = vec![0.0f32; if isolated { n } else { 0 }];

    for (layer, p) in group.layers.iter().zip(&placed) {
        let Some(bbox) = p.bbox else { continue };
        let depth = if group.specular && !group.combined_lighting {
            Some(inner_depth(&p.alpha, size, k))
        } else {
            None
        };
        let ramp_box = if group.combined_lighting { group_bbox.unwrap_or(bbox) } else { bbox };
        let (top, bottom) = (ramp_box.1 as f32, ramp_box.3 as f32);
        for y in bbox.1..=bbox.3 {
            let s = ((y as f32 - top) / (bottom - top).max(1.0)).clamp(0.0, 1.0);
            let see_through = group.translucency.map_or(0.0, |v| v * s.powf(TRANSLUCENCY_EXP));
            for x in bbox.0..=bbox.2 {
                let i = y * size + x;
                let mut a = p.alpha[i];
                if a <= 0.0 {
                    continue;
                }
                a *= 1.0 - see_through;
                let mut col = p.rgb[i];
                if group.specular {
                    let d = depth.as_ref().or(group_depth.as_ref()).map(|d| d[i]).unwrap_or(0.0);
                    let h = specular_amount(d);
                    for c in &mut col {
                        *c += h * (1.0 - *c);
                    }
                }
                if isolated {
                    for c in 0..3 {
                        iso_rgb[i][c] = col[c] * a + iso_rgb[i][c] * (1.0 - a);
                    }
                    iso_a[i] = a + iso_a[i] * (1.0 - a);
                } else {
                    composite(&mut canvas[i], col, a, layer.blend);
                }
            }
        }
    }

    if isolated {
        for i in 0..n {
            let a = iso_a[i] * group.opacity;
            if a <= 0.0 {
                continue;
            }
            let col = [
                iso_rgb[i][0] / iso_a[i],
                iso_rgb[i][1] / iso_a[i],
                iso_rgb[i][2] / iso_a[i],
            ];
            composite(&mut canvas[i], col, a, group.blend);
        }
    }
}

fn draw_shadow(
    canvas: &mut [[f32; 3]],
    size: usize,
    k: f32,
    group: &SceneGroup,
    placed: &[Placed],
    union: &[f32],
) {
    let n = size * size;
    let (dx, dy) = (
        (SHADOW_OFFSET[0] * k).round() as isize,
        (SHADOW_OFFSET[1] * k).round() as isize,
    );
    let layer_color = group.shadow_kind == 2;
    // Darkening per channel: neutral is grey; layer-color subtracts the
    // complement of the casting layer's colour.
    let mut chans: [Vec<f32>; 3] = [vec![0.0; n], vec![0.0; n], vec![0.0; n]];
    for y in 0..size as isize {
        let sy = y - dy;
        if sy < 0 || sy >= size as isize {
            continue;
        }
        for x in 0..size as isize {
            let sx = x - dx;
            if sx < 0 || sx >= size as isize {
                continue;
            }
            let si = sy as usize * size + sx as usize;
            let a = union[si];
            if a <= 0.0 {
                continue;
            }
            let di = y as usize * size + x as usize;
            if layer_color {
                let mut col = [0.0f32; 3];
                let mut w = 0.0;
                for p in placed {
                    if p.alpha[si] > 0.0 {
                        for c in 0..3 {
                            col[c] += p.rgb[si][c] * p.alpha[si];
                        }
                        w += p.alpha[si];
                    }
                }
                for c in 0..3 {
                    let colc = if w > 0.0 { col[c] / w } else { 0.0 };
                    chans[c][di] = a * SHADOW_LAYERCOLOR_PEAK * (1.0 - colc);
                }
            } else {
                for ch in chans.iter_mut() {
                    ch[di] = a * SHADOW_NEUTRAL_PEAK;
                }
            }
        }
    }
    for ch in chans.iter_mut() {
        blur(ch, size, SHADOW_SIGMA * k);
    }
    for i in 0..n {
        let cover = union[i];
        for c in 0..3 {
            let d = chans[c][i] * group.shadow_opacity * (1.0 - cover);
            canvas[i][c] *= 1.0 - d.clamp(0.0, 1.0);
        }
    }
}

fn composite(dst: &mut [f32; 3], src: [f32; 3], a: f32, blend: Blend) {
    for c in 0..3 {
        let cb = dst[c];
        let cs = src[c];
        dst[c] = match blend {
            Blend::PlusLighter => (cb + cs * a).min(1.0),
            Blend::PlusDarker => (cb - (1.0 - cs) * a).max(0.0),
            _ => (1.0 - a) * cb + a * blend_channel(blend, cb, cs),
        };
    }
}

fn blend_channel(mode: Blend, cb: f32, cs: f32) -> f32 {
    let hard = |cb: f32, cs: f32| {
        if cs <= 0.5 {
            2.0 * cb * cs
        } else {
            1.0 - 2.0 * (1.0 - cb) * (1.0 - cs)
        }
    };
    match mode {
        Blend::Multiply => cb * cs,
        Blend::Screen => cb + cs - cb * cs,
        Blend::Overlay => hard(cs, cb),
        Blend::HardLight => hard(cb, cs),
        Blend::Darken => cb.min(cs),
        Blend::Lighten => cb.max(cs),
        Blend::ColorDodge => {
            if cb <= 0.0 {
                0.0
            } else if cs >= 1.0 {
                1.0
            } else {
                (cb / (1.0 - cs)).min(1.0)
            }
        }
        Blend::ColorBurn => {
            if cb >= 1.0 {
                1.0
            } else if cs <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - cb) / cs).min(1.0)
            }
        }
        Blend::SoftLight => {
            if cs <= 0.5 {
                cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
            } else {
                let d = if cb <= 0.25 {
                    ((16.0 * cb - 12.0) * cb + 4.0) * cb
                } else {
                    cb.sqrt()
                };
                cb + (2.0 * cs - 1.0) * (d - cb)
            }
        }
        Blend::Difference => (cb - cs).abs(),
        Blend::Exclusion => cb + cs - 2.0 * cb * cs,
        _ => cs,
    }
}

/// Signed-ish depth (pt) inside the rim band's outer boundary: a square with
/// superellipse corners. Negative outside (the corner wedges).
fn rim_depth(x: f32, y: f32) -> f32 {
    let r = RIM_CORNER_RADIUS;
    let qx = (r - x.min(1024.0 - x)).max(0.0);
    let qy = (r - y.min(1024.0 - y)).max(0.0);
    let edge = x.min(y).min(1024.0 - x).min(1024.0 - y);
    if qx <= 0.0 || qy <= 0.0 {
        return edge;
    }
    let m = RIM_CORNER_EXP;
    let g = ((qx / r).powf(m) + (qy / r).powf(m)).powf(1.0 / m);
    // First-order distance to the g = 1 level set.
    let pow = ((qx / r).powf(m) + (qy / r).powf(m)).powf(1.0 / m - 1.0);
    let gx = pow * (qx / r).powf(m - 1.0) / r;
    let gy = pow * (qy / r).powf(m - 1.0) / r;
    (1.0 - g) / (gx * gx + gy * gy).sqrt().max(1e-6)
}

/// Peak sRGB lift of the rim at canvas point (x, y) for `appearance`. The
/// light catches the top-left rim and, weaker, the bottom-right one: measured
/// along the edges of a flat fill as a function of `u = (x + y) / 2048`.
fn rim_peak(x: f32, y: f32, appearance: IosAppearance) -> f32 {
    let u = (x + y) / 2048.0;
    let ramp = |a: f32, b: f32| ((u - a) / (b - a)).clamp(0.0, 1.0);
    match appearance {
        IosAppearance::Dark => {
            18.0 + 132.0 * ramp(0.40, 0.10).powf(0.7) + 100.0 * ramp(0.60, 0.90).powf(0.8)
        }
        _ => 20.0 + 80.0 * ramp(0.42, 0.07).powf(0.8) + 52.0 * ramp(0.62, 0.92).powf(0.9),
    }
}

fn apply_rim_light(canvas: &mut [[f32; 3]], size: usize, appearance: IosAppearance) {
    let k = size as f32 / 1024.0;
    let band = (RIM_DEPTH + 2.0) * k;
    for y in 0..size {
        for x in 0..size {
            let (px, py) = ((x as f32 + 0.5) / k, (y as f32 + 0.5) / k);
            let near = (x as f32).min(y as f32).min((size - 1 - x) as f32).min((size - 1 - y) as f32);
            if near > band + RIM_CORNER_RADIUS * k {
                continue;
            }
            let d = rim_depth(px, py);
            if d < 0.0 || d >= RIM_DEPTH {
                continue;
            }
            let lift = rim_peak(px, py, appearance) / 255.0 * (1.0 - d / RIM_DEPTH);
            for c in &mut canvas[y * size + x] {
                *c = (*c + lift).min(1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(rgb: [f32; 3]) -> Background {
        Background { start_rgb: rgb, stop_rgb: rgb, start: [0.5, 0.1], stop: [0.5, 0.9] }
    }

    #[test]
    fn rim_light_brightens_edges_but_not_centre() {
        let scene = Scene {
            size: 256,
            appearance: IosAppearance::Any,
            background: Some(flat([0.5, 0.5, 0.5])),
            groups: vec![],
        };
        let out = render(&scene);
        let px = |x: usize, y: usize| out[(y * 256 + x) * 4] as i32;
        assert_eq!(px(128, 128), 128);
        assert!(px(0, 128) > px(128, 128) + 40, "left edge lit");
        assert!(px(255, 128) > px(128, 128) + 5, "right edge lit, weaker");
        assert!(px(0, 128) > px(255, 128), "light comes from the top-left");
        assert!(out.chunks(4).all(|p| p[3] == 255), "flattened icon is opaque");
    }

    #[test]
    fn tinted_is_luma_on_black() {
        let layer = SceneLayer {
            rgba: [255u8, 0, 0, 255].repeat(16 * 16),
            w: 16,
            h: 16,
            x: 120,
            y: 120,
            opacity: 1.0,
            blend: Blend::Normal,
            fill: None,
        };
        let scene = Scene {
            size: 256,
            appearance: IosAppearance::Tinted,
            background: None,
            groups: vec![SceneGroup {
                layers: vec![layer],
                opacity: 1.0,
                blend: Blend::Normal,
                shadow_kind: 0,
                shadow_opacity: 0.0,
                specular: false,
                translucency: None,
                combined_lighting: false,
            }],
        };
        let out = render(&scene);
        let px = |x: usize, y: usize| &out[(y * 256 + x) * 4..(y * 256 + x) * 4 + 4];
        assert_eq!(px(2, 2), &[0, 0, 0, 255]);
        // Rec. 709 luma of pure red, as Apple's tinted rendition stores it.
        assert_eq!(px(128, 128), &[54, 54, 54, 255]);
    }

    #[test]
    fn translucent_glass_shows_backdrop_at_the_bottom() {
        let layer = SceneLayer {
            rgba: [255u8, 255, 255, 255].repeat(100 * 100),
            w: 100,
            h: 100,
            x: 78,
            y: 78,
            opacity: 1.0,
            blend: Blend::Normal,
            fill: None,
        };
        let scene = Scene {
            size: 256,
            appearance: IosAppearance::Any,
            background: Some(flat([0.0, 0.0, 0.0])),
            groups: vec![SceneGroup {
                layers: vec![layer],
                opacity: 1.0,
                blend: Blend::Normal,
                shadow_kind: 0,
                shadow_opacity: 0.0,
                specular: false,
                translucency: Some(0.5),
                combined_lighting: false,
            }],
        };
        let out = render(&scene);
        let g = |y: usize| out[(y * 256 + 128) * 4] as i32;
        assert_eq!(g(79), 255, "top of a glass layer is opaque");
        assert!((g(177) - 128).abs() <= 3, "bottom lets half the backdrop through: {}", g(177));
    }
}
