//! Renders SWF frames to PNG on the host for verification.

#[path = "../../../src/avm1.rs"]
mod avm1;
#[path = "../../../src/font8x8.rs"]
mod font8x8;
#[path = "../../../src/gfx.rs"]
mod gfx;
#[path = "../../../src/jpeg.rs"]
mod jpeg;
#[path = "../../../src/player.rs"]
mod player;
#[path = "../../../src/swf.rs"]
mod swf;

use std::fs;

/// Human-readable form of one AVM1 action, for reading a movie's own code.
fn fmt_action(a: &avm1::Action, pool: &[String]) -> String {
    use avm1::{Action as A, Item, Value};
    fn prop(p: u16) -> String {
        const NAMES: [&str; 22] = [
            "_x", "_y", "_xscale", "_yscale", "_currentframe", "_totalframes", "_alpha",
            "_visible", "_width", "_height", "_rotation", "_target", "_framesloaded", "_name",
            "_droptarget", "_url", "_highquality", "_focusrect", "_soundbuftime", "_quality",
            "_xmouse", "_ymouse",
        ];
        NAMES
            .get(p as usize)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("prop{p}"))
    }
    fn item(i: &Item, pool: &[String]) -> String {
        match i {
            Item::Value(Value::Str(s)) => format!("{s:?}"),
            Item::Value(Value::Num(n)) => format!("{n}"),
            Item::Value(Value::Bool(b)) => format!("{b}"),
            Item::Value(v) => format!("{:?}", v.type_of()),
            // Resolved against the block's own constant pool: obfuscated code
            // keeps its property names in the pool, so this is what makes a
            // member assignment readable in the disassembly.
            Item::Constant(c) => match pool.get(*c as usize) {
                Some(s) => format!("{s:?}"),
                None => format!("constant[{c}]"),
            },
            Item::Register(r) => format!("register{r}"),
        }
    }
    match a {
        A::Push(items) => format!(
            "push {}",
            items.iter().map(|i| item(i, pool)).collect::<Vec<_>>().join(" ")
        ),
        A::GetVariable => "getVariable".into(),
        A::SetVariable => "setVariable".into(),
        A::GetMember => "getMember".into(),
        A::SetMember => "setMember".into(),
        A::CallFunction => "callFunction".into(),
        A::CallMethod => "callMethod".into(),
        A::NewObject => "newObject".into(),
        A::NewMethod => "newMethod".into(),
        A::GetProperty => "getProperty".into(),
        A::SetProperty => "setProperty".into(),
        A::SetTarget(t) => format!("setTarget {t:?}"),
        A::GotoLabel(l) => format!("gotoLabel {l:?}"),
        A::GotoFrame(n) => format!("gotoFrame {n}"),
        A::JumpTo(t) => format!("jump {t}"),
        A::IfJump(t) => format!("ifJump {t}"),
        A::StoreRegister(r) => format!("storeRegister {r}"),
        A::ConstantPool(pool) => {
            let mut shown: Vec<String> = pool
                .iter()
                .take(40)
                .enumerate()
                .map(|(i, s)| format!("[{i}]{s:?}"))
                .collect();
            if pool.len() > 40 {
                shown.push(format!("... {} more", pool.len() - 40));
            }
            format!("constantPool {}", shown.join(" "))
        }
        A::DefineFunction(f) => {
            let ps: Vec<String> = f.params.iter().map(|p| p.name.clone()).collect();
            format!("defineFunction {:?} ({}) {} bytes @{}", f.name, ps.join(","), f.code.len(), f.code_start)
        }
        A::Unsupported(op) => format!("UNSUPPORTED 0x{op:02x}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Visits every instance on a display list and its children.
///
/// Harness tooling only: a game's controls live several sprites deep, so a
/// show/hide hook that only looks at the top level cannot reach them.
fn for_each_instance(
    list: &mut std::collections::BTreeMap<u16, player::Instance>,
    f: &mut dyn FnMut(&mut player::Instance),
) {
    for inst in list.values_mut() {
        f(inst);
        if let Some(sp) = inst.sprite.as_mut() {
            for_each_instance(&mut sp.list, f);
        }
    }
}

/// Prints every addressable action block in a movie, or only the blocks whose
/// strings mention `filter`. Reading the game's own bytecode is how a missing
/// language feature is identified instead of guessed at.
fn disasm_block(label: &str, code: &swf::Code, filter: &str) {
    let prog = avm1::parse_all(&code.bytes);
    // The pool is built as the block is read, exactly as a player does, so a
    // `Constant(n)` operand prints as the string the movie pushed.
    let mut cur: Vec<String> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    for ins in &prog.instrs {
        if let avm1::Action::ConstantPool(p) = &ins.action {
            cur = p.clone();
        }
        shown.push(format!("  {:6} {}", ins.offset, fmt_action(&ins.action, &cur)));
    }
    if !filter.is_empty() && !shown.iter().any(|line| line.contains(filter)) {
        return;
    }
    println!("\n=== {label} @{} ({} bytes, {} instrs) ===", code.off, code.bytes.len(), prog.instrs.len());
    for (i, line) in shown.iter().enumerate() {
        println!("{line}");
        if let avm1::Action::DefineFunction(f) = &prog.instrs[i].action {
            // A body carries its own pool, so resolve independently of the
            // block that defined it.
            let body = avm1::parse(&f.code);
            let mut bp: Vec<String> = Vec::new();
            for b in &body.instrs {
                if let avm1::Action::ConstantPool(p) = &b.action {
                    bp = p.clone();
                }
                println!("        {:6} {}", b.offset, fmt_action(&b.action, &bp));
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if std::env::var_os("FLASH_ARGS").is_some() {
        eprintln!("args {args:?}");
    }
    if args.len() >= 3 && args[1] == "--disasm" {
        // Dumps action bytecode: every frame, init and button block.
        let data = fs::read(&args[2]).expect("cannot read file");
        let filter = args.get(3).cloned().unwrap_or_default();
        let movie = swf::Movie::parse(&data).expect("parse error");
        for (id, blocks) in &movie.init_actions {
            for (i, c) in blocks.iter().enumerate() {
                disasm_block(&format!("initclip char {id} #{i}"), c, &filter);
            }
        }
        for (f, frame) in movie.root.frames.iter().enumerate() {
            for (i, c) in frame.actions.iter().enumerate() {
                disasm_block(&format!("root frame {f} action #{i}"), c, &filter);
            }
        }
        for (id, tl) in &movie.sprites {
            for (f, frame) in tl.frames.iter().enumerate() {
                for (i, c) in frame.actions.iter().enumerate() {
                    disasm_block(&format!("sprite {id} frame {f} action #{i}"), c, &filter);
                }
            }
        }
        for (id, b) in &movie.buttons {
            for (i, c) in b.conds.iter().enumerate() {
                disasm_block(&format!("button {id} cond #{i}"), &c.actions, &filter);
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--avm1" {
        std::process::exit(run_avm1(&args[2]));
    }
    if args.len() >= 4 && args[1] == "--at" {
        // Decodes action bytes at a byte offset of the movie's data stream.
        // Protected movies keep their real program in private tags, so the
        // only way to read the code that a block jumps to is to decode where
        // the runtime actually reported running.
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let start: usize = args[3].parse().expect("bad offset");
        let count: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(40);
        let mut pc = start;
        let mut pool: Vec<String> = Vec::new();
        for _ in 0..count {
            let Some((action, len)) = avm1::decode_at(&movie.data, pc, &mut pool) else {
                println!("{pc}: undecodable");
                break;
            };
            println!("{pc:8} {}", fmt_action(&action, &pool));
            pc += len.max(1);
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--jpeg" {
        // Decode a JPEG (or every bitmap in a SWF) and write it out as PNG so
        // it can be diffed against an independent decoder.
        let data = fs::read(&args[2]).expect("cannot read file");
        let mut images: Vec<(String, u32, u32, Vec<u32>)> = Vec::new();
        if data.starts_with(&[0xFF, 0xD8]) {
            match jpeg::decode(&data) {
                Ok(img) => images.push((String::from("jpeg"), img.w, img.h, img.px)),
                Err(e) => {
                    println!("error: {e}");
                    return;
                }
            }
        } else {
            let movie = swf::Movie::parse(&data).expect("parse error");
            for w in &movie.warnings {
                println!("warning: {w}");
            }
            let mut ids: Vec<&u16> = movie.bitmaps.keys().collect();
            ids.sort();
            for id in ids {
                let b = &movie.bitmaps[id];
                images.push((format!("bitmap-{id}"), b.w, b.h, b.px.clone()));
            }
        }
        for (name, w, h, px) in images {
            let canvas = gfx::Canvas::from_pixels(w as i32, h as i32, px.clone());
            let stem = std::path::Path::new(&args[2])
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone());
            let path = format!(".tmp/{stem}.png");
            write_png(&canvas, &path);
            let n = px.len().max(1) as u64;
            let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
            for p in &px {
                r += (p >> 16) as u64 & 0xFF;
                g += (p >> 8) as u64 & 0xFF;
                b += *p as u64 & 0xFF;
            }
            println!(
                "{name}: {w}x{h} mean rgb({},{},{}) -> {path}",
                r / n,
                g / n,
                b / n
            );
        }
        return;
    }
    if args.len() == 4 && args[1] == "--shape" {
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let id: u16 = args[3].parse().expect("bad id");
        if let Some(sh) = movie.shapes.get(&id) {
            for (i, f) in sh.fills.iter().enumerate() {
                let paint = match &f.paint {
                    swf::Paint::Solid(c) => format!("solid 0x{c:08x}"),
                    swf::Paint::Linear { stops, .. } => format!("linear {stops:?}"),
                    swf::Paint::Radial { stops, .. } => format!("radial {stops:?}"),
                    swf::Paint::Bitmap { id, .. } => format!("bitmap {id}"),
                    swf::Paint::None => String::from("none"),
                };
                let mut bx = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
                for c in &f.contours {
                    for p in c {
                        bx.0 = bx.0.min(p.x);
                        bx.1 = bx.1.max(p.x);
                        bx.2 = bx.2.min(p.y);
                        bx.3 = bx.3.max(p.y);
                    }
                }
                println!(
                    "fill {i} {paint} contours {} bounds x {:.1}..{:.1} y {:.1}..{:.1}",
                    f.contours.len(),
                    bx.0,
                    bx.1,
                    bx.2,
                    bx.3
                );
                for (j, c) in f.contours.iter().enumerate() {
                    println!("  contour {j}: {} points", c.len());
                    for p in c.iter().take(14) {
                        println!("    ({:.1}, {:.1})", p.x, p.y);
                    }
                }
            }
        }
        return;
    }
    if args.len() == 4 && args[1] == "--draw" {
        // Draws one character on its own, centred, and reports the pixels it
        // produced. Isolates a shape from the rest of the display list.
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let id: u16 = args[3].parse().expect("bad id");
        let mut canvas = gfx::Canvas::new(movie.width as i32, movie.height as i32);
        if let Some(shape) = movie.shapes.get(&id) {
            canvas.draw_shape(shape, &movie, &swf::Mat::IDENTITY, [1.0; 4]);
        } else if let Some(bm) = movie.bitmaps.get(&id) {
            canvas.draw_bitmap(bm, &swf::Mat::IDENTITY, [1.0; 4]);
        } else if let Some(text) = movie.texts.get(&id) {
            player::draw_static_text(&mut canvas, &movie, text, &swf::Mat::IDENTITY, [1.0; 4]);
        } else if let Some(et) = movie.edit_texts.get(&id) {
            let v = et.initial.clone();
            player::draw_edit_text(&mut canvas, &movie, et, &swf::Mat::IDENTITY, [1.0; 4], &v);
        } else {
            println!("character {id} is not a shape or bitmap");
            return;
        }
        let mut lit = 0usize;
        for (i, p) in canvas.px.iter().enumerate() {
            if p & 0xFFFFFF != 0 {
                lit += 1;
                let _ = i;
            }
        }
        let mid = ((movie.height / 2) * movie.width + movie.width / 2) as usize;
        println!(
            "{id}: {}x{} lit pixels {lit} of {}, centre 0x{:08x}, puts {}",
            movie.width,
            movie.height,
            canvas.px.len(),
            canvas.px[mid],
            canvas.puts
        );
        write_png(&canvas, ".tmp/draw.png");
        return;
    }
    if args.len() == 4 && args[1] == "--button" {
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let id: u16 = args[3].parse().expect("bad id");
        if let Some(b) = movie.buttons.get(&id) {
            for r in &b.records {
                println!(
                    "record char {} depth {} states 0x{:02x} at ({:.0},{:.0}) scale ({:.3},{:.3})",
                    r.char_id, r.depth, r.states, r.matrix.tx, r.matrix.ty, r.matrix.scale_x, r.matrix.scale_y
                );
            }
            println!("{} conditions", b.conds.len());
            for c in &b.conds {
                println!(
                    "  cond events 0x{:04x} key {} {} bytes",
                    c.conditions,
                    c.key,
                    c.actions.bytes.len()
                );
                let prog = avm1::parse(&c.actions.bytes);
                let mut pool: Vec<String> = Vec::new();
                for ins in &prog.instrs {
                    if let avm1::Action::ConstantPool(p) = &ins.action {
                        pool = p.clone();
                    }
                    println!("      {:6} {}", ins.offset, fmt_action(&ins.action, &pool));
                }
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--bits" {
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        for (id, b) in movie.bitmaps.iter() {
            let n = b.px.len().max(1) as u32;
            let (mut r, mut g, mut bl, mut a) = (0u64, 0u64, 0u64, 0u64);
            for p in &b.px {
                a += (p >> 24) as u64 & 0xFF;
                r += (p >> 16) as u64 & 0xFF;
                g += (p >> 8) as u64 & 0xFF;
                bl += *p as u64 & 0xFF;
            }
            println!(
                "bitmap {id}: {}x{} avg rgba({},{},{},{})",
                b.w,
                b.h,
                r / n as u64,
                g / n as u64,
                bl / n as u64,
                a / n as u64
            );
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--ops" {
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let tl = match args.get(3).and_then(|s| s.parse::<u16>().ok()) {
            Some(id) => movie.sprites.get(&id).expect("no such sprite"),
            None => &movie.root,
        };
        for (i, f) in tl.frames.iter().enumerate() {
            println!("frame {i}: {} ops, {} action blocks", f.ops.len(), f.actions.len());
            for block in &f.actions {
                for ins in avm1::parse(&block.bytes).instrs {
                    println!("    {:?}", ins.action);
                }
            }
            for op in &f.ops {
                match op {
                    swf::Op::Place {
                        depth,
                        character,
                        clip_depth,
                        name,
                        is_move,
                        matrix,
                        cxform,
                        ..
                    } => println!(
                        "  place d{depth} char {character:?} move={is_move} clip={clip_depth:?} name={name:?} at ({:.0},{:.0}) cx={:?}",
                        matrix.map(|m| m.tx).unwrap_or(0.0),
                        matrix.map(|m| m.ty).unwrap_or(0.0),
                        cxform.map(|c| (c.mul, c.add))
                    ),
                    swf::Op::Remove { depth } => println!("  remove d{depth}"),
                }
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--shapes" {
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        for (id, sh) in movie.shapes.iter() {
            let pts: usize = sh.fills.iter().flat_map(|f| f.contours.iter()).map(|c| c.len()).sum();
            let lines: usize = sh.strokes.iter().map(|s| s.lines.len()).sum();
            let paints: Vec<String> = sh
                .fills
                .iter()
                .map(|f| match &f.paint {
                    swf::Paint::Solid(c) => format!("solid {c:08x}"),
                    swf::Paint::Linear { stops, .. } => format!("linear {} stops", stops.len()),
                    swf::Paint::Radial { stops, .. } => format!("radial {} stops", stops.len()),
                    swf::Paint::Bitmap { id, .. } => format!("bitmap {id}"),
                    swf::Paint::None => "none".into(),
                })
                .collect();
            println!(
                "shape {id}: {} fills, {pts} points, {} strokes ({lines} lines) {paints:?}",
                sh.fills.len(),
                sh.strokes.len()
            );
        }
        return;
    }
    if args.len() >= 4 && args[1] == "--glyphs" {
        // Dumps a font's glyph outlines in font units, which is how a text
        // rendering fault is told apart from a font parsing fault.
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let id: u16 = args[3].parse().expect("bad id");
        let Some(font) = movie.fonts.get(&id) else {
            println!("no font {id}");
            return;
        };
        println!(
            "font {id} upem={} ascent={} descent={} leading={}",
            font.units_per_em, font.ascent, font.descent, font.leading
        );
        for (i, g) in font.glyphs.iter().enumerate() {
            let mut b = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
            let mut pts = 0;
            for f in &g.fills {
                for c in &f.contours {
                    for p in c {
                        pts += 1;
                        b.0 = b.0.min(p.x);
                        b.1 = b.1.max(p.x);
                        b.2 = b.2.min(p.y);
                        b.3 = b.3.max(p.y);
                    }
                }
            }
            let code = font.codes.get(i).copied().unwrap_or(0);
            println!(
                "glyph {i:3} code {:?} adv {} fills {} pts {pts} bounds x {:.0}..{:.0} y {:.0}..{:.0}",
                char::from_u32(code as u32).unwrap_or('?'),
                font.advance_of(i),
                g.fills.len(),
                b.0,
                b.1,
                b.2,
                b.3
            );
            for f in &g.fills {
                for (j, c) in f.contours.iter().enumerate() {
                    if j > 2 {
                        break;
                    }
                    println!(
                        "    contour {j} {} pts: {:?}",
                        c.len(),
                        c.iter().take(8).map(|p| (p.x as i32, p.y as i32)).collect::<Vec<_>>()
                    );
                }
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--text" {
        // Decodes the glyph runs of every DefineText back to characters, which
        // is the only way to read a movie's labels without its source.
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let mut ids: Vec<&u16> = movie.texts.keys().collect();
        ids.sort();
        for id in ids {
            let text = &movie.texts[id];
            let mut line = String::new();
            for run in &text.runs {
                let Some(font) = movie.fonts.get(&run.font) else {
                    continue;
                };
                if !line.is_empty() {
                    line.push('|');
                }
                for &(idx, _) in &run.glyphs {
                    let code = font.codes.get(idx as usize).copied().unwrap_or(0);
                    line.push(char::from_u32(code as u32).unwrap_or('?'));
                }
            }
            if !line.trim().is_empty() {
                println!("text {id}: {line}");
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--fonts" {
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        for (id, f) in movie.fonts.iter() {
            let mut glyph_px = 0usize;
            let mut empty = 0usize;
            for g in &f.glyphs {
                let fills: usize = g.fills.iter().map(|p| p.contours.len()).sum();
                if fills == 0 {
                    empty += 1;
                }
                for p in &g.fills {
                    for c in &p.contours {
                        glyph_px += c.len();
                    }
                }
            }
            println!(
                "font {id}: {} glyphs ({} empty, {glyph_px} points) upem={} ascent={} descent={} leading={} codes={}",
                f.glyphs.len(),
                empty,
                f.units_per_em,
                f.ascent,
                f.descent,
                f.leading,
                f.codes.len()
            );
        }
        for (id, t) in movie.texts.iter() {
            println!("text {id}: {} runs", t.runs.len());
            for r in t.runs.iter().take(2) {
                println!(
                    "  run font={} h={} color=0x{:08x} at ({}, {}) glyphs={}",
                    r.font,
                    r.height,
                    r.color,
                    r.x,
                    r.y,
                    r.glyphs.len()
                );
            }
        }
        for (id, t) in movie.edit_texts.iter() {
            println!(
                "edittext {id}: font={} h={} color=0x{:08x} var={:?} initial={:?} bounds={:?} wrap={} align={}",
                t.font, t.height, t.color, t.var, t.initial, t.bounds, t.wrap, t.align
            );
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--timeline" {
        // Dumps a timeline frame by frame with its decoded ActionScript, which is
        // how a stuck preloader or menu is diagnosed.
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let sprite: u16 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
        let max: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
        let tl = if sprite == 0 {
            &movie.root
        } else {
            match movie.sprites.get(&sprite) {
                Some(t) => t,
                None => {
                    println!("no sprite {sprite}");
                    return;
                }
            }
        };
        println!("{} frames", tl.frames.len());
        if !tl.labels.is_empty() {
            let mut ls: Vec<_> = tl.labels.iter().collect();
            ls.sort_by_key(|(_, v)| **v);
            println!("labels {ls:?}");
        }
        for (i, f) in tl.frames.iter().enumerate().take(max) {
            println!("frame {i}: {} ops, {} action blocks", f.ops.len(), f.actions.len());
            for op in &f.ops {
                println!("  {op:?}");
            }
            for code in &f.actions {
                for ins in &avm1::parse(&code.bytes).instrs {
                    // Function bodies dwarf everything else, so print their
                    // signature and size instead of the whole record.
                    let text = format!("{:?}", ins.action);
                    if text.starts_with("DefineFunction") {
                        let name = text
                            .split("name: \"")
                            .nth(1)
                            .and_then(|s| s.split('"').next())
                            .unwrap_or("");
                        println!(
                            "    [{}] DefineFunction {name:?} ({} bytes)",
                            ins.offset,
                            text.len()
                        );
                    } else {
                        println!("    [{}] {text}", ins.offset);
                    }
                }
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--names" {
        // Lists the display list with bounds and visibility after the movie has
        // run for a while: the headless way to find where a control actually is
        // instead of guessing stage coordinates.
        let data = fs::read(&args[2]).expect("cannot read file");
        let seconds: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4.0);
        let movie = swf::Movie::parse(&data).expect("parse error");
        let fps = movie.frame_rate.max(1.0);
        let mut p = player::Player::new(movie);
        for _ in 0..((seconds * fps) as usize) {
            p.tick(1.0 / fps);
        }
        println!("after {seconds}s: frame {} playing={}", p.frame, p.playing);
        fn walk(
            p: &player::Player,
            list: &std::collections::BTreeMap<u16, player::Instance>,
            parent: &swf::Mat,
            prefix: &str,
            level: usize,
        ) {
            for inst in list.values() {
                let name = inst.name.clone().unwrap_or_default();
                let path = if prefix.is_empty() {
                    name.clone()
                } else if name.is_empty() {
                    prefix.to_string()
                } else {
                    format!("{prefix}.{name}")
                };
                let world = parent.mul(&inst.matrix);
                let b = player::bounds_of(&p.movie, inst, parent)
                    .map(|(x0, y0, x1, y1)| format!("{x0:.0},{y0:.0} {x1:.0},{y1:.0}"))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{:indent$}{:22} char {:5} depth {:4} visible {:5} at {:.0},{:.0} bounds {b}",
                    "",
                    if path.is_empty() { "<unnamed>" } else { &path },
                    inst.char_id,
                    inst.depth,
                    inst.visible,
                    world.tx,
                    world.ty,
                    indent = level * 2
                );
                if let Some(sp) = inst.sprite.as_ref() {
                    if level < 4 {
                        walk(p, &sp.list, &world, &path, level + 1);
                    }
                }
            }
        }
        let root = swf::Mat {
            scale_x: 1.0,
            scale_y: 1.0,
            rot_skew_0: 0.0,
            rot_skew_1: 0.0,
            tx: 0.0,
            ty: 0.0,
        };
        walk(&p, &p.list, &root, "", 0);
        return;
    }
    if args.len() >= 3 && args[1] == "--handlers" {
        // Lists every clip event handler with the instance it belongs to: the
        // map of what is clickable in a menu whose art does not draw yet.
        let data = fs::read(&args[2]).expect("cannot read file");
        let seconds: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20.0);
        let movie = swf::Movie::parse(&data).expect("parse error");
        let fps = movie.frame_rate.max(1.0);
        let mut p = player::Player::new(movie);
        for _ in 0..((seconds * fps) as usize) {
            p.tick(1.0 / fps);
        }
        fn walk(
            p: &player::Player,
            list: &std::collections::BTreeMap<u16, player::Instance>,
            prefix: &str,
            level: usize,
        ) {
            for inst in list.values() {
                let name = inst.name.clone().unwrap_or_default();
                let path = if prefix.is_empty() {
                    name.clone()
                } else if name.is_empty() {
                    prefix.to_string()
                } else {
                    format!("{prefix}.{name}")
                };
                for ca in &inst.clip_actions {
                    println!(
                        "{path:32} events 0x{:08x} key {} actions {} bytes at {}",
                        ca.events, ca.key, ca.actions.bytes.len(), ca.actions.off
                    );
                }
                if let Some(sp) = inst.sprite.as_ref() {
                    if level < 8 {
                        walk(p, &sp.list, &path, level + 1);
                    }
                }
            }
        }
        walk(&p, &p.list, "", 0);
        return;
    }
    if args.len() >= 3 && args[1] == "--buttons" {
        // Scans the stage for clickable buttons after the movie settles: which
        // character is under each point, and where. The headless way to find
        // the "Play" button of a menu whose art does not render yet.
        let data = fs::read(&args[2]).expect("cannot read file");
        let seconds: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(12.0);
        let movie = swf::Movie::parse(&data).expect("parse error");
        let fps = movie.frame_rate.max(1.0);
        let mut p = player::Player::new(movie);
        for _ in 0..((seconds * fps) as usize) {
            p.tick(1.0 / fps);
        }
        println!("after {seconds}s: frame {} playing={}", p.frame, p.playing);
        let mut seen: std::collections::BTreeMap<u16, (usize, f32, f32, String)> =
            std::collections::BTreeMap::new();
        for gy in 0..24 {
            for gx in 0..32 {
                let x = (gx as f32 + 0.5) * 640.0 / 32.0;
                let y = (gy as f32 + 0.5) * 480.0 / 24.0;
                if let Some((id, path)) = p.button_at_point(x, y) {
                    let e = seen.entry(id).or_insert((0, x, y, path));
                    e.0 += 1;
                }
            }
        }
        for (id, (n, x, y, path)) in seen {
            println!("button {id} under {n} points, first at ({x:.0},{y:.0}) path {path:?}");
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--run" {
        // Runs a real movie for a few seconds and reports what the engine made
        // of it: warnings from the decoder, trace() output, draw-list size and a
        // PNG of the last frame. This is the headless way to check a game.
        let data = fs::read(&args[2]).expect("cannot read file");
        let seconds: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2.0);
        let movie = swf::Movie::parse(&data).expect("parse error");
        println!(
            "stage {}x{} @ {:.1}fps, {} frames, {} shapes, {} bitmaps, {} sprites, {} buttons",
            movie.width,
            movie.height,
            movie.frame_rate,
            movie.root.frames.len(),
            movie.shapes.len(),
            movie.bitmaps.len(),
            movie.sprites.len(),
            movie.buttons.len()
        );
        println!(
            "fonts {}, static texts {}, edit texts {}",
            movie.fonts.len(),
            movie.texts.len(),
            movie.edit_texts.len()
        );
        for w in &movie.warnings {
            println!("warning: {w}");
        }
        let fps = movie.frame_rate.max(1.0);
        let mut p = player::Player::new(movie);
        let steps = (seconds * fps) as usize;
        for _ in 0..steps {
            p.tick(1.0 / fps);
        }
        // FLASH_SHOW=<instance name> makes one top-level clip visible, which is
        // how a control that a movie keeps hidden until a state it never reaches
        // can still be tried by hand.
        // FLASH_HIDE=<a,b,...> hides top-level clips by name, which splits a
        // full-screen overlay that swallows input apart from the game under it.
        if let Ok(list) = std::env::var("FLASH_HIDE") {
            for want in list.split(',') {
                let before = p.list.len();
                for_each_instance(&mut p.list, &mut |inst| {
                    if inst.name.as_deref() == Some(want) {
                        inst.visible = false;
                    }
                });
                println!("hid {want} (top level {before} -> {})", p.list.len());
            }
        }
        if let Ok(name) = std::env::var("FLASH_SHOW") {
            // Nested, because a control a movie leaves hidden is usually not
            // at the top level: `startrnd_btn` sits 465 deep in a panel.
            for_each_instance(&mut p.list, &mut |inst| {
                if inst.name.as_deref() == Some(name.as_str()) {
                    inst.visible = true;
                    println!("forced {name} visible");
                }
            });
        }
        // `--clicks "x,y x,y ..."` replays a sequence of taps, which is how a
        // game that needs "pick a tower, drop it, start the round" is driven.
        if let Some(pos) = args.iter().position(|a| a == "--clicks") {
            if let Some(script) = args.get(pos + 1) {
                for step in script.split_whitespace() {
                    let mut it = step.split(',');
                    let x: f32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                    let y: f32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                    println!("tap ({x},{y}) button {:?}", p.button_at_point(x, y));
                    p.touch_down(x, y);
                    for _ in 0..3 {
                        p.tick(1.0 / fps);
                    }
                    p.touch_up();
                    for _ in 0..24 {
                        p.tick(1.0 / fps);
                    }
                }
            }
        }
        // `--then N`: keep running after the click script, so a game that reacts
        // over the following seconds (spawning a wave) shows what it did.
        if let Some(pos) = args.iter().position(|a| a == "--then") {
            let secs: f32 = args.get(pos + 1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
            for _ in 0..(secs * fps) as usize {
                p.tick(1.0 / fps);
            }
        }
        // Optional headless click: `--click X Y` presses and releases at a stage
        // point, then runs on so the handler has time to act.
        if args.len() >= 7 && args[4] == "--click" {
            let x: f32 = args[5].parse().unwrap_or(320.0);
            let y: f32 = args[6].parse().unwrap_or(240.0);
            println!("button at ({x},{y}): {:?}", p.button_at_point(x, y));
            p.touch_down(x, y);
            for _ in 0..2 {
                p.tick(1.0 / fps);
            }
            p.touch_up();
            for _ in 0..400 {
                p.tick(1.0 / fps);
            }
        }
        // `--frame N` jumps the root timeline to a frame, which is how a scene
        // behind a gate can be inspected without playing up to it.
        if let Some(pos) = args.iter().position(|a| a == "--frame") {
            if let Some(n) = args.get(pos + 1).and_then(|s| s.parse::<usize>().ok()) {
                p.goto_frame(n);
                for _ in 0..40 {
                    p.tick(1.0 / fps);
                }
                println!("forced frame {n} -> now {}", p.frame);
            }
        }
        if args.iter().any(|a| a == "--scan") {
            let mut seen: std::collections::BTreeMap<u16, (usize, f32, f32, String)> =
                std::collections::BTreeMap::new();
            for gy in 0..24 {
                for gx in 0..32 {
                    let x = (gx as f32 + 0.5) * 640.0 / 32.0;
                    let y = (gy as f32 + 0.5) * 480.0 / 24.0;
                    if let Some((id, path)) = p.button_at_point(x, y) {
                        let e = seen.entry(id).or_insert((0, x, y, path));
                        e.0 += 1;
                    }
                }
            }
            for (id, (n, x, y, path)) in seen {
                println!("button {id} under {n} points, first at ({x:.0},{y:.0}) path {path:?}");
            }
        }
        // Composing one depth at a time and sampling a few points shows which
        // object paints over what: a full-stage shape at a low depth hides the
        // map and the UI, and that is invisible in a single final image.
        if let Some(pos) = args.iter().position(|a| a == "--upto") {
            if let Some(n) = args.get(pos + 1).and_then(|s| s.parse::<u16>().ok()) {
                p.render_upto(n);
                write_png(&p.canvas, ".tmp/upto.png");
                println!("composed up to depth {n} -> .tmp/upto.png");
            }
        }
        if args.iter().any(|a| a == "--order") {
            let probe = [(100, 200), (500, 330), (560, 40)];
            for d in p.list.keys().copied().collect::<Vec<u16>>() {
                p.render_upto(d);
                let mut row = format!("order {d:4}");
                for (px, py) in probe {
                    let i = py * p.canvas.w + px;
                    row.push_str(&format!(" 0x{:08x}", p.canvas.px[i as usize]));
                }
                println!("{row}");
            }
        }
        p.log.clear();
        p.render();
        println!("background: 0x{:08x}", p.movie.background);
        write_png(&p.canvas, ".tmp/run-final.png");
        let mid = ((p.canvas.h / 2) * p.canvas.w + p.canvas.w / 2) as usize;
        println!(
            "drawn pixels: {} (puts {}, centre 0x{:08x})",
            p.canvas.count_non_background(p.movie.background),
            p.canvas.puts,
            p.canvas.px[mid]
        );
        println!("blocks truncated: {}", p.truncated);
        println!("actions executed: {}", p.steps);
        println!(
            "after {seconds}s: frame {} playing={} instances={} variables={}",
            p.frame,
            p.playing,
            p.instance_count(),
            p.variable_count()
        );
        for line in p.log.iter().take(40) {
            println!("trace: {line}");
        }
        println!("variables:");
        for (k, v) in p.vars_dump() {
            println!("  {k} = {v}");
        }
        // The game controller is the object holding `currentTool`; its fields
        // are the readable state of "what the player has selected".
        for (id, props) in p.objects_dump() {
            if props.iter().any(|(k, _)| k == "currentTool") {
                println!("game controller object {id}:");
                for (k, v) in props {
                    let v: String = v.chars().take(60).collect();
                    println!("  {k} = {v}");
                }
            }
        }
        println!("display tree:");
        dump_list(&p, &p.list, 0, 6);
        println!("display list (top level):");
        for (depth, inst) in p.list.iter().take(200) {
            println!(
                "  depth {depth}: char {} name {:?} at ({:.0},{:.0}) visible={} alpha={:.2} sprite={}",
                inst.char_id,
                inst.name,
                inst.matrix.tx,
                inst.matrix.ty,
                inst.visible,
                inst.cxform.mul[3],
                inst.sprite.is_some()
            );
        }
        let mut misses: Vec<(&String, &u32)> = p.misses.iter().collect();
        misses.sort_by(|a, b| b.1.cmp(a.1));
        println!("unimplemented (top 25):");
        for (name, count) in misses.iter().take(25) {
            println!("  {count:6}  {name}");
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--objects" {
        // Dumps the object heap after the movie settles. Game controllers are
        // plain objects, and their property names are not obfuscated even when
        // the code is, so this is where the readable game state lives.
        let data = fs::read(&args[2]).expect("cannot read file");
        let seconds: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(12.0);
        let movie = swf::Movie::parse(&data).expect("parse error");
        let fps = movie.frame_rate.max(1.0);
        let mut p = player::Player::new(movie);
        for _ in 0..(seconds * fps) as usize {
            p.tick(1.0 / fps);
        }
        let all = p.objects_dump();
        println!("{} objects on the heap", all.len());
        for (id, props) in all {
            if props.is_empty() {
                continue;
            }
            println!("object {id}: {} props", props.len());
            for (k, v) in props {
                println!("  {k} = {v}");
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--v" {
        // `--v movie.swf seconds name`: one variable and its object's fields.
        let data = fs::read(&args[2]).expect("cannot read file");
        let seconds: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(6.0);
        let name = args.get(4).cloned().unwrap_or_default();
        let movie = swf::Movie::parse(&data).expect("parse error");
        let fps = movie.frame_rate.max(1.0);
        let mut p = player::Player::new(movie);
        for _ in 0..(seconds * fps) as usize {
            p.tick(1.0 / fps);
        }
        print!("{}", p.inspect_var(&name));
        return;
    }
    if args.len() >= 3 && args[1] == "--audit" {
        // Counts, over every script in a movie, whether the code that would
        // actually drive the display exists: constant-pool references, member
        // access and calls. A movie whose scripts only do arithmetic cannot run
        // in any player, and this reports that as a number.
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let mut blocks = 0usize;
        let mut instrs = 0usize;
        let mut pools = 0usize;
        let mut constants = 0usize;
        let mut set_member = 0usize;
        let mut get_member = 0usize;
        let mut calls = 0usize;
        let mut funcs = 0usize;
        // Function bodies are separate action streams stored after the
        // DefineFunction record, so they must be walked explicitly: a player
        // runs them, and skipping them under-reports what a movie actually does.
        let ops = std::env::var("AUDIT_OPS").is_ok();
        fn account(code: &[u8], stats: &mut [usize; 10], ops: bool) {
            let prog = avm1::parse(code);
            stats[0] += 1;
            for ins in &prog.instrs {
                stats[1] += 1;
                match &ins.action {
                    avm1::Action::ConstantPool(_) => stats[2] += 1,
                    avm1::Action::Push(items) => {
                        stats[3] += items.iter().filter(|i| matches!(i, avm1::Item::Constant(_))).count();
                        // Names a script actually pushes: a movie whose every
                        // string literal is a control character has had its
                        // identifiers stripped.
                        stats[8] += items
                            .iter()
                            .filter(|i| match i {
                                avm1::Item::Value(avm1::Value::Str(s)) => {
                                    s.chars().count() >= 3 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                                }
                                _ => false,
                            })
                            .count();
                    }
                    avm1::Action::SetMember => stats[4] += 1,
                    avm1::Action::GetMember => stats[5] += 1,
                    avm1::Action::CallFunction | avm1::Action::CallMethod => stats[6] += 1,
                    avm1::Action::DefineFunction(f) => {
                        stats[7] += 1;
                        account(&f.code, stats, ops);
                    }
                    avm1::Action::Unsupported(op) => {
                        stats[9] += 1;
                        if ops {
                            println!("op {op:#04x}");
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut stats = [0usize; 10];
        let mut walk = |tl: &swf::Timeline, stats: &mut [usize; 10]| {
            for f in &tl.frames {
                for code in &f.actions {
                    account(&code.bytes, stats, ops);
                }
            }
        };
        walk(&movie.root, &mut stats);
        for tl in movie.sprites.values() {
            walk(tl, &mut stats);
        }
        for blocks_ in movie.init_actions.values() {
            for code in blocks_ {
                account(&code.bytes, &mut stats, ops);
            }
        }
        for btn in movie.buttons.values() {
            for c in &btn.conds {
                account(&c.actions.bytes, &mut stats, ops);
            }
        }
        // Clip actions live on placements, so walk the timelines for those too.
        let mut clip_event_blocks: Vec<swf::Code> = Vec::new();
        let mut collect = |tl: &swf::Timeline, out: &mut Vec<swf::Code>| {
            for f in &tl.frames {
                for op in &f.ops {
                    if let swf::Op::Place { clip_actions, .. } = op {
                        for ca in clip_actions {
                            out.push(ca.actions.clone());
                        }
                    }
                }
            }
        };
        collect(&movie.root, &mut clip_event_blocks);
        for tl in movie.sprites.values() {
            collect(tl, &mut clip_event_blocks);
        }
        for code in &clip_event_blocks {
            account(&code.bytes, &mut stats, ops);
        }
        let _ = (&mut blocks, &mut instrs, &mut pools, &mut constants);
        let _ = (&mut set_member, &mut get_member, &mut calls, &mut funcs);
        println!("script blocks        {}", stats[0]);
        println!("instructions         {}", stats[1]);
        println!("constant pools       {}", stats[2]);
        println!("pool references      {}", stats[3]);
        println!("SetMember (props)    {}", stats[4]);
        println!("GetMember (props)    {}", stats[5]);
        println!("calls                {}", stats[6]);
        println!("function definitions {}", stats[7]);
        println!("named strings pushed  {}", stats[8]);
        return;
    }
    if args.len() >= 3 && args[1] == "--layers" {
        // Renders each top-level depth on its own so it is obvious which object
        // paints over the stage.
        let data = fs::read(&args[2]).expect("cannot read file");
        let seconds: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2.0);
        let movie = swf::Movie::parse(&data).expect("parse error");
        let fps = movie.frame_rate.max(1.0);
        let mut p = player::Player::new(movie);
        for _ in 0..(seconds * fps) as usize {
            p.tick(1.0 / fps);
        }
        let depths: Vec<u16> = p.list.keys().copied().collect();
        for d in depths {
            let before = p.canvas.puts;
            let lit = p.render_depth(d);
            let mid = ((p.canvas.h / 2) * p.canvas.w + p.canvas.w / 2) as usize;
            let centre = p.canvas.px[mid];
            let (char_id, name) = match p.list.get(&d) {
                Some(i) => (i.char_id, i.name.clone().unwrap_or_default()),
                None => (0, String::new()),
            };
            // A shape filled with the background colour paints over what is
            // below it yet leaves every pixel unchanged, so `lit` alone hides
            // exactly the overlays worth finding; count the draws as well.
            let puts = p.canvas.puts - before;
            if std::env::var("FLASH_DUMP_DEPTH").ok().and_then(|v| v.parse().ok()) == Some(d) {
                write_png(&p.canvas, &format!(".tmp/depth-{d}.png"));
            }
            if lit > 0 || puts > 0 || centre != 0xFF00_0000 {
                println!(
                    "depth {d:4} char {char_id:4} {name:6} lit {lit} puts {puts} centre 0x{centre:08x}"
                );
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--trace" {
        let data = fs::read(&args[2]).expect("cannot read file");
        let movie = swf::Movie::parse(&data).expect("parse error");
        let mut p = player::Player::new(movie);
        for _ in 0..4 {
            p.tick(1.0 / 12.0);
        }
        for line in &p.log {
            println!("{line}");
        }
        return;
    }
    if args.len() < 2 {
        eprintln!("usage: hostcheck <file.swf> [output-prefix]");
        std::process::exit(2);
    }
    let path = &args[1];
    let prefix = args.get(2).cloned().unwrap_or_else(|| "frame".to_string());
    let data = fs::read(path).expect("cannot read file");
    let movie = match swf::Movie::parse(&data) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("parse error: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "stage {}x{} @ {:.1}fps, {} frames, {} shapes, {} bitmaps, {} sprites",
        movie.width,
        movie.height,
        movie.frame_rate,
        movie.root.frames.len(),
        movie.shapes.len(),
        movie.bitmaps.len(),
        movie.sprites.len()
    );
    println!("buttons: {}", movie.buttons.len());
    for (id, b) in &movie.buttons {
        println!(
            "  button {id}: {} records, {} conditions {:?}",
            b.records.len(),
            b.conds.len(),
            b.conds
                .iter()
                .map(|c| (c.conditions, c.key, c.actions.bytes.len()))
                .collect::<Vec<_>>()
        );
    }
    for w in &movie.warnings {
        println!("warning: {w}");
    }
    for (id, s) in &movie.shapes {
        println!(
            "shape {id}: {} fills, {} strokes",
            s.fills.len(),
            s.strokes.len()
        );
        for f in &s.fills {
            println!(
                "  fill {:?} contours {:?}",
                f.paint,
                f.contours.iter().map(|c| c.len()).collect::<Vec<_>>()
            );
        }
    }
    for (i, f) in movie.root.frames.iter().enumerate() {
        for op in &f.ops {
            println!("frame {i}: {op:?}");
        }
    }
    let total = movie.root.frames.len().max(1);
    let mut p = player::Player::new(movie);
    for f in 0..total {
        p.goto_frame(f);
        p.render();
        let out = format!("{prefix}-{f:03}.png");
        write_png(&p.canvas, &out);
        println!("wrote {out} ({} draw objects)", p.instance_count());
    }
}

fn char_kind(m: &swf::Movie, id: u16) -> &'static str {
    if m.shapes.contains_key(&id) {
        "shape"
    } else if m.bitmaps.contains_key(&id) {
        "bitmap"
    } else if m.texts.contains_key(&id) {
        "text"
    } else if m.edit_texts.contains_key(&id) {
        "edittext"
    } else if m.buttons.contains_key(&id) {
        "button"
    } else if m.sprites.contains_key(&id) {
        "sprite"
    } else {
        "?"
    }
}

fn dump_list(p: &player::Player, list: &std::collections::BTreeMap<u16, player::Instance>, level: usize, max: usize) {
    if level >= max {
        return;
    }
    for (d, i) in list.iter() {
        let fr = match i.sprite.as_ref() {
            Some(s) => format!(
                " f={}/{} play={}",
                s.frame,
                p.movie
                    .sprites
                    .get(&s.timeline)
                    .map(|t| t.frames.len())
                    .unwrap_or(0),
                s.playing
            ),
            None => String::new(),
        };
        let rot = i.matrix.rot_skew_1.atan2(i.matrix.scale_x).to_degrees();
        let sc = i.matrix.scale_x.hypot(i.matrix.rot_skew_1);
        let mask = match i.clip_depth {
            Some(cd) if i.is_mask => format!(" MASK->{cd}"),
            Some(cd) => format!(" masked->{cd}"),
            None => String::new(),
        };
        println!(
            "{:indent$}d{d} char {} ({}) name {:?} at ({:.0},{:.0}) rot={rot:.0} sc={sc:.2} vis={} a={:.2} n={}{fr}{mask}",
            "",
            i.char_id,
            char_kind(&p.movie, i.char_id),
            i.name,
            i.matrix.tx,
            i.matrix.ty,
            i.visible,
            i.cxform.mul[3],
            i.sprite.as_ref().map(|s| s.list.len()).unwrap_or(0),
            indent = level * 2
        );
        if let Some(sp) = i.sprite.as_ref() {
            dump_list(p, &sp.list, level + 1, max);
        }
    }
}

fn write_png(canvas: &gfx::Canvas, path: &str) {
    let mut pixels = Vec::with_capacity((canvas.w * canvas.h * 4) as usize);
    for c in &canvas.px {
        pixels.push(((c >> 16) & 0xFF) as u8);
        pixels.push(((c >> 8) & 0xFF) as u8);
        pixels.push((c & 0xFF) as u8);
        pixels.push(0xFF);
    }
    let file = fs::File::create(path).expect("cannot create png");
    let w = png::Encoder::new(std::io::BufWriter::new(file), canvas.w as u32, canvas.h as u32);
    let mut encoder = w;
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&pixels).expect("png data");
}

/// Exercises the AVM1 interpreter against the movies built by
/// `tools/gen_avm1.py` and returns a process exit code.
fn run_avm1(dir: &str) -> i32 {
    let load = |name: &str| -> swf::Movie {
        let data = fs::read(format!("{dir}/{name}")).unwrap_or_else(|e| panic!("{name}: {e}"));
        swf::Movie::parse(&data).unwrap_or_else(|e| panic!("{name}: {e}"))
    };

    let mut failures = 0;
    let mut check = |label: &str, ok: bool, detail: String| {
        if ok {
            println!("PASS {label}");
        } else {
            failures += 1;
            println!("FAIL {label}: {detail}");
        }
    };

    {
        let mut p = player::Player::new(load("stop.swf"));
        for _ in 0..12 {
            p.tick(1.0 / 12.0);
        }
        check(
            "stop() halts the playhead",
            !p.playing && p.frame == 0,
            format!("playing={} frame={}", p.playing, p.frame),
        );
    }

    {
        let mut p = player::Player::new(load("goto.swf"));
        let at_load = p.frame;
        p.tick(1.0 / 12.0);
        check(
            "gotoAndPlay(3) jumps and resumes",
            at_load == 2 && p.playing && p.frame == 3,
            format!("load={at_load} now={} playing={}", p.frame, p.playing),
        );
    }

    {
        let p = player::Player::new(load("vars.swf"));
        let y = p.number_var("y").unwrap_or(f64::NAN);
        check(
            "arithmetic, comparison and branch",
            (y - 32.0).abs() < 0.001 && p.frame == 2 && !p.playing,
            format!("y={y} frame={} playing={}", p.frame, p.playing),
        );
    }

    {
        // keys.swf polls all four arrows on a looping timeline, so each held
        // direction must move the box and releasing must stop it. The old movie
        // stopped itself with gotoAndStop, which is why a held key moved it
        // twice and then looked dead on hardware.
        let mut p = player::Player::new(load("keys.swf"));
        let (x0, y0) = (
            p.named_x("hero").unwrap_or(-1.0),
            p.named_y("hero").unwrap_or(-1.0),
        );

        p.set_key(39, true); // right
        for _ in 0..6 {
            p.tick(1.0 / 12.0);
        }
        p.set_key(39, false);
        let x_right = p.named_x("hero").unwrap_or(-1.0);
        for _ in 0..6 {
            p.tick(1.0 / 12.0);
        }
        let x_released = p.named_x("hero").unwrap_or(-1.0);

        p.set_key(37, true); // left
        for _ in 0..6 {
            p.tick(1.0 / 12.0);
        }
        p.set_key(37, false);
        let x_left = p.named_x("hero").unwrap_or(-1.0);

        p.set_key(40, true); // down
        for _ in 0..6 {
            p.tick(1.0 / 12.0);
        }
        p.set_key(40, false);
        let y_down = p.named_y("hero").unwrap_or(-1.0);

        p.set_key(38, true); // up
        for _ in 0..6 {
            p.tick(1.0 / 12.0);
        }
        p.set_key(38, false);
        let y_up = p.named_y("hero").unwrap_or(-1.0);

        check(
            "Key.isDown drives a clip property in all four directions",
            x0 == 50.0
                && y0 == 50.0
                && x_right > x0 + 30.0
                && (x_released - x_right).abs() < 0.01
                && x_left < x_released - 30.0
                && y_down > y0 + 30.0
                && y_up < y_down - 30.0,
            format!(
                "x0={x0} x_right={x_right} x_released={x_released} x_left={x_left} \
                 y0={y0} y_down={y_down} y_up={y_up} playing={} frame={} speed={:?}",
                p.playing,
                p.frame,
                p.number_var("speed")
            ),
        );
    }

    {
        let mut p = player::Player::new(load("button.swf"));
        p.touch_down(110.0, 70.0);
        let pressed = p.number_var("pressed").unwrap_or(-1.0);
        p.touch_up();
        let frame_after = p.frame;
        let stopped = !p.playing;
        p.set_key(32, true);
        let hits1 = p.number_var("space_hits").unwrap_or(-1.0);
        p.set_key(32, true); // a held key must not re-trigger
        let hits2 = p.number_var("space_hits").unwrap_or(-1.0);
        p.set_key(32, false);
        p.set_key(32, true);
        let hits3 = p.number_var("space_hits").unwrap_or(-1.0);
        check(
            "on(press), on(release) and on(keyPress)",
            pressed == 1.0
                && frame_after == 1
                && stopped
                && hits1 == 1.0
                && hits2 == 1.0
                && hits3 == 2.0,
            format!(
                "pressed={pressed} frame={frame_after} stopped={stopped} \
                 keyPress={hits1}/{hits2}/{hits3}"
            ),
        );
    }

    {
        let mut p = player::Player::new(load("clipevent.swf"));
        let load_fired = p.number_var("hero.loaded").unwrap_or(0.0);
        let stopped = !p.playing;
        for _ in 0..5 {
            p.tick(1.0 / 12.0);
        }
        let ticks = p.number_var("ticks").unwrap_or(-1.0);
        let scoped = p.number_var("hero.n").unwrap_or(-1.0);
        p.set_key(65, true);
        let down = p.number_var("hero.keydowns").unwrap_or(-1.0);
        p.set_key(32, true);
        let space = p.number_var("hero.spaces").unwrap_or(-1.0);
        p.set_key(65, false);
        let up = p.number_var("hero.keyups").unwrap_or(-1.0);
        check(
            "onClipEvent load/enterFrame/keyDown/keyUp/keyPress",
            load_fired == 1.0
                && stopped
                && ticks == 5.0
                && scoped == 5.0
                && down == 1.0
                && space == 1.0
                && up == 1.0,
            format!(
                "load={load_fired} stopped={stopped} ticks={ticks} scoped={scoped} \
                 keyDown={down} keyPress={space} keyUp={up}"
            ),
        );
    }

    {
        let p = player::Player::new(load("clipapi.swf"));
        let at = p.number_var("at_frame").unwrap_or(f64::NAN);
        let depth = p.number_var("depth_now").unwrap_or(f64::NAN);
        let next = p.number_var("next_depth").unwrap_or(f64::NAN);
        let swapped = p.number_var("depth_after").unwrap_or(f64::NAN);
        let inside = p.string_var("hit_inside").unwrap_or_default();
        let outside = p.string_var("hit_outside").unwrap_or_default();
        check(
            "MovieClip gotoAndStop/getDepth/getNextHighestDepth/swapDepths/hitTest",
            at == 3.0
                && depth == 1.0
                && next == 2.0
                && swapped == 5.0
                && inside == "true"
                && outside == "false",
            format!(
                "frame={at} depth={depth} next={next} swapped={swapped} \
                 hit={inside}/{outside}"
            ),
        );
    }

    {
        let p = player::Player::new(load("functions.swf"));
        let r = p.number_var("r").unwrap_or(f64::NAN);
        let f = p.number_var("f").unwrap_or(f64::NAN);
        let sum = p.number_var("sum").unwrap_or(f64::NAN);
        // diff(10, 3) is 7; a reversed argument order would give -7.
        let delta = p.number_var("delta").unwrap_or(f64::NAN);
        // A bare assignment inside a root-level function must reach the root.
        let marker = p.number_var("marker").unwrap_or(f64::NAN);
        check(
            "function parameters, return values and recursion",
            r == 42.0 && f == 120.0 && sum == 15.0 && delta == 7.0 && marker == 77.0,
            format!(
                "double(21)={r} fact(5)={f} add3(7,8)={sum} diff(10,3)={delta} \
                 marker={marker} log={:?}",
                p.log
            ),
        );
    }

    {
        let p = player::Player::new(load("functions.swf"));
        let loc = p.number_var("loc").unwrap_or(f64::NAN);
        let t = p.number_var("t").unwrap_or(f64::NAN);
        check(
            "function-local var does not leak to the clip",
            loc == 99.0 && t == 1.0,
            format!("local={loc} clip={t}"),
        );
    }

    {
        let p = player::Player::new(load("functions.swf"));
        let x0 = p.number_var("x0").unwrap_or(f64::NAN);
        let x = p.named_x("hero").unwrap_or(f32::NAN);
        check(
            "method call binds `this` (property and preloaded register)",
            x0 == 50.0 && x == 100.0,
            format!("before={x0} after={x}"),
        );
    }

    {
        let mut p = player::Player::new(load("fnevent.swf"));
        let stopped = !p.playing;
        for _ in 0..4 {
            p.tick(1.0 / 12.0);
        }
        let hits = p.number_var("hits").unwrap_or(-1.0);
        p.set_key(39, true);
        let steps = p.number_var("steps").unwrap_or(-1.0);
        let x = p.named_x("hero").unwrap_or(-1.0);
        check(
            "clip events call user functions after stop()",
            stopped && hits == 4.0 && steps == 5.0 && x == 55.0,
            format!("stopped={stopped} hits={hits} steps={steps} x={x}"),
        );
    }

    {
        let p = player::Player::new(load("objects.swf"));
        let n = |k: &str| p.number_var(k).unwrap_or(f64::NAN);
        let s = |k: &str| p.string_var(k).unwrap_or_else(|| "<none>".into());
        check(
            "objects, arrays, new, arguments and dynamic properties",
            n("sum1") == 3.0
                && n("sum2") == 2.0
                && n("n0") == 3.0
                && n("n1") == 4.0
                && n("last") == 4.0
                && s("joined") == "1-2-3"
                && n("first") == 1.0
                && n("second") == 9.0
                && n("idx") == 1.0
                && n("total") == 7.0
                && n("px") == 3.0
                && n("k") == 3.0
                && n("argsecond") == 8.0
                && n("applied") == 5.0
                && s("up") == "HELLO"
                && s("ch") == "e"
                && s("sub") == "ell"
                && n("slen") == 5.0
                && n("nkeys") == 2.0,
            format!(
                "sum1={} sum2={} len={}/{} pop={} join={:?} a0={} a1={} idx={} \
                 total={} px={} argc={} arg1={} apply={} up={:?} char={:?} sub={:?} slen={} \n                 keys={} log={:?}",
                n("sum1"),
                n("sum2"),
                n("n0"),
                n("n1"),
                n("last"),
                s("joined"),
                n("first"),
                n("second"),
                n("idx"),
                n("total"),
                n("px"),
                n("k"),
                n("argsecond"),
                n("applied"),
                s("up"),
                s("ch"),
                s("sub"),
                n("slen"),
                n("nkeys"),
                p.log
            ),
        );
    }

    if failures == 0 {
        println!("all AVM1 checks passed");
        0
    } else {
        println!("{failures} check(s) failed");
        1
    }
}
