//! Prints one button's records as Ruffle's `swf` crate reads them.
//!
//! Used to compare our decoder with the reference one when a button renders
//! differently in the two players: `swfbutton <movie.swf> <id>`.

use swf::Tag;

fn main() {
    let path = std::env::args().nth(1).expect("usage: swfbutton <movie.swf> <id>");
    let id: u16 = std::env::args()
        .nth(2)
        .expect("missing id")
        .parse()
        .expect("bad id");
    let data = std::fs::read(&path).expect("cannot read file");
    let swf_buf = swf::decompress_swf(&data[..]).expect("cannot decompress");
    let movie = swf::parse_swf(&swf_buf).expect("cannot parse");
    let mut seen = 0usize;
    for tag in movie.tags {
        if let Tag::DefineButton2(btn) = tag {
            seen += 1;
            if std::env::var("ALL").is_ok() {
                println!("button id {}", btn.id);
            }
            if btn.id != id {
                continue;
            }
            println!(
                "button {id} track_as_menu={} records {}",
                btn.is_track_as_menu,
                btn.records.len()
            );
            for rec in &btn.records {
                println!(
                    "  states 0x{:02x} char {} depth {} matrix {:?} cx {:?}",
                    rec.states.bits(),
                    rec.id,
                    rec.depth,
                    rec.matrix,
                    rec.color_transform
                );
            }
            // The action chain is what a click actually runs; printing it is how
            // "our decoder dropped the actions" is told apart from "the button
            // really has none and a script attaches its handlers".
            println!("  actions {}", btn.actions.len());
            for act in &btn.actions {
                println!(
                    "    conditions {:?} {} bytes",
                    act.conditions,
                    act.action_data.len()
                );
            }
        }
    }
    println!("definebutton tags: {seen}");
}
