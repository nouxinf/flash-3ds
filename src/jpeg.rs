//! Baseline JPEG decoder for `DefineBitsJPEG2`.
//!
//! SWF can embed a JPEG as-is, so the movie's artwork cannot be drawn without
//! one. This decodes the subset Flash actually writes: 8-bit baseline
//! (and extended sequential) Huffman streams with any subsampling, restart
//! markers and grayscale, YCbCr or CMYK components. Progressive streams are
//! rejected rather than mis-decoded.

pub struct Image {
    pub w: u32,
    pub h: u32,
    /// Straight-alpha ARGB, the same layout as `swf::Bitmap`.
    pub px: Vec<u32>,
}

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

fn fail<T>(msg: &str) -> Result<T, String> {
    Err(msg.to_string())
}

// ---------------------------------------------------------------------------
// Bit reader over entropy-coded data
// ---------------------------------------------------------------------------

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u32,
    n: u32,
    /// Set when a marker (not a stuffed byte) was hit; no more bits follow.
    marker: Option<u8>,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits {
            d,
            pos: 0,
            buf: 0,
            n: 0,
            marker: None,
        }
    }

    fn fill(&mut self) -> bool {
        if self.n >= 25 || self.marker.is_some() {
            return self.n > 0;
        }
        if self.pos >= self.d.len() {
            self.marker = Some(0xD9);
            return self.n > 0;
        }
        let mut b = self.d[self.pos];
        if b == 0xFF {
            let next = self.d.get(self.pos + 1).copied().unwrap_or(0xD9);
            if next == 0x00 {
                // Stuffed byte: a literal 0xFF in the stream.
                self.pos += 2;
            } else {
                // A marker ends the scan; leave it for the caller.
                self.marker = Some(next);
                self.pos += 2;
                return self.n > 0;
            }
        } else {
            self.pos += 1;
        }
        self.buf |= (b as u32) << (24 - self.n);
        self.n += 8;
        b = 0;
        let _ = b;
        true
    }

    fn bit(&mut self) -> Option<u32> {
        if self.n == 0 && !self.fill() {
            return None;
        }
        // Bits live at the top of the buffer, most significant first.
        let v = self.buf >> 31;
        self.buf <<= 1;
        self.n -= 1;
        Some(v)
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..count {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// Reads a value of `count` bits and sign-extends it, as JPEG's
    /// "receive and extend" step requires.
    fn signed(&mut self, count: u32) -> Option<i32> {
        // Baseline coefficients are at most 16 bits wide; anything larger is a
        // corrupt stream and would overflow the shift below.
        let count = count.min(16);
        if count == 0 {
            return Some(0);
        }
        let v = self.bits(count)? as i32;
        let half = 1 << (count - 1);
        Some(if v < half { v - ((1 << count) - 1) } else { v })
    }

    /// Byte-aligns and consumes a restart marker, if one is next. The marker is
    /// usually still unread when the interval expires, so the stream is probed
    /// directly as well.
    fn restart(&mut self) -> bool {
        self.n = 0;
        self.buf = 0;
        if let Some(m) = self.marker.take() {
            return (0xD0..=0xD7).contains(&m);
        }
        if self.pos + 1 < self.d.len()
            && self.d[self.pos] == 0xFF
            && (0xD0..=0xD7).contains(&self.d[self.pos + 1])
        {
            self.pos += 2;
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Huffman tables
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Huff {
    counts: [u16; 17],
    first_code: [u16; 17],
    first_index: [u16; 17],
    symbols: Vec<u8>,
}

impl Huff {
    fn build(&mut self, counts: [u8; 16], symbols: Vec<u8>) {
        let mut code = 0u32;
        let mut index = 0u32;
        for len in 1..=16 {
            let n = counts[len - 1] as u32;
            self.counts[len] = n as u16;
            self.first_code[len] = code as u16;
            self.first_index[len] = index as u16;
            code = (code + n) << 1;
            index += n;
        }
        self.symbols = symbols;
    }

    fn decode(&self, bits: &mut Bits) -> Option<u8> {
        let mut code = 0u32;
        for len in 1..=16 {
            code = (code << 1) | bits.bit()? as u32;
            let n = self.counts[len] as u32;
            if n > 0 {
                let first = self.first_code[len] as u32;
                if code >= first && code - first < n {
                    let idx = self.first_index[len] as usize + (code - first) as usize;
                    return self.symbols.get(idx).copied();
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// IDCT
// ---------------------------------------------------------------------------

/// Cosine basis, folded with the 1/2 and 1/sqrt(2) normalisation factors.
fn cosine_table() -> [[f32; 8]; 8] {
    let mut t = [[0.0f32; 8]; 8];
    let mut x = 0;
    while x < 8 {
        let mut u = 0;
        while u < 8 {
            // 1/4 * C(u), with C(0) = 1/sqrt(2): a DC-only block must come back
            // as coefficient/8, not coefficient/4.
            let c = if u == 0 {
                0.353_553_4
            } else {
                0.5
            };
            let angle = ((2 * x + 1) as f32) * (u as f32) * std::f32::consts::PI / 16.0;
            t[u][x] = c * angle.cos();
            u += 1;
        }
        x += 1;
    }
    t
}

/// Separable 8x8 inverse DCT. `block` holds dequantised coefficients in raster
/// order; the samples come back as 0..255 floats centred on 128.
fn idct(block: &mut [f32; 64], cos: &[[f32; 8]; 8]) {
    let mut tmp = [0.0f32; 64];
    // Rows.
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0.0;
            for u in 0..8 {
                sum += cos[u][x] * block[y * 8 + u];
            }
            tmp[y * 8 + x] = sum;
        }
    }
    // Columns.
    for x in 0..8 {
        for y in 0..8 {
            let mut sum = 0.0;
            for v in 0..8 {
                sum += cos[v][y] * tmp[v * 8 + x];
            }
            block[y * 8 + x] = sum + 128.0;
        }
    }
}

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

struct Component {
    id: u8,
    hs: u32,
    vs: u32,
    quant: usize,
    dc: usize,
    ac: usize,
    /// Samples at the component's own resolution.
    plane: Vec<u8>,
    stride: usize,
    dc_pred: i32,
}

pub fn decode(data: &[u8]) -> Result<Image, String> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return fail("not a JPEG stream (missing SOI)");
    }
    let mut quants: Vec<[u16; 64]> = Vec::new();
    let mut huffs: Vec<Huff> = Vec::new();
    let mut comps: Vec<Component> = Vec::new();
    let mut width = 0u32;
    let mut height = 0u32;
    let mut progressive = false;
    let mut restart_interval = 0usize;
    let mut pos = 2usize;
    let cos = cosine_table();

    while pos + 1 < data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let mut marker = data[pos + 1];
        pos += 2;
        // Fill bytes are allowed before a marker.
        while marker == 0xFF && pos < data.len() {
            marker = data[pos];
            pos += 1;
        }
        match marker {
            0xD8 | 0x01 | 0xD0..=0xD7 => continue,
            0xD9 => break,
            _ => {}
        }
        if pos + 2 > data.len() {
            break;
        }
        let len = ((data[pos] as usize) << 8) | data[pos + 1] as usize;
        if len < 2 || pos + len > data.len() {
            return fail("truncated JPEG segment");
        }
        let seg = &data[pos + 2..pos + len];
        pos += len;
        match marker {
            0xDB => {
                // DefineQuantisationTable: 8- or 16-bit entries, zigzag order.
                let mut p = 0usize;
                while p < seg.len() {
                    let pq = seg[p] >> 4;
                    let tq = (seg[p] & 0x0F) as usize;
                    p += 1;
                    let mut table = [0u16; 64];
                    for i in 0..64 {
                        if pq == 0 {
                            if p >= seg.len() {
                                return fail("truncated quantisation table");
                            }
                            table[ZIGZAG[i]] = seg[p] as u16;
                            p += 1;
                        } else {
                            if p + 1 >= seg.len() {
                                return fail("truncated quantisation table");
                            }
                            table[ZIGZAG[i]] = ((seg[p] as u16) << 8) | seg[p + 1] as u16;
                            p += 2;
                        }
                    }
                    while quants.len() <= tq {
                        quants.push([16; 64]);
                    }
                    quants[tq] = table;
                }
            }
            0xC0 | 0xC1 | 0xC2 => {
                if seg.len() < 6 {
                    return fail("truncated frame header");
                }
                progressive = marker == 0xC2;
                let precision = seg[0];
                if precision != 8 {
                    return fail("only 8-bit JPEG samples are supported");
                }
                height = ((seg[1] as u32) << 8) | seg[2] as u32;
                width = ((seg[3] as u32) << 8) | seg[4] as u32;
                let n = seg[5] as usize;
                if n == 0 || seg.len() < 6 + n * 3 {
                    return fail("truncated frame component list");
                }
                comps.clear();
                for i in 0..n {
                    let b = &seg[6 + i * 3..9 + i * 3];
                    let hs = (b[1] >> 4) as u32;
                    let vs = (b[1] & 0x0F) as u32;
                    comps.push(Component {
                        id: b[0],
                        hs: hs.max(1),
                        vs: vs.max(1),
                        quant: (b[2] & 0x03) as usize,
                        dc: 0,
                        ac: 0,
                        plane: Vec::new(),
                        stride: 0,
                        dc_pred: 0,
                    });
                }
            }
            0xC4 => {
                // DefineHuffmanTable: one or more tables in a single segment.
                let mut p = 0usize;
                while p + 17 <= seg.len() {
                    let class = seg[p] >> 4;
                    let id = (seg[p] & 0x0F) as usize;
                    p += 1;
                    let mut counts = [0u8; 16];
                    let mut total = 0usize;
                    for i in 0..16 {
                        counts[i] = seg[p + i];
                        total += counts[i] as usize;
                    }
                    p += 16;
                    if p + total > seg.len() {
                        return fail("truncated Huffman table");
                    }
                    let symbols = seg[p..p + total].to_vec();
                    p += total;
                    let mut h = Huff::default();
                    h.build(counts, symbols);
                    let slot = class as usize * 4 + id;
                    while huffs.len() <= slot {
                        huffs.push(Huff::default());
                    }
                    huffs[slot] = h;
                }
            }
            0xDD => {
                if seg.len() >= 2 {
                    restart_interval = ((seg[0] as usize) << 8) | seg[1] as usize;
                }
            }
            0xDA => {
                if comps.is_empty() || width == 0 || height == 0 {
                    return fail("scan before frame header");
                }
                if progressive {
                    return fail("progressive JPEG is not supported");
                }
                let n = seg[0] as usize;
                if seg.len() < 1 + n * 2 + 3 {
                    return fail("truncated scan header");
                }
                for i in 0..n {
                    let cid = seg[1 + i * 2];
                    let tables = seg[2 + i * 2];
                    if let Some(c) = comps.iter_mut().find(|c| c.id == cid) {
                        c.dc = (tables >> 4) as usize;
                        c.ac = (tables & 0x0F) as usize;
                    }
                }
                // Allocate the component planes at their own resolution.
                let hmax = comps.iter().map(|c| c.hs).max().unwrap_or(1);
                let vmax = comps.iter().map(|c| c.vs).max().unwrap_or(1);
                for c in comps.iter_mut() {
                    let cw = ((width * c.hs + hmax - 1) / hmax) as usize;
                    let ch = ((height * c.vs + vmax - 1) / vmax) as usize;
                    c.stride = cw;
                    c.plane = vec![128u8; cw * ch];
                    c.dc_pred = 0;
                }
                let scan_end = data.len();
                let mut bits = Bits::new(&data[pos..]);
                decode_scan(
                    &mut bits,
                    &mut comps,
                    &quants,
                    &huffs,
                    &cos,
                    restart_interval,
                    width,
                    height,
                    scan_end - pos,
                )?;
                // The scan consumed the rest of the file; we are done.
                break;
            }
            _ => {}
        }
    }

    if comps.is_empty() || width == 0 || height == 0 {
        return fail("JPEG has no frame");
    }
    if std::env::var("JPEG_DEBUG").is_ok() {
        for c in &comps {
            let n = c.plane.len().max(1) as u32;
            let sum: u32 = c.plane.iter().map(|v| *v as u32).sum();
            eprintln!(
                "component {} hs={} vs={} {}x{} mean {} first {:?}",
                c.id,
                c.hs,
                c.vs,
                c.stride,
                c.plane.len() / c.stride.max(1),
                sum / n,
                &c.plane[..c.plane.len().min(8)]
            );
        }
    }
    Ok(to_image(&comps, width, height))
}

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    bits: &mut Bits,
    comps: &mut Vec<Component>,
    quants: &[[u16; 64]],
    huffs: &[Huff],
    cos: &[[f32; 8]; 8],
    restart_interval: usize,
    width: u32,
    height: u32,
    _len: usize,
) -> Result<(), String> {
    let hmax = comps.iter().map(|c| c.hs).max().unwrap_or(1);
    let vmax = comps.iter().map(|c| c.vs).max().unwrap_or(1);
    let mcu_w = 8 * hmax as u32;
    let mcu_h = 8 * vmax as u32;
    let mcus_x = ((width + mcu_w - 1) / mcu_w) as usize;
    let mcus_y = ((height + mcu_h - 1) / mcu_h) as usize;

    let mut block = [0f32; 64];
    let mut since_restart = 0usize;
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart_interval > 0 && since_restart == restart_interval {
                if !bits.restart() {
                    // A missing restart marker means the stream is malformed;
                    // stop rather than emit garbage.
                    return Ok(());
                }
                for c in comps.iter_mut() {
                    c.dc_pred = 0;
                }
                since_restart = 0;
            }
            since_restart += 1;
            for ci in 0..comps.len() {
                let (hs, vs) = (comps[ci].hs as usize, comps[ci].vs as usize);
                for by in 0..vs {
                    for bx in 0..hs {
                        let (dc_id, ac_id) = (comps[ci].dc, comps[ci].ac);
                        let quant = quants.get(comps[ci].quant).copied().unwrap_or([16; 64]);
                        let (dc_t, ac_t) = (huffs.get(dc_id), huffs.get(4 + ac_id));
                        let (Some(dc_t), Some(ac_t)) = (dc_t, ac_t) else {
                            return fail("scan references a missing Huffman table");
                        };
                        let Some(size) = dc_t.decode(bits) else {
                            return Ok(());
                        };
                        let diff = bits.signed(size as u32).unwrap_or(0);
                        comps[ci].dc_pred += diff;
                        block[0] = (comps[ci].dc_pred * quant[0] as i32) as f32;
                        let mut k = 1usize;
                        while k < 64 {
                            let Some(rs) = ac_t.decode(bits) else {
                                return Ok(());
                            };
                            let run = (rs >> 4) as usize;
                            let size = (rs & 0x0F) as u32;
                            if size == 0 {
                                if run == 15 {
                                    // ZRL: sixteen zeros, not one.
                                    k += 16;
                                    continue;
                                }
                                break; // EOB
                            }
                            k += run;
                            if k >= 64 {
                                break;
                            }
                            let v = bits.signed(size).unwrap_or(0);
                            let raster = ZIGZAG[k];
                            block[raster] = (v * quant[raster] as i32) as f32;
                            k += 1;
                        }
                        idct(&mut block, cos);
                        // Place the block into the component plane.
                        let cw = comps[ci].stride;
                        let ch = comps[ci].plane.len() / cw.max(1);
                        let px0 = mx * hs + bx;
                        let py0 = my * vs + by;
                        for y in 0..8 {
                            let ty = py0 * 8 + y;
                            if ty >= ch {
                                break;
                            }
                            for x in 0..8 {
                                let tx = px0 * 8 + x;
                                if tx >= cw {
                                    continue;
                                }
                                let v = block[y * 8 + x];
                                comps[ci].plane[ty * cw + tx] = v.clamp(0.0, 255.0) as u8;
                            }
                        }
                        // Clear coefficients for the next block.
                        for v in block.iter_mut() {
                            *v = 0.0;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Expands a component plane to full resolution. Subsampled chroma is
/// interpolated with the 3:1 triangle filter libjpeg calls "fancy upsampling",
/// which is what Flash and every other decoder produce; plain replication
/// leaves visible stair-steps along colour edges.
fn upsample(c: &Component, sh: usize, sv: usize) -> (Vec<f32>, usize) {
    let cw = c.stride.max(1);
    let ch = (c.plane.len() / cw).max(1);
    let (hs, vs) = (sh.max(1), sv.max(1));

    // Horizontal pass: out[2x] = (3 in[x] + in[x-1]) / 4 and
    // out[2x+1] = (3 in[x] + in[x+1]) / 4, clamping at the edges.
    let hw = cw * hs;
    let mut hbuf = vec![0u8; hw * ch];
    for y in 0..ch {
        let row = &c.plane[y * cw..y * cw + cw];
        let out = &mut hbuf[y * hw..y * hw + hw];
        for x in 0..cw {
            let v = row[x] as u32;
            if hs >= 2 {
                out[2 * x] = if x == 0 {
                    row[0]
                } else {
                    ((3 * v + row[x - 1] as u32 + 1) >> 2) as u8
                };
                out[2 * x + 1] = if x + 1 == cw {
                    row[x]
                } else {
                    ((3 * v + row[x + 1] as u32 + 2) >> 2) as u8
                };
            } else {
                out[x] = row[x];
            }
        }
    }

    // Vertical pass with the same weights.
    let vh = ch * vs;
    let mut out = vec![0f32; hw * vh];
    for y in 0..ch {
        for x in 0..hw {
            let v = hbuf[y * hw + x] as u32;
            if vs >= 2 {
                out[(2 * y) * hw + x] = if y == 0 {
                    v as f32
                } else {
                    ((3 * v + hbuf[(y - 1) * hw + x] as u32 + 1) >> 2) as f32
                };
                out[(2 * y + 1) * hw + x] = if y + 1 >= ch {
                    v as f32
                } else {
                    ((3 * v + hbuf[(y + 1) * hw + x] as u32 + 2) >> 2) as f32
                };
            } else {
                out[y * hw + x] = v as f32;
            }
        }
    }
    (out, hw)
}


fn to_image(comps: &[Component], width: u32, height: u32) -> Image {
    let mut px = vec![0xFF00_0000u32; (width * height) as usize];
    let hmax = comps.iter().map(|c| c.hs.max(1) as usize).max().unwrap_or(1);
    let vmax = comps.iter().map(|c| c.vs.max(1) as usize).max().unwrap_or(1);
    // A plane's scale relative to the image is hmax/hs, since the stride was
    // allocated from the highest sampling factor in the frame.
    let planes: Vec<(Vec<f32>, usize)> = comps
        .iter()
        .map(|c| upsample(c, hmax / c.hs.max(1) as usize, vmax / c.vs.max(1) as usize))
        .collect();
    let planes: Vec<(&[f32], usize)> = planes.iter().map(|(p, w)| (p.as_slice(), *w)).collect();
    let sample = |i: usize, x: u32, y: u32| -> f32 {
        let (p, w) = planes[i];
        let h = p.len() / w.max(1);
        let sx = (x as usize).min(w - 1);
        let sy = (y as usize).min(h.saturating_sub(1));
        p[sy * w + sx]
    };

    for y in 0..height {
        for x in 0..width {
            let argb = match comps.len() {
                1 => {
                    let v = sample(0, x, y).clamp(0.0, 255.0) as u32;
                    0xFF00_0000 | (v << 16) | (v << 8) | v
                }
                3 => {
                    let yy = sample(0, x, y);
                    let cb = sample(1, x, y) - 128.0;
                    let cr = sample(2, x, y) - 128.0;
                    let r = yy + 1.402 * cr;
                    let g = yy - 0.344_136 * cb - 0.714_136 * cr;
                    let b = yy + 1.772 * cb;
                    0xFF00_0000
                        | ((r.clamp(0.0, 255.0) as u32) << 16)
                        | ((g.clamp(0.0, 255.0) as u32) << 8)
                        | b.clamp(0.0, 255.0) as u32
                }
                _ => {
                    // Four components are Adobe-style CMYK, inverted.
                    let c = sample(0, x, y);
                    let m = sample(1, x, y);
                    let yy = sample(2, x, y);
                    let k = sample(3, x, y);
                    let r = (c * k) / 255.0;
                    let g = (m * k) / 255.0;
                    let b = (yy * k) / 255.0;
                    0xFF00_0000
                        | ((r.clamp(0.0, 255.0) as u32) << 16)
                        | ((g.clamp(0.0, 255.0) as u32) << 8)
                        | b.clamp(0.0, 255.0) as u32
                }
            };
            px[(y * width + x) as usize] = argb;
        }
    }
    Image {
        w: width,
        h: height,
        px,
    }
}
