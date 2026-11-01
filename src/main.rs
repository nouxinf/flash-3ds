//! flash3ds — a Flash (SWF) player for the Nintendo 3DS.
//!
//! The app owns the top screen for the movie and the bottom screen for
//! controls, and keeps everything in two software canvases that are blitted to
//! the LCD each frame.

mod avm1;
mod font8x8;
mod fs;
mod gfx;
mod jpeg;
mod player;
mod swf;
mod ui;

use std::time::Instant;

use ctru::prelude::*;
use ctru::services::gfx::{Flush, RawFrameBuffer, Screen, Swap};
use ctru::services::gspgpu::FramebufferFormat;

use fs::Sdmc;
use gfx::{Canvas, BOTTOM_H, BOTTOM_W, TOP_H, TOP_W};
use player::Player;
use swf::Movie;
use ui::{Browser, PlayerHud};

enum Mode {
    Browse,
    Play {
        player: Box<Player>,
        hud: PlayerHud,
        path: String,
    },
    Settings,
    Message {
        title: String,
        body: String,
        error: bool,
    },
}

struct App {
    sdmc: Sdmc,
    mode: Mode,
    browser: Browser,
    settings_sel: usize,
    show_stats: bool,
    last_frame: Instant,
    fps: f32,
    touch_active: bool,
    quit: bool,
}

fn main() {    let gfx = Gfx::new().expect("could not start the graphics service");
    let mut hid = Hid::new().expect("could not start the input service");
    let apt = Apt::new().expect("could not start the app service");
    let mut top = Canvas::new(TOP_W, TOP_H);
    let mut bottom = Canvas::new(BOTTOM_W, BOTTOM_H);

    let sdmc = match Sdmc::open() {
        Ok(s) => s,
        Err(e) => {
            ui::draw_message(&mut top, &mut bottom, "SD card error", &e, ui::ERR);
            present(&gfx, &top, &bottom);
            while apt.main_loop() {
                hid.scan_input();
                gfx.wait_for_vblank();
                if hid.keys_down().contains(KeyPad::START) {
                    break;
                }
            }
            return;
        }
    };

    let browser = Browser::new(&sdmc);
    let mut app = App {
        sdmc,
        mode: Mode::Browse,
        browser,
        settings_sel: 0,
        show_stats: true,
        last_frame: Instant::now(),
        fps: 0.0,
        touch_active: false,
        quit: false,
    };

    while apt.main_loop() {
        hid.scan_input();
        let now = Instant::now();
        let dt = now
            .duration_since(app.last_frame)
            .as_secs_f32()
            .clamp(0.0, 0.1);
        app.last_frame = now;
        if dt > 0.0 {
            app.fps = app.fps * 0.9 + (1.0 / dt) * 0.1;
        }

        app.update(&hid, dt);
        if app.quit {
            break;
        }
        app.draw(&mut top, &mut bottom);
        present(&gfx, &top, &bottom);
        gfx.wait_for_vblank();
    }
}

fn present(gfx: &ctru::services::gfx::Gfx, top: &Canvas, bottom: &Canvas) {
    {
        let mut screen = gfx.top_screen.borrow_mut();
        let fmt = screen.framebuffer_format();
        let mut fb = screen.raw_framebuffer();
        blit_screen(top, &mut fb, fmt);
        screen.flush_buffers();
        screen.swap_buffers();
    }
    {
        let mut screen = gfx.bottom_screen.borrow_mut();
        let fmt = screen.framebuffer_format();
        let mut fb = screen.raw_framebuffer();
        blit_screen(bottom, &mut fb, fmt);
        screen.flush_buffers();
        screen.swap_buffers();
    }
}

/// Copies a canvas to a 3DS screen in the default Bgr8 layout. The framebuffer
/// is rotated (one memory row is one screen column) and stored bottom-up:
///
///   offset = (x * 240 + (239 - y)) * 3   ->   bytes B, G, R
///
/// This mapping comes from libctru's own examples; it is the one place where a
/// mistake would show up as a mirrored or sideways image on hardware.
fn blit_screen(canvas: &Canvas, fb: &mut RawFrameBuffer, format: FramebufferFormat) {
    if format != FramebufferFormat::Bgr8 {
        return;
    }
    let stride = fb.width as i32;
    for y in 0..canvas.h {
        let row = (y * canvas.w) as usize;
        for x in 0..canvas.w {
            let c = canvas.px[row + x as usize];
            let o = (x * stride + (239 - y)) * 3;
            unsafe {
                let p = fb.ptr.add(o as usize);
                *p = (c & 0xFF) as u8;
                *p.add(1) = ((c >> 8) & 0xFF) as u8;
                *p.add(2) = ((c >> 16) & 0xFF) as u8;
            }
        }
    }
}

/// Maps a point on the touchscreen to movie pixels. The movie is shown on the
/// top screen (400x240) while the touch panel is the bottom one (320x240), so
/// the touch panel acts as a proportional trackpad for the same image.
fn stage_point(player: &Player, sx: f32, sy: f32) -> (f32, f32) {
    let mw = player.canvas.w.max(1) as f32;
    let mh = player.canvas.h.max(1) as f32;
    let scale = (TOP_W as f32 / mw).min(TOP_H as f32 / mh);
    let dx = (TOP_W as f32 - mw * scale) * 0.5;
    let dy = (TOP_H as f32 - mh * scale) * 0.5;
    let tx = sx * TOP_W as f32 / BOTTOM_W as f32;
    let x = (tx - dx) / scale.max(0.0001);
    let y = (sy - dy) / scale.max(0.0001);
    (x.clamp(0.0, mw), y.clamp(0.0, mh))
}

impl App {
    fn update(&mut self, hid: &Hid, dt: f32) {
        let keys = hid.keys_down();

        // Match on a tag rather than on `self.mode` so each arm can still take
        // `&mut self` (the browse arm calls open_movie, which replaces the mode).
        let tag = match &self.mode {
            Mode::Browse => 0,
            Mode::Play { .. } => 1,
            Mode::Settings => 2,
            Mode::Message { .. } => 3,
        };

        match tag {
            0 => {
                if keys.contains(KeyPad::START) {
                    self.quit = true;
                    return;
                }
                if keys.intersects(KeyPad::DOWN) {
                    self.browser.move_cursor(1);
                }
                if keys.intersects(KeyPad::UP) {
                    self.browser.move_cursor(-1);
                }
                if keys.intersects(KeyPad::LEFT) {
                    self.browser.move_cursor(-8);
                }
                if keys.intersects(KeyPad::RIGHT) {
                    self.browser.move_cursor(8);
                }
                if keys.contains(KeyPad::X) {
                    self.browser.refresh(&self.sdmc);
                }
                if keys.contains(KeyPad::Y) {
                    self.browser.cwd = "/".into();
                    self.browser.refresh(&self.sdmc);
                }
                if keys.contains(KeyPad::B) {
                    self.browser.parent(&self.sdmc);
                }
                if keys.contains(KeyPad::SELECT) {
                    self.mode = Mode::Settings;
                    return;
                }
                if keys.contains(KeyPad::A) {
                    if let Some(path) = self.browser.enter(&self.sdmc) {
                        self.open_movie(&path);
                    }
                }
            }
            1 => self.update_play(hid, keys, dt),
            2 => {
                if keys.intersects(KeyPad::DOWN) {
                    self.settings_sel = (self.settings_sel + 1) % 5;
                }
                if keys.intersects(KeyPad::UP) {
                    self.settings_sel = (self.settings_sel + 4) % 5;
                }
                if keys.contains(KeyPad::A) && self.settings_sel == 0 {
                    self.show_stats = !self.show_stats;
                }
                if keys.contains(KeyPad::B) {
                    self.mode = Mode::Browse;
                }
            }
            _ => {
                if keys.contains(KeyPad::B) || keys.contains(KeyPad::A) {
                    self.mode = Mode::Browse;
                }
            }
        }
    }

    fn update_play(&mut self, hid: &Hid, keys: KeyPad, dt: f32) {
        if keys.contains(KeyPad::B) {
            if let Mode::Play { player, .. } = &mut self.mode {
                player.release_all_keys();
                player.touch_up();
            }
            self.touch_active = false;
            self.mode = Mode::Browse;
            return;
        }

        let overlay_open = match &self.mode {
            Mode::Play { hud, .. } => hud.overlay,
            _ => return,
        };

        if keys.contains(KeyPad::START) {
            if let Mode::Play { hud, .. } = &mut self.mode {
                hud.overlay = !hud.overlay;
                hud.overlay_sel = 0;
            }
            return;
        }

        if keys.contains(KeyPad::SELECT) {
            self.show_stats = !self.show_stats;
            if let Mode::Play { hud, .. } = &mut self.mode {
                hud.show_stats = self.show_stats;
            }
        }

        if overlay_open {
            let mut chosen: Option<usize> = None;
            if let Mode::Play { hud, .. } = &mut self.mode {
                if keys.intersects(KeyPad::DOWN) {
                    hud.overlay_sel = (hud.overlay_sel + 1) % ui::OVERLAY_ITEMS.len();
                }
                if keys.intersects(KeyPad::UP) {
                    hud.overlay_sel = (hud.overlay_sel + ui::OVERLAY_ITEMS.len() - 1)
                        % ui::OVERLAY_ITEMS.len();
                }
                if keys.contains(KeyPad::A) {
                    chosen = Some(hud.overlay_sel);
                    hud.overlay = false;
                }
            }
            match chosen {
                Some(1) => {
                    if let Mode::Play { player, hud, .. } = &mut self.mode {
                        player.playing = !player.playing;
                        hud.paused = !player.playing;
                    }
                }
                Some(2) => {
                    if let Mode::Play { player, hud, .. } = &mut self.mode {
                        player.goto_frame(0);
                        player.playing = true;
                        hud.paused = false;
                    }
                }
                Some(3) => {
                    self.show_stats = !self.show_stats;
                    if let Mode::Play { hud, .. } = &mut self.mode {
                        hud.show_stats = self.show_stats;
                    }
                }
                Some(4) => {
                    self.mode = Mode::Browse;
                }
                _ => {}
            }
            return;
        }

        if let Mode::Play {
            player,
            hud,
            path,
        } = &mut self.mode
        {
            // Feed the movie its input first: unless the user paused it, the
            // D-pad is the game's arrow keys and the touchscreen is its mouse.
            // A script's own stop() does not count as a pause, so a title screen
            // that stopped the timeline can still be driven by input.
            let held = hid.keys_held();
            let paused = hud.paused;
            for (pad, code) in [
                (KeyPad::LEFT, 37u32),
                (KeyPad::UP, 38),
                (KeyPad::RIGHT, 39),
                (KeyPad::DOWN, 40),
            ] {
                // KeyPad::LEFT/UP/RIGHT/DOWN are composed of the D-pad bit and
                // the circle-pad bit, and `contains` requires every bit of its
                // argument: a plain D-pad press would never match. `intersects`
                // is the test that means "either source is pressed".
                player.set_key(code, !paused && held.intersects(pad));
            }
            player.set_key(32, held.contains(KeyPad::Y));

            if held.contains(KeyPad::TOUCH) {
                let (sx, sy) = hid.touch_position();
                let (px, py) = stage_point(player, sx as f32, sy as f32);
                if self.touch_active {
                    player.touch_move(px, py);
                } else {
                    player.touch_down(px, py);
                    self.touch_active = true;
                }
            } else if self.touch_active {
                player.touch_up();
                self.touch_active = false;
            }

            if keys.contains(KeyPad::A) {
                player.playing = !player.playing;
                hud.paused = !player.playing;
            }
            if keys.contains(KeyPad::X) {
                player.goto_frame(0);
                player.playing = true;
                hud.paused = false;
            }
            // Stepping is a paused-only control. While the movie plays the D-pad
            // belongs to the movie, so a game that moves with the arrow keys keeps
            // getting them instead of being paused by the first press.
            if hud.paused {
                if keys.intersects(KeyPad::RIGHT) {
                    let next = (player.frame + 1) % player.frame_count();
                    player.goto_frame(next);
                }
                if keys.intersects(KeyPad::LEFT) {
                    let prev = (player.frame + player.frame_count() - 1) % player.frame_count();
                    player.goto_frame(prev);
                }
            }
            player.tick(dt);
            player.render();
            hud.frame = player.frame;
            hud.frames = player.frame_count();
            hud.fps = self.fps;
            hud.show_stats = self.show_stats;
            let _ = path;
        }
    }

    fn draw(&mut self, top: &mut Canvas, bottom: &mut Canvas) {
        match &self.mode {
            Mode::Browse => ui::draw_browser(top, bottom, &self.browser),
            Mode::Play { player, hud, .. } => ui::draw_player(top, bottom, player, hud),
            Mode::Settings => ui::draw_settings(top, bottom, self.settings_sel, self.show_stats),
            Mode::Message { title, body, error } => {
                let color = if *error { ui::ERR } else { ui::ACCENT };
                ui::draw_message(top, bottom, title, body, color);
            }
        }
    }

    fn open_movie(&mut self, path: &str) {
        let data = match self.sdmc.read_file(path) {
            Ok(d) => d,
            Err(e) => {
                self.mode = Mode::Message {
                    title: "Cannot read file".into(),
                    body: e,
                    error: true,
                };
                return;
            }
        };
        match Movie::parse(&data) {
            Ok(movie) => {
                let mut warnings = movie.warnings.clone();
                if movie.width == 0 || movie.height == 0 {
                    warnings.push("movie has an empty stage".into());
                }
                let hud = PlayerHud {
                    path: path.to_string(),
                    fps: 0.0,
                    overlay: false,
                    overlay_sel: 0,
                    paused: false,
                    frame: 0,
                    frames: movie.root.frames.len().max(1),
                    warnings,
                    show_stats: self.show_stats,
                };
                self.mode = Mode::Play {
                    player: Box::new(Player::new(movie)),
                    hud,
                    path: path.to_string(),
                };
            }
            Err(e) => {
                self.mode = Mode::Message {
                    title: "Not playable".into(),
                    body: format!("{path}\n\n{e}"),
                    error: true,
                };
            }
        }
    }
}
