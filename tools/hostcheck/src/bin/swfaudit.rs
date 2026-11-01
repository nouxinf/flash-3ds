//! Audits a movie's ActionScript with Ruffle's `swf` crate.
//!
//! Answers "is the code really there?" independently of our own decoder:
//! `swfaudit <movie.swf>` walks every tag, decodes each script with the
//! reference AVM1 reader, and counts instructions, constant pools, named
//! strings, `GetMember`/`SetMember` and calls.

use std::collections::BTreeMap;
use swf::avm1::read::Reader;
use swf::avm1::types::{Action, Value};
use swf::Tag;

#[derive(Default)]
struct Stats {
    blocks: usize,
    instrs: usize,
    pools: usize,
    pool_entries: usize,
    pool_refs: usize,
    push_strs: usize,
    named_push: usize,
    get_member: usize,
    set_member: usize,
    call_fn: usize,
    call_method: usize,
    errors: usize,
    samples: Vec<Vec<u8>>,
    opcodes: BTreeMap<u8, usize>,
}

fn is_named(s: &[u8]) -> bool {
    s.iter().any(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn audit(code: &[u8], version: u8, st: &mut Stats) {
    st.blocks += 1;
    let mut r = Reader::new(code, version);
    while !r.get_ref().is_empty() {
        let op_byte = r.get_ref()[0];
        let act = match r.read_action() {
            Ok(a) => a,
            Err(_) => {
                st.errors += 1;
                break;
            }
        };
        *st.opcodes.entry(op_byte).or_insert(0) += 1;
        st.instrs += 1;
        match act {
            Action::Push(p) => {
                for v in p.values {
                    match v {
                        Value::Str(s) => {
                            st.push_strs += 1;
                            let b = s.as_bytes();
                            if is_named(b) {
                                st.named_push += 1;
                            }
                            if st.samples.len() < 24 && !st.samples.iter().any(|x: &Vec<u8>| x == b) {
                                st.samples.push(b.to_vec());
                            }
                        }
                        Value::ConstantPool(_) => st.pool_refs += 1,
                        _ => {}
                    }
                }
            }
            Action::ConstantPool(p) => {
                st.pools += 1;
                st.pool_entries += p.strings.len();
            }
            Action::GetMember => st.get_member += 1,
            Action::SetMember => st.set_member += 1,
            Action::CallFunction => st.call_fn += 1,
            Action::CallMethod => st.call_method += 1,
            _ => {}
        }
    }
}

fn walk(tags: &[Tag], version: u8, st: &mut Stats, depth: usize) {
    for tag in tags {
        match tag {
            Tag::DoAction(data) => audit(data, version, st),
            Tag::DoInitAction {
                action_data, ..
            } => audit(action_data, version, st),
            Tag::DefineButton2(btn) => {
                for a in &btn.actions {
                    audit(a.action_data, version, st);
                }
            }
            Tag::DefineSprite(sprite) => {
                if depth < 12 {
                    walk(&sprite.tags, version, st, depth + 1);
                }
            }
            Tag::PlaceObject(po) => {
                if let Some(recs) = &po.clip_actions {
                    for r in recs {
                        audit(r.action_data, version, st);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Independent tag histogram: 8-byte header, then `code<<6 | len`, with the
/// long form for `len == 0x3f`. A walk that desyncs shows up as absurd codes.
fn raw_tags(d: &[u8], out: &mut BTreeMap<u16, usize>) -> usize {
    let mut payloads = Vec::new();
    let n = raw_tags_ex(d, out, &mut payloads);
    if std::env::var("PAYLOADS").is_ok() {
        for (i, p) in payloads.iter().enumerate() {
            let mut st = Stats::default();
            audit(p, 8, &mut st);
            let names = st.samples.iter().filter(|s| is_named(s)).count();
            println!(
                "  payload {i:2} {:6} bytes: {} instrs, {} errors, {} named strings",
                p.len(), st.instrs, st.errors, names
            );
            if st.instrs > 0 {
                let mut ops: Vec<_> = st.opcodes.iter().collect();
                ops.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
                println!(
                    "      ops {}",
                    ops.iter()
                        .take(10)
                        .map(|(o, c)| format!("{o:02x}:{c}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                for s in st.samples.iter().filter(|s| is_named(s)).take(4) {
                    println!("      str {:?}", String::from_utf8_lossy(s));
                }
            }
        }
    }
    n
}

fn raw_tags_ex(d: &[u8], out: &mut BTreeMap<u16, usize>, payloads: &mut Vec<Vec<u8>>) -> usize {
    // `SwfBuf::data` starts at the first tag; a raw file starts with an
    // 8-byte header followed by the bit-packed RECT.
    let mut pos = if d.starts_with(b"FWS") || d.starts_with(b"CWS") || d.starts_with(b"ZWS") {
        let nbits = (d[8] >> 3) as usize;
        8 + (5 + 4 * nbits + 7) / 8 + 4
    } else {
        0
    };
    let mut count = 0;
    while pos + 2 <= d.len() {
        let h = u16::from_le_bytes([d[pos], d[pos + 1]]);
        pos += 2;
        let code = h >> 6;
        let mut len = (h & 0x3f) as usize;
        if len == 0x3f {
            if pos + 4 > d.len() {
                break;
            }
            len = u32::from_le_bytes([d[pos], d[pos + 1], d[pos + 2], d[pos + 3]]) as usize;
            pos += 4;
        }
        if pos + len > d.len() {
            break;
        }
        *out.entry(code).or_insert(0) += 1;
        if code == 253 {
            payloads.push(d[pos..pos + len].to_vec());
        }
        count += 1;
        pos += len;
        if code == 0 {
            break;
        }
    }
    count
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: swfaudit <movie.swf>");
    let data = std::fs::read(&path).expect("cannot read file");
    let buf = swf::decompress_swf(&data[..]).expect("cannot decompress");
    let mut codes = BTreeMap::new();
    let raw_count = raw_tags(&buf.data, &mut codes);
    let movie = swf::parse_swf(&buf).expect("cannot parse");
    if std::env::var("TAGS").is_ok() {
        let mut unknown: BTreeMap<u16, (usize, usize)> = BTreeMap::new();
        fn count(tags: &[Tag], unk: &mut BTreeMap<u16, (usize, usize)>) {
            for t in tags {
                match t {
                    Tag::Unknown { tag_code, data } => {
                        let e = unk.entry(*tag_code).or_insert((0, 0));
                        e.0 += 1;
                        e.1 += data.len();
                    }
                    Tag::DefineSprite(s) => count(&s.tags, unk),
                    _ => {}
                }
            }
        }
        count(&movie.tags, &mut unknown);
        println!("reference unknown tags: {unknown:?}");
        fn walk_names(tags: &[Tag], depth: usize) {
            let mut n = 0usize;
            for t in tags {
                n += 1;
                if let Tag::DefineSprite(s) = t {
                    if depth < 1 {
                        if s.id < 200 {
                            println!("  sprite {} frames {} tags {}", s.id, s.num_frames, s.tags.len());
                        }
                        walk_names(&s.tags, depth + 1);
                    }
                }
            }
            if depth == 0 {
                println!("  top-level tags {n}");
            }
        }
        walk_names(&movie.tags, 0);
    }
    let version = movie.header.version();
    let mut st = Stats::default();
    walk(&movie.tags, version, &mut st, 0);
    let stage = movie.header.stage_size();
    println!(
        "swf {path} v{version} {}x{} rate {} tags {raw_count}",
        stage.width().to_pixels() as i64,
        stage.height().to_pixels() as i64,
        f32::from(movie.header.frame_rate()),
    );
    println!(
        "scripts {} instrs {} pools {} ({} entries) pool_refs {} errors {}",
        st.blocks, st.instrs, st.pools, st.pool_entries, st.pool_refs, st.errors
    );
    println!(
        "push strings {} (named {}) get_member {} set_member {} call_fn {} call_method {}",
        st.push_strs, st.named_push, st.get_member, st.set_member, st.call_fn, st.call_method
    );
    let hist = codes
        .iter()
        .map(|(c, n)| format!("{c}:{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!("tag codes {hist}");
    for s in &st.samples {
        let hex = s.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join("");
        let asc = s
            .iter()
            .map(|b| if (32..127).contains(b) { *b as char } else { '.' })
            .collect::<String>();
        println!("  str len {:2} {hex}  {asc}", s.len());
    }
    let mut ops: Vec<_> = st.opcodes.iter().collect();
    ops.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    println!(
        "ops ({} kinds) {}",
        ops.len(),
        ops.iter()
            .map(|(o, n)| format!("{o:02x}:{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}
