//! SWF parsing: container, shapes, bitmaps, sprites and the timeline.
//!
//! This is a focused decoder for the tags that classic (AS1/AS2) games use.
//! Unknown tags are skipped rather than treated as errors, which is how the
//! format is designed to be extended; unsupported *visible* features (video,
//! filters, Flash 8 shapes) are reported so the UI can warn instead of
//! silently drawing the wrong thing.

use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, Default)]
pub struct Mat {
    pub scale_x: f32,
    pub scale_y: f32,
    pub rot_skew_0: f32,
    pub rot_skew_1: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Mat {
    pub const IDENTITY: Mat = Mat {
        scale_x: 1.0,
        scale_y: 1.0,
        rot_skew_0: 0.0,
        rot_skew_1: 0.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// Returns `self * other`, i.e. `other` applied first.
    pub fn mul(&self, o: &Mat) -> Mat {
        Mat {
            scale_x: self.scale_x * o.scale_x + self.rot_skew_1 * o.rot_skew_0,
            rot_skew_0: self.rot_skew_0 * o.scale_x + self.scale_y * o.rot_skew_0,
            rot_skew_1: self.scale_x * o.rot_skew_1 + self.rot_skew_1 * o.scale_y,
            scale_y: self.rot_skew_0 * o.rot_skew_1 + self.scale_y * o.scale_y,
            tx: self.scale_x * o.tx + self.rot_skew_1 * o.ty + self.tx,
            ty: self.rot_skew_0 * o.tx + self.scale_y * o.ty + self.ty,
        }
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.scale_x * x + self.rot_skew_1 * y + self.tx,
            self.rot_skew_0 * x + self.scale_y * y + self.ty,
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CxForm {
    pub mul: [f32; 4],
    pub add: [f32; 4],
}

impl CxForm {
    pub const IDENTITY: CxForm = CxForm {
        mul: [1.0; 4],
        add: [0.0; 4],
    };

    pub fn then(&self, child: &CxForm) -> CxForm {
        let mut mul = [0.0; 4];
        let mut add = [0.0; 4];
        for i in 0..4 {
            mul[i] = self.mul[i] * child.mul[i];
            add[i] = self.mul[i] * child.add[i] + self.add[i];
        }
        CxForm { mul, add }
    }

    pub fn apply(&self, c: u32) -> u32 {
        let a = ((c >> 24) & 0xFF) as f32 / 255.0;
        let r = ((c >> 16) & 0xFF) as f32 / 255.0;
        let g = ((c >> 8) & 0xFF) as f32 / 255.0;
        let b = (c & 0xFF) as f32 / 255.0;
        let ch = [a, r, g, b];
        let mut o = [0u8; 4];
        for i in 0..4 {
            let v = ch[i] * self.mul[i] + self.add[i];
            o[i] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        ((o[0] as u32) << 24) | ((o[1] as u32) << 16) | ((o[2] as u32) << 8) | o[3] as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug)]
pub enum Paint {
    Solid(u32),
    /// Linear gradient with stops; matrix maps shape space to gradient space.
    Linear {
        matrix: Mat,
        stops: Vec<(u8, u32)>,
    },
    Radial {
        matrix: Mat,
        stops: Vec<(u8, u32)>,
        focal: f32,
    },
    Bitmap {
        id: u16,
        matrix: Mat,
    },
    None,
}

#[derive(Clone, Debug)]
pub struct FillPath {
    pub paint: Paint,
    pub contours: Vec<Vec<Pt>>,
}

#[derive(Clone, Debug, Default)]
pub struct Shape {
    pub fills: Vec<FillPath>,
    pub strokes: Vec<StrokePath>,
}

#[derive(Clone, Debug)]
pub struct StrokePath {
    pub color: u32,
    pub width: f32,
    pub lines: Vec<(Pt, Pt)>,
}

#[derive(Clone)]
pub struct Bitmap {
    pub w: u32,
    pub h: u32,
    /// Straight-alpha ARGB.
    pub px: Vec<u32>,
}

/// One embedded font: the glyph outlines plus the metrics needed to lay text
/// out. Coordinates are stored in the font's own units (20480 per em for the
/// high-resolution `DefineFont3`, 1024 for `DefineFont2`), so drawing scales
/// them by `height / units_per_em`.
pub struct Font {
    pub units_per_em: f32,
    /// Glyph index -> character code.
    pub codes: Vec<u16>,
    /// Character code -> glyph index.
    map: HashMap<u16, u16>,
    /// Glyph index -> outline, in font units.
    pub glyphs: Vec<Shape>,
    /// Glyph index -> horizontal advance, in font units.
    pub advance: Vec<f32>,
    pub ascent: f32,
    pub descent: f32,
    pub leading: f32,
}

impl Font {
    pub fn glyph_index(&self, code: u16) -> Option<usize> {
        self.map.get(&code).map(|&i| i as usize)
    }

    pub fn advance_of(&self, index: usize) -> f32 {
        self.advance.get(index).copied().unwrap_or(0.0)
    }
}

/// A styled stretch of static text, positioned in twips.
pub struct TextRun {
    pub font: u16,
    pub height: f32,
    pub color: u32,
    pub x: f32,
    pub y: f32,
    /// Glyph index and its advance, in pixels.
    pub glyphs: Vec<(u16, f32)>,
}

/// A `DefineText`/`DefineText2` character.
pub struct Text {
    pub matrix: Mat,
    pub runs: Vec<TextRun>,
}

/// A `DefineEditText` field: the value comes from `var` at draw time.
pub struct EditText {
    pub bounds: (f32, f32, f32, f32),
    pub font: u16,
    pub height: f32,
    pub color: u32,
    pub align: u8,
    pub var: String,
    pub initial: String,
    pub wrap: bool,
    pub multiline: bool,
}

/// A button is a tiny display list plus a set of conditional actions: the
/// records draw the up/over/down states and the conditions are the `on(...)`
/// handlers compiled into the tag.
#[derive(Clone, Debug)]
pub struct ButtonRecord {
    pub char_id: u16,
    pub depth: u16,
    pub matrix: Mat,
    pub cxform: CxForm,
    /// Bit 0 up, 1 over, 2 down, 3 hit-test area.
    pub states: u8,
}

#[derive(Clone, Debug)]
pub struct ButtonCond {
    /// Low nine bits of the SWF CondId: bit 0 idle, 2 press, 3 release, ...
    pub conditions: u16,
    /// Key code for a key-press handler (bits 9..15), 0 otherwise.
    pub key: u8,
    pub actions: Code,
}

#[derive(Clone, Debug, Default)]
pub struct Button {
    pub records: Vec<ButtonRecord>,
    pub conds: Vec<ButtonCond>,
}

/// Clip event flags from a PlaceObject2 ClipActions block.
pub const EV_LOAD: u32 = 0x0001;
pub const EV_ENTER_FRAME: u32 = 0x0002;
pub const EV_UNLOAD: u32 = 0x0004;
pub const EV_MOUSE_MOVE: u32 = 0x0008;
pub const EV_MOUSE_DOWN: u32 = 0x0010;
pub const EV_MOUSE_UP: u32 = 0x0020;
pub const EV_KEY_DOWN: u32 = 0x0040;
pub const EV_KEY_UP: u32 = 0x0080;
pub const EV_PRESS: u32 = 0x0400;
pub const EV_RELEASE: u32 = 0x0800;
pub const EV_ROLL_OVER: u32 = 0x2000;
pub const EV_ROLL_OUT: u32 = 0x4000;
pub const EV_KEY_PRESS: u32 = 0x0002_0000;

#[derive(Clone, Debug)]
pub struct ClipEventAction {
    /// Bitmask of EV_* events this handler is registered for.
    pub events: u32,
    /// Key code for EV_KEY_PRESS, otherwise 0.
    pub key: u8,
    pub actions: Code,
}

#[derive(Clone, Debug)]
pub enum Op {
    Place {
        depth: u16,
        character: Option<u16>,
        matrix: Option<Mat>,
        cxform: Option<CxForm>,
        ratio: Option<f32>,
        name: Option<String>,
        clip_depth: Option<u16>,
        is_move: bool,
        /// onClipEvent handlers attached to this placement.
        clip_actions: Vec<ClipEventAction>,
    },
    Remove {
        depth: u16,
    },
}

/// A block of AVM1 bytecode and where it starts inside `Movie::data`.
///
/// The offset matters: AVM1 branch offsets are resolved against the movie's
/// byte stream, not the tag, so code is allowed to jump out of its own block.
/// Obfuscators (SWF Encrypt and friends) rely on exactly that: a small decoy
/// block jumps into a private tag where the real program sits, which is why
/// such movies run in Flash even though their visible scripts do nothing.
#[derive(Clone, Debug, Default)]
pub struct Code {
    pub off: usize,
    pub bytes: Vec<u8>,
}

impl Code {
    pub fn new(off: usize, bytes: Vec<u8>) -> Code {
        Code { off, bytes }
    }
}

#[derive(Default, Clone)]
pub struct Frame {
    pub ops: Vec<Op>,
    pub actions: Vec<Code>,
}

#[derive(Default, Clone)]
pub struct Timeline {
    pub frames: Vec<Frame>,
    pub labels: HashMap<String, usize>,
}

impl Timeline {
    fn current(&mut self) -> &mut Frame {
        if self.frames.is_empty() {
            self.frames.push(Frame::default());
        }
        self.frames.last_mut().unwrap()
    }
}

pub struct Movie {
    pub version: u8,
    pub width: u32,
    pub height: u32,
    pub frame_rate: f32,
    pub frame_count: u16,
    pub background: u32,
    pub shapes: HashMap<u16, Shape>,
    pub bitmaps: HashMap<u16, Bitmap>,
    pub sprites: HashMap<u16, Timeline>,
    pub buttons: HashMap<u16, Button>,
    pub fonts: HashMap<u16, Font>,
    pub texts: HashMap<u16, Text>,
    pub edit_texts: HashMap<u16, EditText>,
    /// Linkage names from `ExportAssets`, so `attachMovie("dart", ...)` can
    /// find the character it names.
    pub exports: HashMap<String, u16>,
    /// `DoInitAction` code per symbol, keyed by the sprite it belongs to.
    pub init_actions: HashMap<u16, Vec<Code>>,
    pub root: Timeline,
    pub warnings: Vec<String>,
    /// The decompressed SWF byte stream these blocks were cut from. AVM1 jumps
    /// are resolved against it, so execution needs the whole buffer.
    pub data: Rc<Vec<u8>>,
}

const TAG_END: u16 = 0;
const TAG_SHOW_FRAME: u16 = 1;
const TAG_DEFINE_SHAPE: u16 = 2;
const TAG_PLACE_OBJECT: u16 = 4;
const TAG_REMOVE_OBJECT: u16 = 5;
const TAG_BACKGROUND: u16 = 9;
const TAG_DO_ACTION: u16 = 12;
const TAG_DO_INIT_ACTION: u16 = 59;
const TAG_DEFINE_SHAPE2: u16 = 22;
const TAG_DEFINE_BITS_LOSSLESS: u16 = 20;
const TAG_DEFINE_BITS_LOSSLESS2: u16 = 36;
const TAG_EXPORT_ASSETS: u16 = 56;
const TAG_SYMBOL_CLASS: u16 = 76;
const TAG_DEFINE_BITS_JPEG2: u16 = 21;
const TAG_DEFINE_BITS_JPEG3: u16 = 35;
const TAG_DEFINE_BITS_JPEG4: u16 = 90;
const TAG_DEFINE_SHAPE3: u16 = 32;
const TAG_DEFINE_SHAPE4: u16 = 83;
const TAG_DEFINE_SPRITE: u16 = 39;
const TAG_DEFINE_BUTTON2: u16 = 34;
const TAG_PLACE_OBJECT2: u16 = 26;
const TAG_PLACE_OBJECT3: u16 = 70;
const TAG_REMOVE_OBJECT2: u16 = 28;
const TAG_FRAME_LABEL: u16 = 43;
const TAG_DEFINE_FONT2: u16 = 48;
const TAG_DEFINE_FONT3: u16 = 75;
const TAG_DEFINE_TEXT: u16 = 11;
const TAG_DEFINE_TEXT2: u16 = 33;
const TAG_DEFINE_EDIT_TEXT: u16 = 37;
const TAG_DEFINE_VIDEO_STREAM: u16 = 60;

struct Reader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(d: &'a [u8]) -> Self {
        Reader { d, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.d.len().saturating_sub(self.pos)
    }
    fn u8(&mut self) -> u8 {
        let v = *self.d.get(self.pos).unwrap_or(&0);
        self.pos += 1;
        v
    }
    fn u16(&mut self) -> u16 {
        (self.u8() as u16) | ((self.u8() as u16) << 8)
    }
    fn u32(&mut self) -> u32 {
        (self.u16() as u32) | ((self.u16() as u32) << 16)
    }
    fn i16(&mut self) -> i16 {
        self.u16() as i16
    }
    fn f32(&mut self) -> f32 {
        f32::from_bits(self.u32())
    }
    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let start = self.pos.min(self.d.len());
        let end = (self.pos + n).min(self.d.len());
        self.pos = end;
        &self.d[start..end]
    }
    fn skip(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.d.len());
    }
    fn string(&mut self) -> String {
        let mut s = Vec::new();
        loop {
            let b = self.u8();
            if b == 0 {
                break;
            }
            s.push(b);
        }
        String::from_utf8_lossy(&s).into_owned()
    }
    fn rect(&mut self) -> (i32, i32, i32, i32) {
        let mut bits = BitReader::new(self.d, self.pos * 8);
        let nbits = bits.read(5) as u32;
        let xmin = bits.read_signed(nbits);
        let xmax = bits.read_signed(nbits);
        let ymin = bits.read_signed(nbits);
        let ymax = bits.read_signed(nbits);
        self.pos = bits.byte_pos();
        (xmin, xmax, ymin, ymax)
    }
    fn matrix(&mut self) -> Mat {
        let mut bits = BitReader::new(self.d, self.pos * 8);
        let mut m = Mat::IDENTITY;
        m.scale_x = 1.0;
        m.scale_y = 1.0;
        if bits.read(1) == 1 {
            let n = bits.read(5) as u32;
            m.scale_x = fixed(bits.read_signed(n));
            m.scale_y = fixed(bits.read_signed(n));
        }
        if bits.read(1) == 1 {
            let n = bits.read(5) as u32;
            m.rot_skew_0 = fixed(bits.read_signed(n));
            m.rot_skew_1 = fixed(bits.read_signed(n));
        }
        // Scale and rotate are FIXED 16.16, but translations are plain twips.
        let n = bits.read(5) as u32;
        m.tx = bits.read_signed(n) as f32 / 20.0;
        m.ty = bits.read_signed(n) as f32 / 20.0;
        self.pos = bits.byte_pos();
        m
    }
    fn cxform(&mut self, with_alpha: bool) -> CxForm {
        let mut bits = BitReader::new(self.d, self.pos * 8);
        let mut cx = CxForm::IDENTITY;
        let has_add = bits.read(1) == 1;
        let has_mul = bits.read(1) == 1;
        let n = bits.read(4) as u32;
        if has_mul {
            for i in 0..4 {
                if i == 3 && !with_alpha {
                    continue;
                }
                cx.mul[i] = bits.read_signed(n) as f32 / 256.0;
            }
        }
        if has_add {
            for i in 0..4 {
                if i == 3 && !with_alpha {
                    continue;
                }
                cx.add[i] = bits.read_signed(n) as f32 / 256.0;
            }
        }
        self.pos = bits.byte_pos();
        cx
    }
}

fn fixed(v: i32) -> f32 {
    v as f32 / 65536.0
}

struct BitReader<'a> {
    d: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(d: &'a [u8], bit: usize) -> Self {
        BitReader { d, bit }
    }
    fn read(&mut self, n: u32) -> i32 {
        let mut v: u32 = 0;
        for _ in 0..n {
            let byte = self.bit / 8;
            let off = self.bit % 8;
            let b = *self.d.get(byte).unwrap_or(&0);
            v = (v << 1) | (((b >> (7 - off)) & 1) as u32);
            self.bit += 1;
        }
        v as i32
    }
    fn read_signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        // NumBits is at most 31 in a well-formed file. A desynchronised stream
        // can hold anything, and shifting by 32 is a panic: clamp instead of
        // losing the whole movie to one bad shape.
        let n = n.min(31);
        let v = self.read(n) as u32;
        let sign = 1u32 << (n - 1);
        // Widen before subtracting: for n = 31 the result does not fit in i32
        // until after the subtraction, and `v - (1 << 31)` would overflow.
        if v & sign != 0 {
            (v as i64 - (1i64 << n)) as i32
        } else {
            v as i32
        }
    }
    fn byte_pos(&self) -> usize {
        (self.bit + 7) / 8
    }
}

impl Movie {
    pub fn parse(data: &[u8]) -> Result<Movie, String> {
        if data.len() < 8 {
            return Err("file is too small to be a SWF".into());
        }
        let sig = &data[0..3];
        let version = data[3];
        let declared = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

        let body: Vec<u8> = match sig {
            b"FWS" => data[8..].to_vec(),
            b"CWS" => miniz_oxide::inflate::decompress_to_vec_zlib(&data[8..])
                .map_err(|e| format!("corrupt compressed SWF: {e:?}"))?,
            b"ZWS" => return Err("LZMA-compressed SWF (ZWS) is not supported".into()),
            _ => return Err("not a SWF file (bad signature)".into()),
        };

        let mut r = Reader::new(&body);
        let (xmin, xmax, ymin, ymax) = r.rect();
        let width = ((xmax - xmin) as f32 / 20.0).round().max(0.0) as u32;
        let height = ((ymax - ymin) as f32 / 20.0).round().max(0.0) as u32;
        let frame_rate = r.u16() as f32 / 256.0;
        let frame_count = r.u16();

        let mut mv = Movie {
            version,
            width,
            height,
            frame_rate: if frame_rate > 0.0 { frame_rate } else { 12.0 },
            frame_count,
            background: 0xFFFFFFFF,
            shapes: HashMap::new(),
            bitmaps: HashMap::new(),
            sprites: HashMap::new(),
            buttons: HashMap::new(),
            exports: HashMap::new(),
            init_actions: HashMap::new(),
            fonts: HashMap::new(),
            texts: HashMap::new(),
            edit_texts: HashMap::new(),
            root: Timeline::default(),
            warnings: Vec::new(),
            data: Rc::new(Vec::new()),
        };
        if version > 9 {
            mv.warnings
                .push(format!("SWF version {version} (AS3); only AS2 content is supported"));
        }
        if declared as usize != data.len() {
            // Not fatal: some files have a stale length field.
            mv.warnings.push(format!(
                "declared length {declared} differs from file size {}",
                data.len()
            ));
        }

        let mut tl = Timeline::default();
        mv.parse_tags(&mut r, &mut tl)?;
        trim_empty_tail(&mut tl);
        mv.root = tl;
        if mv.root.frames.is_empty() {
            mv.root.frames.push(Frame::default());
        }
        // Action blocks keep offsets into this buffer; execution decodes from
        // it so a branch can leave its own tag, as Flash allows.
        mv.data = Rc::new(body);
        Ok(mv)
    }

    fn parse_tags(&mut self, r: &mut Reader, tl: &mut Timeline) -> Result<(), String> {
        loop {
            if r.remaining() < 2 {
                break;
            }
            let h = r.u16();
            let code = h >> 6;
            let mut len = (h & 0x3F) as usize;
            if len == 0x3F {
                if r.remaining() < 4 {
                    break;
                }
                len = r.u32() as usize;
            }
            if r.remaining() < len {
                // Truncated final tag: stop cleanly rather than reading garbage.
                break;
            }
            let start = r.pos;
            let ok = self.parse_tag(code, r, tl, len);
            if let Err(e) = ok {
                self.warnings.push(e);
            }
            // Always resume at the declared end of the tag, even if a handler
            // read less than the full length.
            r.pos = start + len;
            if code == TAG_END {
                break;
            }
        }
        Ok(())
    }

    fn parse_tag(
        &mut self,
        code: u16,
        r: &mut Reader,
        tl: &mut Timeline,
        len: usize,
    ) -> Result<(), String> {
        let start = r.pos;
        match code {
            TAG_END => {}
            TAG_SHOW_FRAME => {
                // A ShowFrame closes the current frame. The next record starts a
                // new one; trailing empty frames are dropped after parsing.
                tl.frames.push(Frame::default());
            }
            TAG_BACKGROUND => {
                let rgb = r.bytes(3);
                if rgb.len() == 3 {
                    self.background =
                        0xFF00_0000 | ((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32;
                }
            }
            TAG_FRAME_LABEL => {
                let name = r.string();
                let idx = tl.frames.len().max(1) - 1;
                tl.labels.insert(name, idx);
            }
            TAG_DEFINE_SHAPE | TAG_DEFINE_SHAPE2 | TAG_DEFINE_SHAPE3 | TAG_DEFINE_SHAPE4 => {
                let id = r.u16();
                let version = match code {
                    TAG_DEFINE_SHAPE => 1,
                    TAG_DEFINE_SHAPE2 => 2,
                    TAG_DEFINE_SHAPE3 => 3,
                    _ => 4,
                };
                // DefineShape payload: shape id, bounds RECT, then style tables.
                let _bounds = r.rect();
                let shape = parse_shape(id, r, version, self.version)?;
                self.shapes.insert(id, shape);
            }
            TAG_DEFINE_BUTTON2 => {
                let id = r.u16();
                let _flags = r.u8();
                let action_offset = r.u16() as usize;
                // Where the CondAction records start. A DefineButton2 declares
                // that with an offset from the tag body, but obfuscators rewrite
                // the record block and leave the offset pointing into it, so a
                // button whose records carry blend-mode or filter flags ended up
                // with no actions at all - the movie's "Start Round" button was
                // exactly that, and a button with no actions is dead to a click.
                // The declaration is tried first and the tag is scanned for a
                // well-formed CondAction chain when it does not hold up.
                let limit = start + len;
                let declared = if action_offset != 0 {
                    (start + action_offset).min(limit)
                } else {
                    limit
                };
                // A DefineButton2 stores its CondAction list immediately after
                // the record terminator, so where the records actually ended is
                // the most reliable place to look; the declared offset is only a
                // hint and obfuscators leave it pointing into the record block.
                let mut records = Vec::new();
                loop {
                    let flags = r.u8();
                    // Records end on a zero flags byte; a record with no state
                    // bits would draw nothing, so it only marks the end.
                    if flags == 0 || r.remaining() < 9 {
                        break;
                    }
                    let char_id = r.u16();
                    let depth = r.u16();
                    let matrix = r.matrix();
                    let cxform = r.cxform(true);
                    records.push(ButtonRecord {
                        char_id,
                        depth,
                        matrix,
                        cxform,
                        states: flags & 0x0F,
                    });
                }
                if std::env::var("FLASH_BTN_DUMP").ok().is_some_and(|v| v == id.to_string()) {
                    let lo = start.min(r.d.len());
                    let hi = limit.min(r.d.len());
                    let text: Vec<String> = r.d[lo..hi]
                        .iter()
                        .take(1600)
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    eprintln!(
                        "BTNDUMP {id} records ended at {} (tag {lo}..{hi}): {}",
                        r.pos,
                        text.join(" ")
                    );
                }
                let after_records = r.pos;
                let mut actions_at = None;
                for candidate in [after_records, declared, start + 5] {
                    if candidate > limit {
                        continue;
                    }
                    if cond_chain(r.d, candidate, limit, true) > 0 {
                        actions_at = Some(candidate);
                        break;
                    }
                }
                if actions_at.is_none() {
                    actions_at = scan_cond_chain(r.d, after_records.max(start + 5), limit)
                        .or_else(|| scan_cond_chain(r.d, start + 5, limit));
                }
                let mut conds = Vec::new();
                if let Some(q) = actions_at {
                    r.pos = q;
                    while r.pos + 4 <= limit {
                        let start_of_record = r.pos;
                        let size = r.u16() as usize;
                        let cond = r.u16();
                        // A zero size marks the final record: its actions run
                        // to the end of the tag. That is the declared encoding,
                        // not an obfuscation, and treating it as a bad size
                        // drops a button's only handler - its click then does
                        // nothing at all.
                        let total = if size == 0 {
                            limit - start_of_record
                        } else {
                            size
                        };
                        if total < 4 || start_of_record + total > limit {
                            break;
                        }
                        let off = r.pos;
                        let actions = r.bytes(total - 4).to_vec();
                        conds.push(ButtonCond {
                            actions: Code::new(off, actions.clone()),
                            conditions: cond & 0x01FF,
                            key: ((cond >> 9) & 0x7F) as u8,
                        });
                        if size == 0 {
                            break;
                        }
                    }
                }
                if std::env::var_os("FLASH_BUTTONS").is_some() {
                    eprintln!(
                        "button {id}: {} records, {} conds, offset {action_offset}, actions at {:?} of {}..{limit}",
                        records.len(),
                        conds.len(),
                        actions_at,
                        start
                    );
                }
                self.buttons.insert(id, Button { records, conds });
            }
            TAG_DEFINE_SPRITE => {
                let id = r.u16();
                let _frames = r.u16();
                let mut sprite = Timeline::default();
                self.parse_tags(r, &mut sprite)?;
                trim_empty_tail(&mut sprite);
                if sprite.frames.is_empty() {
                    sprite.frames.push(Frame::default());
                }
                self.sprites.insert(id, sprite);
                let _ = _frames;
            }
            TAG_PLACE_OBJECT2 => {
                let op = parse_place2(r, 2, self.version, start + len)?;
                tl.current().ops.push(op);
            }
            TAG_PLACE_OBJECT3 => {
                // PlaceObject3 uses a 16-bit flag word and adds filters, blend
                // mode and visibility fields before any clip actions.
                let op = parse_place2(r, 3, self.version, start + len)?;
                tl.current().ops.push(op);
            }
            TAG_PLACE_OBJECT => {
                let character = r.u16();
                let depth = r.u16();
                let matrix = r.matrix();
                // PlaceObject1 CXFORM has no alpha and no name/flags.
                let cxform = if r.remaining() >= 1 {
                    Some(r.cxform(false))
                } else {
                    None
                };
                tl.current().ops.push(Op::Place {
                    depth,
                    character: Some(character),
                    matrix: Some(matrix),
                    cxform,
                    ratio: None,
                    name: None,
                    clip_depth: None,
                    is_move: false,
                    clip_actions: Vec::new(),
                });
            }
            TAG_REMOVE_OBJECT => {
                let character = r.u16();
                let depth = r.u16();
                let _ = character;
                tl.current().ops.push(Op::Remove { depth });
            }
            TAG_REMOVE_OBJECT2 => {
                let depth = r.u16();
                tl.current().ops.push(Op::Remove { depth });
            }
            TAG_DO_ACTION => {
                let off = r.pos;
                let actions = r.bytes(len).to_vec();
                tl.current().actions.push(Code::new(off, actions));
            }
            TAG_DO_INIT_ACTION => {
                // Per-symbol initialisation code: it runs once, when the clip
                // first exists, and is where games set up their sprites.
                let sprite = r.u16();
                let off = r.pos;
                let actions = r.bytes(len.saturating_sub(2)).to_vec();
                self.init_actions
                    .entry(sprite)
                    .or_default()
                    .push(Code::new(off, actions));
            }
            TAG_DEFINE_BITS_LOSSLESS => {
                let id = r.u16();
                let bmp = parse_lossless(r, false)?;
                self.bitmaps.insert(id, bmp);
            }
            TAG_DEFINE_BITS_LOSSLESS2 => {
                let id = r.u16();
                let bmp = parse_lossless(r, true)?;
                self.bitmaps.insert(id, bmp);
            }
            TAG_DEFINE_BITS_JPEG2 | TAG_DEFINE_BITS_JPEG3 | TAG_DEFINE_BITS_JPEG4 => {
                let id = r.u16();
                let alpha_len = if code == TAG_DEFINE_BITS_JPEG2 {
                    None
                } else {
                    Some(r.u32() as usize)
                };
                if code == TAG_DEFINE_BITS_JPEG4 {
                    let _deblock = r.u16();
                }
                let start = r.pos;
                let end = start + len;
                let (image_end, alpha_start) = match alpha_len {
                    Some(n) if n <= end - start => (start + n, Some(start + n)),
                    Some(n) => {
                        self.warnings.push(format!(
                            "JPEG bitmap {id} declares {n} image bytes but the tag holds {}",
                            end - start
                        ));
                        (end, None)
                    }
                    None => (end, None),
                };
                let image = &r.d[start..image_end.min(r.d.len())];
                let mut bmp = match decode_jpeg_image(image) {
                    Ok(b) => b,
                    Err(e) => {
                        self.warnings.push(format!("JPEG bitmap {id}: {e}"));
                        r.pos = end;
                        return Ok(());
                    }
                };
                if let Some(astart) = alpha_start {
                    if astart < end {
                        match miniz_oxide::inflate::decompress_to_vec_zlib(&r.d[astart..end]) {
                            Ok(alpha) if alpha.len() >= bmp.px.len() => {
                                for (i, a) in alpha.iter().enumerate().take(bmp.px.len()) {
                                    let p = &mut bmp.px[i];
                                    let rgb = *p & 0x00FF_FFFF;
                                    *p = ((*a as u32) << 24) | rgb;
                                }
                            }
                            Ok(_) => self
                                .warnings
                                .push(format!("JPEG bitmap {id}: alpha channel is too short")),
                            Err(e) => self
                                .warnings
                                .push(format!("JPEG bitmap {id}: corrupt alpha data ({e:?})")),
                        }
                    }
                }
                self.bitmaps.insert(id, bmp);
                r.pos = end;
            }
            TAG_DEFINE_FONT2 | TAG_DEFINE_FONT3 => {
                let (id, font) = parse_font(r, code == TAG_DEFINE_FONT3)?;
                self.fonts.insert(id, font);
            }
            TAG_DEFINE_TEXT | TAG_DEFINE_TEXT2 => {
                let (id, text) = parse_text(r, code == TAG_DEFINE_TEXT2)?;
                self.texts.insert(id, text);
            }
            TAG_DEFINE_EDIT_TEXT => {
                let (id, field) = parse_edit_text(r)?;
                self.edit_texts.insert(id, field);
            }
            TAG_EXPORT_ASSETS | TAG_SYMBOL_CLASS => {
                // Both tags are a count followed by (character id, name) pairs.
                let count = r.u16() as usize;
                for _ in 0..count.min(4096) {
                    let id = r.u16();
                    let name = r.string();
                    if !name.is_empty() {
                        self.exports.entry(name).or_insert(id);
                    }
                }
            }
            TAG_DEFINE_VIDEO_STREAM => {
                self.warnings.push("Embedded video is not supported".into());
            }
            _ => {}
        }
        Ok(())
    }
}

/// Removes frames that contain nothing, which ShowFrame leaves behind at the
/// end of a timeline.
fn trim_empty_tail(tl: &mut Timeline) {
    while tl
        .frames
        .last()
        .is_some_and(|f| f.ops.is_empty() && f.actions.is_empty())
    {
        tl.frames.pop();
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// `DefineFont2`/`DefineFont3`: glyph outlines plus the metrics needed to lay
/// text out. The offset table bounds every glyph exactly, which is what makes a
/// glyph shape failure local instead of corrupting the rest of the font.
fn parse_font(r: &mut Reader, high_res: bool) -> Result<(u16, Font), String> {
    let id = r.u16();
    let flags = r.u8();
    let has_layout = flags & 0x80 != 0;
    let wide_offsets = flags & 0x08 != 0;
    let wide_codes = flags & 0x04 != 0;
    let _lang = r.u8();
    let name_len = r.u8() as usize;
    let _name = r.bytes(name_len);
    let num_glyphs = r.u16() as usize;
    if num_glyphs == 0 || num_glyphs > 8192 {
        return Err(format!("font {id} declares {num_glyphs} glyphs"));
    }
    // The offset table holds one entry per glyph; the code table offset that
    // follows it doubles as the end of the last glyph.
    let table_pos = r.pos;
    let mut offsets = Vec::with_capacity(num_glyphs);
    for _ in 0..num_glyphs {
        offsets.push(if wide_offsets {
            r.u32() as usize
        } else {
            r.u16() as usize
        });
    }
    let code_offset = if wide_offsets {
        r.u32() as usize
    } else {
        r.u16() as usize
    };

    let mut glyphs = Vec::with_capacity(num_glyphs);
    for i in 0..num_glyphs {
        let end = if i + 1 < num_glyphs {
            offsets[i + 1]
        } else {
            code_offset
        };
        let a = table_pos + offsets[i];
        let b = table_pos + end;
        if a >= b || b > r.d.len() {
            glyphs.push(Shape::default());
            continue;
        }
        glyphs.push(parse_glyph(&r.d[a..b]));
    }

    let mut codes = Vec::with_capacity(num_glyphs);
    let code_width = if wide_codes { 2 } else { 1 };
    let mut pos = table_pos + code_offset;
    for _ in 0..num_glyphs {
        let code = if wide_codes {
            let v = (r.d.get(pos).copied().unwrap_or(0) as u16)
                | ((r.d.get(pos + 1).copied().unwrap_or(0) as u16) << 8);
            v
        } else {
            r.d.get(pos).copied().unwrap_or(0) as u16
        };
        pos += code_width;
        codes.push(code);
    }
    let mut map = HashMap::new();
    for (i, &code) in codes.iter().enumerate() {
        map.entry(code).or_insert(i as u16);
    }

    // DefineFont3 stores coordinates at twenty times the resolution of
    // DefineFont2, so an em is 20480 units there and 1024 units here.
    let units_per_em = if high_res { 20480.0 } else { 1024.0 };
    let mut advance = vec![units_per_em * 0.5; num_glyphs];
    let mut ascent = units_per_em * 0.8;
    let mut descent = units_per_em * 0.2;
    let mut leading = 0.0;
    if has_layout {
        let mut l = Reader { d: r.d, pos };
        ascent = l.u16() as f32;
        descent = l.u16() as f32;
        leading = l.u16() as f32;
        for slot in advance.iter_mut() {
            // A zero advance is legal (combining marks); keep the default.
            let a = l.u16() as i16 as f32;
            if a != 0.0 {
                *slot = a;
            }
        }
        // Kerning records follow; they only matter for kerning, so the reader
        // is left at the start of that table.
        r.pos = l.pos;
    } else {
        r.pos = pos;
    }
    Ok((
        id,
        Font {
            units_per_em,
            codes,
            map,
            glyphs,
            advance,
            ascent,
            descent,
            leading,
        },
    ))
}

/// A glyph's outline. Font glyphs carry no style table: the fill/line bit widths
/// are two bytes and every edge belongs to the same single fill, so all contours
/// go into one path and the even-odd rasteriser produces the letter's holes.
fn parse_glyph(data: &[u8]) -> Shape {
    let mut shape = Shape::default();
    if data.len() < 3 {
        return shape;
    }
    // A glyph's shape header is one byte: `NumFillBits` in the high nibble and
    // `NumLineBits` in the low one, packed straight into the record stream.
    // Reading them as two bytes shifted every edge by a byte and decoded the
    // outlines into nonsense, which drew text as scattered letter fragments.
    let mut bits = BitReader::new(data, 0);
    let fill_bits = bits.read(4) as u32;
    let line_bits = bits.read(4) as u32;
    let mut contours: Vec<Vec<Pt>> = Vec::new();
    let mut current: Vec<Pt> = Vec::new();
    let mut cursor = Pt { x: 0.0, y: 0.0 };
    while bits.bit + 6 <= data.len() * 8 {
        if bits.read(1) == 1 {
            let straight = bits.read(1) == 1;
            let n = (bits.read(4) + 2) as u32;
            if straight {
                let general = bits.read(1) == 1;
                let (dx, dy) = if general {
                    (
                        bits.read_signed(n) as f32,
                        bits.read_signed(n) as f32,
                    )
                } else {
                    let vertical = bits.read(1) == 1;
                    let v = bits.read_signed(n) as f32;
                    if vertical {
                        (0.0, v)
                    } else {
                        (v, 0.0)
                    }
                };
                let next = Pt {
                    x: cursor.x + dx,
                    y: cursor.y + dy,
                };
                current.push(next);
                cursor = next;
            } else {
                let cx = cursor.x + bits.read_signed(n) as f32;
                let cy = cursor.y + bits.read_signed(n) as f32;
                let ax = cx + bits.read_signed(n) as f32;
                let ay = cy + bits.read_signed(n) as f32;
                let mut prev = cursor;
                const STEPS: usize = 6;
                for i in 1..=STEPS {
                    let t = i as f32 / STEPS as f32;
                    let mt = 1.0 - t;
                    let p = Pt {
                        x: mt * mt * cursor.x + 2.0 * mt * t * cx + t * t * ax,
                        y: mt * mt * cursor.y + 2.0 * mt * t * cy + t * t * ay,
                    };
                    let _ = prev;
                    current.push(p);
                    prev = p;
                }
                cursor = Pt { x: ax, y: ay };
            }
        } else {
            let flags = bits.read(5) as u8;
            if flags == 0 {
                break;
            }
            if flags & 0x01 != 0 {
                let n = bits.read(5) as u32;
                let dx = bits.read_signed(n) as f32;
                let dy = bits.read_signed(n) as f32;
                if current.len() >= 3 {
                    contours.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
                // A MoveTo is an absolute position in the shape's own space, not
                // a delta from the pen: reading it as a delta pushed every
                // subpath after the first (glyph counters, clip masks, the second
                // lobe of a star) out to the wrong place.
                cursor = Pt { x: dx, y: dy };
                current.push(cursor);
            }
            if flags & 0x02 != 0 {
                bits.read(fill_bits);
            }
            if flags & 0x04 != 0 {
                bits.read(fill_bits);
            }
            if flags & 0x08 != 0 {
                bits.read(line_bits);
            }
            if flags & 0x10 != 0 {
                break;
            }
        }
    }
    if current.len() >= 3 {
        contours.push(current);
    }
    if !contours.is_empty() {
        shape.fills.push(FillPath {
            paint: Paint::Solid(0xFF00_0000),
            contours,
        });
    }
    shape
}

fn parse_text(r: &mut Reader, alpha: bool) -> Result<(u16, Text), String> {
    let id = r.u16();
    let _bounds = r.rect();
    let matrix = r.matrix();
    let glyph_bits = r.u8() as u32;
    let advance_bits = r.u8() as u32;
    let mut runs = Vec::new();
    let mut font = 0u16;
    let mut height = 12.0f32;
    let mut color = 0xFF00_0000u32;
    let mut x = 0.0f32;
    let mut y = 0.0f32;
    // Each record starts on a byte boundary, so the bit reader is rebuilt per
    // record: style fields are byte-aligned, only the glyph list is packed.
    while r.pos < r.d.len() {
        let flags = r.u8();
        if flags == 0 {
            break;
        }
        let has_text = flags & 0x80 != 0;
        let has_font = flags & 0x08 != 0;
        let has_color = flags & 0x04 != 0;
        let has_x = flags & 0x01 != 0;
        let has_y = flags & 0x02 != 0;
        if has_font {
            font = r.u16();
        }
        if has_color {
            let rr = r.u8() as u32;
            let gg = r.u8() as u32;
            let bb = r.u8() as u32;
            let aa = if alpha { r.u8() as u32 } else { 255 };
            color = (aa << 24) | (rr << 16) | (gg << 8) | bb;
        }
        if has_x {
            x = r.i16() as f32 / 20.0;
        }
        if has_y {
            y = r.i16() as f32 / 20.0;
        }
        if has_font {
            height = r.u16() as f32 / 20.0;
        }
        if !has_text {
            continue;
        }
        let count = r.u8() as usize;
        let mut bits = BitReader::new(r.d, r.pos * 8);
        let mut glyphs = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let index = bits.read(glyph_bits.max(1)) as u16;
            // The record's advance is in twips like every other coordinate in
            // the tag, while `x`/`y`/`height` above are converted to pixels; a
            // zero advance width means the font's own advance table applies.
            let advance = if advance_bits == 0 {
                0.0
            } else {
                bits.read_signed(advance_bits) as f32 / 20.0
            };
            glyphs.push((index, advance));
        }
        r.pos = bits.byte_pos();
        if glyphs.is_empty() {
            continue;
        }
        runs.push(TextRun {
            font,
            height,
            color,
            x,
            y,
            glyphs,
        });
    }
    Ok((id, Text { matrix, runs }))
}

fn parse_edit_text(r: &mut Reader) -> Result<(u16, EditText), String> {
    let id = r.u16();
    let (x0, x1, y0, y1) = r.rect();
    // These flags are a bit field read from the most significant bit down, so
    // HasText is 0x8000 and UseOutlines is 0x0001. Reading them from the low
    // end put every optional field at the wrong offset: the font id came out
    // as 0 and the height as 0, so no dynamic field could ever draw a glyph.
    let flags = r.u16();
    let has_text = flags & 0x8000 != 0;
    let wrap = flags & 0x4000 != 0;
    let _multiline = flags & 0x2000 != 0;
    let _password = flags & 0x1000 != 0;
    let _read_only = flags & 0x0800 != 0;
    let has_color = flags & 0x0400 != 0;
    let has_max_length = flags & 0x0200 != 0;
    let has_font = flags & 0x0100 != 0;
    let has_font_class = flags & 0x0080 != 0;
    let has_layout = flags & 0x0020 != 0;

    let mut font = 0u16;
    let mut height = 12.0f32;
    if has_font {
        font = r.u16();
    }
    if has_font_class {
        let _class = r.string();
    }
    if has_font {
        height = r.u16() as f32 / 20.0;
    }
    let mut color = 0xFF00_0000u32;
    if has_color {
        let rr = r.u8() as u32;
        let gg = r.u8() as u32;
        let bb = r.u8() as u32;
        let aa = r.u8() as u32;
        color = (aa << 24) | (rr << 16) | (gg << 8) | bb;
    }
    if has_max_length {
        let _ = r.u16();
    }
    let mut align = 0u8;
    if has_layout {
        align = r.u8();
        let _left = r.u16();
        let _right = r.u16();
        let _indent = r.u16();
        let _leading = r.u16();
    }
    let var = r.string();
    let initial = if has_text { r.string() } else { String::new() };
    Ok((
        id,
        EditText {
            bounds: (
                x0 as f32 / 20.0,
                y0 as f32 / 20.0,
                x1 as f32 / 20.0,
                y1 as f32 / 20.0,
            ),
            font,
            height,
            color,
            align,
            var,
            initial,
            wrap,
            multiline: _multiline,
        },
    ))
}

/// Reads a chain of `CondAction` records starting at `q`, returning how many it
/// found (0 when the bytes there are not one).
///
/// `strict` is for searching: it also requires every record's action stream to
/// end with ActionEnd. Those checks reject chance matches in a search, but a
/// real player accepts a declared chain without them, because the last record of
/// a list is not required to be padded the same way.
fn actions_walk(d: &[u8], start: usize, len: usize) -> bool {
    let mut pool: Vec<String> = Vec::new();
    let mut pc = start;
    let end = start + len;
    let mut ended = false;
    while pc < end {
        let Some((act, n)) = crate::avm1::decode_at(d, pc, &mut pool) else {
            return false;
        };
        pc += n.max(1);
        if matches!(act, crate::avm1::Action::End) {
            ended = true;
            break;
        }
    }
    ended || pc == end
}

fn cond_chain(d: &[u8], mut q: usize, limit: usize, strict: bool) -> usize {
    if q + 4 > limit {
        return 0;
    }
    let mut seen = 0;
    while q + 4 <= limit {
        let size = u16::from_le_bytes([d[q], d[q + 1]]) as usize;
        let cond = u16::from_le_bytes([d[q + 2], d[q + 3]]);
        // A zero size is the last record of the chain and its actions run to
        // the end of the tag, exactly as for the reader above.
        let total = if size == 0 { limit - q } else { size };
        if cond & 0x01FF == 0 || total < 4 || q + total > limit {
            break;
        }
        if strict {
            // An empty action list (`End` only) is what a chance match looks
            // like; a cond that a movie wrote has code in it.
            if total <= 4 || d[q + 4] == 0 || !actions_walk(d, q + 4, total - 4) {
                break;
            }
        }
        seen += 1;
        q += total;
        if size == 0 || seen > 8 {
            break;
        }
    }
    seen
}

/// Finds a CondAction chain by reading the tag instead of trusting its offset.
///
/// Only a chain that runs to the end of the tag counts: an obfuscated button
/// whose declared offset points into its own record block still stores its real
/// actions last.
fn scan_cond_chain(d: &[u8], from: usize, limit: usize) -> Option<usize> {
    for q in from..limit.saturating_sub(4) {
        if cond_chain(d, q, limit, true) == 0 {
            continue;
        }
        let mut p = q;
        while p + 4 <= limit {
            let size = u16::from_le_bytes([d[p], d[p + 1]]) as usize;
            // Same last-record encoding as `cond_chain`: zero runs to the end.
            let total = if size == 0 { limit - p } else { size };
            if total < 4 || p + total > limit {
                break;
            }
            p += total;
        }
        if p + 3 >= limit {
            return Some(q);
        }
    }
    None
}

fn parse_place2(
    r: &mut Reader,
    place_version: u8,
    swf_version: u8,
    tag_end: usize,
) -> Result<Op, String> {
    // PlaceObject3 widened the flag word and added field groups after the
    // PlaceObject2 prefix; reading it as a PlaceObject2 desynchronises the tag.
    let flags: u32 = if place_version >= 3 {
        r.u16() as u32
    } else {
        r.u8() as u32
    };
    let depth = r.u16();
    let has = |bit: u32| flags & bit != 0;
    let is_move = has(0x01);
    let has_character = has(0x02);
    let has_matrix = has(0x04);
    let has_cxform = has(0x08);
    let has_ratio = has(0x10);
    let has_name = has(0x20);
    let has_clip = has(0x40);
    let has_clip_actions = has(0x80);

    let _class_name = if place_version >= 3 && has(1 << 11) {
        Some(r.string())
    } else {
        None
    };
    let character = if has_character { Some(r.u16()) } else { None };
    let matrix = if has_matrix { Some(r.matrix()) } else { None };
    let cxform = if has_cxform {
        Some(r.cxform(true))
    } else {
        None
    };
    let ratio = if has_ratio {
        Some(r.u16() as f32 / 65535.0)
    } else {
        None
    };
    let name = if has_name { Some(r.string()) } else { None };
    let clip_depth = if has_clip { Some(r.u16()) } else { None };

    let mut clip_actions = Vec::new();
    if place_version >= 3 {
        // Filters are a variable-length list; since we neither render them nor
        // need anything after them, stop at the prefix in that case.
        if has(1 << 8) {
            return Ok(Op::Place {
                depth,
                character,
                matrix,
                cxform,
                ratio,
                name,
                clip_depth,
                is_move,
                clip_actions,
            });
        }
        if has(1 << 9) {
            r.u8(); // blend mode
        }
        if has(1 << 10) {
            r.u8(); // cache as bitmap
        }
        if has(1 << 13) {
            r.u8(); // visible
        }
        if has(1 << 14) {
            r.u32(); // opaque background colour
        }
    }

    if has_clip_actions {
        clip_actions = parse_clip_actions(r, swf_version, tag_end);
    }

    Ok(Op::Place {
        depth,
        character,
        matrix,
        cxform,
        ratio,
        name,
        clip_depth,
        is_move,
        clip_actions,
    })
}

/// ClipActions: a reserved word, the union of all event flags, then records.
/// Each record is an event mask, a byte size, an optional key code and the
/// handler's action stream. A zero event mask ends the list.
fn parse_clip_actions(r: &mut Reader, version: u8, end: usize) -> Vec<ClipEventAction> {
    let _reserved = r.u16();
    let _all_events = if version >= 6 { r.u32() } else { r.u16() as u32 };
    let mut out = Vec::new();
    while r.pos + 4 < end {
        let events = if version >= 6 {
            r.u32()
        } else {
            r.u16() as u32
        };
        if events == 0 {
            break;
        }
        let size = r.u32() as usize;
        // The declared size includes the key code byte when there is one.
        let key = if events & EV_KEY_PRESS != 0 { r.u8() } else { 0 };
        let key_len = if events & EV_KEY_PRESS != 0 { 1 } else { 0 };
        let start = r.pos;
        let n = size.saturating_sub(key_len);
        let limit = end.min(start + n);
        let actions = r.d[start.min(r.d.len())..limit.min(r.d.len())].to_vec();
        r.pos = limit;
        out.push(ClipEventAction {
            events,
            key,
            actions: Code::new(start, actions),
        });
    }
    out
}

fn parse_shape(id: u16, r: &mut Reader, version: u8, swf_version: u8) -> Result<Shape, String> {
    let mut shape = Shape::default();
    let styles_at = r.pos;
    let (fill_styles, line_styles) = read_styles(r, version, swf_version, false)?;
    if std::env::var("FLASH_SHAPES").is_ok() {
        println!(
            "shape {id}: v{version} fills {} lines {} styles {styles_at}..{} paint {:?}",
            fill_styles.len(),
            line_styles.len(),
            r.pos,
            fill_styles.iter().map(|f| &f.paint).collect::<Vec<_>>()
        );
    }

    // Shape records are a single continuous bit stream. Reading them with a
    // fresh reader per record (or byte-aligning between records) desynchronises
    // on real files, so one cursor is kept for the whole section.
    let mut bits = BitReader::new(r.d, r.pos * 8);
    let mut fill_bits = bits.read(4) as u32;
    let mut line_bits = bits.read(4) as u32;

    let mut contours: HashMap<u16, Vec<Vec<Pt>>> = HashMap::new();
    let mut lines: HashMap<u16, Vec<(Pt, Pt)>> = HashMap::new();
    let mut cursor = Pt { x: 0.0, y: 0.0 };
    let mut fill0 = 0u16;
    let mut fill1 = 0u16;
    let mut line = 0u16;

    while bits.bit + 6 < r.d.len() * 8 {
        let is_edge = bits.read(1) == 1;
        if is_edge {
            let straight = bits.read(1) == 1;
            if straight {
                let n = (bits.read(4) + 2) as u32;
                let general = bits.read(1) == 1;
                let (dx, dy) = if general {
                    (bits.read_signed(n) as f32, bits.read_signed(n) as f32)
                } else {
                    let vertical = bits.read(1) == 1;
                    let v = bits.read_signed(n) as f32;
                    if vertical {
                        (0.0, v)
                    } else {
                        (v, 0.0)
                    }
                };
                let next = Pt {
                    x: cursor.x + dx,
                    y: cursor.y + dy,
                };
                push_edge(&mut contours, &mut lines, fill0, fill1, line, cursor, next);
                cursor = next;
            } else {
                let n = (bits.read(4) + 2) as u32;
                let cx = cursor.x + bits.read_signed(n) as f32;
                let cy = cursor.y + bits.read_signed(n) as f32;
                let ax = cx + bits.read_signed(n) as f32;
                let ay = cy + bits.read_signed(n) as f32;
                let mut prev = cursor;
                const STEPS: usize = 6;
                for i in 1..=STEPS {
                    let t = i as f32 / STEPS as f32;
                    let mt = 1.0 - t;
                    let p = Pt {
                        x: mt * mt * cursor.x + 2.0 * mt * t * cx + t * t * ax,
                        y: mt * mt * cursor.y + 2.0 * mt * t * cy + t * t * ay,
                    };
                    push_edge(&mut contours, &mut lines, fill0, fill1, line, prev, p);
                    prev = p;
                }
                cursor = Pt { x: ax, y: ay };
            }
        } else {
            let flags = bits.read(5) as u8;
            if std::env::var("FLASH_SHAPES").is_ok() {
                println!("  shape {id}: style record {flags:#04x} at bit {}", bits.bit - 6);
            }
            if flags == 0 {
                break;
            }
            if flags & 0x01 != 0 {
                // MoveTo takes NumBits directly; only the edge records are
                // biased by +2. Reading two bits too many shifted every later
                // coordinate in real (spec-encoded) movies.
                let n = bits.read(5) as u32;
                let dx = bits.read_signed(n) as f32;
                let dy = bits.read_signed(n) as f32;
                // Absolute, not a delta: every subpath after the first
                // (a mask's second region, a shape's inner hole) landed in the
                // wrong place when the pen offset was added in.
                cursor = Pt { x: dx, y: dy };
            }
            if flags & 0x02 != 0 {
                fill0 = bits.read(fill_bits) as u16;
            }
            if flags & 0x04 != 0 {
                fill1 = bits.read(fill_bits) as u16;
            }
            if flags & 0x08 != 0 {
                line = bits.read(line_bits) as u16;
            }
            if flags & 0x10 != 0 {
                // New style tables are always byte-aligned.
                r.pos = bits.byte_pos();
                let _ = read_styles(r, version, swf_version, true)?;
                bits = BitReader::new(r.d, r.pos * 8);
                fill_bits = bits.read(4) as u32;
                line_bits = bits.read(4) as u32;
            }
        }
    }
    r.pos = bits.byte_pos();
    if std::env::var("FLASH_SHAPES").is_ok() {
        let pts: usize = contours.values().map(|c| c.iter().map(|p| p.len()).sum::<usize>()).sum();
        println!(
            "  shape {id}: records end at bit {} (byte {}), {} fills with geometry, {} points, keys {:?}",
            bits.bit,
            r.pos,
            contours.len(),
            pts,
            contours
                .iter()
                .map(|(k, v)| (*k, v.iter().map(|p| p.len()).max().unwrap_or(0)))
                .collect::<Vec<_>>()
        );
    }

    for (idx, pos) in fill_styles.iter().enumerate() {
        let key = idx as u16 + 1;
        if let Some(c) = contours.remove(&key) {
            if c.iter().any(|p| p.len() >= 3) {
                shape.fills.push(FillPath {
                    paint: pos.paint.clone(),
                    contours: c,
                });
            }
        }
    }
    for (idx, ls) in line_styles.iter().enumerate() {
        let key = idx as u16 + 1;
        if let Some(l) = lines.remove(&key) {
            shape.strokes.push(StrokePath {
                color: ls.color,
                width: ls.width.max(0.5),
                lines: l,
            });
        }
    }
    Ok(shape)
}

#[allow(clippy::type_complexity)]
fn push_edge(
    contours: &mut HashMap<u16, Vec<Vec<Pt>>>,
    lines: &mut HashMap<u16, Vec<(Pt, Pt)>>,
    fill0: u16,
    fill1: u16,
    line: u16,
    a: Pt,
    b: Pt,
) {
    // Shape coordinates are twips (1/20 px); everything downstream, including
    // the bitmap and gradient spaces, works in pixels.
    const TWIPS: f32 = 20.0;
    let a = Pt { x: a.x / TWIPS, y: a.y / TWIPS };
    let b = Pt { x: b.x / TWIPS, y: b.y / TWIPS };
    if fill1 != 0 {
        add_seg(contours, fill1, a, b);
    }
    if fill0 != 0 && fill0 != fill1 {
        // The left-hand fill runs the edge backwards.
        add_seg(contours, fill0, b, a);
    }
    if line != 0 {
        lines.entry(line).or_default().push((a, b));
    }
}

fn add_seg(contours: &mut HashMap<u16, Vec<Vec<Pt>>>, idx: u16, a: Pt, b: Pt) {
    let set = contours.entry(idx).or_default();
    // A right-hand fill (FillStyle0) is walked backwards, so its edges arrive
    // end-first and have to be joined at the *front* of the contour. Only
    // joining at the back split every reversed shape into two-point fragments,
    // which then looked degenerate and were dropped - that is how whole fills
    // (the game's grass background among them) disappeared.
    for cur in set.iter_mut().rev() {
        let (Some(first), Some(last)) = (cur.first().copied(), cur.last().copied()) else {
            continue;
        };
        if same(&last, &a) {
            cur.push(b);
            return;
        }
        if same(&first, &b) {
            cur.insert(0, a);
            return;
        }
        if same(&last, &b) {
            cur.push(a);
            return;
        }
        if same(&first, &a) {
            cur.insert(0, b);
            return;
        }
    }
    set.push(vec![a, b]);
}

fn same(a: &Pt, b: &Pt) -> bool {
    (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01
}

#[derive(Clone)]
struct LineStyle {
    color: u32,
    width: f32,
}

#[derive(Clone)]
struct FillStyle {
    paint: Paint,
}

fn read_styles(
    r: &mut Reader,
    version: u8,
    swf_version: u8,
    with_alpha_count: bool,
) -> Result<(Vec<FillStyle>, Vec<LineStyle>), String> {
    let with_alpha = version >= 3;
    let mut count = r.u8() as usize;
    if count == 0xFF && with_alpha_count {
        count = r.u16() as usize;
    }
    let mut fills = Vec::with_capacity(count);
    for _ in 0..count {
        fills.push(read_fill_style(r, with_alpha, swf_version)?);
    }
    let mut lcount = r.u8() as usize;
    if lcount == 0xFF && with_alpha_count {
        lcount = r.u16() as usize;
    }
    let mut lines = Vec::with_capacity(lcount);
    for _ in 0..lcount {
        let width = r.u16() as f32 / 20.0;
        let color = read_color(r, with_alpha);
        lines.push(LineStyle { color, width });
    }
    Ok((fills, lines))
}

fn read_color(r: &mut Reader, alpha: bool) -> u32 {
    let rgb = r.bytes(3);
    let a = if alpha { r.u8() } else { 0xFF };
    if rgb.len() < 3 {
        return 0xFF00_0000 | a as u32;
    }
    ((a as u32) << 24) | ((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32
}

fn read_fill_style(r: &mut Reader, with_alpha: bool, swf_version: u8) -> Result<FillStyle, String> {
    let kind = r.u8();
    let paint = match kind {
        0x00 => Paint::Solid(read_color(r, with_alpha)),
        0x10 | 0x12 | 0x13 => {
            let matrix = r.matrix();
            // SWF 8 changed the gradient header: the record count sits in the
            // low nibble of a flags byte, with the spread mode in bits 6-7 and
            // the interpolation mode in bits 4-5. Older files put a plain
            // count byte there and had no flags at all, so reading the modern
            // layout unconditionally swallowed a byte and every gradient fill
            // decoded to nothing.
            let n = if swf_version >= 8 {
                (r.u8() & 0x0F) as usize
            } else {
                r.u8() as usize
            };
            let mut stops = Vec::new();
            for _ in 0..n {
                // Stop ratios are absolute positions (0-255), not deltas.
                let pos = r.u8();
                let c = read_color(r, with_alpha);
                stops.push((pos, c));
            }
            // A focal radial gradient carries its focal point after the stops.
            let focal = if kind == 0x13 {
                r.u16() as f32 / 256.0
            } else {
                0.0
            };
            match kind {
                0x10 => Paint::Linear { matrix, stops },
                0x12 => Paint::Radial {
                    matrix,
                    stops,
                    focal: 0.0,
                },
                _ => Paint::Radial {
                    matrix,
                    stops,
                    focal,
                },
            }
        }
        0x40 | 0x41 => {
            let id = r.u16();
            let matrix = r.matrix();
            Paint::Bitmap { id, matrix }
        }
        _ => {
            // Unknown fill: the tag length bounds the damage.
            Paint::None
        }
    };
    Ok(FillStyle { paint })
}

/// Decodes the image bytes of a `DefineBitsJPEG*` tag. SWF 8 allows PNG or GIF
/// payloads in the same tag; those are reported rather than guessed at.
fn decode_jpeg_image(data: &[u8]) -> Result<Bitmap, String> {
    if data.len() >= 8 && data[1..4] == [0x50, 0x4E, 0x47] {
        return Err("PNG payloads in DefineBitsJPEG are not supported".into());
    }
    if data.len() >= 6 && &data[..3] == b"GIF" {
        return Err("GIF payloads in DefineBitsJPEG are not supported".into());
    }
    let img = crate::jpeg::decode(data)?;
    Ok(Bitmap {
        w: img.w,
        h: img.h,
        px: img.px,
    })
}

fn parse_lossless(r: &mut Reader, alpha: bool) -> Result<Bitmap, String> {
    let format = r.u8();
    let w = r.u16() as u32;
    let h = r.u16() as u32;
    if w == 0 || h == 0 || w > 2048 || h > 2048 {
        return Err(format!("bitmap {w}x{h} is out of range"));
    }
    // For 8-bit colormapped bitmaps only the *count* sits in the tag header:
    // the colour table itself is inside the compressed stream, in front of the
    // pixel indices. Reading it from the tag body shifted the zlib header by
    // 769 bytes and made every such bitmap look corrupt.
    let colors = if format == 3 { r.u8() as usize + 1 } else { 0 };
    let n = r.remaining();
    let compressed = r.bytes(n);
    // The Debug form of this error carries the whole partial output buffer, so
    // one bad bitmap would turn into megabytes of warning text on the screen.
    let data = miniz_oxide::inflate::decompress_to_vec_zlib(compressed).map_err(|e| {
        format!(
            "corrupt bitmap data: {} compressed bytes, {} decompressed before failing",
            compressed.len(),
            e.output.len()
        )
    })?;

    let mut px = vec![0u32; (w * h) as usize];
    match format {
        3 => {
            let entry = if alpha { 4 } else { 3 };
            let table_len = colors * entry;
            let palette: Vec<u32> = (0..colors)
                .map(|i| {
                    let o = i * entry;
                    let rr = *data.get(o).unwrap_or(&0) as u32;
                    let gg = *data.get(o + 1).unwrap_or(&0) as u32;
                    let bb = *data.get(o + 2).unwrap_or(&0) as u32;
                    let aa = if alpha {
                        *data.get(o + 3).unwrap_or(&255) as u32
                    } else {
                        255
                    };
                    let c = (aa << 24) | (rr << 16) | (gg << 8) | bb;
                    if alpha {
                        unpremultiply(c)
                    } else {
                        c
                    }
                })
                .collect();
            // Scanlines are aligned to four bytes, but small movies (and every
            // bitmap whose width is already a multiple of four) omit padding.
            let row = w as usize;
            let padded = (row + 3) & !3;
            let avail = data.len().saturating_sub(table_len);
            let stride = if padded > row && avail >= padded * h as usize {
                padded
            } else {
                row
            };
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let idx = *data.get(table_len + y * stride + x).unwrap_or(&0) as usize;
                    px[y * w as usize + x] = *palette.get(idx).unwrap_or(&0xFF00_0000);
                }
            }
        }
        5 => {
            let mut i = 0;
            for y in 0..h {
                for x in 0..w {
                    let a = *data.get(i).unwrap_or(&255);
                    let rr = *data.get(i + 1).unwrap_or(&0);
                    let gg = *data.get(i + 2).unwrap_or(&0);
                    let bb = *data.get(i + 3).unwrap_or(&0);
                    i += 4;
                    let c = ((a as u32) << 24) | ((rr as u32) << 16) | ((gg as u32) << 8) | bb as u32;
                    px[(y * w + x) as usize] = unpremultiply(c);
                }
            }
        }
        4 => {
            let mut i = 0;
            for y in 0..h {
                for x in 0..w {
                    let lo = *data.get(i).unwrap_or(&0) as u32;
                    let hi = *data.get(i + 1).unwrap_or(&0) as u32;
                    i += 2;
                    let v = (lo << 8) | hi;
                    let r5 = (v >> 10) & 0x1F;
                    let g5 = (v >> 5) & 0x1F;
                    let b5 = v & 0x1F;
                    let c = 0xFF00_0000
                        | (r5 << 19)
                        | (g5 << 11)
                        | (b5 << 3);
                    px[(y * w + x) as usize] = c;
                }
                while i % 4 != 0 && i < data.len() {
                    i += 1;
                }
            }
        }
        _ => return Err(format!("unsupported bitmap format {format}")),
    }
    Ok(Bitmap { w, h, px })
}

fn unpremultiply(c: u32) -> u32 {
    let a = (c >> 24) & 0xFF;
    if a == 0 || a == 0xFF {
        return c;
    }
    let r = (((c >> 16) & 0xFF) * 255 / a).min(255);
    let g = (((c >> 8) & 0xFF) * 255 / a).min(255);
    let b = ((c & 0xFF) * 255 / a).min(255);
    ((a) << 24) | (r << 16) | (g << 8) | b
}
