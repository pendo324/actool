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
    pub glass: bool,
    /// Solid fill replacing the layer's own colours (keeps its alpha).
    pub fill: Option<[f32; 3]>,
}

/// A group's layers (back-to-front) and its appearance-resolved effects. The
/// group's translucency and specular only act on its glass layers (a
/// non-glass disc in a translucent group stays solid); its shadow is cast by
/// every layer.
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

/// See-through of a translucent group's glass, measured on a glass disc over
/// black (tinted) for translucency values 0.12…1.0: the fraction of the
/// backdrop showing through at fraction `s` down the glass. Opaque at the
/// top, an S-shaped ramp towards the bottom whose depth grows faster than
/// linearly with the value. Rows: value; columns: `TRANSLUCENCY_S`.
const TRANSLUCENCY_S: [f32; 7] = [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];
const TRANSLUCENCY_TABLE: [(f32, [f32; 7]); 6] = [
    (0.0, [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    (0.12, [0.0, 0.0, 0.007, 0.013, 0.020, 0.033, 0.046]),
    (0.25, [0.0, 0.0, 0.013, 0.040, 0.093, 0.126, 0.152]),
    (0.5, [0.0, 0.007, 0.046, 0.159, 0.318, 0.424, 0.450]),
    (0.8, [0.0, 0.020, 0.106, 0.351, 0.649, 0.808, 0.815]),
    (1.0, [0.0, 0.026, 0.159, 0.503, 0.841, 0.974, 0.974]),
];

fn interp(xs: &[f32], ys: &[f32], x: f32) -> f32 {
    let x = x.clamp(xs[0], xs[xs.len() - 1]);
    let i = xs.windows(2).position(|w| x <= w[1]).unwrap_or(xs.len() - 2);
    let t = (x - xs[i]) / (xs[i + 1] - xs[i]).max(1e-6);
    ys[i] + (ys[i + 1] - ys[i]) * t
}

fn translucency_see_through(value: f32, s: f32) -> f32 {
    let at = |row: &[f32; 7]| interp(&TRANSLUCENCY_S, row, s);
    let values: Vec<f32> = TRANSLUCENCY_TABLE.iter().map(|(v, _)| *v).collect();
    let column: Vec<f32> = TRANSLUCENCY_TABLE.iter().map(|(_, row)| at(row)).collect();
    interp(&values, &column, value)
}

/// Specular rim: a white highlight in a band 6…18 pt inside the
/// layer edge, strongest (0.41 toward white) at its outer side.
const SPECULAR_INNER: f32 = 6.0;
const SPECULAR_WIDTH: f32 = 12.0;
const SPECULAR_PEAK: f32 = 0.25;

/// Glass edge: the outer rim of a glass layer is an opaque band (not
/// see-through) that blends into the interior over ~24 pt. Without specular
/// it is darker (0.84 × its colour) with a slight dip around 11 pt (t_trans
/// probe: 124 at the edge → 112 at 10 pt → 128 interior); with specular it
/// keeps the layer colour and carries the highlight instead (glass probe).
const GLASS_EDGE_SHADE: f32 = 0.84;
const GLASS_EDGE_WIDTH: f32 = 24.0;
const GLASS_EDGE_DIP: f32 = 0.12;
const GLASS_EDGE_DIP_AT: f32 = 11.0;
const GLASS_EDGE_DIP_WIDTH: f32 = 6.0;

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
/// Rim strength (white screened over the tile) along the edge: a base level
/// plus a top-left and a weaker bottom-right highlight; see [`rim_peak`].
const RIM_BASE: f32 = 0.13;
const RIM_TOP_LEFT: f32 = 0.65;
const RIM_BOTTOM_RIGHT: f32 = 0.45;

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
        apply_rim_light(&mut canvas, size);
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

/// Weight of the interior (vs the opaque edge band) at depth `d` inside a
/// glass layer.
fn glass_interior_weight(d: f32) -> f32 {
    let t = (d / GLASS_EDGE_WIDTH).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Composite a group: its layers are flattened into an isolated buffer (so a
/// translucent group shows what lies *behind the group*, not its own lower
/// layers — Apple's tinted eyes stay dark over the dome), then the group's
/// see-through ramp (over its glass coverage), opacity and blend apply as a
/// unit.
fn draw_group(canvas: &mut [[f32; 3]], size: usize, k: f32, group: &SceneGroup) {
    let n = size * size;
    let placed: Vec<Placed> = group.layers.iter().map(|l| place(l, size, l.opacity)).collect();
    let Some(bbox) = placed.iter().filter_map(|p| p.bbox).reduce(|a, b| {
        (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
    }) else {
        return;
    };

    let mut union = vec![0.0f32; n];
    let mut glass_union = vec![0.0f32; n];
    let mut glass_bbox: Option<(usize, usize, usize, usize)> = None;
    for (layer, p) in group.layers.iter().zip(&placed) {
        for i in 0..n {
            union[i] = union[i] + p.alpha[i] * (1.0 - union[i]);
            if layer.glass {
                glass_union[i] = glass_union[i] + p.alpha[i] * (1.0 - glass_union[i]);
            }
        }
        if layer.glass {
            if let Some(b) = p.bbox {
                glass_bbox = Some(match glass_bbox {
                    None => b,
                    Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
                });
            }
        }
    }

    if group.shadow_kind != 0 && group.shadow_opacity > 0.0 {
        draw_shadow(canvas, size, k, group, &placed, &union);
    }

    let glass_depth = glass_bbox.map(|_| inner_depth(&glass_union, size, k));

    // Premultiplied group buffer.
    let mut g_rgb = vec![[0.0f32; 3]; n];
    let mut g_a = vec![0.0f32; n];
    for (layer, p) in group.layers.iter().zip(&placed) {
        let Some(lb) = p.bbox else { continue };
        let depth = if !layer.glass {
            None
        } else if group.combined_lighting {
            glass_depth.clone()
        } else {
            Some(inner_depth(&p.alpha, size, k))
        };
        for y in lb.1..=lb.3 {
            for x in lb.0..=lb.2 {
                let i = y * size + x;
                let a = p.alpha[i];
                if a <= 0.0 {
                    continue;
                }
                let mut col = p.rgb[i];
                if let Some(depth) = &depth {
                    let d = depth[i];
                    if group.specular {
                        let h = specular_amount(d);
                        for c in &mut col {
                            *c += h * (1.0 - *c);
                        }
                    } else {
                        let w = glass_interior_weight(d);
                        let dip = 1.0
                            - GLASS_EDGE_DIP
                                * (-((d - GLASS_EDGE_DIP_AT) / GLASS_EDGE_DIP_WIDTH).powi(2))
                                    .exp();
                        for c in &mut col {
                            *c = (1.0 - w) * GLASS_EDGE_SHADE * *c + w * dip * *c;
                        }
                    }
                }
                // Additive layers (the antenna glow) light up whatever is
                // behind the group too, so they bypass the isolated buffer.
                if matches!(layer.blend, Blend::PlusLighter | Blend::PlusDarker) {
                    composite(&mut canvas[i], col, a * group.opacity, layer.blend);
                } else {
                    composite_premul(&mut g_rgb[i], &mut g_a[i], col, a, layer.blend);
                }
            }
        }
    }

    let ramp = match (group.translucency, glass_bbox, &glass_depth) {
        (Some(v), Some(gb), Some(gd)) => Some((v, gb, gd)),
        _ => None,
    };
    for y in bbox.1..=bbox.3 {
        for x in bbox.0..=bbox.2 {
            let i = y * size + x;
            if g_a[i] <= 0.0 {
                continue;
            }
            let see_through = ramp.map_or(0.0, |(v, gb, gd)| {
                let s = ((y as f32 - gb.1 as f32) / (gb.3 as f32 - gb.1 as f32).max(1.0))
                    .clamp(0.0, 1.0);
                translucency_see_through(v, s) * glass_union[i] * glass_interior_weight(gd[i])
            });
            let a = g_a[i] * (1.0 - see_through) * group.opacity;
            let col = [g_rgb[i][0] / g_a[i], g_rgb[i][1] / g_a[i], g_rgb[i][2] / g_a[i]];
            composite(&mut canvas[i], col, a, group.blend);
        }
    }
}

/// Source-over with a blend mode into a premultiplied, possibly transparent
/// buffer (W3C compositing: the blend only applies where the backdrop has
/// coverage).
fn composite_premul(dst: &mut [f32; 3], dst_a: &mut f32, src: [f32; 3], a: f32, blend: Blend) {
    let ab = *dst_a;
    if blend == Blend::PlusLighter {
        for c in 0..3 {
            dst[c] = (dst[c] + src[c] * a).min(1.0);
        }
        *dst_a = (ab + a).min(1.0);
        return;
    }
    for c in 0..3 {
        let cb = if ab > 0.0 { dst[c] / ab } else { 0.0 };
        let mixed = (1.0 - ab) * src[c] + ab * blend_channel(blend, cb, src[c]);
        dst[c] = a * mixed + (1.0 - a) * dst[c];
    }
    *dst_a = a + ab * (1.0 - a);
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

/// Peak strength of the rim light at canvas point (x, y): it acts like white
/// screened over the tile, with the same strength on light and dark
/// backgrounds (the Any render of a dark icon carries the same rim as its
/// Dark render). The light catches the top-left rim and, weaker, the
/// bottom-right one: fitted along the edges of flat fills as a function of
/// `u = (x + y) / 2048`.
fn rim_peak(x: f32, y: f32) -> f32 {
    let u = (x + y) / 2048.0;
    let ramp = |a: f32, b: f32| ((u - a) / (b - a)).clamp(0.0, 1.0);
    RIM_BASE + RIM_TOP_LEFT * ramp(0.42, 0.07).powf(0.75)
        + RIM_BOTTOM_RIGHT * ramp(0.62, 0.92).powf(0.85)
}

fn apply_rim_light(canvas: &mut [[f32; 3]], size: usize) {
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
            let a = (rim_peak(px, py) * (1.0 - d / RIM_DEPTH)).min(1.0);
            for c in &mut canvas[y * size + x] {
                *c += a * (1.0 - *c);
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
            glass: false,
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

    fn slab(glass: bool) -> Scene {
        let layer = SceneLayer {
            rgba: [255u8, 255, 255, 255].repeat(100 * 100),
            w: 100,
            h: 100,
            x: 78,
            y: 78,
            opacity: 1.0,
            blend: Blend::Normal,
            glass,
            fill: None,
        };
        Scene {
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
        }
    }

    #[test]
    fn translucent_glass_shows_backdrop_at_the_bottom() {
        let out = render(&slab(true));
        let g = |y: usize| out[(y * 256 + 128) * 4] as i32;
        assert!(g(86) >= 250, "top of a glass layer is opaque: {}", g(86));
        // 93 % of the way down, a 0.5 translucency lets ~43 % through.
        assert!((130..=160).contains(&g(170)), "bottom shows the backdrop: {}", g(170));
        // The outer edge band is darker but not see-through.
        assert!((200..=225).contains(&g(176)), "edge band: {}", g(176));
    }

    #[test]
    fn translucency_only_affects_glass() {
        let out = render(&slab(false));
        assert_eq!(out[(170 * 256 + 128) * 4], 255);
    }

    #[test]
    fn translucency_table_interpolates() {
        assert_eq!(translucency_see_through(0.0, 0.9), 0.0);
        assert!((translucency_see_through(1.0, 0.5) - 0.503).abs() < 1e-6);
        let mid = translucency_see_through(0.65, 0.5);
        assert!(mid > 0.159 && mid < 0.351);
    }
}
