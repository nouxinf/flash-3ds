//! Prints one shape as Ruffle's `swf` crate reads it.
//!
//! `swfshape <movie.swf> <id>` - used to check a fill our own decoder reports as
//! empty (a gradient, usually) against the reference implementation.

use swf::Tag;

fn main() {
    let path = std::env::args().nth(1).expect("usage: swfshape <movie.swf> <id>");
    let id: u16 = std::env::args()
        .nth(2)
        .expect("missing id")
        .parse()
        .expect("bad id");
    let data = std::fs::read(&path).expect("cannot read file");
    let swf_buf = swf::decompress_swf(&data[..]).expect("cannot decompress");
    let movie = swf::parse_swf(&swf_buf).expect("cannot parse");
    for tag in movie.tags {
        if let Tag::DefineShape(s) = tag {
            if s.id != id {
                continue;
            }
            println!("version {}", s.version);
            println!("bounds {:?}", s.shape_bounds);
            println!("fills {:#?}", s.styles.fill_styles);
            println!("lines {}", s.styles.line_styles.len());
            println!("records {}", s.shape.len());
            return;
        }
    }
    println!("shape {id} not found");
}
