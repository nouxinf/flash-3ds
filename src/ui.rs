//! Screens: file browser, player chrome, settings and error display.

use crate::fs::{self, Entry, Sdmc};
use crate::gfx::{self, rgb, rgba, Canvas, Rgba, BOTTOM_H, BOTTOM_W, TOP_H, TOP_W};

pub const BG: Rgba = rgb(0x0E, 0x12, 0x17);
pub const PANEL: Rgba = rgb(0x16, 0x1C, 0x24);
pub const TEXT: Rgba = rgb(0xE6, 0xED, 0xF3);
pub const MUTED: Rgba = rgb(0x8B, 0x96, 0xA3);
pub const ACCENT: Rgba = rgb(0x4F, 0xD1, 0xC5);
pub const SEL: Rgba = rgb(0x1B, 0x4F, 0x57);
pub const WARN: Rgba = rgb(0xE3, 0xB3, 0x41);
pub const ERR: Rgba = rgb(0xF0, 0x6A, 0x6A);

pub struct Browser {
    pub cwd: String,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub scroll: usize,
    pub error: Option<String>,
    pub status: String,
}

impl Browser {
    pub fn new(sdmc: &Sdmc) -> Self {
        let mut b = Browser {
            // Prefer the conventional home for homebrew data, but never trap the
            // user there if the folder does not exist yet.
            cwd: "/3ds/flash3ds".into(),
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            error: None,
            status: String::new(),
        };
        b.refresh(sdmc);
        if b.error.is_some() {
            b.cwd = "/".into();
            b.refresh(sdmc);
        }
        b
    }

    pub fn refresh(&mut self, sdmc: &Sdmc) {
        match sdmc.list_dir(&self.cwd) {
            Ok(mut list) => {
                list.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                self.entries = list;
                self.error = None;
                if self.cursor >= self.entries.len() {
                    self.cursor = self.entries.len().saturating_sub(1);
                }
                self.status = format!("{} entries", self.entries.len());
            }
            Err(e) => {
                self.entries.clear();
                self.cursor = 0;
                self.error = Some(e);
            }
        }
        self.ensure_visible();
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    pub fn move_cursor(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as i32;
        let mut c = self.cursor as i32 + delta;
        if c < 0 {
            c = 0;
        }
        if c >= n {
            c = n - 1;
        }
        self.cursor = c as usize;
        self.ensure_visible();
    }

    pub fn ensure_visible(&mut self) {
        const ROWS: usize = 8;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + ROWS {
            self.scroll = self.cursor + 1 - ROWS;
        }
        let max = self.entries.len().saturating_sub(ROWS);
        if self.scroll > max {
            self.scroll = max;
        }
    }

    pub fn enter(&mut self, sdmc: &Sdmc) -> Option<String> {
        let e = self.selected()?.clone_entry();
        if e.is_dir {
            self.cwd = fs::join(&self.cwd, &e.name);
            self.cursor = 0;
            self.scroll = 0;
            self.refresh(sdmc);
            None
        } else if e.name.to_lowercase().ends_with(".swf") {
            Some(fs::join(&self.cwd, &e.name))
        } else {
            self.status = "Not a .swf file".into();
            None
        }
    }

    pub fn parent(&mut self, sdmc: &Sdmc) {
        let up = fs::parent(&self.cwd);
        if up != self.cwd {
            self.cwd = up;
            self.cursor = 0;
            self.scroll = 0;
            self.refresh(sdmc);
        }
    }
}

impl Entry {
    fn clone_entry(&self) -> Entry {
        Entry {
            name: self.name.clone(),
            is_dir: self.is_dir,
            size: self.size,
        }
    }
}

pub fn draw_browser(top: &mut Canvas, bottom: &mut Canvas, b: &Browser) {
    top.clear(BG);
    // Header.
    top.fill_rect(0, 0, TOP_W, 34, PANEL);
    top.draw_text(14, 13, "flash3ds", ACCENT, 2);
    let title_w = Canvas::text_width("flash3ds", 2);
    top.draw_text_clipped(
        14 + title_w + 12,
        17,
        &b.cwd,
        TOP_W - title_w - 40,
        MUTED,
        1,
    );

    // List.
    const ROW_H: i32 = 22;
    const TOP_Y: i32 = 44;
    let rows = ((TOP_H - TOP_Y - 26) / ROW_H) as usize;
    for i in 0..rows {
        let idx = b.scroll + i;
        if idx >= b.entries.len() {
            break;
        }
        let e = &b.entries[idx];
        let y = TOP_Y + i as i32 * ROW_H;
        if idx == b.cursor {
            top.fill_rect(8, y - 3, TOP_W - 16, ROW_H - 2, SEL);
            top.fill_rect(8, y - 3, 3, ROW_H - 2, ACCENT);
        }
        let label = if e.is_dir {
            format!("{}/", e.name)
        } else {
            e.name.clone()
        };
        let color = if e.is_dir { ACCENT } else { TEXT };
        top.draw_text_clipped(20, y, &label, TOP_W - 150, color, 1);
        let meta = if e.is_dir {
            "dir".to_string()
        } else {
            format!("{} KB", (e.size + 1023) / 1024)
        };
        top.draw_text(TOP_W - 20 - Canvas::text_width(&meta, 1), y, &meta, MUTED, 1);
    }

    if b.entries.is_empty() {
        top.draw_text(20, TOP_Y + 8, "Empty folder", MUTED, 1);
    }
    if let Some(err) = &b.error {
        top.draw_text_clipped(20, TOP_H - 40, err, TOP_W - 40, ERR, 1);
    }

    // Footer hints.
    top.fill_rect(0, TOP_H - 22, TOP_W, 22, PANEL);
    top.draw_text(14, TOP_H - 17, "A open   B back   X rescan   START quit", MUTED, 1);

    draw_browser_bottom(bottom, b);
}

fn draw_browser_bottom(bottom: &mut Canvas, b: &Browser) {
    bottom.clear(BG);
    bottom.fill_rect(0, 0, BOTTOM_W, 26, PANEL);
    bottom.draw_text(12, 9, "SELECTED", ACCENT, 1);

    match b.selected() {
        Some(e) => {
            let mut name = e.name.clone();
            if e.is_dir {
                name.push('/');
            }
            bottom.draw_text_clipped(12, 38, &name, BOTTOM_W - 24, TEXT, 2);
            let kind = if e.is_dir { "Folder" } else { "Flash movie" };
            bottom.draw_text(12, 60, kind, MUTED, 1);
            if !e.is_dir {
                bottom.draw_text(
                    12,
                    76,
                    &format!("{} bytes", e.size),
                    MUTED,
                    1,
                );
            }
        }
        None => {
            bottom.draw_text(12, 38, "Nothing here", MUTED, 1);
        }
    }

    bottom.fill_rect(0, BOTTOM_H - 92, BOTTOM_W, 92, PANEL);
    bottom.draw_text(12, BOTTOM_H - 82, "WHERE TO PUT GAMES", ACCENT, 1);
    bottom.draw_text(12, BOTTOM_H - 66, "sdmc:/3ds/flash3ds/", TEXT, 1);
    bottom.draw_text(12, BOTTOM_H - 52, "Any folder on the SD card works.", MUTED, 1);
    bottom.draw_text(12, BOTTOM_H - 38, b.status.as_str(), MUTED, 1);
    if let Some(err) = &b.error {
        bottom.draw_text_clipped(12, BOTTOM_H - 24, err, BOTTOM_W - 24, ERR, 1);
    }
}

pub struct PlayerHud {
    pub path: String,
    pub fps: f32,
    pub overlay: bool,
    pub overlay_sel: usize,
    pub paused: bool,
    pub frame: usize,
    pub frames: usize,
    pub warnings: Vec<String>,
    pub show_stats: bool,
}

pub const OVERLAY_ITEMS: [&str; 5] = ["Resume", "Pause / Play", "Restart", "FPS counter", "Exit to browser"];

pub fn draw_player(top: &mut Canvas, bottom: &mut Canvas, p: &crate::player::Player, hud: &PlayerHud) {
    top.clear(rgb(0, 0, 0));

    // Fit the movie into the top screen, preserving aspect ratio.
    let mw = p.canvas.w.max(1) as f32;
    let mh = p.canvas.h.max(1) as f32;
    let scale = (TOP_W as f32 / mw).min(TOP_H as f32 / mh);
    let dw = (mw * scale) as i32;
    let dh = (mh * scale) as i32;
    let dx = (TOP_W - dw) / 2;
    let dy = (TOP_H - dh) / 2;
    top.blit_scaled(&p.canvas, dx, dy, scale);

    if hud.show_stats {
        let stats = format!("{:.0} fps  f{}/{}", hud.fps, hud.frame + 1, hud.frames);
        top.fill_rect(6, 6, Canvas::text_width(&stats, 1) + 10, 16, rgba(0, 0, 0, 160));
        top.draw_text(11, 10, &stats, ACCENT, 1);
    }

    if hud.overlay {
        top.fill_rect(0, 0, TOP_W, TOP_H, rgba(0, 0, 0, 180));
        let w = 260;
        let h = 40 + OVERLAY_ITEMS.len() as i32 * 24;
        let x = (TOP_W - w) / 2;
        let y = (TOP_H - h) / 2;
        top.fill_rect(x, y, w, h, PANEL);
        top.rect_outline(x, y, w, h, 2, ACCENT);
        top.draw_text(x + 16, y + 14, "PAUSED", ACCENT, 1);
        for (i, item) in OVERLAY_ITEMS.iter().enumerate() {
            let iy = y + 40 + i as i32 * 24;
            if i == hud.overlay_sel {
                top.fill_rect(x + 8, iy - 4, w - 16, 22, SEL);
            }
            let label = if *item == "Pause / Play" && hud.paused {
                "Play"
            } else {
                item
            };
            top.draw_text(x + 18, iy, label, if i == hud.overlay_sel { TEXT } else { MUTED }, 1);
        }
    }

    // Bottom screen: controls and file info.
    bottom.clear(BG);
    bottom.fill_rect(0, 0, BOTTOM_W, 26, PANEL);
    bottom.draw_text(12, 9, "NOW PLAYING", ACCENT, 1);
    let name = hud
        .path
        .rsplit('/')
        .next()
        .unwrap_or(&hud.path)
        .to_string();
    bottom.draw_text_clipped(12, 36, &name, BOTTOM_W - 24, TEXT, 1);

    let info = format!(
        "{}x{}  {:.0} fps  frame {}/{}",
        p.movie.width,
        p.movie.height,
        p.movie.frame_rate,
        hud.frame + 1,
        hud.frames
    );
    bottom.draw_text(12, 52, &info, MUTED, 1);

    bottom.fill_rect(12, 72, BOTTOM_W - 24, 1, rgb(0x2A, 0x33, 0x3D));
    bottom.draw_text(12, 78, "START  menu", TEXT, 1);
    bottom.draw_text(12, 92, "A  play / pause", TEXT, 1);
    bottom.draw_text(12, 106, "B  back to browser", TEXT, 1);
    bottom.draw_text(12, 120, "D-PAD  arrow keys (paused: step)", TEXT, 1);
    bottom.draw_text(12, 134, "Y  spacebar", TEXT, 1);
    bottom.draw_text(12, 148, "TOUCH  mouse / buttons", TEXT, 1);
    bottom.draw_text(12, 162, "X  restart", TEXT, 1);
    bottom.draw_text(12, 176, "SELECT  toggle FPS", TEXT, 1);

    if !hud.warnings.is_empty() {
        let msg = &hud.warnings[0];
        bottom.fill_rect(0, BOTTOM_H - 34, BOTTOM_W, 34, PANEL);
        bottom.draw_text_clipped(10, BOTTOM_H - 24, &format!("Note: {msg}"), BOTTOM_W - 20, WARN, 1);
    }
}

pub fn draw_message(top: &mut Canvas, bottom: &mut Canvas, title: &str, body: &str, color: Rgba) {
    top.clear(BG);
    top.fill_rect(0, 0, TOP_W, 34, PANEL);
    top.draw_text(14, 13, title, color, 2);
    let mut y = 60;
    for line in wrap(body, 46) {
        top.draw_text(20, y, &line, TEXT, 1);
        y += 14;
        if y > TOP_H - 30 {
            break;
        }
    }
    top.fill_rect(0, TOP_H - 22, TOP_W, 22, PANEL);
    top.draw_text(14, TOP_H - 17, "B  back", MUTED, 1);
    bottom.clear(BG);
    bottom.draw_text(12, 12, "flash3ds", ACCENT, 2);
    bottom.draw_text(12, 40, "AS1/AS2 (SWF 4-9) playback", MUTED, 1);
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.len() + 1 + word.len() > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

pub fn draw_settings(top: &mut Canvas, bottom: &mut Canvas, sel: usize, show_stats: bool) {
    top.clear(BG);
    top.fill_rect(0, 0, TOP_W, 34, PANEL);
    top.draw_text(14, 13, "Settings", ACCENT, 2);

    let items = [
        ("FPS counter", if show_stats { "on" } else { "off" }),
        ("Scaling", "fit to screen"),
        ("Audio", "off (v1)"),
        ("Flash support", "AS1/AS2"),
        ("About", "flash3ds 0.1"),
    ];
    for (i, (k, v)) in items.iter().enumerate() {
        let y = 56 + i as i32 * 26;
        if i == sel {
            top.fill_rect(10, y - 4, TOP_W - 20, 24, SEL);
            top.fill_rect(10, y - 4, 3, 24, ACCENT);
        }
        top.draw_text(22, y, k, TEXT, 1);
        top.draw_text(TOP_W - 22 - Canvas::text_width(v, 1), y, v, MUTED, 1);
    }
    top.fill_rect(0, TOP_H - 22, TOP_W, 22, PANEL);
    top.draw_text(14, TOP_H - 17, "A toggle   B back", MUTED, 1);

    bottom.clear(BG);
    bottom.draw_text(12, 12, "About this build", ACCENT, 1);
    bottom.draw_text(12, 30, "v0.1 - software renderer", TEXT, 1);
    bottom.draw_text(12, 46, "No audio in this build.", MUTED, 1);
    bottom.draw_text(12, 62, "Video, filters and 3D", MUTED, 1);
    bottom.draw_text(12, 78, "content are unsupported.", MUTED, 1);
    let _ = (gfx::TOP_W, BOTTOM_W, BOTTOM_H);
}
