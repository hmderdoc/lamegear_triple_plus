//! RGB framebuffer filled from the emulator core each frame.
//!
//! Unlike lameboy's Game Boy path (per-pixel `PixelMapper` callbacks with DMG
//! palette machinery), smsgg-core hands us a complete RGBA frame; this is just
//! a resizable copy of it that the renderer reads.

use jgenesis_common::frontend::Color;

#[derive(Clone, Copy, PartialEq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Integer luma approximation (ITU-R 601-ish) for the ASCII render mode.
    pub fn to_grey(&self) -> u8 {
        ((self.r as u16 * 77 + self.g as u16 * 150 + self.b as u16 * 29) >> 8) as u8
    }
}

pub struct FrameBuffer {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Rgb>,
}

impl FrameBuffer {
    pub fn new(width: usize, height: usize) -> Self {
        FrameBuffer { width, height, pixels: vec![Rgb::default(); width * height] }
    }

    #[inline]
    pub fn get_pixel(&self, x: usize, y: usize) -> Rgb {
        self.pixels[y * self.width + x]
    }

    /// Copy one core frame in, resizing if the video mode changed (GG 160x144,
    /// SMS 256x192/224/240, Genesis-style H32/H40 switches all land here).
    /// Returns true when dimensions changed so the caller can re-fit.
    pub fn update_from(&mut self, pixels: &[Color], width: u32, height: u32) -> bool {
        let (w, h) = (width as usize, height as usize);
        let resized = w != self.width || h != self.height;
        if resized {
            self.width = w;
            self.height = h;
            self.pixels.resize(w * h, Rgb::default());
        }
        for (dst, src) in self.pixels.iter_mut().zip(pixels.iter()) {
            *dst = Rgb { r: src.r, g: src.g, b: src.b };
        }
        resized
    }
}
