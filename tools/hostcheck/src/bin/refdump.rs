//! Decodes a movie's frame actions with Ruffle's `swf` crate.
//!
//! This exists only to cross-check our own decoder against an independent
//! implementation: `--timeline` shows what we think the bytecode says, and
//! `refdump` shows what Ruffle thinks. Any disagreement is our bug.

use swf::Tag;

fn main() {
    let path = std::env::args().nth(1).expect("usage: refdump <movie.swf> [frames]");
    let frames: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let data = std::fs::read(&path).expect("cannot read file");
    let swf_buf = swf::decompress_swf(&data[..]).expect("cannot decompress");
    let movie = swf::parse_swf(&swf_buf).expect("cannot parse");
    println!("version {}", movie.header.version());
    let mut frame = 0usize;
    for tag in movie.tags {
        match tag {
            Tag::ShowFrame => frame += 1,
            Tag::DoAction(code) => {
                if frame < frames {
                    println!("frame {frame}:");
                    for action in decode(code, movie.header.version()) {
                        println!("  {action:?}");
                    }
                }
            }
            Tag::DoInitAction { id, action_data } => {
                if frame < frames {
                    println!("frame {frame} sprite {id}:");
                    for a in decode(action_data, movie.header.version()) {
                        println!("  {a:?}");
                    }
                }
            }
            _ => {}
        }
        if frame >= frames {
            break;
        }
    }
}

/// Decodes a whole action block, stopping on the first malformed record.
fn decode<'a>(code: &'a [u8], version: u8) -> Vec<String> {
    let mut reader = swf::avm1::read::Reader::new(code, version);
    let mut out = Vec::new();
    loop {
        let before = code.len() - reader.get_mut().len();
        match reader.read_action() {
            Ok(a) => out.push(format!("{a:?}")),
            Err(e) => {
                out.push(format!("<error {e:?} at {before}>"));
                break;
            }
        }
        if reader.get_mut().is_empty() {
            break;
        }
    }
    out
}
