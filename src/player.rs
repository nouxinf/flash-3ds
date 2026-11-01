//! Display list, rasterizer, timeline playback and AVM1 state.
//!
//! The movie is drawn into a stage-sized canvas (one canvas pixel is one SWF
//! pixel); the UI scales that canvas to the screen, so nothing here depends on
//! the LCD resolution.

use std::collections::{BTreeMap, HashMap};

use crate::avm1::{self, Action, Item, Value};
use crate::gfx::{rgba, Canvas, Rgba};
use crate::swf::{
    Bitmap, ClipEventAction, Code, CxForm, EditText, FillPath, Font, Mat, Movie, Op, Paint, Shape,
    StrokePath, Text, EV_ENTER_FRAME, EV_KEY_DOWN, EV_KEY_PRESS, EV_KEY_UP, EV_LOAD,
    EV_MOUSE_DOWN, EV_MOUSE_UP, EV_PRESS, EV_RELEASE,
};

/// One object on a display list. Sprites carry their own nested list and
/// playhead, so the same instance keeps animating independently of the root.
pub struct Instance {
    pub char_id: u16,
    pub depth: u16,
    pub name: Option<String>,
    pub matrix: Mat,
    pub cxform: CxForm,
    pub ratio: f32,
    pub visible: bool,
    pub sprite: Option<SpriteState>,
    /// `onClipEvent(...)` handlers attached to this placement.
    pub clip_actions: Vec<ClipEventAction>,
    /// Set when the placement declares a clip depth: Flash uses such an object
    /// as a mask for the depths above it, and masks are usually pure shapes.
    pub is_mask: bool,
    /// Last depth this object masks, when it is a mask (`PlaceObject2`'s clip
    /// depth). Everything between its own depth and this one is clipped to it.
    pub clip_depth: Option<u16>,
    /// Whether the `load` handler has already run for this instance.
    pub loaded: bool,
    /// Set once this instance's `DoInitAction` scripts have run.
    pub init_done: bool,
    /// Root frame that placed this instance, if the timeline did. Jumping back
    /// removes what later frames added, while objects the target frame also
    /// places keep their runtime state - which is why a looping animation does
    /// not reset `_x`. `None` means a script created it, so it stays.
    pub placed_frame: Option<usize>,
}

impl Instance {
    /// The name the movie gave this instance, if any.
    ///
    /// Anonymous clips carry a synthetic `@depth` name so their scope stays
    /// addressable from inside their own scripts, but that name is not a target
    /// path segment: `_root.@3` is not something a movie can resolve, so path
    /// building uses this instead.
    pub fn real_name(&self) -> Option<&str> {
        self.name.as_deref().filter(|n| !n.starts_with('@'))
    }

    /// The segment this instance answers to in a path.
    ///
    /// Flash generates a name for every clip it places, so even an unnamed
    /// button has an address and its handlers run with the right `_parent`. Our
    /// placeholders are named after their depth, which keeps the same property
    /// without inventing names the movie can collide with.
    pub fn path_seg(&self) -> String {
        match self.real_name() {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => format!("@{}", self.depth),
        }
    }

    /// Whether a path segment addresses this instance.
    pub fn answers_to(&self, seg: &str) -> bool {
        if self.name.as_deref() == Some(seg) {
            return true;
        }
        match seg.strip_prefix('@') {
            Some(d) => d.parse::<u16>().ok() == Some(self.depth),
            None => false,
        }
    }

    /// A detached copy of this instance and everything below it, for drawing
    /// part of the display list in isolation.
    pub fn copy(&self) -> Instance {
        let sprite = self.sprite.as_ref().map(|s| SpriteState {
            timeline: s.timeline,
            frame: s.frame,
            playing: s.playing,
            list: copy_list(&s.list),
        });
        Instance {
            char_id: self.char_id,
            depth: self.depth,
            name: self.name.clone(),
            matrix: self.matrix,
            cxform: self.cxform,
            ratio: self.ratio,
            visible: self.visible,
            sprite,
            clip_actions: Vec::new(),
            is_mask: self.is_mask,
            clip_depth: self.clip_depth,
            loaded: self.loaded,
            init_done: self.init_done,
            placed_frame: self.placed_frame,
        }
    }
}

fn copy_list(list: &BTreeMap<u16, Instance>) -> BTreeMap<u16, Instance> {
    list.iter().map(|(d, i)| (*d, i.copy())).collect()
}

/// One pending `setInterval`/`setTimeout` callback.
#[derive(Clone)]
pub struct Timer {
    id: u32,
    /// `time_ms` at which it next runs.
    due: f64,
    /// Repeat period for `setInterval`; `None` fires once.
    period: Option<f64>,
    /// The function (or function name) the movie asked to run.
    func: Value,
    args: Vec<Value>,
    /// Timeline that scheduled it: `this` inside the callback, as Flash does.
    path: String,
    canceled: bool,
}

pub struct SpriteState {
    pub timeline: u16,
    pub frame: usize,
    /// Whether this clip's own playhead advances. `clip.stop()` clears it, and
    /// games use that constantly to hold a nested animation still.
    pub playing: bool,
    pub list: BTreeMap<u16, Instance>,
}

impl SpriteState {
    /// An empty clip, as `createEmptyMovieClip` makes: a scratch container
    /// scripts can attach children into.
    pub fn empty() -> Self {
        SpriteState {
            timeline: 0,
            frame: 0,
            playing: true,
            list: BTreeMap::new(),
        }
    }
}

/// A touch drag: which object, where the finger started, and the object origin
/// at that moment, so movement is absolute rather than accumulated.
struct Drag {
    depth: u16,
    start: (f32, f32),
    base: (f32, f32),
}

pub struct Player {
    pub movie: Movie,
    pub canvas: Canvas,
    pub list: BTreeMap<u16, Instance>,
    pub frame: usize,
    pub playing: bool,
    pub speed: f32,
    pub dt: f32,
    elapsed: f64,

    // AVM1 state
    vars: HashMap<String, Value>,
    target_path: String,
    /// Constant pool of the action block being executed; function bodies
    /// inherit it, as Flash does.
    pool: Vec<String>,
    /// Every function defined so far; `Value::Function` indexes into this.
    functions: Vec<avm1::Function>,
    /// Prototype object of each function, created on demand for `new` and
    /// `F.prototype.x = ...`.
    fn_protos: Vec<Option<usize>>,
    /// Object heap; `Value::Object` indexes into this.
    objects: Vec<Object>,
    /// Slots the collector emptied, reused before the heap grows. A game runs
    /// for minutes and allocates arrays and objects the whole time, so without
    /// this the heap filled within seconds and every `new` returned undefined.
    free_objects: Vec<usize>,
    /// The receiver of the running method, when it is an object rather than a
    /// clip path.
    this_value: Value,
    /// Activation stack for user-defined function calls.
    frames: Vec<Frame>,
    /// Set by `return` so the running body stops unwinding outwards.
    unwinding: bool,
    /// Whether the current frame's place/remove tags have been applied.
    frame_applied: bool,
    keys: [bool; 256],
    last_key: u32,
    mouse: (f32, f32),
    mouse_down: bool,
    drag: Option<Drag>,
    /// Depth of the button currently held down, if any (for the down state).
    pressed: Option<u16>,
    /// Clip that received `onClipEvent(press)`, so `release` reaches it too.
    pressed_clip: Option<String>,
    /// Clip that received a dynamic `onPress`, whose `onRelease` belongs here.
    pressed_dyn: Option<String>,
    /// Clip the pointer is over, for `onRollOver`/`onRollOut`.
    hover: Option<String>,
    /// The held button's character and path, so release runs its handler.
    pressed_btn: Option<(u16, String)>,
    /// Symbol name to class, from `Object.registerClass`: an instance created
    /// from a registered symbol is an instance of that class, so its methods
    /// resolve through the class prototype.
    classes: HashMap<u16, ClassesEntry>,
    rng: u32,
    time_ms: f64,
    pub log: Vec<String>,
    /// Calls and opcodes the movie used that this runtime has no
    /// implementation for, and how often. It is how the host runner finds out
    /// what a game actually needs instead of guessing from the file.
    pub misses: HashMap<String, u32>,
    /// Blocks whose instruction budget ran out before they finished. A
    /// non-zero count means the engine, not the movie, cut a script short.
    pub truncated: u32,
    /// Total AVM1 actions executed. A game whose picture never changes but
    /// whose counter climbs is running; one with a near-zero count is not.
    pub steps: u64,
    /// Debug: dump every AVM1 instruction the root clip runs (FLASH_TRACE=1).
    pub trace: bool,
    /// Debug: trace one function body by its code offset (FLASH_FN=<offset>),
    /// which is how a class constructor that stops early is followed.
    pub trace_fn: Option<usize>,
    /// Keep the instruction history while an unimplemented call is being hunted.
    pub miss_trace: bool,
    /// Constant pool captured when each function was defined. AVM1 resolves
    /// `Constant(n)` against the pool of the code that *defined* the function,
    /// not whichever pool happens to be active when it is later called.
    pub fn_pools: Vec<Vec<String>>,
    /// Registers for code running outside any function body (frame actions and
    /// `#initclip` scripts), which AVM1 also gives registers.
    pub script_regs: Vec<Value>,
    /// Debug: keep the last few instructions around so a failed member write can
    /// say which expression produced its receiver.
    pub recent_ops: Vec<String>,
    /// `setInterval`/`setTimeout` callbacks waiting to run, oldest first. They
    /// are part of the clock, not of a clip: a preloader that polls for its
    /// data through a timer never continues without them.
    pub timers: Vec<Timer>,
    next_timer_id: u32,
    /// Last state printed by the `FLASH_WATCH` trace, so changes stand out.
    pub watch_last: Option<String>,
}

/// Recursion guard for nested clip timelines, not a layout limit.
///
/// Flash lets clips nest as deep as memory allows, and a game's decoration
/// clips sit five or six levels down; capping at a handful of levels silently
/// emptied every clip that deep (the intro's balloons lost all their parts).
/// Real content nests a dozen levels at most, so this only stops runaway or
/// circular trees.
const MAX_DEPTH: u32 = 32;

fn build_sprite(movie: &Movie, char_id: u16, depth: u32) -> Option<SpriteState> {
    let tl = movie.sprites.get(&char_id)?;
    let mut st = SpriteState {
        timeline: char_id,
        frame: 0,
        playing: true,
        list: BTreeMap::new(),
    };
    if depth <= MAX_DEPTH {
        if let Some(f) = tl.frames.first() {
            apply_ops(movie, &mut st.list, &f.ops, depth, 0);
        }
    }
    Some(st)
}

fn apply_ops(
    movie: &Movie,
    list: &mut BTreeMap<u16, Instance>,
    ops: &[Op],
    depth: u32,
    frame: usize,
) {
    // A frame that removes a depth and also places a character there describes
    // one object, not a removal followed by a new one: "frame 6 holds the intro
    // clip at depth 1" is written as RemoveObject2(1) then PlaceObject2(193).
    // Applying the removal first destroyed the clip every time the playhead was
    // sent back to that frame (a "hold here until done" loop, which is how the
    // Bloons intro waits), so its own animation never got past its first frames.
    let placed: Vec<u16> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Place { depth: d, .. } => Some(*d),
            Op::Remove { .. } => None,
        })
        .collect();
    let order: Vec<&Op> = ops
        .iter()
        .filter(|op| matches!(op, Op::Place { .. }))
        .chain(
            ops.iter()
                .filter(|op| matches!(op, Op::Remove { depth: d } if !placed.contains(d))),
        )
        .collect();
    for op in order {
        match op {
            Op::Place {
                depth: d,
                character,
                matrix,
                cxform,
                ratio,
                name,
                clip_depth,
                is_move,
                clip_actions,
            } => {
                if let Some(cid) = *character {
                    if *is_move {
                        if let Some(inst) = list.get_mut(d) {
                            if let Some(m) = matrix {
                                inst.matrix = *m;
                            }
                            if let Some(cx) = cxform {
                                inst.cxform = *cx;
                            }
                            if let Some(r) = ratio {
                                inst.ratio = *r;
                            }
                            if let Some(n) = name {
                                if !n.is_empty() {
                                    inst.name = Some(n.clone());
                                }
                            }
                            continue;
                        }
                    }
                    // Flash keeps the object that already occupies a depth: a
                    // clip re-placed by a frame it is already showing is the
                    // same clip, with its playhead and nested list intact.
                    // Rebuilding it instead restarted every animation whenever
                    // a script re-entered the current frame (gotoAndPlay to the
                    // frame it is on), which froze nested clips at frame 0.
                    if let Some(inst) = list.get_mut(d) {
                        if inst.char_id == cid {
                            if let Some(m) = matrix {
                                inst.matrix = *m;
                            }
                            if let Some(cx) = cxform {
                                inst.cxform = *cx;
                            }
                            if let Some(r) = ratio {
                                inst.ratio = *r;
                            }
                            if let Some(n) = name {
                                if !n.is_empty() {
                                    inst.name = Some(n.clone());
                                }
                            }
                            // The creation frame stays: a later frame that
                            // re-places the clip does not make it "new", and
                            // jumping back over that frame must keep it.
                            continue;
                        }
                    }
                    // An unnamed placement still owns a scope: a menu clip
                    // attached without a name runs its own timeline, and
                    // `bloonpop1.inner` inside it has to resolve there. A
                    // synthetic name keeps that scope addressable, which is
                    // what the collapse-to-parent path destroyed.
                    let inst_name = name
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or_else(|| Some(format!("@{d}")));
                    let inst = Instance {
                        char_id: cid,
                        depth: *d,
                        name: inst_name,
                        matrix: matrix.unwrap_or(Mat::IDENTITY),
                        cxform: cxform.unwrap_or(CxForm::IDENTITY),
                        ratio: ratio.unwrap_or(1.0),
                        visible: true,
                        sprite: build_sprite(movie, cid, depth + 1),
                        clip_actions: clip_actions.clone(),
                        is_mask: clip_depth.is_some(),
                        clip_depth: *clip_depth,
                        loaded: false,
                        init_done: false,
                        placed_frame: Some(frame),
                    };
                    list.insert(*d, inst);
                } else if let Some(inst) = list.get_mut(d) {
                    // A move record with no character updates the existing slot.
                    if let Some(m) = matrix {
                        inst.matrix = *m;
                    }
                    if let Some(cx) = cxform {
                        inst.cxform = *cx;
                    }
                    if let Some(cd) = clip_depth {
                        inst.clip_depth = Some(*cd);
                        inst.is_mask = true;
                    }
                }
            }
            Op::Remove { depth: d } => {
                list.remove(d);
            }
        }
    }
}

/// Steps one level of clip timelines, then recurses into their children.
fn advance_clips(
    list: &mut BTreeMap<u16, Instance>,
    movie: &Movie,
    prefix: &str,
    depth: u32,
    out: &mut Vec<(String, Code)>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for inst in list.values_mut() {
        let path = clip_path(prefix, inst);
        let Some(state) = inst.sprite.as_mut() else {
            continue;
        };
        if state.playing {
            if let Some(tl) = movie.sprites.get(&state.timeline) {
                let total = tl.frames.len().max(1);
                state.frame = (state.frame + 1) % total;
                let f = &tl.frames[state.frame];
                let ops = f.ops.clone();
                let sf = state.frame;
                apply_ops(movie, &mut state.list, &ops, depth + 1, sf);
                for code in &f.actions {
                    out.push((path.clone(), code.clone()));
                }
            }
        }
        advance_clips(&mut state.list, movie, &path, depth + 1, out);
    }
}

impl Player {
    pub fn new(movie: Movie) -> Player {
        // A zero-sized stage would make the canvas unpaintable and every fit
        // division by zero, so fall back to a sane default.
        let w = if movie.width == 0 { 550 } else { movie.width } as i32;
        let h = if movie.height == 0 { 400 } else { movie.height } as i32;
        let bg = movie.background;
        let mut p = Player {
            canvas: Canvas::new(w, h),
            frame: 0,
            playing: true,
            speed: 1.0,
            dt: 0.0,
            elapsed: 0.0,
            movie,
            list: BTreeMap::new(),
            vars: HashMap::new(),
            target_path: String::new(),
            pool: Vec::new(),
            functions: Vec::new(),
            fn_protos: Vec::new(),
            objects: Vec::new(),
            free_objects: Vec::new(),
            this_value: Value::Str("_root".into()),
            frames: Vec::new(),
            unwinding: false,
            frame_applied: false,
            keys: [false; 256],
            last_key: 0,
            mouse: (0.0, 0.0),
            mouse_down: false,
            drag: None,
            pressed: None,
            pressed_clip: None,
            pressed_dyn: None,
            hover: None,
            pressed_btn: None,
            classes: HashMap::new(),
            rng: 0x5EED_1234,
            time_ms: 0.0,
            log: Vec::new(),
            misses: HashMap::new(),
            truncated: 0,
            steps: 0,
            trace: std::env::var("FLASH_TRACE").is_ok(),
            trace_fn: std::env::var("FLASH_FN").ok().and_then(|v| v.parse().ok()),
            // A miss trace needs the instruction history even when no tracing
            // flag is on, because the miss is what is being investigated.
            miss_trace: std::env::var("FLASH_MISS").is_ok(),
            fn_pools: Vec::new(),
            script_regs: Vec::new(),
            recent_ops: Vec::new(),
            timers: Vec::new(),
            next_timer_id: 1,
            watch_last: None,
        };
        // `_url` and the sandbox type are what a protected movie's domain lock
        // reads, so a player with no browser still has to answer them. The
        // override exists for testing which answer a given movie wants.
        let url = std::env::var("FLASH_URL")
            .unwrap_or_else(|_| String::from("file:///bloons.swf"));
        let sandbox = std::env::var("FLASH_SANDBOX")
            .unwrap_or_else(|_| String::from("localWithFile"));
        p.vars.insert(String::from("_url"), Value::Str(url));
        p.vars.insert(
            String::from("System.security.sandboxType"),
            Value::Str(sandbox),
        );
        p.canvas.clear(bg);
        // Built-in classes a movie may use without defining them itself.
        p.install_builtins();
        // AS2 classes are registered by `#initclip` code, which Flash runs for
        // every exported symbol before the first frame.
        p.run_symbol_inits();
        p.enter_frame(0, 0);
        p
    }

    pub fn frame_count(&self) -> usize {
        self.movie.root.frames.len().max(1)
    }

    pub fn instance_count(&self) -> usize {
        self.list.len()
    }

    /// Runs `onClipEvent(event)` handlers across the display list. `key` is
    /// only consulted for key events.
    fn fire_clip_event(&mut self, event: u32, key: Option<u8>) {
        let mut pending: Vec<(String, Code)> = Vec::new();
        collect_clip_events(&self.list, "", event, key, &mut pending);
        self.run_scoped(pending);
    }

    /// Fires one clip event on the clip under a point (and its descendants).
    fn fire_clip_event_at(&mut self, x: f32, y: f32, event: u32) -> Option<String> {
        let (depth, _) = self.hit_test(x, y)?;
        let path = self
            .list
            .get(&depth)
            .map(|i| clip_path("", i))
            .unwrap_or_default();
        self.fire_clip_event_on(&path, event);
        Some(path)
    }

    /// Fires one clip event on a named clip's subtree.
    fn fire_clip_event_on(&mut self, path: &str, event: u32) {
        let mut pending: Vec<(String, Code)> = Vec::new();
        collect_clip_events(&self.list, "", event, None, &mut pending);
        let prefix = format!("{path}.");
        pending.retain(|(p, _)| path.is_empty() || p == path || p.starts_with(&prefix));
        self.run_scoped(pending);
    }

    /// Runs the `DoInitAction` of every exported symbol once at startup.
    ///
    /// That is how AS2 `#initclip` works: class definitions live in a symbol's
    /// init script, and the player executes them all when the movie loads, which
    /// is why `new BloonsTD()` resolves before anything places the symbol.
    /// Running init scripts only for instances left every class undefined.
    fn run_symbol_inits(&mut self) {
        // Flash runs `#initclip` scripts in the order their tags appear in the
        // file, not in symbol order. Bloons TD defines its class hierarchy in a
        // late symbol's script and registers the symbol classes from an earlier
        // one, so sorting by character id left every `Object.registerClass`
        // with an undefined constructor and its instances had no methods.
        let mut blocks: Vec<(usize, &Code)> = Vec::new();
        for codes in self.movie.init_actions.values() {
            for code in codes {
                blocks.push((code.off, code));
            }
        }
        blocks.sort_by_key(|(off, _)| *off);
        let pending: Vec<(String, Code)> = blocks
            .into_iter()
            .map(|(_, code)| (String::new(), code.clone()))
            .collect();
        self.run_scoped(pending);
    }

    /// Runs `DoInitAction` code for every instance that has just appeared.
    ///
    /// This is where a symbol's own script lives: games use it to wire up
    /// balloons, towers and panels the moment a clip exists.
    fn fire_pending_inits(&mut self) {
        let mut pending: Vec<(String, Code)> = Vec::new();
        collect_inits(
            &self.movie,
            &mut self.list,
            "",
            &mut pending,
        );
        self.run_scoped(pending);
    }

    /// Runs the `load` handler of every instance that has not seen it yet, then
    /// its class constructor for symbols registered with `registerClass`.
    fn fire_pending_loads(&mut self) {
        let mut pending: Vec<(String, Code)> = Vec::new();
        let mut fresh: Vec<String> = Vec::new();
        collect_unloaded(&self.list, "", &mut fresh);
        mark_loaded(&mut self.list, "", &mut pending);
        self.run_scoped(pending);
        for path in fresh {
            self.run_clip_handler(&path, "onLoad");
            self.run_constructor(&path);
        }
    }

    /// Executes handler streams with `this` bound to the clip's path, so
    /// variables written by a handler belong to that clip.
    fn run_scoped(&mut self, pending: Vec<(String, Code)>) {
        for (path, code) in pending {
            let saved = std::mem::replace(&mut self.target_path, path);
            self.exec_block(&code, 1);
            self.target_path = saved;
        }
    }

    /// Reads a root-level ActionScript variable; the host checks use this to
    /// assert on script effects rather than on pixels.
    pub fn number_var(&self, name: &str) -> Option<f64> {
        self.vars.get(name).map(|v| v.as_num())
    }

    /// Records one unsupported call or opcode.
    fn miss(&mut self, what: String) {
        // FLASH_MISS=<substring> shows where a repeated miss comes from: the
        // receiver being undefined says which expression the movie built, and
        // the last few instructions say how it built it.
        if let Ok(want) = std::env::var("FLASH_MISS") {
            if what.contains(&want) {
                eprintln!(
                    "MISS {what} in {:?} this {:?} frames {} last {:?}",
                    self.target_path,
                    self.this_value,
                    self.frames.len(),
                    self.recent_ops
                );
            }
        }
        *self.misses.entry(what).or_insert(0) += 1;
    }

    /// Number of root-level variables, for the host runner's diagnostics.
    pub fn variable_count(&self) -> usize {
        self.vars.len()
    }

    /// A variable read as a string, for the host checks.
    pub fn string_var(&self, name: &str) -> Option<String> {
        self.vars.get(name).map(|v| self.value_str(v))
    }

    /// Stage x of a named instance, for the same reason.
    pub fn named_x(&self, name: &str) -> Option<f32> {
        self.list
            .values()
            .find(|i| i.name.as_deref() == Some(name))
            .map(|i| i.matrix.tx)
    }

    /// Stage y of a named instance.
    pub fn named_y(&self, name: &str) -> Option<f32> {
        self.list
            .values()
            .find(|i| i.name.as_deref() == Some(name))
            .map(|i| i.matrix.ty)
    }

    pub fn restart(&mut self) {
        self.vars.clear();
        self.target_path.clear();
        self.pool.clear();
        self.functions.clear();
        self.fn_protos.clear();
        self.objects.clear();
        self.this_value = Value::Str("_root".into());
        self.frames.clear();
        self.unwinding = false;
        self.log.clear();
        self.misses.clear();
        self.drag = None;
        self.mouse_down = false;
        self.elapsed = 0.0;
        self.time_ms = 0.0;
        self.frame = 0;
        self.playing = true;
        self.frame_applied = false;
        self.list.clear();
        self.enter_frame(0, 0);
    }

    /// Jumps to a frame and runs its actions. Frame indexes wrap, as they do
    /// for gotoAndPlay past the last frame.
    pub fn goto_frame(&mut self, idx: usize) {
        let n = self.frame_count();
        self.enter_frame(idx % n, 0);
    }

    fn enter_frame(&mut self, idx: usize, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        let n = self.frame_count();
        let idx = idx % n;
        // Place and remove tags belong to a *frame change*: a script that sends
        // the playhead to the frame it is already on re-runs that frame's
        // actions, but Flash leaves the display list alone. Re-applying them
        // destroyed and recreated clips, so a "hold here until done" loop kept
        // restarting the very animation it was waiting for.
        if depth == 0 && std::env::var_os("FLASH_FRAMES").is_some() && idx != self.frame {
            eprintln!("ROOT frame {} -> {idx} at {:.0}ms", self.frame, self.time_ms);
        }
        let moved = idx != self.frame || !self.frame_applied;
        let (ops, actions) = {
            let f = &self.movie.root.frames[idx];
            (f.ops.clone(), f.actions.clone())
        };
        if moved {
            // Going backwards - including looping round to the first frame -
            // shows the list as it was built up to that frame. Leaving the
            // objects placed on the frames we are jumping back over is what
            // makes a looping preloader stack its later frames on top of the
            // first one.
            if idx <= self.frame {
                self.list
                    .retain(|_, i| i.placed_frame.map_or(true, |f| f <= idx));
            }
            apply_ops(&self.movie, &mut self.list, &ops, depth, idx);
            self.frame_applied = true;
        }
        self.frame = idx;
        for code in actions {
            self.exec_block(&code, depth + 1);
        }
        self.fire_pending_inits();
        self.fire_pending_loads();
    }

    /// Advances every sprite timeline in the display tree by one frame.
    ///
    /// Every `MovieClip` owns a playhead and they all move together, at every
    /// level of nesting. Advancing only the clips placed directly on the root
    /// left whole animations frozen: the intro's balloon clip kept its first
    /// frame forever, so the script waiting for it to finish never ran.
    fn step_sprites(&mut self, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        let mut actions: Vec<(String, Code)> = Vec::new();
        advance_clips(&mut self.list, &self.movie, "", depth, &mut actions);
        for (path, code) in actions {
            // Clip actions run with `this` bound to the clip, so their
            // variables are scoped to the clip's path.
            let saved = std::mem::replace(&mut self.target_path, path);
            self.exec_block(&code, depth + 1);
            self.target_path = saved;
        }
        self.fire_pending_inits();
        self.fire_pending_loads();
    }

    /// Called once per rendered frame with the real elapsed time.
    pub fn tick(&mut self, dt: f32) {
        self.dt = dt;
        self.time_ms += dt as f64 * 1000.0;
        let fps = if self.movie.frame_rate > 0.0 {
            self.movie.frame_rate
        } else {
            12.0
        };
        self.elapsed += dt as f64 * self.speed as f64;
        let step = 1.0 / fps as f64;
        let mut guard = 0;
        while self.elapsed >= step && guard < 4 {
            self.elapsed -= step;
            guard += 1;
            if self.playing {
                let next = self.frame + 1;
                self.enter_frame(next, 0);
            }
            // Clip events run on the timeline clock even when a script stopped
            // the root, which is how enterFrame-driven games animate.
            self.fire_clip_event(EV_ENTER_FRAME, None);
            // Nested clips animate regardless of the root playhead, which is
            // what classic games rely on for idle animations.
            self.step_sprites(0);
            // `clip.onEnterFrame = function () {...}` runs once per frame, after
            // the timelines have advanced.
            self.fire_dynamic_enter_frame();
        }
        // Timers run on the movie clock, so they keep firing while the playhead
        // is stopped waiting for whatever they are polling.
        self.run_timers();
        if self.elapsed > step * 4.0 {
            self.elapsed = 0.0;
        }
        // Between frames nothing is executing, so this is the one safe moment to
        // tell which objects the movie still holds. Collecting before the heap
        // is full keeps allocation from ever failing.
        if self.heap_pressure() > 0.75 {
            self.collect_garbage();
        }
        if let Ok(want) = std::env::var("FLASH_WATCH") {
            // Watches one top-level instance over time: whether it is visible,
            // where it sits and which frame it shows, printed when any of that
            // changes. A menu that hides itself and comes back is only visible
            // as a sequence of states, not in the final image.
            if let Some(inst) = self.list.values().find(|i| i.name.as_deref() == Some(want.as_str())) {
                let state = format!(
                    "vis={} a={:.2} at ({:.0},{:.0}) sprite={} frame={}",
                    inst.visible,
                    inst.cxform.mul[3],
                    inst.matrix.tx,
                    inst.matrix.ty,
                    inst.sprite.is_some(),
                    inst.sprite.as_ref().map(|s| s.frame).unwrap_or(0)
                );
                if self.watch_last.as_deref() != Some(state.as_str()) {
                    eprintln!("WATCH {want} {state} at {:.0}ms frame {}", self.time_ms, self.frame);
                    self.watch_last = Some(state);
                }
            } else if self.watch_last.as_deref() != Some("absent") {
                eprintln!("WATCH {want} absent at {:.0}ms frame {}", self.time_ms, self.frame);
                self.watch_last = Some("absent".to_string());
            }
        }
    }

    /// Every object on the heap with its properties, for the headless runner.
    /// Game controllers live here (`td`, `sound`), and their property names are
    /// the only readable part of an obfuscated movie.
    pub fn objects_dump(&self) -> Vec<(usize, Vec<(String, String)>)> {
        let mut out = Vec::new();
        for (i, o) in self.objects.iter().enumerate() {
            let mut props: Vec<(String, String)> = o
                .props
                .iter()
                .map(|(k, v)| {
                    // Functions are named by their code offset so FLASH_FN can
                    // trace one of them out of the dump.
                    let text = match v {
                        Value::Function(id) => self
                            .functions
                            .get(*id)
                            .map(|f| format!("[fn @{}]", f.code_start))
                            .unwrap_or_else(|| "[type Function]".to_string()),
                        other => self.value_str(other),
                    };
                    (k.clone(), text)
                })
                .collect();
            props.sort();
            out.push((i, props));
        }
        out
    }

    /// Every variable, sorted, for the headless runner.
    pub fn vars_dump(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), self.value_str(v)))
            .collect();
        out.sort();
        out
    }

    /// Renders the current display list into `canvas`.
    /// Renders a single top-level depth on its own. The headless inspector uses
    /// this to find which instance covers what.
    pub fn render_depth(&mut self, depth: u16) -> usize {
        self.canvas.clear(self.movie.background);
        let texts = self.texts_map();
        // Disjoint field borrows: the display list is read while the canvas is
        // written, which a `&mut self` helper could not do.
        let list = &self.list;
        let pressed = self.pressed == Some(depth);
        let mask = list
            .values()
            .filter_map(|i| i.clip_depth.map(|cd| (i.depth, cd)))
            .find(|(md, cd)| depth > *md && depth <= *cd)
            .and_then(|(md, _)| list.get(&md));
        let canvas = &mut self.canvas;
        if let Some(inst) = list.get(&depth) {
            match mask {
                Some(m) => {
                    canvas.clear(self.movie.background);
                    if let Some(b) = bounds_of(&self.movie, m, &Mat::IDENTITY) {
                        canvas.begin_capture(
                            b.0.floor() as i32 - 1,
                            b.1.floor() as i32 - 1,
                            b.2.ceil() as i32 + 1,
                            b.3.ceil() as i32 + 1,
                        );
                        draw_instance(canvas, &self.movie, m, &Mat::IDENTITY, [1.0; 4], false, &texts);
                        canvas.finish_capture();
                        canvas.set_masked(true);
                    }
                    draw_instance(canvas, &self.movie, inst, &Mat::IDENTITY, [1.0; 4], pressed, &texts);
                    canvas.set_masked(false);
                }
                None => draw_instance(canvas, &self.movie, inst, &Mat::IDENTITY, [1.0; 4], pressed, &texts),
            }
        }
        self.canvas.count_non_background(self.movie.background)
    }

    pub fn render(&mut self) {
        self.canvas.clear(self.movie.background);
        // Dynamic text reads its variable every frame, so `money = 120` shows
        // up on the next render without any extra plumbing.
        let texts = self.texts_map();
        let pressed = self.pressed;
        draw_list(
            &mut self.canvas,
            &self.movie,
            &self.list,
            &Mat::IDENTITY,
            [1.0; 4],
            pressed,
            &texts,
        );
    }

    /// Draws every top-level depth up to `limit`, in timeline order, which is
    /// what a diagnostic needs to see who paints over what.
    pub fn render_upto(&mut self, limit: u16) {
        self.canvas.clear(self.movie.background);
        let texts = self.texts_map();
        let depths: Vec<u16> = self.list.keys().copied().filter(|d| *d <= limit).collect();
        for d in depths {
            // The display list is borrowed while the canvas is written, so the
            // object being drawn is copied out of it first.
            let mut one = BTreeMap::new();
            if let Some(inst) = self.list.get(&d) {
                one.insert(d, inst.copy());
            }
            draw_list(
                &mut self.canvas,
                &self.movie,
                &one,
                &Mat::IDENTITY,
                [1.0; 4],
                self.pressed,
                &texts,
            );
        }
    }

    /// Debug: prints a variable and, when it holds an object, that object's own
    /// properties. A game controller whose constructor never ran shows up here
    /// as an object with nothing in it.
    pub fn inspect_var(&self, name: &str) -> String {
        let mut out = String::new();
        for (k, v) in self.vars.iter() {
            if !(k == name || k.ends_with(&format!(".{name}"))) {
                continue;
            }
            out.push_str(&format!("{k} = {}\n", self.value_str(v)));
            if let Value::Function(id) = v {
                if let Some(f) = self.functions.get(*id) {
                    let head: Vec<String> = f
                        .code
                        .iter()
                        .take(16)
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    out.push_str(&format!(
                        "    fn {:?} params {} code {} bytes @{} first [{}]\n",
                        f.name,
                        f.params.len(),
                        f.code.len(),
                        f.code_start,
                        head.join(" ")
                    ));
                    if let Some(Some(proto)) = self.fn_protos.get(*id) {
                        let names: Vec<String> = self
                            .objects
                            .get(*proto)
                            .map(|o| o.props.iter().map(|(k, _)| k.clone()).collect())
                            .unwrap_or_default();
                        out.push_str(&format!("    prototype object {proto}: {names:?}\n"));
                    } else {
                        out.push_str("    prototype: none created\n");
                    }
                }
            }
            if let Value::Object(i) = v {
                if let Some(o) = self.objects.get(*i) {
                    for (pk, pv) in &o.props {
                        out.push_str(&format!("    .{pk} = {}\n", self.value_str(pv)));
                    }
                    if let Some(items) = &o.items {
                        out.push_str(&format!("    [{} items]\n", items.len()));
                    }
                    if let Some(proto) = o.proto {
                        out.push_str(&format!("    proto -> object {proto}\n"));
                    }
                }
            }
        }
        out
    }

    /// The text an edit-text field currently displays: its variable's value if
    /// one is set, otherwise the author-time default.
    fn field_text(&self, et: &EditText) -> String {
        if et.var.is_empty() {
            return et.initial.clone();
        }
        match self.vars.get(&et.var) {
            Some(v) => self.value_str(v),
            None => et.initial.clone(),
        }
    }

    /// Text for every dynamic field, keyed by character id.
    ///
    /// A field can be filled two ways: through its bound variable (`text = value`
    /// in the authoring tool) or through the clip property (`money_txt.text =
    /// "650"`). Games use the second, so the value stored at `<path>.text` wins
    /// over the bound variable - otherwise every counter on screen reads empty.
    fn texts_map(&self) -> HashMap<u16, String> {
        let mut out: HashMap<u16, String> = self
            .movie
            .edit_texts
            .iter()
            .map(|(id, et)| (*id, self.field_text(et)))
            .collect();
        fn walk(
            p: &Player,
            list: &BTreeMap<u16, Instance>,
            prefix: &str,
            out: &mut HashMap<u16, String>,
        ) {
            for inst in list.values() {
                let path = clip_path(prefix, inst);
                if p.movie.edit_texts.contains_key(&inst.char_id) {
                    if let Some(v) = p.vars.get(&format!("{path}.text")) {
                        out.insert(inst.char_id, p.value_str(v));
                    }
                }
                if let Some(sp) = inst.sprite.as_ref() {
                    walk(p, &sp.list, &path, out);
                }
            }
        }
        walk(self, &self.list, "", &mut out);
        out
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// Feeds a Flash key code; `Key.isDown()` polls this, and buttons with an
    /// `on(keyPress)` handler are notified on the press edge.
    pub fn set_key(&mut self, code: u32, down: bool) {
        if code >= 256 {
            return;
        }
        let was_down = self.keys[code as usize];
        self.keys[code as usize] = down;
        if down == was_down {
            return;
        }
        if down {
            self.last_key = code;
            self.fire_clip_event(EV_KEY_DOWN, Some(code as u8));
            self.fire_clip_event(EV_KEY_PRESS, Some(code as u8));
        } else {
            self.fire_clip_event(EV_KEY_UP, Some(code as u8));
            return;
        }
        let handled: Vec<u16> = self
            .list
            .iter()
            .filter(|(_, inst)| {
                self.movie.buttons.get(&inst.char_id).is_some_and(|b| {
                    b.conds
                        .iter()
                        .any(|c| c.key != 0 && c.key as u32 == code)
                })
            })
            .map(|(d, _)| *d)
            .collect();
        for depth in handled {
            let Some(inst) = self.list.get(&depth) else {
                continue;
            };
            let char_id = inst.char_id;
            let path = inst.real_name().unwrap_or_default().to_string();
            let Some(btn) = self.movie.buttons.get(&char_id) else {
                continue;
            };
            let codes: Vec<Code> = btn
                .conds
                .iter()
                .filter(|c| c.key != 0 && c.key as u32 == code)
                .map(|c| c.actions.clone())
                .collect();
            let saved = std::mem::replace(&mut self.target_path, path);
            for bytes in codes {
                self.exec_block(&bytes, 1);
            }
            self.target_path = saved;
        }
    }

    pub fn release_all_keys(&mut self) {
        self.keys = [false; 256];
    }

    /// Stage-space mouse position, for UI point mapping.
    pub fn mouse_pos(&self) -> (f32, f32) {
        self.mouse
    }

    pub fn touch_down(&mut self, x: f32, y: f32) {
        // The pointer had to arrive at the point before it pressed, and games
        // recompute "can I build here?" on the move. A press that skips the move
        // keeps whatever the last position decided - which left every build
        // spot looking blocked.
        if self.mouse != (x, y) {
            self.touch_move(x, y);
        }
        self.mouse = (x, y);
        self.mouse_down = true;
        self.fire_clip_event(EV_MOUSE_DOWN, None);
        // A clip can be its own button: `onClipEvent(press)` on a full-stage
        // clip is how a title screen says "click to continue" without using a
        // DefineButton2 character.
        self.pressed_clip = self.fire_clip_event_at(x, y, EV_PRESS);
        // The usual AS2 wiring is a function stored on the clip
        // (`btn.onPress = Delegate.create(this, OnClick)`), which only a
        // dynamic dispatch reaches.
        // Remember what was under the pointer even when nothing handles the
        // press: a clip that only defines `onRelease` must still get it.
        let hit = self.press_target(x, y).or_else(|| self.clip_at(x, y));
        self.dispatch_on_path(hit.clone(), &["onPress", "onMouseDown"]);
        self.pressed_dyn = hit;
        let found = self.button_at(x, y);
        if std::env::var("FLASH_HIT").is_ok() {
            eprintln!("HIT touch_down ({x:.0},{y:.0}) button={found:?} clip={:?}", self.clip_at(x, y));
        }
        // A button with no actions is drawn art: it has nothing to run, so it
        // must not swallow the click. Panel artwork behind a game board is
        // built this way, and capturing there made the board unclickable.
        if let Some((char_id, path)) = found.filter(|(id, _)| self.button_has_actions(*id)) {
            self.pressed = path.split('.').last().and_then(|n| self.named_depth(n));
            self.pressed_btn = Some((char_id, path.clone()));
            self.fire_button(char_id, path, false);
            return;
        }
        if let Some((depth, base)) = self.hit_test(x, y) {
            self.drag = Some(Drag {
                depth,
                start: (x, y),
                base,
            });
        }
    }

    pub fn touch_move(&mut self, x: f32, y: f32) {
        self.mouse = (x, y);
        let was = self.drag.is_some();
        if let Some(drag) = self.drag.as_ref() {
            let dx = x - drag.start.0;
            let dy = y - drag.start.1;
            let (bx, by) = drag.base;
            let depth = drag.depth;
            if let Some(inst) = self.list.get_mut(&depth) {
                inst.matrix.tx = bx + dx;
                inst.matrix.ty = by + dy;
            }
        }
        if !was {
            // Rollover bookkeeping, then the move handler a tower preview
            // follows the pointer with.
            let hit = self.clip_at_bounds(x, y);
            if hit != self.hover {
                if let Some(old) = self.hover.take() {
                    self.run_clip_handler(&old, "onRollOut");
                }
                if let Some(new) = hit.clone() {
                    self.run_clip_handler(&new, "onRollOver");
                }
                self.hover = hit;
            }
            let hit = self.clip_at_bounds(x, y);
            self.dispatch_on_path(hit, &["onMouseMove"]);
        }
    }

    pub fn touch_up(&mut self) {
        self.mouse_down = false;
        self.drag = None;
        self.fire_clip_event(EV_MOUSE_UP, None);
        let (x, y) = self.mouse;
        if let Some(path) = self.pressed_clip.take() {
            self.fire_clip_event_on(&path, EV_RELEASE);
        }
        if let Some(path) = self.pressed_dyn.take() {
            let under = self.clip_at(x, y);
            // Re-rasterizing after a few frames of animation can land on a
            // sibling: a pressed card that is still swinging into place would
            // then get `onReleaseOutside` and never fire its `onRelease`.
            let inside = under
                .as_ref()
                .map(|p| *p == path || p.starts_with(&format!("{path}.")))
                .unwrap_or(false)
                || self.point_in_path(&path, x, y);
            if std::env::var_os("FLASH_HIT").is_some() {
                eprintln!("HIT touch_up pressed={path:?} under={under:?} inside={inside}");
            }
            // Release goes to the clip that was pressed, from the clip itself
            // outwards, so a panel's handler still sees a child's release.
            let name = if inside { "onRelease" } else { "onReleaseOutside" };
            self.dispatch_on_path(Some(path), &[name]);
            self.dispatch_on_path(under, &["onMouseUp"]);
        }
        self.pressed = None;
        if let Some((char_id, path)) = self.pressed_btn.take() {
            self.fire_button(char_id, path, true);
        }
    }

    /// Runs the first of `names` that the clip under the point (or one of its
    /// parents) defines, and returns the path that handled it.
    fn dispatch_on_path(&mut self, hit: Option<String>, names: &[&str]) -> Option<String> {
        let mut path = hit?;
        loop {
            for name in names {
                if self.run_clip_handler(&path, name) {
                    return Some(path);
                }
            }
            match path.rfind('.') {
                Some(i) => path.truncate(i),
                None => return None,
            }
        }
    }

    /// Topmost clip whose drawn pixels cover a stage point. Flash hit-tests the
    /// shapes a clip actually paints, so a full-stage transparent backdrop must
    /// not swallow a click on the button in front of it.
    fn clip_at(&mut self, x: f32, y: f32) -> Option<String> {
        let texts = self.texts_map();
        let mut prefix = Vec::new();
        clip_at_in(
            &mut self.canvas,
            &self.movie,
            &self.list,
            &mut prefix,
            &Mat::IDENTITY,
            x,
            y,
            &texts,
        )
    }

    /// Whether a stage point falls inside a clip's drawn bounds.
    ///
    /// Cheap and stable compared with re-rasterizing: release handling only
    /// needs to know the pointer never left the object that was pressed.
    fn point_in_path(&self, path: &str, x: f32, y: f32) -> bool {
        let parts: Vec<&str> = path.split('.').collect();
        match path_bounds(&self.movie, &self.list, &parts, &Mat::IDENTITY) {
            Some(b) => x >= b.0 && x <= b.2 && y >= b.1 && y <= b.3,
            None => false,
        }
    }

    /// Whether a clip (or one of its parents) reacts to a press.
    ///
    /// Flash only lets an *interactive* object swallow a click: a panel with no
    /// handler is transparent to the mouse even though it paints pixels. Hit
    /// tests that return the topmost painted clip therefore block whatever is
    /// behind a decorative overlay, which is how a level-screen panel stopped
    /// every click on the board underneath it.
    fn handles_press(&self, path: &str) -> bool {
        let mut p = path.to_string();
        loop {
            // A clip that only defines `onRelease` still captures the press:
            // Bloons wires its menu balloons that way, and excluding them sent
            // every click to the board behind the menu, which then did nothing.
            // Decorative art has no handlers at all, so it stays transparent.
            for name in ["onPress", "onMouseDown", "onRelease", "onReleaseOutside"] {
                if self.clip_handler(&p, name).is_some() {
                    return true;
                }
            }
            let parts: Vec<&str> = p.split('.').collect();
            if let Some(inst) = find_instance(&self.list, &parts) {
                if inst
                    .clip_actions
                    .iter()
                    .any(|ca| ca.events & (EV_PRESS | EV_RELEASE) != 0)
                {
                    return true;
                }
            }
            match p.rfind('.') {
                Some(i) => p.truncate(i),
                None => return false,
            }
        }
    }

    /// The clip a press belongs to: the topmost one under the point that is
    /// actually interactive.
    fn press_target(&mut self, x: f32, y: f32) -> Option<String> {
        let depths: Vec<u16> = self.list.keys().rev().copied().collect();
        let texts = self.texts_map();
        for d in depths {
            let mut one = BTreeMap::new();
            if let Some(inst) = self.list.get(&d) {
                one.insert(d, inst.copy());
            }
            let mut prefix = Vec::new();
            let hit = clip_at_in(
                &mut self.canvas,
                &self.movie,
                &one,
                &mut prefix,
                &Mat::IDENTITY,
                x,
                y,
                &texts,
            );
            if let Some(path) = hit {
                if self.handles_press(&path) {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Same order as `clip_at`, but by bounding box: cheap enough to run on
    /// every pointer move.
    fn clip_at_bounds(&self, x: f32, y: f32) -> Option<String> {
        let mut prefix = Vec::new();
        clip_at_bounds_in(
            &self.movie,
            &self.list,
            &mut prefix,
            &Mat::IDENTITY,
            x,
            y,
        )
    }

    /// Topmost button whose records contain the point.
    /// Which button sits under a point, for tests and tooling.
    pub fn button_at_point(&self, x: f32, y: f32) -> Option<(u16, String)> {
        self.button_at(x, y)
    }

    /// Hit-tests buttons, including ones nested inside menus and panels, which
    /// is where most of a game's clickable targets live.
    fn button_at(&self, x: f32, y: f32) -> Option<(u16, String)> {
        let mut prefix = Vec::new();
        button_in_list(&self.movie, &self.list, &Mat::IDENTITY, x, y, &mut prefix)
    }

    /// Whether a button character has any handler at all.
    fn button_has_actions(&self, char_id: u16) -> bool {
        self.movie
            .buttons
            .get(&char_id)
            .is_some_and(|b| b.conds.iter().any(|c| !c.actions.bytes.is_empty()))
    }

    /// Runs a button's `on(press)` / `on(release)` handlers.
    fn fire_button(&mut self, char_id: u16, path: String, release: bool) {
        let Some(btn) = self.movie.buttons.get(&char_id) else {
            return;
        };
        // CondId bit 2 is press (up->over-down), bit 3 is release.
        let bit: u16 = if release { 0x08 } else { 0x04 };
        let codes: Vec<Code> = btn
            .conds
            .iter()
            .filter(|c| c.conditions & bit != 0)
            .map(|c| c.actions.clone())
            .collect();
        // Button actions run in the scope of the clip that contains the button,
        // not the button itself: a button is not a clip, so its `this` is the
        // timeline it sits on. Writing to `this` from `on(press)` therefore has
        // to reach that parent (`pressed = 1` on the main timeline, not on a
        // placeholder named after the button's depth).
        let scope = match path.rfind('.') {
            Some(i) => path[..i].to_string(),
            None => String::new(),
        };
        let saved = std::mem::replace(&mut self.target_path, scope);
        for code in codes {
            self.exec_block(&code, 1);
        }
        self.target_path = saved;
    }

    /// Topmost named instance whose bounding box contains the point.
    fn hit_test(&self, x: f32, y: f32) -> Option<(u16, (f32, f32))> {
        let ids: Vec<u16> = self.list.keys().rev().copied().collect();
        for d in ids {
            let inst = self.list.get(&d)?;
            let Some(inv) = inst.matrix.invert() else {
                continue;
            };
            let (lx, ly) = inv.apply(x, y);
            if let Some(state) = inst.sprite.as_ref() {
                if let Some(sp) = self.movie.sprites.get(&state.timeline) {
                    // Approximate the clip extent by its shape bounds; a clip
                    // with no shapes gets a generous default box.
                    let mut inside = false;
                    for child in state.list.values() {
                        if let Some(shape) = self.movie.shapes.get(&child.char_id) {
                            if let Some(b) = shape_bounds(shape) {
                                let (cx, cy) = child.matrix.apply(
                                    (b.0 + b.2) * 0.5,
                                    (b.1 + b.3) * 0.5,
                                );
                                let hw = (b.2 - b.0) * 0.5 * child.matrix.scale_x.abs() + 4.0;
                                let hh = (b.3 - b.1) * 0.5 * child.matrix.scale_y.abs() + 4.0;
                                if (lx - cx).abs() <= hw && (ly - cy).abs() <= hh {
                                    inside = true;
                                    break;
                                }
                            }
                        }
                    }
                    let _ = sp;
                    if !inside {
                        continue;
                    }
                }
            }
            if inst.name.is_some() {
                return Some((d, (inst.matrix.tx, inst.matrix.ty)));
            }
        }
        None
    }

    pub fn next_rand(&mut self) -> f64 {
        self.rng = self.rng.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        ((self.rng >> 8) & 0x00FF_FFFF) as f64 / 16_777_216.0
    }
}

fn clip_path(prefix: &str, inst: &Instance) -> String {
    let seg = inst.path_seg();
    if prefix.is_empty() {
        seg
    } else {
        format!("{prefix}.{seg}")
    }
}

fn collect_clip_events(
    list: &BTreeMap<u16, Instance>,
    prefix: &str,
    event: u32,
    key: Option<u8>,
    out: &mut Vec<(String, Code)>,
) {
    for inst in list.values() {
        let path = clip_path(prefix, inst);
        for ca in &inst.clip_actions {
            let matches = if event == EV_KEY_PRESS {
                ca.events & EV_KEY_PRESS != 0 && key == Some(ca.key)
            } else {
                ca.events & event != 0
            };
            if matches {
                out.push((path.clone(), ca.actions.clone()));
            }
        }
        if let Some(sp) = inst.sprite.as_ref() {
            collect_clip_events(&sp.list, &path, event, key, out);
        }
    }
}

/// Collects `DoInitAction` blocks for instances that have just been created,
/// walking nested sprites the same way clip events are collected.
fn collect_inits(
    movie: &Movie,
    list: &mut BTreeMap<u16, Instance>,
    prefix: &str,
    out: &mut Vec<(String, Code)>,
) {
    for inst in list.values_mut() {
        let path = clip_path(prefix, inst);
        if !inst.init_done {
            inst.init_done = true;
            if let Some(blocks) = movie.init_actions.get(&inst.char_id) {
                for code in blocks {
                    out.push((path.clone(), code.clone()));
                }
            }
        }
        if let Some(sp) = inst.sprite.as_mut() {
            collect_inits(movie, &mut sp.list, &path, out);
        }
    }
}

fn mark_loaded(
    list: &mut BTreeMap<u16, Instance>,
    prefix: &str,
    out: &mut Vec<(String, Code)>,
) {
    for inst in list.values_mut() {
        let path = clip_path(prefix, inst);
        if !inst.loaded {
            inst.loaded = true;
            for ca in &inst.clip_actions {
                if ca.events & EV_LOAD != 0 {
                    out.push((path.clone(), ca.actions.clone()));
                }
            }
        }
        if let Some(sp) = inst.sprite.as_mut() {
            mark_loaded(&mut sp.list, &path, out);
        }
    }
}

/// Depth of a top-level instance by name, for the pressed-button highlight.
impl Player {
    fn named_depth(&self, name: &str) -> Option<u16> {
        self.list
            .iter()
            .find(|(_, i)| i.name.as_deref() == Some(name))
            .map(|(d, _)| *d)
    }
}

/// Depth-first, front-to-back search for a button under the pointer. The path
/// is rebuilt from instance names so the hit can be fired on the right clip.
fn button_in_list(
    movie: &Movie,
    list: &BTreeMap<u16, Instance>,
    parent: &Mat,
    x: f32,
    y: f32,
    prefix: &mut Vec<String>,
) -> Option<(u16, String)> {
    let ids: Vec<u16> = list.keys().rev().copied().collect();
    for d in ids {
        let Some(inst) = list.get(&d) else {
            continue;
        };
        let world = parent.mul(&inst.matrix);
        // An invisible object is not clickable, and neither is its subtree: a
        // hidden menu card must not keep answering for the screen behind it.
        if inst.is_mask || !inst.visible || inst.cxform.mul[3] <= 0.004 {
            continue;
        }
        if let Some(sp) = inst.sprite.as_ref() {
            prefix.push(inst.path_seg());
            if let Some(found) = button_in_list(movie, &sp.list, &world, x, y, prefix) {
                return Some(found);
            }
            prefix.pop();
            continue;
        }
        let Some(btn) = movie.buttons.get(&inst.char_id) else {
            continue;
        };
        let Some(inv) = world.invert() else {
            continue;
        };
        let (lx, ly) = inv.apply(x, y);
        for rec in &btn.records {
            // A button record can name a shape, a bitmap, a text block or even
            // a sprite, and the clickable area follows whichever it is: Bloons'
            // "Start Round" is a text-only button, which a shapes-only test
            // never hit no matter where on the label the player pressed.
            let Some((bx0, by0, bx1, by1)) = char_local_bounds(movie, rec.char_id) else {
                continue;
            };
            // Skew and rotation make the hull wider than two diagonal corners.
            let p: Vec<(f32, f32)> = [(bx0, by0), (bx1, by0), (bx1, by1), (bx0, by1)]
                .iter()
                .map(|&(x, y)| rec.matrix.apply(x, y))
                .collect();
            let x0 = p.iter().fold(f32::MAX, |a, q| a.min(q.0));
            let x1 = p.iter().fold(f32::MIN, |a, q| a.max(q.0));
            let y0 = p.iter().fold(f32::MAX, |a, q| a.min(q.1));
            let y1 = p.iter().fold(f32::MIN, |a, q| a.max(q.1));
            if lx >= x0 && lx <= x1 && ly >= y0 && ly <= y1 {
                let mut path = prefix.clone();
                path.push(inst.path_seg());
                return Some((inst.char_id, path.join(".")));
            }
        }
    }
    None
}

/// Front-to-back search for the topmost clip whose *pixels* cover a point.
///
/// Children paint over their parent, so they are searched first; each candidate
/// is rasterized through the normal drawing path into the coverage buffer, which
/// is the same answer the screen gave. Bounding boxes are not enough: a panel
/// with a transparent background must not swallow clicks meant for the button
/// in front of it.
fn clip_at_in(
    canvas: &mut Canvas,
    movie: &Movie,
    list: &BTreeMap<u16, Instance>,
    prefix: &mut Vec<String>,
    parent: &Mat,
    x: f32,
    y: f32,
    texts: &HashMap<u16, String>,
) -> Option<String> {
    let ids: Vec<u16> = list.keys().rev().copied().collect();
    for d in ids {
        let Some(inst) = list.get(&d) else {
            continue;
        };
        if !inst.visible || inst.is_mask {
            continue;
        }
        prefix.push(inst.path_seg());
        let world = parent.mul(&inst.matrix);
        if let Some(sp) = inst.sprite.as_ref() {
            if let Some(found) = clip_at_in(canvas, movie, &sp.list, prefix, &world, x, y, texts) {
                return Some(found);
            }
        }
        let mut hit = false;
        if let Some(b) = bounds_of(movie, inst, parent) {
            if x >= b.0 && x <= b.2 && y >= b.1 && y <= b.3 {
                canvas.begin_capture(
                    b.0.floor() as i32 - 1,
                    b.1.floor() as i32 - 1,
                    b.2.ceil() as i32 + 1,
                    b.3.ceil() as i32 + 1,
                );
                draw_instance(canvas, movie, inst, parent, [1.0; 4], false, texts);
                canvas.finish_capture();
                hit = canvas.covered(x as i32, y as i32);
            }
        }
        if hit {
            return Some(prefix.join("."));
        }
        prefix.pop();
    }
    None
}

/// Bounding-box version of `clip_at_in`, for pointer moves where rasterizing
/// every candidate would be wasteful.
fn clip_at_bounds_in(
    movie: &Movie,
    list: &BTreeMap<u16, Instance>,
    prefix: &mut Vec<String>,
    parent: &Mat,
    x: f32,
    y: f32,
) -> Option<String> {
    let ids: Vec<u16> = list.keys().rev().copied().collect();
    for d in ids {
        let Some(inst) = list.get(&d) else {
            continue;
        };
        if !inst.visible || inst.is_mask {
            continue;
        }
        prefix.push(inst.path_seg());
        if let Some(sp) = inst.sprite.as_ref() {
            let world = parent.mul(&inst.matrix);
            if let Some(found) = clip_at_bounds_in(movie, &sp.list, prefix, &world, x, y) {
                return Some(found);
            }
        }
        if let Some(b) = bounds_of(movie, inst, parent) {
            if x >= b.0 && x <= b.2 && y >= b.1 && y <= b.3 {
                return Some(prefix.join("."));
            }
        }
        prefix.pop();
    }
    None
}

fn draw_instance(
    canvas: &mut Canvas,
    movie: &Movie,
    inst: &Instance,
    parent: &Mat,
    mul: [f32; 4],
    pressed: bool,
    texts: &HashMap<u16, String>,
) {
    if !inst.visible || inst.is_mask {
        return;
    }
    let tm = [
        mul[0] * inst.cxform.mul[0],
        mul[1] * inst.cxform.mul[1],
        mul[2] * inst.cxform.mul[2],
        mul[3] * inst.cxform.mul[3],
    ];
    if tm[3] <= 0.004 {
        return;
    }
    let world = parent.mul(&inst.matrix);
    if let Some(shape) = movie.shapes.get(&inst.char_id) {
        canvas.draw_shape(shape, movie, &world, tm);
        return;
    }
    if let Some(text) = movie.texts.get(&inst.char_id) {
        draw_static_text(canvas, movie, text, &world, tm);
        return;
    }
    if let Some(et) = movie.edit_texts.get(&inst.char_id) {
        let empty = String::new();
        let value = texts.get(&inst.char_id).unwrap_or(&empty);
        draw_edit_text(canvas, movie, et, &world, tm, value);
        return;
    }
    if let Some(bm) = movie.bitmaps.get(&inst.char_id) {
        canvas.draw_bitmap(bm, &world, tm);
        return;
    }
    if let Some(btn) = movie.buttons.get(&inst.char_id) {
        for rec in &btn.records {
            // Up and over share the idle artwork; a held button adds its down
            // state, which is how pressed buttons usually look different.
            let visible = if pressed {
                rec.states & 0x07 != 0
            } else {
                rec.states & 0x03 != 0
            };
            if !visible {
                continue;
            }
            let rw = world.mul(&rec.matrix);
            let rm = [
                tm[0] * rec.cxform.mul[0],
                tm[1] * rec.cxform.mul[1],
                tm[2] * rec.cxform.mul[2],
                tm[3] * rec.cxform.mul[3],
            ];
            // Records name any character, not just shapes: buttons labelled
            // with `DefineText` (very common for menu labels) drew nothing when
            // only shapes and bitmaps were handled.
            draw_char(canvas, movie, rec.char_id, &rw, rm, texts);
        }
        return;
    }
    if let Some(state) = inst.sprite.as_ref() {
        draw_list(canvas, movie, &state.list, &world, tm, None, texts);
    }
}

/// Draws one character id in a given space.
///
/// A display list and a button record both name characters the same way - shape,
/// bitmap, static text, editable field or sprite - so the mapping lives here and
/// every caller draws the same set.
fn draw_char(
    canvas: &mut Canvas,
    movie: &Movie,
    char_id: u16,
    world: &Mat,
    mul: [f32; 4],
    texts: &HashMap<u16, String>,
) {
    if let Some(shape) = movie.shapes.get(&char_id) {
        canvas.draw_shape(shape, movie, world, mul);
    } else if let Some(bm) = movie.bitmaps.get(&char_id) {
        canvas.draw_bitmap(bm, world, mul);
    } else if let Some(text) = movie.texts.get(&char_id) {
        draw_static_text(canvas, movie, text, world, mul);
    } else if let Some(et) = movie.edit_texts.get(&char_id) {
        let empty = String::new();
        let value = texts.get(&char_id).unwrap_or(&empty);
        draw_edit_text(canvas, movie, et, world, mul, value);
    } else if let Some(st) = build_sprite(movie, char_id, 0) {
        // Button artwork is not a live clip: a state shows its sprite's first
        // frame for as long as the state is on screen.
        draw_list(canvas, movie, &st.list, world, mul, None, texts);
    }
}

/// Draws one display list in depth order, applying clip-depth masks.
///
/// A `PlaceObject2` with a clip depth turns the object *carrying* it into a
/// mask for every depth above it up to that clip depth, and the mask itself is
/// never shown. Bloons builds its whole logo this way: balloon clips clipped by
/// letter shapes, which is why an unclipped render showed a pile of balloons.
fn draw_list(
    canvas: &mut Canvas,
    movie: &Movie,
    list: &BTreeMap<u16, Instance>,
    parent: &Mat,
    mul: [f32; 4],
    pressed: Option<u16>,
    texts: &HashMap<u16, String>,
) {
    if list.is_empty() {
        return;
    }
    // A mask drawn inside a mask would clobber the coverage being collected.
    let masks: Vec<(u16, u16)> = if canvas.is_capturing() {
        Vec::new()
    } else {
        list.values()
            .filter_map(|i| i.clip_depth.map(|cd| (i.depth, cd)))
            .collect()
    };
    for (depth, inst) in list.iter() {
        // Diagnostic: hide instances by name fragment to see what is under
        // them (FLASH_HIDE=pathhit).
        if let Some(pat) = std::env::var_os("FLASH_HIDE") {
            let pat = pat.to_string_lossy();
            if inst
                .name
                .as_deref()
                .is_some_and(|n| n.contains(pat.as_ref()))
            {
                continue;
            }
        }
        let is_pressed = pressed == Some(*depth);
        let mask = if masks.is_empty() {
            None
        } else {
            masks
                .iter()
                .find(|(md, cd)| *depth > *md && *depth <= *cd)
                .and_then(|(md, _)| list.get(md))
        };
        match mask {
            Some(mask) => {
                let Some(b) = bounds_of(movie, mask, parent) else {
                    draw_instance(canvas, movie, inst, parent, mul, is_pressed, texts);
                    continue;
                };
                canvas.begin_capture(
                    b.0.floor() as i32 - 1,
                    b.1.floor() as i32 - 1,
                    b.2.ceil() as i32 + 1,
                    b.3.ceil() as i32 + 1,
                );
                draw_instance(canvas, movie, mask, parent, mul, false, texts);
                canvas.finish_capture();
                canvas.set_masked(true);
                draw_instance(canvas, movie, inst, parent, mul, is_pressed, texts);
                canvas.set_masked(false);
            }
            None => draw_instance(canvas, movie, inst, parent, mul, is_pressed, texts),
        }
    }
}



/// Local-space extent of any character a display list can name.
///
/// Flash lets a `PlaceObject` and a button record reference a shape, a bitmap,
/// a static text block, an editable field or a sprite, so anything that measures
/// artwork has to know all of them. Text has no rect of its own in `DefineText`,
/// so its extent comes from the glyphs it lays out - the same advance walk the
/// renderer does.
pub fn char_local_bounds(movie: &Movie, char_id: u16) -> Option<(f32, f32, f32, f32)> {
    if let Some(shape) = movie.shapes.get(&char_id) {
        return shape_bounds(shape);
    }
    if let Some(bm) = movie.bitmaps.get(&char_id) {
        return Some((0.0, 0.0, bm.w as f32, bm.h as f32));
    }
    if let Some(et) = movie.edit_texts.get(&char_id) {
        return Some(et.bounds);
    }
    if let Some(text) = movie.texts.get(&char_id) {
        return text_bounds(movie, text);
    }
    None
}

fn text_bounds(movie: &Movie, text: &Text) -> Option<(f32, f32, f32, f32)> {
    let mut b: Option<(f32, f32, f32, f32)> = None;
    for run in &text.runs {
        let Some(font) = movie.fonts.get(&run.font) else {
            continue;
        };
        let scale = run.height / font.units_per_em.max(1.0);
        if scale <= 0.0 {
            continue;
        }
        let mut pen = run.x;
        for &(idx, adv) in &run.glyphs {
            if let Some(g) = font.glyphs.get(idx as usize).and_then(shape_bounds) {
                let m = Mat {
                    scale_x: scale,
                    scale_y: scale,
                    rot_skew_0: 0.0,
                    rot_skew_1: 0.0,
                    tx: pen,
                    ty: run.y,
                };
                for (x, y) in [(g.0, g.1), (g.2, g.1), (g.2, g.3), (g.0, g.3)] {
                    let (wx, wy) = m.apply(x, y);
                    b = Some(match b {
                        None => (wx, wy, wx, wy),
                        Some(o) => (o.0.min(wx), o.1.min(wy), o.2.max(wx), o.3.max(wy)),
                    });
                }
            }
            pen += if adv > 0.0 {
                adv
            } else {
                font.advance_of(idx as usize) * scale
            };
        }
    }
    b
}

fn shape_bounds(shape: &Shape) -> Option<(f32, f32, f32, f32)> {
    let mut b: Option<(f32, f32, f32, f32)> = None;
    for f in &shape.fills {
        for c in &f.contours {
            for p in c {
                b = Some(match b {
                    None => (p.x, p.y, p.x, p.y),
                    Some((x0, y0, x1, y1)) => (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y)),
                });
            }
        }
    }
    b
}

impl Mat {
    pub fn invert(&self) -> Option<Mat> {
        let det = self.scale_x * self.scale_y - self.rot_skew_1 * self.rot_skew_0;
        if det.abs() < 1e-9 {
            return None;
        }
        let (a, b, c, d) = (
            self.scale_y / det,
            -self.rot_skew_1 / det,
            -self.rot_skew_0 / det,
            self.scale_x / det,
        );
        Some(Mat {
            scale_x: a,
            rot_skew_0: c,
            rot_skew_1: b,
            scale_y: d,
            tx: -(a * self.tx + b * self.ty),
            ty: -(c * self.tx + d * self.ty),
        })
    }
}

/// World-space bounds of one instance, with every matrix above it applied.
/// `hitTest` and `getBounds` are built on this.
pub fn bounds_of(
    movie: &Movie,
    inst: &Instance,
    parent: &Mat,
) -> Option<(f32, f32, f32, f32)> {
    if !inst.visible {
        return None;
    }
    let world = parent.mul(&inst.matrix);
    let mut out: Option<(f32, f32, f32, f32)> = None;
    let mut add = |x0: f32, y0: f32, x1: f32, y1: f32| {
        // Transform the corners: a rotated or skewed clip needs the hull, not
        // just the two diagonal points.
        let pts = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        let mut b = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in pts {
            let wx = world.scale_x * x + world.rot_skew_1 * y + world.tx;
            let wy = world.rot_skew_0 * x + world.scale_y * y + world.ty;
            b.0 = b.0.min(wx);
            b.1 = b.1.min(wy);
            b.2 = b.2.max(wx);
            b.3 = b.3.max(wy);
        }
        match out {
            Some(o) => out = Some((o.0.min(b.0), o.1.min(b.1), o.2.max(b.2), o.3.max(b.3))),
            None => out = Some(b),
        }
    };
    if let Some(b) = char_local_bounds(movie, inst.char_id) {
        add(b.0, b.1, b.2, b.3);
    } else if let Some(sp) = &inst.sprite {
        for child in sp.list.values() {
            if let Some(b) = bounds_of(movie, child, &world) {
                match out {
                    Some(o) => {
                        out = Some((o.0.min(b.0), o.1.min(b.1), o.2.max(b.2), o.3.max(b.3)))
                    }
                    None => out = Some(b),
                }
            }
        }
    }
    out
}

/// Bounds of a display-list path (names below the root), in stage pixels.
pub fn path_bounds(
    movie: &Movie,
    list: &BTreeMap<u16, Instance>,
    parts: &[&str],
    parent: &Mat,
) -> Option<(f32, f32, f32, f32)> {
    let Some(&first) = parts.first() else {
        return None;
    };
    for inst in list.values() {
        if inst.name.as_deref() != Some(first) {
            continue;
        }
        if parts.len() == 1 {
            return bounds_of(movie, inst, parent);
        }
        let world = parent.mul(&inst.matrix);
        if let Some(sp) = &inst.sprite {
            return path_bounds(movie, &sp.list, &parts[1..], &world);
        }
    }
    None
}

fn mul_rgba(c: Rgba, mul: [f32; 4]) -> Rgba {
    let a = ((c >> 24) & 0xFF) as f32 / 255.0;
    let r = ((c >> 16) & 0xFF) as f32;
    let g = ((c >> 8) & 0xFF) as f32;
    let b = (c & 0xFF) as f32;
    rgba(
        (r * mul[0]).clamp(0.0, 255.0) as u8,
        (g * mul[1]).clamp(0.0, 255.0) as u8,
        (b * mul[2]).clamp(0.0, 255.0) as u8,
        ((a * mul[3]).clamp(0.0, 1.0) * 255.0) as u8,
    )
}

fn grad_color(stops: &[(u8, u32)], t: f32) -> Rgba {
    if stops.is_empty() {
        return 0xFFFF_FFFF;
    }
    let t = t.clamp(0.0, 1.0);
    let p = t * 255.0;
    let mut prev = stops[0];
    if p <= prev.0 as f32 {
        return prev.1;
    }
    for &next in stops.iter().skip(1) {
        if p <= next.0 as f32 {
            let span = (next.0 as f32 - prev.0 as f32).max(1.0);
            let f = (p - prev.0 as f32) / span;
            let a = prev.1;
            let b = next.1;
            let mix = |sh: u32| {
                let x = ((a >> sh) & 0xFF) as f32;
                let y = ((b >> sh) & 0xFF) as f32;
                ((x + (y - x) * f) as u32) & 0xFF
            };
            return (mix(24) << 24) | (mix(16) << 16) | (mix(8) << 8) | mix(0);
        }
        prev = next;
    }
    prev.1
}

impl Canvas {
    pub fn draw_shape(&mut self, shape: &Shape, movie: &Movie, m: &Mat, mul: [f32; 4]) {
        self.draw_shape_with(shape, movie, m, mul, None);
    }

    /// Draws a shape in one flat colour. Glyph outlines carry no paint of their
    /// own, so text is drawn this way.
    fn draw_shape_solid(
        &mut self,
        shape: &Shape,
        movie: &Movie,
        m: &Mat,
        mul: [f32; 4],
        color: Rgba,
    ) {
        self.draw_shape_with(shape, movie, m, mul, Some(color));
    }

    fn draw_shape_with(
        &mut self,
        shape: &Shape,
        movie: &Movie,
        m: &Mat,
        mul: [f32; 4],
        solid: Option<Rgba>,
    ) {
        let inv = m.invert().unwrap_or(Mat::IDENTITY);
        for fp in &shape.fills {
            self.fill_path(fp, movie, m, &inv, mul, solid);
        }
        for st in &shape.strokes {
            self.stroke_path(st, m, mul);
        }
    }

    fn fill_path(
        &mut self,
        fp: &FillPath,
        movie: &Movie,
        m: &Mat,
        inv: &Mat,
        mul: [f32; 4],
        solid: Option<Rgba>,
    ) {
        // Rasterize every contour of this fill together so opposite winding
        // cancels (even-odd), which is how SWF holes are expressed.
        let mut edges: Vec<(f32, f32, f32, f32)> = Vec::new();
        let mut y0 = f32::MAX;
        let mut y1 = f32::MIN;
        let mut x0 = f32::MAX;
        let mut x1 = f32::MIN;
        for contour in &fp.contours {
            if contour.len() < 2 {
                continue;
            }
            // A subpath of two points is a lone line, not a loop: Flash emits
            // these for the edges of a shape's outer frame, and closing them
            // would cancel the crossing that carves the hole. Longer subpaths
            // are closed polygons.
            let close = contour.len() >= 3;
            let edges_in = if close {
                contour.len()
            } else {
                contour.len() - 1
            };
            for i in 0..edges_in {
                let a = contour[i];
                let b = contour[(i + 1) % contour.len()];
                let (ax, ay) = m.apply(a.x, a.y);
                let (bx, by) = m.apply(b.x, b.y);
                if (ay - by).abs() < 0.0001 {
                    continue;
                }
                edges.push((ax, ay, bx, by));
                y0 = y0.min(ay).min(by);
                y1 = y1.max(ay).max(by);
                x0 = x0.min(ax).min(bx);
                x1 = x1.max(ax).max(bx);
            }
        }
        if edges.is_empty() {
            return;
        }
        let cy0 = (y0.floor().max(0.0) as i32).min(self.h);
        let cy1 = (y1.ceil() as i32 + 1).min(self.h);
        let cx0 = (x0.floor() as i32).max(0);
        let cx1 = (x1.ceil() as i32 + 1).min(self.w);
        if cy1 <= cy0 || cx1 <= cx0 {
            return;
        }
        let mut xs: Vec<f32> = Vec::new();
        for y in cy0..cy1 {
            let sy = y as f32 + 0.5;
            xs.clear();
            for &(ax, ay, bx, by) in &edges {
                let (lo, hi) = if ay < by { (ay, by) } else { (by, ay) };
                if sy < lo || sy >= hi {
                    continue;
                }
                xs.push(ax + (sy - ay) * (bx - ax) / (by - ay));
            }
            if xs.len() < 2 {
                continue;
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            for pair in xs.chunks_exact(2) {
                let start = (pair[0].ceil() as i32).max(cx0);
                let end = (pair[1].floor() as i32).min(cx1 - 1);
                for x in start..=end {
                    let color = match solid {
                        Some(c) => c,
                        None => self.paint_at(&fp.paint, x, y, inv, movie),
                    };
                    if color != 0 {
                        self.put(x, y, mul_rgba(color, mul));
                    }
                }
            }
        }
    }

    fn paint_at(&self, paint: &Paint, x: i32, y: i32, inv: &Mat, movie: &Movie) -> Rgba {
        match paint {
            // The parser already supplies 0xFF for shapes without an alpha
            // channel; forcing it here made DefineShape4's fully transparent
            // hit areas paint as opaque blocks.
            Paint::Solid(c) => *c,
            Paint::Linear { matrix, stops } => {
                let (lx, ly) = inv.apply(x as f32 + 0.5, y as f32 + 0.5);
                let Some(gi) = matrix.invert() else {
                    return 0;
                };
                let (gx, _) = gi.apply(lx, ly);
                // Gradient space spans -8192..8192 (twips) around the origin.
                grad_color(stops, (gx / 16384.0) + 0.5)
            }
            Paint::Radial {
                matrix,
                stops,
                focal,
            } => {
                let (lx, ly) = inv.apply(x as f32 + 0.5, y as f32 + 0.5);
                let Some(gi) = matrix.invert() else {
                    return 0;
                };
                let (gx, gy) = gi.apply(lx, ly);
                let r = ((gx - focal * 8192.0).powi(2) + gy * gy).sqrt() / 8192.0;
                grad_color(stops, r)
            }
            Paint::Bitmap { id, matrix } => {
                let Some(bm) = movie.bitmaps.get(id) else {
                    return 0;
                };
                let (lx, ly) = inv.apply(x as f32 + 0.5, y as f32 + 0.5);
                let Some(bi) = matrix.invert() else {
                    return 0;
                };
                let (u, v) = bi.apply(lx, ly);
                if u < 0.0 || v < 0.0 || u >= bm.w as f32 || v >= bm.h as f32 {
                    return 0;
                }
                *bm.px
                    .get(v as usize * bm.w as usize + u as usize)
                    .unwrap_or(&0)
            }
            Paint::None => 0,
        }
    }

    fn stroke_path(&mut self, st: &StrokePath, m: &Mat, mul: [f32; 4]) {
        let det = (m.scale_x * m.scale_y - m.rot_skew_0 * m.rot_skew_1).abs();
        let scale = det.sqrt().clamp(0.05, 16.0);
        let width = (st.width * scale).clamp(0.75, 64.0);
        let color = mul_rgba(st.color | 0xFF00_0000, mul);
        for (a, b) in &st.lines {
            let pa = m.apply(a.x, a.y);
            let pb = m.apply(b.x, b.y);
            self.thick_line(pa, pb, width, color);
        }
    }

    fn thick_line(&mut self, a: (f32, f32), b: (f32, f32), width: f32, color: Rgba) {
        let hw = width * 0.5;
        let x0 = (a.0.min(b.0) - hw).floor() as i32;
        let x1 = (a.0.max(b.0) + hw).ceil() as i32;
        let y0 = (a.1.min(b.1) - hw).floor() as i32;
        let y1 = (a.1.max(b.1) + hw).ceil() as i32;
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        let r2 = hw * hw;
        for y in y0.max(0)..=y1.min(self.h - 1) {
            for x in x0.max(0)..=x1.min(self.w - 1) {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let t = if len2 <= 1e-6 {
                    0.0
                } else {
                    (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0)
                };
                let (cx, cy) = (a.0 + dx * t, a.1 + dy * t);
                if (px - cx).powi(2) + (py - cy).powi(2) <= r2 {
                    self.put(x, y, color);
                }
            }
        }
    }

    pub fn draw_bitmap(&mut self, bm: &Bitmap, m: &Mat, mul: [f32; 4]) {
        let Some(inv) = m.invert() else {
            return;
        };
        let corners = [
            m.apply(0.0, 0.0),
            m.apply(bm.w as f32, 0.0),
            m.apply(0.0, bm.h as f32),
            m.apply(bm.w as f32, bm.h as f32),
        ];
        let (mut x0, mut y0) = (f32::MAX, f32::MAX);
        let (mut x1, mut y1) = (f32::MIN, f32::MIN);
        for c in corners {
            x0 = x0.min(c.0);
            y0 = y0.min(c.1);
            x1 = x1.max(c.0);
            y1 = y1.max(c.1);
        }
        let cx0 = (x0.floor() as i32).max(0);
        let cy0 = (y0.floor() as i32).max(0);
        let cx1 = (x1.ceil() as i32).min(self.w - 1);
        let cy1 = (y1.ceil() as i32).min(self.h - 1);
        for y in cy0..=cy1 {
            for x in cx0..=cx1 {
                let (u, v) = inv.apply(x as f32 + 0.5, y as f32 + 0.5);
                if u < 0.0 || v < 0.0 || u >= bm.w as f32 || v >= bm.h as f32 {
                    continue;
                }
                let c = *bm
                    .px
                    .get(v as usize * bm.w as usize + u as usize)
                    .unwrap_or(&0);
                if c >> 24 == 0 {
                    continue;
                }
                self.put(x, y, mul_rgba(c, mul));
            }
        }
    }
}

include!("player_avm1.rs");

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Width of one line of text in stage units.
fn measure(font: &Font, scale: f32, text: &str) -> f32 {
    let mut w = 0.0;
    for ch in text.chars() {
        match font.glyph_index(ch as u16) {
            Some(i) => w += font.advance_of(i as usize) * scale,
            None => w += font.units_per_em as f32 * 0.25 * scale,
        }
    }
    w
}

/// Greedy word wrap; words longer than a line are kept whole, which is what
/// Flash does for unbreakable strings.
fn wrap_lines(font: &Font, scale: f32, text: &str, width: f32) -> Vec<String> {
    let mut out = Vec::new();
    for hard in text.split('\n') {
        let mut line = String::new();
        for word in hard.split(' ') {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if measure(font, scale, &candidate) > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
        }
        out.push(line.trim_end().to_string());
    }
    out
}

/// `DefineText`/`DefineText2`: a list of runs of glyph indices, already laid
/// out by the authoring tool, so positions come straight from the records.
pub fn draw_static_text(canvas: &mut Canvas, movie: &Movie, text: &Text, world: &Mat, mul: [f32; 4]) {
    for run in &text.runs {
        let Some(font) = movie.fonts.get(&run.font) else {
            continue;
        };
        let scale = run.height / font.units_per_em.max(1.0);
        if scale <= 0.0 {
            continue;
        }
        let mut pen = run.x;
        for &(idx, adv) in &run.glyphs {
            if let Some(glyph) = font.glyphs.get(idx as usize) {
                let m = Mat {
                    scale_x: scale,
                    scale_y: scale,
                    rot_skew_0: 0.0,
                    rot_skew_1: 0.0,
                    tx: pen,
                    ty: run.y,
                };
                canvas.draw_shape_solid(glyph, movie, &world.mul(&m), mul, run.color);
            }
            pen += if adv > 0.0 {
                adv
            } else {
                font.advance_of(idx as usize) * scale
            };
        }
    }
}

/// Strips the handful of HTML tags Flash allows in text fields. Rich markup
/// (`<b>`, `<font>`) is rare in games; the glyphs matter more than the styling.
fn strip_tags(text: &str) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// `DefineEditText`: the game supplies the string, so it is laid out here
/// (wrap, alignment, line advance) rather than read from records.
pub fn draw_edit_text(
    canvas: &mut Canvas,
    movie: &Movie,
    et: &EditText,
    world: &Mat,
    mul: [f32; 4],
    value: &str,
) {
    if std::env::var_os("FLASH_TEXT").is_some() && !value.is_empty() {
        eprintln!(
            "TEXT {value:?} font {} h={} scale_glyphs={} bounds={:?} wrap={} align={}",
            et.font,
            et.height,
            movie.fonts.get(&et.font).map(|f| f.glyphs.len()).unwrap_or(0),
            et.bounds,
            et.wrap,
            et.align
        );
    }
    let Some(font) = movie.fonts.get(&et.font) else {
        return;
    };
    let scale = et.height / font.units_per_em.max(1.0);
    if scale <= 0.0 {
        return;
    }
    let (bx0, by0, bx1, _by1) = et.bounds;
    let width = (bx1 - bx0).max(1.0);
    let text = strip_tags(value);
    let lines = if et.wrap {
        wrap_lines(font, scale, &text, width)
    } else {
        text.split('\n').map(|l| l.to_string()).collect()
    };
    let line_h = (font.ascent + font.descent + font.leading) as f32 * scale;
    let mut baseline = by0 + font.ascent as f32 * scale;
    for line in lines {
        if line.is_empty() {
            baseline += line_h;
            continue;
        }
        let w = measure(font, scale, &line);
        let mut pen = match et.align {
            1 => (bx1 - w).max(bx0),
            2 => bx0 + (width - w) * 0.5,
            _ => bx0,
        };
        for ch in line.chars() {
            let Some(idx) = font.glyph_index(ch as u16) else {
                continue;
            };
            if let Some(glyph) = font.glyphs.get(idx as usize) {
                let m = Mat {
                    scale_x: scale,
                    scale_y: scale,
                    rot_skew_0: 0.0,
                    rot_skew_1: 0.0,
                    tx: pen,
                    ty: baseline,
                };
                canvas.draw_shape_solid(glyph, movie, &world.mul(&m), mul, et.color);
            }
            pen += font.advance_of(idx as usize) * scale;
        }
        baseline += line_h;
    }
}
