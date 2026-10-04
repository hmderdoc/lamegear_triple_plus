//! 16-color cell matcher that shades with CP437 `░▒▓`, ported from the
//! shadeans converter.
//!
//! Classic ANSI has 16 foregrounds and 8 backgrounds. Half blocks alone give
//! each half-cell one of those flat colors; the shade glyphs blend a
//! foreground over a background at 25/50/75% coverage, which turns the
//! palette into a few hundred tones. Each cell picks whichever candidate fits
//! its pixels best:
//!
//!   - **Uniform** (space, full block, the three shades): the eye fuses the
//!     cell into one mixed color M (mixed in linear light), so the cost is
//!     every pixel's Oklab distance to M plus a texture cost
//!     `LAMBDA * a(1-a) * |fg-bg|^2` — the variance of a two-color pattern of
//!     coverage `a`, i.e. how much dither the eye still sees. That favours
//!     shading between close colors, the way ramps are drawn by hand.
//!   - **Half block** (`▀▄`, and `▌▐` when a cell covers more than one source
//!     column): the two halves get their own palette colors, keeping detail
//!     where the picture has an edge — thin text strokes, sprite outlines.
//!
//! Both kinds may only use **supported** colors: ones at least a fifth of
//! the region's pixels are closest to. A cell with only one supported color
//! is a smooth area, so its shades may also use the palette color nearest
//! its mean plus the one that mixes with it toward the mean, which a
//! gradient between palette steps needs. Without that rule the mean of yellow text on black is an
//! olive that lands on green or brown, and bluish-grey rock picks up cyan —
//! hues nothing in the picture has.
//!
//! The unrestricted best uniform candidate for every mean color is
//! precomputed once per process into a 15-bit RGB table. It is used as-is
//! when its colors are supported, which is most cells; otherwise only the
//! few supported pairs are searched. The choice is a pure function of the
//! pixels, so a static scene still deltas to nothing.

use crate::color::ANSI16;
use std::sync::OnceLock;

type V3 = [f32; 3];

/// Share of the dither pattern the eye still notices (shadeans' default).
const LAMBDA: f32 = 0.10;
/// Extra weight on chroma when choosing stand-in colors (see `hue_dist2`).
const CHROMA_WEIGHT: f32 = 9.0;

const SPACE: u8 = 0x20;
const FULL_BLOCK: u8 = 0xDB;
const UPPER_HALF: u8 = 0xDF;
const LOWER_HALF: u8 = 0xDC;
const LEFT_HALF: u8 = 0xDD;
const RIGHT_HALF: u8 = 0xDE;
/// `░▒▓` and their VGA-font coverage (2, 4 and 6 of every 8 pixels).
const SHADES: [(u8, f32); 3] = [(0xB0, 0.25), (0xB1, 0.5), (0xB2, 0.75)];

/// Bits per channel of the lookup table.
const BITS: u32 = 5;
const LEVELS: usize = 1 << BITS;
const LIN_STEPS: usize = 4096;

/// One 15-bit RGB bin: its Oklab color, nearest palette colors, and the
/// best uniform candidate for a cell whose mean is this color.
#[derive(Clone, Copy)]
struct Bin {
    lab: V3,
    sq: f32,
    near_any: u8,
    near_dark: u8,
    /// The nearest palette color and its gradient partner, as a bitmask.
    near2: u16,
    uniform: u16,
}

/// A uniform candidate: what the eye sees and the texture it pays for.
#[derive(Clone, Copy)]
struct Uniform {
    ch: u8,
    fg: u8,
    bg: u8,
    lab: V3,
    texture: f32,
    /// Palette colors visible in the cell.
    mask: u16,
}

pub struct Shade16 {
    bins: Vec<Bin>,
    /// 16 solids, then one slot per (level, fg, bg) — see `shade_slot`.
    uniforms: Vec<Uniform>,
    pal_lab: [V3; 16],
    /// Each palette color's dark stand-in (itself if dark), as a bitmask.
    dark_of: [u16; 16],
    /// sRGB byte -> table level.
    quant: [u8; 256],
    /// Linear light (0..1 in LIN_STEPS) -> sRGB byte.
    lin_to_srgb: Vec<u8>,
}

/// Pixel statistics of one region of a cell, for squared-error costs in Oklab.
#[derive(Clone, Copy, Default)]
pub struct Region {
    n: f32,
    sum: V3,
    sum_sq: f32,
    /// How many pixels are closest to each palette color.
    support: [u16; 16],
    /// The first pixel's key, and whether any later pixel differed.
    first: u16,
    mixed: bool,
}

impl Region {
    /// Sum over the region's pixels of |pixel - c|^2.
    #[inline]
    fn cost(&self, c: V3) -> f32 {
        self.sum_sq - 2.0 * dot(c, self.sum) + self.n * dot(c, c)
    }

    /// The key every pixel of this region shares, if it is one flat color.
    #[inline]
    pub fn flat_key(&self) -> Option<u16> {
        (self.n > 0.0 && !self.mixed).then_some(self.first)
    }

    /// Squared error left when the region is painted its own mean color:
    /// a lower bound on any flat color's cost.
    #[inline]
    fn spread(&self) -> f32 {
        (self.sum_sq - dot(self.sum, self.sum) / self.n).max(0.0)
    }

    pub fn merge(&self, o: &Region) -> Region {
        if o.n == 0.0 {
            return *self;
        }
        if self.n == 0.0 {
            return *o;
        }
        Region {
            n: self.n + o.n,
            sum: std::array::from_fn(|k| self.sum[k] + o.sum[k]),
            sum_sq: self.sum_sq + o.sum_sq,
            support: std::array::from_fn(|k| self.support[k] + o.support[k]),
            first: self.first,
            mixed: self.mixed || o.mixed || self.first != o.first,
        }
    }

    /// Palette colors at least a fifth of the pixels are closest to.
    fn supported(&self) -> u16 {
        // ceil(n / 5), at least 1.
        let needed = ((self.n as u32 + 4) / 5).max(1) as u16;
        let mut mask = 0;
        for (k, &c) in self.support.iter().enumerate() {
            mask |= ((c >= needed) as u16) << k;
        }
        mask
    }
}

/// The shared table, built on first use (tens of milliseconds).
pub fn table() -> &'static Shade16 {
    static T: OnceLock<Shade16> = OnceLock::new();
    T.get_or_init(Shade16::build)
}

/// Index of the shade candidate (level, fg, bg) in `Shade16::uniforms`.
#[inline]
fn shade_slot(level: usize, f: usize, b: usize) -> usize {
    16 + (level * 16 + f) * 8 + b
}

/// One candidate (glyph, fg, bg) with its total squared error.
type Pick = ((u8, u8, u8), f32);

impl Shade16 {
    fn build() -> Self {
        let lin = srgb_to_linear;
        let pal_lin: [V3; 16] =
            std::array::from_fn(|i| [lin(ANSI16[i][0]), lin(ANSI16[i][1]), lin(ANSI16[i][2])]);
        let pal_lab: [V3; 16] = std::array::from_fn(|i| linear_to_oklab(pal_lin[i]));

        let mut uniforms = Vec::new();
        for c in 0..16u8 {
            let (ch, fg, bg) = solid(c);
            let lab = pal_lab[c as usize];
            uniforms.push(Uniform {
                ch,
                fg,
                bg,
                lab,
                texture: 0.0,
                mask: 1 << c,
            });
        }
        for &(ch, a) in &SHADES {
            for f in 0..16 {
                for b in 0..8 {
                    let (pf, pb) = (pal_lin[f], pal_lin[b]);
                    let mix = std::array::from_fn(|k| pf[k] * a + pb[k] * (1.0 - a));
                    uniforms.push(Uniform {
                        ch,
                        fg: f as u8,
                        bg: b as u8,
                        lab: linear_to_oklab(mix),
                        texture: LAMBDA * a * (1.0 - a) * dist2(pal_lab[f], pal_lab[b]),
                        mask: (1 << f) | (1 << b),
                    });
                }
            }
        }
        // f == b slots are a solid under another glyph; never pick them.
        let usable: Vec<usize> = (0..uniforms.len())
            .filter(|&i| i < 16 || uniforms[i].fg != uniforms[i].bg)
            .collect();

        let level = |q: usize| (q * 255 / (LEVELS - 1)) as u8;
        let mut bins = Vec::with_capacity(LEVELS * LEVELS * LEVELS);
        for r in 0..LEVELS {
            for g in 0..LEVELS {
                for b in 0..LEVELS {
                    let lab = linear_to_oklab([lin(level(r)), lin(level(g)), lin(level(b))]);
                    let mut order: [usize; 16] = std::array::from_fn(|i| i);
                    order.sort_by(|&x, &y| {
                        hue_dist2(pal_lab[x], lab).total_cmp(&hue_dist2(pal_lab[y], lab))
                    });
                    let cost = |i: usize| dist2(uniforms[i].lab, lab) + uniforms[i].texture;
                    let uniform = *usable
                        .iter()
                        .min_by(|&&x, &&y| cost(x).total_cmp(&cost(y)))
                        .unwrap();
                    // Gradient partner: the color that, mixed with the
                    // nearest, gets closest to this one without a loud
                    // dither (the same texture cost as the shades). Ties —
                    // no mix beats the nearest alone — go to the next
                    // nearest by hue.
                    let near = pal_lab[order[0]];
                    let mut partner = order[1];
                    let mut partner_cost = mix_cost(near, pal_lab[partner], lab);
                    for &k in &order[2..] {
                        let c = mix_cost(near, pal_lab[k], lab);
                        if c < partner_cost - 1e-6 {
                            partner = k;
                            partner_cost = c;
                        }
                    }
                    bins.push(Bin {
                        lab,
                        sq: dot(lab, lab),
                        near_any: order[0] as u8,
                        near_dark: *order.iter().find(|&&k| k < 8).unwrap() as u8,
                        near2: (1 << order[0]) | (1 << partner),
                        uniform: uniform as u16,
                    });
                }
            }
        }

        let quant = std::array::from_fn(|v| ((v * (LEVELS - 1) + 127) / 255) as u8);
        let lin_to_srgb = (0..LIN_STEPS)
            .map(|i| linear_to_srgb(i as f32 / (LIN_STEPS - 1) as f32))
            .collect();
        let dark_of = std::array::from_fn(|c| {
            let d = (0..8)
                .min_by(|&x, &y| {
                    hue_dist2(pal_lab[x], pal_lab[c]).total_cmp(&hue_dist2(pal_lab[y], pal_lab[c]))
                })
                .unwrap();
            1u16 << d
        });
        Shade16 {
            bins,
            uniforms,
            pal_lab,
            dark_of,
            quant,
            lin_to_srgb,
        }
    }

    #[inline]
    fn bin_index(&self, r: u8, g: u8, b: u8) -> usize {
        let q = &self.quant;
        ((q[r as usize] as usize) << (2 * BITS))
            | ((q[g as usize] as usize) << BITS)
            | q[b as usize] as usize
    }

    /// Opaque key for a pixel: equal keys always match identically.
    #[inline]
    pub fn key(&self, r: u8, g: u8, b: u8) -> u16 {
        self.bin_index(r, g, b) as u16
    }

    /// Add one pixel to a region's statistics.
    #[inline]
    pub fn add(&self, h: &mut Region, r: u8, g: u8, b: u8) {
        let key = self.bin_index(r, g, b);
        if h.n == 0.0 {
            h.first = key as u16;
        } else {
            h.mixed |= h.first != key as u16;
        }
        let bin = &self.bins[key];
        h.n += 1.0;
        h.sum[0] += bin.lab[0];
        h.sum[1] += bin.lab[1];
        h.sum[2] += bin.lab[2];
        h.sum_sq += bin.sq;
        h.support[bin.near_any as usize] += 1;
    }

    /// Fast path for a cell of exactly one pixel per half (native and clipped
    /// fits), keyed by `key()`. Same answer as `match_cell` on those pixels.
    pub fn match_pair(&self, top: u16, bot: u16) -> (u8, u8, u8) {
        let (t, b) = (&self.bins[top as usize], &self.bins[bot as usize]);
        if top == bot {
            return self.best_uniform(t.lab, 1 << t.near_any, 2.0).0;
        }
        let mean = std::array::from_fn(|k| (t.lab[k] + b.lab[k]) * 0.5);
        let supported = (1 << t.near_any) | (1 << b.near_any);
        let (u, u_cost) = self.best_uniform(mean, supported, 2.0);
        // Uniform cost above is relative to the mean; add the pixels' spread.
        let uniform = (u, u_cost + 0.5 * dist2(t.lab, b.lab));
        // One pixel per half: its nearest color is its only supported one,
        // and its background stand-in that color's dark one.
        let d = |bin: &Bin, k: u8| (k, dist2(self.pal_lab[k as usize], bin.lab));
        let dark = |bin: &Bin| {
            d(
                bin,
                self.dark_of[bin.near_any as usize].trailing_zeros() as u8,
            )
        };
        let split = halves(
            UPPER_HALF,
            LOWER_HALF,
            (d(t, t.near_any), dark(t)),
            (d(b, b.near_any), dark(b)),
        );
        finish(if uniform.1 <= split.1 { uniform } else { split })
    }

    /// Match a cell from its quadrants' statistics. `split_x` says the
    /// quadrants really differ left to right (more than one source column);
    /// otherwise pass the same column as both left and right.
    pub fn match_cell(
        &self,
        tl: &Region,
        tr: &Region,
        bl: &Region,
        br: &Region,
        split_x: bool,
    ) -> (u8, u8, u8) {
        let (top, bot) = if split_x {
            (tl.merge(tr), bl.merge(br))
        } else {
            (*tl, *bl)
        };
        // A half with no pixels (the source's last row) mirrors the other.
        let (top, bot) = match (top.n > 0.0, bot.n > 0.0) {
            (true, true) => (top, bot),
            (true, false) => (top, top),
            (false, true) => (bot, bot),
            (false, false) => return solid(0),
        };
        let all = top.merge(&bot);
        let mean = std::array::from_fn(|k| all.sum[k] / all.n);
        let (u, u_cost) = self.best_uniform(mean, all.supported(), all.n);
        // Uniform cost above is relative to the mean; add the pixels' spread.
        let mut best = (u, u_cost + all.spread());
        // Try the split along the axis that separates the cell best (least
        // spread left within its halves) — and only if it can win at all: a
        // split costs at least its halves' own spreads.
        let mut split = (top, bot, (UPPER_HALF, LOWER_HALF));
        let mut floor = top.spread() + bot.spread();
        if split_x {
            let (left, right) = (tl.merge(bl), tr.merge(br));
            if left.n > 0.0 && right.n > 0.0 && left.spread() + right.spread() < floor {
                floor = left.spread() + right.spread();
                split = (left, right, (LEFT_HALF, RIGHT_HALF));
            }
        }
        if floor < best.1 {
            let (a, b, (ga, gb)) = split;
            let p = halves(ga, gb, self.half_colors(&a), self.half_colors(&b));
            if p.1 < best.1 {
                best = p;
            }
        }
        finish(best)
    }

    /// Best uniform candidate for a cell of this mean whose colors are all
    /// supported (or, in a smooth area, the mean's nearest color or its gradient partner); its
    /// cost for `n` pixels sitting exactly on the mean.
    fn best_uniform(&self, mean: V3, supported: u16, n: f32) -> Pick {
        let bin = &self.bins[self.lab_to_bin(mean)];
        let allowed = if supported.count_ones() >= 2 {
            supported
        } else {
            supported | bin.near2
        };
        let cost = |i: usize| {
            let u = &self.uniforms[i];
            n * (dist2(u.lab, mean) + u.texture)
        };
        let mut best = bin.uniform as usize;
        if self.uniforms[best].mask & !allowed != 0 {
            // Search only the allowed colors: usually two to four of them.
            let colors = || bits(allowed);
            let mut best_cost = f32::INFINITY;
            let mut consider = |i: usize| {
                let c = cost(i);
                if c < best_cost {
                    best = i;
                    best_cost = c;
                }
            };
            for f in colors() {
                consider(f);
                for b in bits(allowed & 0xFF & !(1 << f)) {
                    for level in 0..SHADES.len() {
                        consider(shade_slot(level, f, b));
                    }
                }
            }
        }
        let u = &self.uniforms[best];
        ((u.ch, u.fg, u.bg), cost(best))
    }

    /// A half's best (any color, dark color) with their costs, each from
    /// its supported colors — or from all of them when none is supported.
    /// A region's squared error to a flat color is least for the color
    /// nearest its mean, so the unrestricted answer is a table lookup.
    fn half_colors(&self, h: &Region) -> ((u8, f32), (u8, f32)) {
        let inv = 1.0 / h.n;
        let mean: V3 = std::array::from_fn(|k| h.sum[k] * inv);
        let supported = h.supported();
        let nearest = |mask: u16| {
            let mut best = (0u8, f32::INFINITY);
            for k in bits(mask) {
                let c = hue_dist2(self.pal_lab[k], mean);
                if c < best.1 {
                    best = (k as u8, c);
                }
            }
            best.0
        };
        let (any, dark) = if supported == 0 {
            let b = &self.bins[self.lab_to_bin(mean)];
            (b.near_any, b.near_dark)
        } else if supported & 0xFF == 0 {
            // Only bright colors present: the background is one of their
            // dark stand-ins (not the mean's, which can be a third hue —
            // green over gray averages to something nearest cyan).
            let stand_ins = bits(supported).fold(0, |m, k| m | self.dark_of[k]);
            (nearest(supported), nearest(stand_ins))
        } else {
            (nearest(supported), nearest(supported & 0xFF))
        };
        let cost = |k: u8| (k, h.cost(self.pal_lab[k as usize]));
        (cost(any), cost(dark))
    }

    #[inline]
    fn lab_to_bin(&self, lab: V3) -> usize {
        let lin = oklab_to_linear(lab);
        let s = |c: f32| {
            let i = (c.clamp(0.0, 1.0) * (LIN_STEPS - 1) as f32 + 0.5) as usize;
            self.lin_to_srgb[i]
        };
        self.bin_index(s(lin[0]), s(lin[1]), s(lin[2]))
    }
}

/// Indexes of the set bits of a palette mask.
#[inline]
fn bits(mut mask: u16) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let k = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        Some(k)
    })
}

/// The better of the two glyphs splitting a cell into halves A and B, given
/// each half's best (any color, dark color). Only the foreground can be
/// bright, so glyph `a_glyph` (fg paints A) needs a dark B, and `b_glyph`
/// a dark A.
fn halves(a_glyph: u8, b_glyph: u8, a: ((u8, f32), (u8, f32)), b: ((u8, f32), (u8, f32))) -> Pick {
    let (a_any, a_dark) = a;
    let (b_any, b_dark) = b;
    let a_fg = a_any.1 + b_dark.1;
    let b_fg = b_any.1 + a_dark.1;
    if a_fg <= b_fg {
        ((a_glyph, a_any.0, b_dark.0), a_fg)
    } else {
        ((b_glyph, b_any.0, a_dark.0), b_fg)
    }
}

/// Canonicalize a half block whose halves landed on the same color.
fn finish(p: Pick) -> (u8, u8, u8) {
    let (ch, fg, bg) = p.0;
    match ch {
        UPPER_HALF | LOWER_HALF | LEFT_HALF | RIGHT_HALF if fg == bg => solid(fg),
        _ => (ch, fg, bg),
    }
}

/// A flat palette color: a dark one is a space on that background; a bright
/// one needs the full block (classic backgrounds are dark only).
fn solid(c: u8) -> (u8, u8, u8) {
    if c < 8 {
        (SPACE, 7, c)
    } else {
        (FULL_BLOCK, c, 0)
    }
}

#[inline]
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[inline]
fn dist2(a: V3, b: V3) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    dot(d, d)
}

/// How well some mix of colors `a` and `b` stands for `p`: the squared
/// distance from `p` to the segment between them, plus the texture cost of
/// that mix's dither.
fn mix_cost(a: V3, b: V3, p: V3) -> f32 {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ap = [p[0] - a[0], p[1] - a[1], p[2] - a[2]];
    let t = (dot(ap, ab) / dot(ab, ab).max(1e-12)).clamp(0.0, 1.0);
    dist2(p, [a[0] + t * ab[0], a[1] + t * ab[1], a[2] + t * ab[2]])
        + LAMBDA * t * (1.0 - t) * dot(ab, ab)
}

/// Distance for picking which palette color *stands for* a color, with
/// hue differences counted extra. The palette is lightness-sparse, so by
/// plain distance mid gray (101) is nearer brown than light gray, and dark
/// gray's nearest background color is brown: neutral areas would pick up
/// a hue they don't have. Costs still use plain `dist2`.
#[inline]
fn hue_dist2(a: V3, b: V3) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    d[0] * d[0] + CHROMA_WEIGHT * (d[1] * d[1] + d[2] * d[2])
}

fn srgb_to_linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

fn linear_to_oklab(c: V3) -> V3 {
    let [r, g, b] = [c[0].max(0.0), c[1].max(0.0), c[2].max(0.0)];
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

fn oklab_to_linear(c: V3) -> V3 {
    let l = c[0] + 0.396_337_78 * c[1] + 0.215_803_76 * c[2];
    let m = c[0] - 0.105_561_346 * c[1] - 0.063_854_17 * c[2];
    let s = c[0] - 0.089_484_18 * c[1] - 1.291_485_5 * c[2];
    let (l, m, s) = (l * l * l, m * m * m, s * s * s);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_4 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(rgb_t: [u8; 3], rgb_b: [u8; 3]) -> (u8, u8, u8) {
        let t = table();
        t.match_pair(
            t.key(rgb_t[0], rgb_t[1], rgb_t[2]),
            t.key(rgb_b[0], rgb_b[1], rgb_b[2]),
        )
    }

    fn region(px: &[[u8; 3]]) -> Region {
        let mut r = Region::default();
        for p in px {
            table().add(&mut r, p[0], p[1], p[2]);
        }
        r
    }

    #[test]
    fn palette_colors_come_out_solid() {
        for (i, c) in ANSI16.iter().enumerate() {
            assert_eq!(pair(*c, *c), solid(i as u8), "palette color {i}");
        }
    }

    #[test]
    fn mid_tones_shade() {
        // Halfway between black and dark gray: no palette color, a shade.
        let (ch, _, _) = pair([40, 40, 40], [40, 40, 40]);
        assert!(SHADES.iter().any(|s| s.0 == ch), "got {ch:#x}");
    }

    #[test]
    fn hard_edges_keep_half_blocks() {
        // White over blue: bright top rides the foreground of an upper half.
        assert_eq!(pair([255, 255, 255], [0, 0, 170]), (UPPER_HALF, 15, 4));
        // Bright bottom flips to the lower half.
        assert_eq!(pair([170, 0, 0], [255, 255, 85]), (LOWER_HALF, 11, 1));
    }

    #[test]
    fn cell_matcher_agrees_with_the_pair_fast_path() {
        let px = [
            [255, 255, 255],
            [0, 0, 170],
            [40, 40, 40],
            [200, 120, 60],
            [0, 0, 0],
            [90, 140, 200],
        ];
        for a in px {
            for b in px {
                let (t, bt) = (region(&[a]), region(&[b]));
                assert_eq!(
                    table().match_cell(&t, &t, &bt, &bt, false),
                    pair(a, b),
                    "{a:?} / {b:?}"
                );
            }
        }
    }

    #[test]
    fn vertical_stroke_uses_a_side_half_block() {
        // Yellow stroke down the left of a downscaled cell, black on the right.
        let (y, k) = ([255, 255, 85], [0, 0, 0]);
        let (l, r) = (region(&[y, y]), region(&[k, k]));
        assert_eq!(table().match_cell(&l, &r, &l, &r, true), (LEFT_HALF, 11, 0));
    }

    #[test]
    fn shades_stick_to_supported_hues() {
        // Yellow text on black averages to an olive. Too dim for any yellow
        // shade, it may go gray, but never green or brown.
        let (y, k) = ([255, 255, 85], [0, 0, 0]);
        for px in [&[y, k, k, y, k, k][..], &[y, y, k, y, y, k]] {
            let mix = region(px);
            let (_, fg, bg) = table().match_cell(&mix, &mix, &mix, &mix, true);
            for c in [fg, bg] {
                assert!(![2, 3, 10].contains(&c), "{px:?} -> {fg}/{bg}");
            }
        }
    }

    #[test]
    fn background_is_always_dark() {
        let t = table();
        for i in 0..4096u32 {
            let c = |s: u32| ((i >> s & 15) * 17) as u8;
            let (_, _, bg) = t.match_pair(t.key(c(0), c(4), c(8)), t.key(c(8), c(0), c(4)));
            assert!(bg < 8);
        }
    }
}
