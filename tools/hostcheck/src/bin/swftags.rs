//! Lists the root timeline's placements as Ruffle's `swf` crate reads them.
//!
//! The reference parser is the neutral third opinion when our own placement
//! decoding is in doubt: `swftags <movie.swf> [depth ...]`.

use swf::Tag;

fn main() {
    let path = std::env::args().nth(1).expect("usage: swftags <movie.swf> [depth]");
    let want: Vec<u16> = std::env::args()
        .skip(2)
        .filter_map(|a| a.parse().ok())
        .collect();
    let data = std::fs::read(&path).expect("cannot read file");
    let swf_buf = swf::decompress_swf(&data[..]).expect("cannot decompress");
    let movie = swf::parse_swf(&swf_buf).expect("cannot parse");
    // The root timeline is the movie's own tag list; DefineSprite bodies are
    // separate and only appear when a sprite id is asked for.
    {
        let root = &movie;
        let mut frame = 0u16;
        for tag in &root.tags {
            match tag {
                Tag::ShowFrame => frame += 1,
                Tag::PlaceObject(po) if want.is_empty() || want.contains(&po.depth) => {
                    println!(
                        "frame {frame:3} depth {:5} ratio {:?} action {:?}",
                        po.depth, po.ratio, po.action
                    );
                }
                _ => {}
            }
        }
        println!("root tags {}", movie.tags.len());
    }
}
