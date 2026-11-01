//! Software compositor and the 3DS framebuffer blit.
//!
//! Everything is drawn into an RGBA8888 `Canvas` and copied to the LCD once per
//! frame. The 3DS framebuffer is stored rotated (a memory row is one screen
//! column) and bottom-up, so the blit is not a straight memcpy:
//!
//!   offset = (x * 240 + (239 - y)) * 3        (BGR8 framebuffer)
//!
//! which is the mapping libctru's own examples use, with the byte order the
//! hardware expects for the default Bgr8 format (B, G, R in memory).

// Kept free of ctru dependencies so the compositor, the SWF decoder and the
// player can also be built and tested on a desktop host (tools/hostcheck).
use crate::font8x8::FONT8X8;

pub const TOP_W: i32 = 400;
pub const TOP_H: i32 = 240;
pub const BOTTOM_W: i32 = 320;
pub const BOTTOM_H: i32 = 240;

/// 0xAARRGGBB, straight (non-premultiplied) alpha.
pub type Rgba = u32;

pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Rgba {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

pub struct Canvas {
    pub w: i32,
    pub h: i32,
    pub px: Vec<Rgba>,
    /// Pixels written since creation; a cheap way to tell a silent renderer
    /// from an empty display list.
    pub puts: usize,
    /// Coverage of the clip-depth mask being drawn.
    ///
    /// Flash masks a *range* of depths: the object placed with a clip depth
    /// hides everything stacked above it up to that depth, and it is itself
    /// invisible. Rasterizing the mask through the ordinary drawing path is
    /// what makes arbitrarily complex masks (nested sprites, bitmaps, letter
    /// shapes) work without a second rasterizer.
    clip: Vec<u8>,
    /// Region the last capture wrote, so the next one can clear just that.
    cap: (i32, i32, i32, i32),
    /// Drawing into the coverage buffer instead of the screen.
    capture: bool,
    /// Drawing through the coverage buffer.
    masked: bool,
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Self {
        Canvas {
            w,
            h,
            px: vec![0xFF00_0000; (w * h) as usize],
            puts: 0,
            clip: Vec::new(),
            cap: (0, 0, 0, 0),
            capture: false,
            masked: false,
        }
    }

    /// A canvas over existing pixels, for tools that inspect a decoded bitmap.
    pub fn from_pixels(w: i32, h: i32, px: Vec<Rgba>) -> Self {
        let mut c = Canvas::new(w.max(0), h.max(0));
        c.px = px;
        c
    }

    #[inline]
    pub fn put(&mut self, x: i32, y: i32, c: Rgba) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = (y * self.w + x) as usize;
        if self.capture {
            self.clip[i] = 1;
            return;
        }
        if self.masked && self.clip[i] == 0 {
            return;
        }
        let a = (c >> 24) & 0xFF;
        self.puts += 1;
        if a == 0xFF {
            self.px[i] = c;
        } else if a != 0 {
            self.px[i] = blend(self.px[i], c);
        }
    }

    /// Starts rasterizing a mask into the coverage buffer. `bounds` is the
    /// mask's stage-space extent: clearing only that region keeps a masked
    /// frame cheap instead of wiping a full-screen buffer per mask.
    pub fn begin_capture(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let n = (self.w * self.h) as usize;
        if self.clip.len() != n {
            self.clip = vec![0u8; n];
            self.cap = (0, 0, 0, 0);
        }
        let (px0, py0, px1, py1) = self.cap;
        let ax = px0.max(0);
        let ay = py0.max(0);
        let bx = px1.min(self.w);
        let by = py1.min(self.h);
        if bx > ax && by > ay {
            for y in ay..by {
                let row = (y * self.w) as usize;
                self.clip[row + ax as usize..row + bx as usize].fill(0);
            }
        }
        self.cap = (
            x0.max(0),
            y0.max(0),
            x1.min(self.w),
            y1.min(self.h),
        );
        self.capture = true;
    }

    /// Ends the capture and makes later drawing honour the coverage.
    pub fn finish_capture(&mut self) {
        self.capture = false;
    }

    pub fn set_masked(&mut self, on: bool) {
        self.masked = on && !self.capture;
    }

    /// True while a mask is being rasterized, so nested masks are drawn
    /// normally instead of overwriting the coverage being collected.
    pub fn is_capturing(&self) -> bool {
        self.capture
    }

    pub fn covered(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return false;
        }
        self.clip
            .get((y * self.w + x) as usize)
            .copied()
            .unwrap_or(0)
            != 0
    }

    /// Copies a pixel without any alpha blending.
    #[inline]
    pub fn put_opaque(&mut self, x: i32, y: i32, c: Rgba) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        self.px[(y * self.w + x) as usize] = c | 0xFF00_0000;
    }

    pub fn clear(&mut self, c: Rgba) {
        self.px.fill(c | 0xFF00_0000);
    }

    /// How many pixels differ from the background colour. A cheap way to tell
    /// "drew something" from "drew nothing" without looking at the image.
    pub fn count_non_background(&self, bg: Rgba) -> usize {
        let bg = bg | 0xFF00_0000;
        self.px.iter().filter(|&&p| p != bg).count()
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgba) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let a = (c >> 24) & 0xFF;
        if a == 0xFF {
            for yy in y0..y1 {
                let row = (yy * self.w) as usize;
                for xx in x0..x1 {
                    self.px[row + xx as usize] = c;
                }
            }
        } else if a != 0 {
            for yy in y0..y1 {
                let row = (yy * self.w) as usize;
                for xx in x0..x1 {
                    let i = row + xx as usize;
                    self.px[i] = blend(self.px[i], c);
                }
            }
        }
    }

    pub fn hline(&mut self, x0: i32, x1: i32, y: i32, c: Rgba) {
        let a = x0.min(x1);
        let b = x0.max(x1) + 1;
        self.fill_rect(a, y, b - a, 1, c);
    }

    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, c: Rgba) {
        let a = y0.min(y1);
        let b = y0.max(y1) + 1;
        self.fill_rect(x, a, 1, b - a, c);
    }

    pub fn rect_outline(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, c: Rgba) {
        self.fill_rect(x, y, w, t, c);
        self.fill_rect(x, y + h - t, w, t, c);
        self.fill_rect(x, y, t, h, c);
        self.fill_rect(x + w - t, y, t, h, c);
    }

    pub fn draw_char(&mut self, x: i32, y: i32, ch: char, c: Rgba, scale: i32) {
        let code = ch as usize;
        if code >= 128 {
            return;
        }
        let glyph = &FONT8X8[code];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                if bits & (1 << col) != 0 {
                    self.fill_rect(x + col as i32 * scale, y + row as i32 * scale, scale, scale, c);
                }
            }
        }
    }

    /// Draws `text` and returns the x coordinate just past the last glyph.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, c: Rgba, scale: i32) -> i32 {
        let mut cx = x;
        for ch in text.chars() {
            if ch == '\n' {
                cx = x;
                continue;
            }
            self.draw_char(cx, y, ch, c, scale);
            cx += 8 * scale;
        }
        cx
    }

    /// Draws text clipped to `max_w` pixels, appending an ellipsis when cut.
    pub fn draw_text_clipped(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        max_w: i32,
        c: Rgba,
        scale: i32,
    ) {
        let per_char = 8 * scale;
        let max_chars = (max_w / per_char).max(0) as usize;
        if max_chars == 0 {
            return;
        }
        if text.chars().count() <= max_chars {
            self.draw_text(x, y, text, c, scale);
            return;
        }
        let mut s: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('>');
        self.draw_text(x, y, &s, c, scale);
    }

    pub fn text_width(text: &str, scale: i32) -> i32 {
        text.chars().count() as i32 * 8 * scale
    }

    /// Nearest-neighbour blit of another canvas, used to scale movie output
    /// into the UI canvas.
    pub fn blit_scaled(&mut self, src: &Canvas, dst_x: i32, dst_y: i32, scale: f32) {
        if scale <= 0.0 {
            return;
        }
        let dw = (src.w as f32 * scale) as i32;
        let dh = (src.h as f32 * scale) as i32;
        for dy in 0..dh {
            let sy = ((dy as f32) / scale) as i32;
            if sy < 0 || sy >= src.h {
                continue;
            }
            let y = dst_y + dy;
            if y < 0 || y >= self.h {
                continue;
            }
            let srow = (sy * src.w) as usize;
            let drow = (y * self.w) as usize;
            for dx in 0..dw {
                let sx = ((dx as f32) / scale) as i32;
                if sx < 0 || sx >= src.w {
                    continue;
                }
                let x = dst_x + dx;
                if x < 0 || x >= self.w {
                    continue;
                }
                let c = src.px[srow + sx as usize];
                self.px[drow + x as usize] = blend(self.px[drow + x as usize], c | 0xFF00_0000);
            }
        }
    }
}

#[inline]
fn blend(dst: Rgba, src: Rgba) -> Rgba {
    let sa = (src >> 24) & 0xFF;
    if sa == 0 {
        return dst;
    }
    if sa == 0xFF {
        return src;
    }
    let da = 255 - sa;
    let r = (((src >> 16) & 0xFF) * sa + ((dst >> 16) & 0xFF) * da) / 255;
    let g = (((src >> 8) & 0xFF) * sa + ((dst >> 8) & 0xFF) * da) / 255;
    let b = ((src & 0xFF) * sa + (dst & 0xFF) * da) / 255;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}


