//! AVM1 (ActionScript 1/2) bytecode decoding and the value model.
//!
//! Wire format rules that matter for real files:
//!   * only opcodes >= 0x80 carry a 2-byte length; the rest are single bytes,
//!   * branch offsets (ActionJump/ActionIf) are relative to the end of the
//!     action record,
//!   * ActionIf carries its own offset and jumps when the condition is true,
//!   * operands that ActionScript pushes on the stack (property indexes, call
//!     names and argument counts) are *not* part of the action body,
//!   * DefineFunction and friends embed code after their declared length.

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    /// Index into the player's function table. Functions are values in AVM1,
    /// so they travel through variables and arguments like any other.
    Function(usize),
    /// Index into the player's object heap (plain objects and arrays).
    Object(usize),
}

impl Value {
    pub fn as_num(&self) -> f64 {
        match self {
            Value::Num(n) => *n,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Str(s) => {
                // Number("") is 0, "0x13" is 19, and other non-numeric text is
                // NaN, which is what makes comparisons with it false.
                let t = s.trim();
                if t.is_empty() {
                    return 0.0;
                }
                if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    return u32::from_str_radix(hex, 16).map(|v| v as f64).unwrap_or(f64::NAN);
                }
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
            Value::Null => 0.0,
            Value::Undefined => 0.0,
            Value::Function(_) | Value::Object(_) => f64::NAN,
        }
    }

    pub fn as_bool(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty() && s != "0" && s != "false",
            Value::Null | Value::Undefined => false,
            Value::Function(_) | Value::Object(_) => true,
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Num(n) => format_number(*n),
            Value::Bool(b) => b.to_string(),
            // Flash stringifies a function as "[type Function]" unless it has a
            // custom toString.
            Value::Function(_) => "[type Function]".into(),
            // Arrays and objects with a custom toString need the heap, so the
            // interpreter stringifies those; this is the fallback form.
            Value::Object(_) => "[object Object]".into(),
            Value::Null => "null".into(),
            // Flash's String(undefined) is the word, not an empty string.
            Value::Undefined => "undefined".into(),
        }
    }

    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Function(_) => "function",
            Value::Object(_) => "object",
        }
    }
}

pub fn format_number(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// An operand of ActionPush, resolved against the constant pool at runtime.
#[derive(Clone, Debug)]
pub enum Item {
    Value(Value),
    Constant(u16),
    Register(u8),
}

/// One parameter of a DefineFunction/DefineFunction2 body. A non-zero
/// `register` means the argument arrives in that register and has no named
/// local; zero means it is bound by name.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub register: u8,
    pub name: String,
}

/// A function body decoded out of a DefineFunction record.
#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    pub name: String,
    pub params: Vec<Param>,
    /// Registers the body may use, including preloaded ones.
    pub register_count: u8,
    /// DefineFunction2 preload/suppress flags; always 0 for DefineFunction.
    pub flags: u16,
    pub code: Vec<u8>,
    /// Where `code` starts in the movie's byte stream. Function bodies execute
    /// from the stream too, so a jump out of a body still resolves.
    pub code_start: usize,
    /// Whether the body ever mentions `arguments`. Games call their own helpers
    /// millions of times, and the arguments array is the single largest source of
    /// heap churn, so it is only materialised for bodies that ask for it.
    pub uses_arguments: bool,
}

// DefineFunction2 flag bits, in the order the preloaded registers are assigned.
// The bit values are the ones the SWF format defines (and Ruffle reads): the
// preloads occupy the low bits and the suppresses follow, so a movie compiled
// by Flash gets `this` in register 1 and its named parameters right after.
pub const PRELOAD_THIS: u16 = 1 << 0;
pub const PRELOAD_ARGUMENTS: u16 = 1 << 1;
pub const PRELOAD_SUPER: u16 = 1 << 2;
pub const PRELOAD_ROOT: u16 = 1 << 3;
pub const PRELOAD_PARENT: u16 = 1 << 4;
pub const PRELOAD_GLOBAL: u16 = 1 << 5;
pub const SUPPRESS_THIS: u16 = 1 << 6;
pub const SUPPRESS_ARGUMENTS: u16 = 1 << 7;
pub const SUPPRESS_SUPER: u16 = 1 << 8;

#[derive(Clone, Debug)]
pub enum Action {
    End,
    NextFrame,
    PrevFrame,
    /// `new obj.method(...)`, and a class extending another class.
    NewMethod,
    Extends,
    Play,
    Stop,
    StopSounds,
    ToggleQuality,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equals,
    StrictEquals,
    Less,
    Greater,
    And,
    Or,
    Not,
    StringEquals,
    StringLess,
    StringAdd,
    /// `Add2` (0x47): numeric add unless either operand is a string.
    Add2,
    StringLength,
    StringExtract,
    Pop,
    PushDuplicate,
    StackSwap,
    ToNumber,
    ToString,
    ToInteger,
    Increment,
    Decrement,
    TypeOf,
    GetVariable,
    SetVariable,
    Delete,
    Delete2,
    DefineLocal,
    DefineLocal2,
    /// Property index comes from the stack.
    GetProperty,
    SetProperty,
    GetMember,
    SetMember,
    SetTarget(String),
    SetTarget2,
    Trace,
    RandomNumber,
    GetTime,
    /// Name and argument count come from the stack.
    CallFunction,
    /// Pushes the property names of a variable's value, `for...in` style.
    Enumerate,
    /// Same, but the object comes from the stack.
    Enumerate2,
    CallMethod,
    Call,
    GotoFrame(u16),
    GotoLabel(String),
    GotoFrame2 { play: bool },
    /// Resolved instruction index.
    JumpTo(usize),
    /// Resolved instruction index; taken when the condition is true.
    IfJump(usize),
    ConstantPool(Vec<String>),
    Push(Vec<Item>),
    /// Element count comes from the stack, not the record body.
    InitArray,
    InitObject,
    /// Constructor name, argument count and arguments all come from the stack.
    NewObject,
    StoreRegister(u8),
    /// Defines a function value (named: as a local, anonymous: on the stack).
    DefineFunction(Function),
    /// Pops the return value and unwinds the current function body.
    Return,
    GetUrl,
    Unsupported(u8),
}

#[derive(Clone, Debug)]
pub struct Instr {
    pub offset: usize,
    pub len: usize,
    pub action: Action,
}

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub instrs: Vec<Instr>,
}

struct Rd<'a> {
    d: &'a [u8],
    i: usize,
}

impl<'a> Rd<'a> {
    fn new(d: &'a [u8]) -> Self {
        Rd { d, i: 0 }
    }
    fn u8(&mut self) -> u8 {
        let v = *self.d.get(self.i).unwrap_or(&0);
        self.i += 1;
        v
    }
    fn u16(&mut self) -> u16 {
        (self.u8() as u16) | ((self.u8() as u16) << 8)
    }
    fn i16(&mut self) -> i16 {
        self.u16() as i16
    }
    fn u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        for v in b.iter_mut() {
            *v = self.u8();
        }
        u32::from_le_bytes(b)
    }
    fn i32(&mut self) -> i32 {
        self.u32() as i32
    }
    fn f32(&mut self) -> f32 {
        f32::from_bits(self.u32())
    }
    /// SWF stores doubles as two 32-bit halves in the opposite order from a
    /// little-endian u64, so the halves have to be swapped back.
    fn f64_me(&mut self) -> f64 {
        let hi = self.u32() as u64;
        let lo = self.u32() as u64;
        f64::from_bits((hi << 32) | lo)
    }
    fn string(&mut self) -> String {
        let start = self.i;
        while self.i < self.d.len() && self.d[self.i] != 0 {
            self.i += 1;
        }
        // A truncated stream runs the cursor to the end (or past it, after a
        // record that overran the block), so both ends have to be clamped: a
        // malformed action stream must produce a short string, not a panic.
        let end = self.i.min(self.d.len());
        let from = start.min(end);
        let s = String::from_utf8_lossy(&self.d[from..end]).into_owned();
        self.i += 1;
        s
    }
}

/// Reads a DefineFunction/DefineFunction2 record header.
///
/// Returns name, parameters, register count, flags, code length and whether the
/// reading consumed the declared record length exactly. `by_index` selects the
/// pool-index variant some compilers emit instead of strings.
#[allow(clippy::type_complexity)]
fn read_function_record(
    body: &[u8],
    op: u8,
    pool: &[String],
    by_index: bool,
) -> Option<(String, Vec<Param>, u8, u16, usize, bool)> {
    let mut q = Rd::new(body);
    let mut read_name = |q: &mut Rd| -> String {
        if by_index {
            pool.get(q.u16() as usize).cloned().unwrap_or_default()
        } else {
            q.string()
        }
    };
    let name = read_name(&mut q);
    let count = q.u16() as usize;
    let mut register_count = 0u8;
    let mut flags = 0u16;
    if op == 0x8E {
        register_count = q.u8();
        flags = q.u16();
    }
    if count > 64 {
        return None;
    }
    let mut params = Vec::with_capacity(count);
    for _ in 0..count {
        let register = if op == 0x8E { q.u8() } else { 0 };
        let param_name = read_name(&mut q);
        params.push(Param {
            register,
            name: param_name,
        });
    }
    let code_len = q.u16() as usize;
    Some((name, params, register_count, flags, code_len, q.i == body.len()))
}

/// Where a branch lands: an offset from the end of the record. Targets outside
/// the block stop it, which is what a real player does when its reader runs off
/// the end of the data; clamping them to the first instruction would re-run the
/// block's preamble forever.
fn jump_target(code: &[u8], end: usize, rel: i16) -> usize {
    let t = end as isize + rel as isize;
    if t < 0 || t as usize > code.len() {
        code.len()
    } else {
        t as usize
    }
}

/// Decodes the single action at `start`, returning it and its total length.
///
/// Actions are read straight out of the byte stream rather than from a
/// pre-decoded list because branch targets are byte offsets: real movies jump
/// into the middle of records, and the player has to decode from wherever the
/// jump lands.
pub fn decode_at(code: &[u8], start: usize, pool: &mut Vec<String>) -> Option<(Action, usize)> {
    if start >= code.len() {
        return None;
    }
    let op = code[start];
    let (body, header) = if op >= 0x80 {
        if start + 3 > code.len() {
            return None;
        }
        let len = u16::from_le_bytes([code[start + 1], code[start + 2]]) as usize;
        let end = (start + 3 + len).min(code.len());
        (&code[start + 3..end], 3usize)
    } else {
        (&code[start + 1..start + 1], 1usize)
    };
    let mut total = header + body.len();
    let mut r = Rd::new(body);
    let action = match op {
        0x00 => Action::End,
        0x04 => Action::NextFrame,
        0x05 => Action::PrevFrame,
        0x06 => Action::Play,
        0x07 => Action::Stop,
        0x08 => Action::ToggleQuality,
        0x09 => Action::StopSounds,
        0x0A => Action::Add,
        0x0B => Action::Subtract,
        0x0C => Action::Multiply,
        0x0D => Action::Divide,
        0x0E => Action::Equals,
        0x0F => Action::Less,
        0x10 => Action::And,
        0x11 => Action::Or,
        0x12 => Action::Not,
        0x13 => Action::StringEquals,
        0x14 => Action::StringLength,
        0x15 => Action::StringExtract,
        0x17 => Action::Pop,
        0x18 => Action::ToInteger,
        0x1C => Action::GetVariable,
        0x1D => Action::SetVariable,
        0x20 => Action::SetTarget2,
        0x21 => Action::StringAdd,
        0x22 => Action::GetProperty,
        0x23 => Action::SetProperty,
        0x26 => Action::Trace,
        0x29 => Action::StringLess,
        0x30 => Action::RandomNumber,
        0x34 => Action::GetTime,
        0x3A => Action::Delete,
        0x3B => Action::Delete2,
        0x3C => Action::DefineLocal,
        0x3D => Action::CallFunction,
        0x3E => Action::Return,
        0x3F => Action::Modulo,
        0x40 => Action::NewObject,
        0x41 => Action::DefineLocal2,
        0x42 => Action::InitArray,
        0x43 => Action::InitObject,
        0x44 => Action::TypeOf,
        0x46 => Action::Enumerate,
        0x47 => Action::Add2,
        0x55 => Action::Enumerate2,
        0x48 => Action::Less,
        0x49 => Action::Equals,
        0x4A => Action::ToNumber,
        0x4B => Action::ToString,
        0x4C => Action::PushDuplicate,
        0x4D => Action::StackSwap,
        0x4E => Action::GetMember,
        0x4F => Action::SetMember,
        0x50 => Action::Increment,
        0x51 => Action::Decrement,
        0x52 => Action::CallMethod,
        0x53 => Action::NewMethod,
        0x69 => Action::Extends,
        0x63 => Action::StringLength,
        0x66 => Action::StrictEquals,
        0x67 => Action::Greater,
        0x68 => Action::StringLess,
        0x81 => {
            let f = r.u16();
            Action::GotoFrame(f)
        }
        0x83 => {
            let _url = r.string();
            let _target = r.string();
            Action::GetUrl
        }
        0x87 => Action::StoreRegister(r.u8()),
        0x88 => {
            let n = r.u16() as usize;
            let mut strings = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                strings.push(r.string());
            }
            *pool = strings.clone();
            Action::ConstantPool(strings)
        }
        0x8A => Action::Unsupported(op), // WaitForFrame
        0x8B => Action::SetTarget(r.string()),
        0x8C => Action::GotoLabel(r.string()),
        0x8D => Action::Unsupported(op), // WaitForFrame2
        0x8E | 0x9B => {
            // DefineFunction2 (0x8E) adds a register count, preload flags and
            // a register number per parameter; DefineFunction (0x9B) has
            // plain names. Either way the code sits *after* the record's
            // declared length, not inside it.
            //
            // Name and parameter names are null-terminated strings per the
            // spec. Some compilers store two-byte pool indices there instead,
            // which is indistinguishable from a two-letter name except by
            // length, so take whichever reading consumes the record exactly.
            let rec = read_function_record(body, op, &pool, false)
                .filter(|r| r.5)
                .or_else(|| read_function_record(body, op, &pool, true).filter(|r| r.5))
                .unwrap_or_else(|| {
                    read_function_record(body, op, &pool, false)
                        .unwrap_or_else(|| (String::new(), Vec::new(), 0, 0, 0, false))
                });
            let (name, params, register_count, flags, code_len, _) = rec;
            r.i = body.len();
            let body_start = start + header + body.len();
            let end = (body_start + code_len).min(code.len());
            let code_body = if body_start <= end {
                code[body_start..end].to_vec()
            } else {
                Vec::new()
            };
            total += code_len;
            let uses_arguments = code_body.windows(9).any(|w| w == b"arguments");
            Action::DefineFunction(Function {
                name,
                params,
                register_count,
                flags,
                code: code_body,
                code_start: body_start,
                uses_arguments,
            })
        }
        0x94 => {
            // With: a u16 block length followed by the block itself.
            let block = r.u16() as usize;
            total += block;
            Action::Unsupported(op)
        }
        0x96 => {
            let mut items = Vec::new();
            while r.i < body.len() {
                let ty = r.u8();
                match ty {
                    0 => items.push(Item::Value(Value::Str(r.string()))),
                    1 => items.push(Item::Value(Value::Num(r.f32() as f64))),
                    2 => items.push(Item::Value(Value::Null)),
                    3 => items.push(Item::Value(Value::Undefined)),
                    4 => items.push(Item::Register(r.u8())),
                    5 => items.push(Item::Value(Value::Bool(r.u8() != 0))),
                    6 => items.push(Item::Value(Value::Num(r.f64_me()))),
                    7 => items.push(Item::Value(Value::Num(r.i32() as f64))),
                    8 => items.push(Item::Constant(r.u8() as u16)),
                    9 => items.push(Item::Constant(r.u16())),
                    _ => break,
                }
            }
            Action::Push(items)
        }
        0x99 => {
            // Branch offsets are relative to the end of the record.
            let rel = r.i16();
            Action::JumpTo(jump_target(code, start + total, rel))
        }
        0x9A => Action::GetUrl,
        0x9D => {
            let rel = r.i16();
            Action::IfJump(jump_target(code, start + total, rel))
        }
        0x9E => Action::Call,
        0x9F => {
            let flags = r.u8();
            if flags & 0x02 != 0 {
                let _scene = r.u16();
                total += 2;
            }
            Action::GotoFrame2 {
                play: flags & 0x01 != 0,
            }
        }
        other => Action::Unsupported(other),
    };

    Some((action, total))
}

/// Decodes a whole action stream in order, for disassembly and audits. Branch
/// targets stay byte offsets; execution walks the stream with `decode_at`.
pub fn parse(code: &[u8]) -> Program {
    parse_inner(code, false)
}

/// Like [`parse`], but keeps decoding past `Action::End` records.
///
/// Protected movies split one tag into a decoy program that ends early and a
/// real one after it, reached only by a jump. Reading a block with `parse`
/// reports the decoy and nothing else, which makes such a movie look like it
/// contains no logic at all; disassembly and audits want every instruction.
pub fn parse_all(code: &[u8]) -> Program {
    parse_inner(code, true)
}

fn parse_inner(code: &[u8], past_end: bool) -> Program {
    let mut instrs: Vec<Instr> = Vec::new();
    let mut pool: Vec<String> = Vec::new();
    let mut pc = 0usize;
    let mut ended = false;
    while let Some((action, len)) = decode_at(code, pc, &mut pool) {
        let end = matches!(action, Action::End);
        instrs.push(Instr {
            offset: pc,
            len,
            action,
        });
        pc += len.max(1);
        if end {
            ended = true;
            if !past_end {
                break;
            }
        }
    }
    let _ = ended;
    Program { instrs }
}
