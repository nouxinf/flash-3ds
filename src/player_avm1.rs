// ---------------------------------------------------------------------------
// AVM1 interpreter (included from player.rs)
//
// The decoder lives in `avm1.rs`; this half executes its instructions against
// the display list, variables and input state owned by `Player`.
// ---------------------------------------------------------------------------

/// Where `gotoAndPlay` and friends send a playhead.
enum Jump {
    /// Absolute 0-based frame index.
    Abs(usize),
    /// Frames relative to the current one.
    Delta(i32),
    /// A frame label.
    Label(String),
}

fn norm(path: &str) -> String {
    let mut s = path.trim().to_string();
    loop {
        let lower = s.to_ascii_lowercase();
        if lower.starts_with("_root.") {
            s = s[6..].to_string();
        } else if lower.starts_with("_level0.") {
            s = s[8..].to_string();
        } else if lower.starts_with("this.") {
            s = s[5..].to_string();
        } else if s.starts_with('/') {
            s = s.trim_start_matches('/').to_string();
        } else {
            break;
        }
    }
    s.replace('/', ".")
}

/// Flash clip properties, as spelled in ActionScript. Dot access such as
/// `panel._visible = false` compiles to GetMember/SetMember, so the name table
/// is shared with ActionGetProperty/ActionSetProperty.
fn prop_id(name: &str) -> Option<u16> {
    Some(match name {
        "_x" => 0,
        "_y" => 1,
        "_xscale" => 2,
        "_yscale" => 3,
        "_currentframe" => 4,
        "_totalframes" => 5,
        "_alpha" => 6,
        "_visible" => 7,
        "_width" => 8,
        "_height" => 9,
        "_rotation" => 10,
        "_framesloaded" => 12,
        "_name" => 13,
        "_xmouse" => 20,
        "_ymouse" => 21,
        _ => return None,
    })
}

/// Splits `clip._alpha` into the clip path and the property it addresses.
fn split_prop(name: &str) -> Option<(&str, u16)> {
    let (path, last) = name.rsplit_once('.')?;
    prop_id(last).map(|p| (path, p))
}

/// `escape()`: everything except `A-Za-z0-9@*_+-./` becomes `%XX`, or
/// `%uXXXX` above 0xFF.
fn escape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        if ch.is_ascii_alphanumeric() || "@*_+-./".contains(ch) {
            out.push(ch);
        } else if c < 0x100 {
            out.push_str(&format!("%{c:02X}"));
        } else {
            out.push_str(&format!("%u{c:04X}"));
        }
    }
    out
}

fn unescape_str(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == '%' && i + 1 < b.len() {
            if (b[i + 1] == 'u' || b[i + 1] == 'U') && i + 5 < b.len() {
                let hex: String = b[i + 2..i + 6].iter().collect();
                if let Ok(v) = u32::from_str_radix(&hex, 16) {
                    if let Some(c) = char::from_u32(v) {
                        out.push(c);
                        i += 6;
                        continue;
                    }
                }
            } else if i + 2 < b.len() {
                let hex: String = b[i + 1..i + 3].iter().collect();
                if let Ok(v) = u32::from_str_radix(&hex, 16) {
                    out.push(v as u8 as char);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn pop(stack: &mut Vec<Value>) -> Value {
    stack.pop().unwrap_or(Value::Undefined)
}

/// Pops `count` call arguments in declaration order. The compiler pushes them
/// right-to-left, so the first argument is on top and must not be reversed -
/// doing so silently swaps every multi-argument call.
fn pop_args(stack: &mut Vec<Value>, count: usize) -> Vec<Value> {
    let n = count.min(stack.len()).min(MAX_ARRAY_LEN);
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(pop(stack));
    }
    args
}

/// A plain object or an array. Arrays keep their elements in `items`; every
/// other property lives in `props`, and `proto` is the prototype chain link.
pub struct Object {
    props: Vec<(String, Value)>,
    proto: Option<usize>,
    items: Option<Vec<Value>>,
    /// `Object.addProperty(name, get, set)` descriptors: name, getter, setter.
    /// Games give objects computed properties and then assign to them, and a
    /// plain data property would swallow the write while the real player ran the
    /// setter, so the game would keep a stale value forever.
    accessors: Vec<(String, Option<Value>, Option<Value>)>,
}

/// A symbol registered with `Object.registerClass`. Instances of the symbol
/// resolve members through the class prototype, and the constructor runs once
/// when an instance appears - AS2's whole class system rests on this, and games
/// built with it (Bloons' `Bloon`, `Tower` and `Bullet`) have no methods at all
/// without it.
pub struct ClassesEntry {
    ctor: Value,
    proto: Option<usize>,
}

/// One function activation. Parameters and `var` declarations live here; a
/// bare assignment that matches no local falls through to the clip `this`
/// names, which is what ActionScript 1/2 does.
struct Frame {
    locals: Vec<(String, Value)>,
    registers: Vec<Value>,
    ret: Value,
}

/// Each call recurses in Rust, so runaway recursion would overflow the 3DS
/// stack rather than fail neatly. Flash's own default limit is 256; 16 is
/// plenty for game code and keeps memory flat.
const MAX_CALL_DEPTH: usize = 16;

/// There is no garbage collector: objects live for the movie's lifetime. A cap
/// turns a script that allocates every frame into one log line instead of a
/// slow out-of-memory death.
const MAX_OBJECTS: usize = 4096;

/// Flash arrays are sparse and can be indexed up to 2^32; these are dense, so
/// an out-of-range index is ignored rather than sizing a vector to it. Without
/// this, `a[302231454903659441160191] = 1` asks the allocator for tens of GB.
const MAX_ARRAY_LEN: usize = 1 << 16;

fn set_reg(f: &mut Frame, idx: usize, v: Value) {
    if f.registers.len() <= idx {
        f.registers.resize(idx + 1, Value::Undefined);
    }
    f.registers[idx] = v;
}

/// Every named clip path in the tree, parents before children.
fn collect_clip_paths(list: &BTreeMap<u16, Instance>, prefix: &str, out: &mut Vec<String>) {
    for inst in list.values() {
        let path = clip_path(prefix, inst);
        if !path.is_empty() && path != prefix {
            out.push(path.clone());
        }
        if let Some(sp) = inst.sprite.as_ref() {
            collect_clip_paths(&sp.list, &path, out);
        }
    }
}

/// Paths of instances that have not had their `load` handler yet.
fn collect_unloaded(list: &BTreeMap<u16, Instance>, prefix: &str, out: &mut Vec<String>) {
    for inst in list.values() {
        let path = clip_path(prefix, inst);
        if !inst.loaded {
            out.push(path.clone());
        }
        if let Some(sp) = inst.sprite.as_ref() {
            collect_unloaded(&sp.list, &path, out);
        }
    }
}

fn find_instance<'a>(list: &'a BTreeMap<u16, Instance>, parts: &[&str]) -> Option<&'a Instance> {
    let first = *parts.first()?;
    for inst in list.values() {
        if inst.answers_to(first) {
            if parts.len() == 1 {
                return Some(inst);
            }
            // Several clips can share a name at different depths; a match with
            // the wrong tree (an empty leftover, say) must not end the search.
            if let Some(sp) = inst.sprite.as_ref() {
                if let Some(found) = find_instance(&sp.list, &parts[1..]) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// The display list named by an instance path, so a clip can be attached
/// into `_root.holder` rather than always at the root.
fn nested_list_mut<'a>(
    list: &'a mut BTreeMap<u16, Instance>,
    parts: &[&str],
) -> Option<&'a mut BTreeMap<u16, Instance>> {
    let Some(&first) = parts.first() else {
        return Some(list);
    };
    for inst in list.values_mut() {
        if !inst.answers_to(first) {
            continue;
        }
        let sp = inst.sprite.as_mut()?;
        return nested_list_mut(&mut sp.list, &parts[1..]);
    }
    None
}

fn with_instance_mut<F: FnOnce(&mut Instance)>(
    list: &mut BTreeMap<u16, Instance>,
    parts: &[&str],
    f: F,
) -> bool {
    let Some(&first) = parts.first() else {
        return false;
    };
    let mut f = Some(f);
    for inst in list.values_mut() {
        if !inst.answers_to(first) {
            continue;
        }
        if parts.len() == 1 {
            if let Some(f) = f.take() {
                f(inst);
                return true;
            }
        } else if let Some(sp) = inst.sprite.as_mut() {
            if let Some(f) = f.take() {
                return with_instance_mut(&mut sp.list, &parts[1..], f);
            }
        }
    }
    false
}

impl Player {
    // -----------------------------------------------------------------------
    // Variables and object model
    // -----------------------------------------------------------------------

    fn var_key(&self, name: &str) -> String {
        let lower = name.trim().to_ascii_lowercase();
        let n = norm(name);
        // An explicitly rooted path always means the main timeline, even from
        // inside a clip handler.
        if lower.starts_with("_root.") || lower.starts_with("_level0.") {
            return n;
        }
        let t = norm(&self.target_path);
        if t.is_empty() || n == "_root" || n.is_empty() {
            if t.is_empty() {
                n
            } else {
                t
            }
        } else {
            format!("{t}.{n}")
        }
    }

    /// Splits a property target path into display-list parts. An empty target
    /// or `this` refers to the clip whose handler is running.
    fn prop_parts(&self, target: &Value) -> Vec<String> {
        let raw = norm(&target.as_str());
        let base = if raw.is_empty() || raw == "this" {
            norm(&self.target_path)
        } else if raw == "_root" || raw == "_level0" {
            // The main timeline itself, not a clip called "_root".
            String::new()
        } else {
            raw
        };
        base.split('.')
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect()
    }

    /// Scope chain a name resolves through, nearest first: the running clip,
    /// then its parents, then the main timeline (empty string).
    fn var_scopes(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = norm(&self.target_path);
        loop {
            out.push(cur.clone());
            if cur.is_empty() {
                break;
            }
            match cur.rfind('.') {
                Some(i) => cur.truncate(i),
                None => cur.clear(),
            }
        }
        out
    }

    /// Every variable key a name could resolve to, nearest scope first.
    fn var_candidates(&self, name: &str) -> Vec<String> {
        let n = norm(name);
        let lower = name.trim().to_ascii_lowercase();
        if lower.starts_with("_root.") || lower.starts_with("_level0.") {
            return vec![n];
        }
        let mut out = Vec::new();
        for scope in self.var_scopes() {
            let key = if scope.is_empty() {
                n.clone()
            } else {
                format!("{scope}.{n}")
            };
            if !out.contains(&key) {
                out.push(key);
            }
        }
        if n.contains('.') && !out.contains(&n) {
            out.push(n);
        }
        out
    }

    fn local_get(&self, name: &str) -> Option<Value> {
        self.frames.last().and_then(|f| {
            f.locals
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        })
    }

    fn local_set(&mut self, name: &str, v: Value) -> bool {
        if let Some(f) = self.frames.last_mut() {
            if let Some(slot) = f.locals.iter_mut().find(|(k, _)| k == name) {
                slot.1 = v;
                return true;
            }
        }
        false
    }

    fn local_define(&mut self, name: &str, v: Value) {
        if let Some(f) = self.frames.last_mut() {
            if let Some(slot) = f.locals.iter_mut().find(|(k, _)| k == name) {
                slot.1 = v;
            } else {
                f.locals.push((name.to_string(), v));
            }
        }
    }

    /// Register read. A script that is not inside a function body still has
    /// registers: `#initclip` class definitions keep their class function in a
    /// register and then read `register.prototype`, so an empty register file
    /// made every AS2 class method definition land on `undefined`.
    fn reg_get(&self, idx: u8) -> Value {
        match self.frames.last() {
            Some(f) => f
                .registers
                .get(idx as usize)
                .cloned()
                .unwrap_or(Value::Undefined),
            None => self
                .script_regs
                .get(idx as usize)
                .cloned()
                .unwrap_or(Value::Undefined),
        }
    }

    fn reg_set(&mut self, idx: u8, v: Value) {
        let i = idx as usize;
        match self.frames.last_mut() {
            Some(f) => {
                if f.registers.len() <= i {
                    f.registers.resize(i + 1, Value::Undefined);
                }
                f.registers[i] = v;
            }
            None => {
                if self.script_regs.len() <= i {
                    self.script_regs.resize(i + 1, Value::Undefined);
                }
                self.script_regs[i] = v;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Dynamic clip handlers and registered classes
    // -----------------------------------------------------------------------

    /// The handler a clip holds as a variable: `btn.onPress = function () {...}`
    /// is stored exactly like any other clip variable, under `path.name`.
    fn clip_handler(&self, path: &str, name: &str) -> Option<Value> {
        let key = norm(&if path.is_empty() {
            name.to_string()
        } else {
            format!("{path}.{name}")
        });
        match self.vars.get(&key) {
            Some(v @ Value::Function(_)) => Some(v.clone()),
            // A handler bound with Delegate is a bound-method object.
            Some(v) if self.is_delegate(v) => Some(v.clone()),
            _ => None,
        }
    }

    /// Calls `clip.handler()` with `this` bound to the clip. Returns whether such
    /// a handler existed.
    fn run_clip_handler(&mut self, path: &str, name: &str) -> bool {
        let Some(f) = self.clip_handler(path, name) else {
            return false;
        };
        if std::env::var_os("FLASH_EVENTS").is_some() {
            eprintln!("EVENT {name} on {:?} at {:.0}ms", path, self.time_ms);
        }
        // FLASH_EVDUMP shows what the main loop is bound to: a delegate whose
        // method never resolves looks identical to a handler that does nothing.
        if std::env::var_os("FLASH_EVDUMP").is_some() && name == "onEnterFrame" && path.is_empty() {
            let mut extra = String::new();
            if let Value::Object(i) = &f {
                if let Some(o) = self.objects.get(*i) {
                    for (k, v) in &o.props {
                        if k.starts_with("__delegate") {
                            extra.push_str(&format!(" {k}={v:?}"));
                        }
                    }
                }
            }
            eprintln!("EVDUMP _root.onEnterFrame = {f:?}{extra}");
        }
        let saved = std::mem::replace(&mut self.target_path, path.to_string());
        match f {
            Value::Function(id) => {
                let this = if path.is_empty() {
                    Value::Str("_root".into())
                } else {
                    Value::Str(path.to_string())
                };
                self.invoke(id, this, Vec::new(), 2);
            }
            other => {
                self.call_value(&other, Vec::new(), 2);
            }
        }
        self.target_path = saved;
        true
    }

    /// Runs `onEnterFrame` handlers that were assigned to clips as properties.
    /// Games animate with these instead of `onClipEvent(enterFrame)`, which is
    /// the other way to attach the same event.
    fn fire_dynamic_enter_frame(&mut self) {
        // The main timeline is a clip too: `_root.onEnterFrame` is how many
        // games run their whole loop.
        let mut paths: Vec<String> = vec![String::new()];
        collect_clip_paths(&self.list, "", &mut paths);
        for path in paths {
            if self.clip_handler(&path, "onEnterFrame").is_some() {
                self.run_clip_handler(&path, "onEnterFrame");
            }
        }
    }

    /// `Object.registerClass(linkage, ctor)`: every instance of the symbol
    /// becomes an instance of `ctor`, so its prototype chain answers member
    /// lookups.
    fn register_class(&mut self, link: &str, ctor: &Value) {
        let name = norm(link);
        if std::env::var_os("FLASH_RC").is_some() {
            eprintln!(
                "registerClass({link:?}) -> {:?} ctor {:?} this {:?} Tower {:?} ops {:?}",
                self.movie.exports.get(&name),
                ctor,
                self.target_path,
                self.vars.get("Tower"),
                self.recent_ops
            );
        }
        let Some(&char_id) = self
            .movie
            .exports
            .get(&name)
            .or_else(|| self.movie.exports.get(link))
        else {
            self.miss(format!("registerClass({name})"));
            return;
        };
        let proto = match ctor {
            Value::Function(id) => match self.fn_proto(*id) {
                Value::Object(p) => Some(p),
                _ => None,
            },
            Value::Object(i) => Some(*i),
            _ => None,
        };
        self.classes.insert(
            char_id,
            ClassesEntry {
                ctor: ctor.clone(),
                proto,
            },
        );
    }

    /// A property a clip inherits from its registered class.
    fn class_member(&mut self, path: &str, name: &str) -> Option<Value> {
        let owned = norm(path);
        let parts: Vec<&str> = owned.split('.').filter(|p| !p.is_empty()).collect();
        let inst = find_instance(&self.list, &parts)?;
        let entry = self.classes.get(&inst.char_id);
        if std::env::var_os("FLASH_PROTO").is_some() && name == "Init" {
            eprintln!(
                "PROTO {owned:?} char {} class {:?} proto {:?}",
                inst.char_id,
                entry.is_some(),
                entry.as_ref().and_then(|e| e.proto)
            );
        }
        let proto = entry?.proto?;
        let v = self.obj_get(proto, name);
        if v == Value::Undefined {
            None
        } else {
            Some(v)
        }
    }

    /// Runs the class constructor of a clip that has just appeared.
    fn run_constructor(&mut self, path: &str) {
        let owned = norm(path);
        let parts: Vec<&str> = owned.split('.').filter(|p| !p.is_empty()).collect();
        let Some(inst) = find_instance(&self.list, &parts) else {
            return;
        };
        let Some(entry) = self.classes.get(&inst.char_id) else {
            return;
        };
        let Value::Function(id) = entry.ctor else {
            return;
        };
        let this = if path.is_empty() {
            Value::Str("_root".into())
        } else {
            Value::Str(path.to_string())
        };
        let saved = std::mem::replace(&mut self.target_path, path.to_string());
        self.invoke(id, this, Vec::new(), 2);
        self.target_path = saved;
    }

    fn get_var(&mut self, name: &str) -> Value {
        let bare = norm(name);
        if std::env::var_os("FLASH_UNDEF").is_some() && bare.starts_with("bloonpop") {
            let parts = [bare.as_str()];
            let mut names: Vec<String> = self
                .list
                .values()
                .filter_map(|i| i.name.clone())
                .take(6)
                .collect();
            names.sort();
            eprintln!(
                "GET {bare:?} t={:.0}ms target {:?} inst {:?} root {:?} ({})",
                self.time_ms,
                self.target_path,
                find_instance(&self.list, &parts).map(|i| i.char_id),
                names,
                self.list.len()
            );
        }
        // `_global` is the namespace AS2 classes and singletons are defined in.
        // Resolving it to undefined made every `_global.Foo = ...` write into
        // nothing, so no class the movie registered ever existed and `new
        // BloonsTD()` built an empty object with no game logic behind it.
        let trimmed = name.trim();
        let lower_name = trimmed.to_ascii_lowercase();
        if lower_name == "_global" {
            return Value::Str("_global".into());
        }
        if lower_name.starts_with("_global.") {
            let rest = trimmed[8..].to_string();
            if let Some(v) = self.vars.get(&rest) {
                return v.clone();
            }
        }
        // The underscore properties belong to the clip the code runs in, before
        // any variable of the same name is considered (`if (_visible)` and
        // `this._x += 3` must agree on what they read).
        if !bare.contains('.') {
            if let Some(prop) = prop_id(&bare) {
                let target = self.clip_value();
                let v = self.prop_get(prop, &target);
                if v != Value::Undefined {
                    return v;
                }
            }
        }
        // A function local shadows the clip, as in ActionScript.
        if !bare.contains('.') {
            if let Some(v) = self.local_get(&bare) {
                return v;
            }
            // Then `this` when the running method's receiver is an object.
            if let Value::Object(i) = self.this_value.clone() {
                let v = self.obj_get(i, &bare);
                if v != Value::Undefined {
                    return v;
                }
            }
        }
        // `clip._visible` and friends are instance state, not variables.
        if let Some((path, prop)) = split_prop(name) {
            let target = Value::Str(norm(path));
            let v = self.prop_get(prop, &target);
            if v != Value::Undefined {
                return v;
            }
        }
        // Outwards from the clip the code runs in. The bare name is the last
        // candidate, so globals like `System.security.sandboxType` still match;
        // checking it *first* let a root variable shadow a clip's own, which
        // made any clip-scoped state machine read the root's value and never
        // agree with the value it had just written.
        for key in self.var_candidates(name) {
            if let Some(v) = self.vars.get(&key) {
                return v.clone();
            }
        }
        // Builtin objects resolve to their own name, which lets member calls
        // such as Key.isDown() dispatch without a full object model.
        if matches!(
            bare.as_str(),
            "Key"
                | "Math"
                | "Mouse"
                | "String"
                | "Number"
                | "Boolean"
                | "Object"
                | "Array"
                | "Sound"
                | "MochiAd"
                | "MochiBot"
        ) {
            return Value::Str(bare);
        }
        if bare == "this" {
            // `this` is the clip or object the running code belongs to.
            return self.current_this();
        }
        if bare == "arguments" {
            if let Some(v) = self.local_get("arguments") {
                return v;
            }
        }
        if bare.is_empty() || bare == "_root" || bare == "_level0" {
            return Value::Str("_root".into());
        }
        // A bare name can name a clip. Returning its path is what makes
        // `clip.someVar` (GetVariable then GetMember) read the clip's own
        // variables, e.g. the `_root.introclip.done` flag a title screen sets.
        for key in self.var_candidates(name) {
            let parts: Vec<&str> = key.split('.').filter(|s| !s.is_empty()).collect();
            if !parts.is_empty() && find_instance(&self.list, &parts).is_some() {
                return Value::Str(key);
            }
        }
        Value::Undefined
    }

    /// Debug: report a root navigation, which is how a movie changes screens.
    fn note_nav(&self, what: String) {
        if std::env::var_os("FLASH_NAV").is_some() {
            eprintln!("NAV {what} at {:.0}ms (frame {})", self.time_ms, self.frame);
        }
    }

    fn debugging(&self) -> bool {
        self.trace
            || self.trace_fn.is_some()
            || std::env::var_os("FLASH_UNDEF").is_some()
            || std::env::var_os("FLASH_EVENTS").is_some()
            || std::env::var_os("FLASH_SETMEMBER").is_some()
    }

    /// Debug: report event handlers as a movie wires them up. A game whose
    /// buttons never fire is usually one whose handlers attach somewhere this
    /// runtime does not dispatch to.
    fn note_event_assign(&self, name: &str, value: &Value) {
        self.note_event_assign_on(name, value, "clip-root");
    }

    /// Same, for a member assignment on a known receiver.
    fn note_event_assign_on(&self, name: &str, value: &Value, on: &str) {
        if std::env::var_os("FLASH_EVENTS").is_none() {
            return;
        }
        let n = norm(name);
        let last = n.rsplit('.').next().unwrap_or(&n);
        if ["onPress", "onRelease", "onReleaseOutside", "onEnterFrame", "onLoad"]
            .iter()
            .any(|e| e.eq_ignore_ascii_case(last))
        {
            let kind = match value {
                Value::Function(_) => "function".to_string(),
                other => self.value_str(other),
            };
            eprintln!("EVENT {n} <- {kind} on {on}");
        }
    }

    fn set_var(&mut self, name: &str, v: Value) {
        // Debug: follow one variable name through every write (FLASH_SETVAR=x).
        if let Ok(want) = std::env::var("FLASH_SETVAR") {
            if name == want || name.ends_with(&format!(".{want}")) {
                eprintln!(
                    "SETVAR {name} = {} at {:.0}ms in {:?}",
                    self.value_str(&v),
                    self.time_ms,
                    if self.target_path.is_empty() {
                        "_root"
                    } else {
                        &self.target_path
                    }
                );
            }
        }
        self.note_event_assign(name, &v);
        // Globals live in the bare namespace, so `_global.Foo` and a read of
        // `Foo` see the same value no matter which clip wrote it.
        let trimmed = name.trim();
        if trimmed.to_ascii_lowercase().starts_with("_global.") {
            let key = trimmed[8..].to_string();
            self.vars.insert(key, v);
            return;
        }
        let bare = norm(name);
        // Flash resolves the underscore properties through the scope chain, so a
        // bare `_visible = false` inside a clip hides that clip. Writing it as a
        // variable instead left a "clip._visible" entry that nothing read, and
        // every panel the movie tried to dismiss stayed on screen.
        if !bare.contains('.') {
            if let Some(prop) = prop_id(&bare) {
                let target = self.clip_value();
                self.prop_set(prop, &target, v);
                return;
            }
        }
        // Assignment updates an existing local, otherwise it defines the
        // variable on the clip the code runs in. Reads are what walk outwards.
        if !bare.contains('.') && self.local_set(&bare, v.clone()) {
            return;
        }
        // A method's unqualified assignment belongs to `this` when that is an
        // object, which is how AS2 classes keep their state. Without this every
        // `new BloonsTD()` came out empty and the game had nothing to run.
        if !bare.contains('.') {
            if let Value::Object(i) = self.this_value.clone() {
                self.obj_set(i, &bare, v);
                return;
            }
        }
        // Writing a property updates the instance instead of defining a
        // variable named "clip._alpha".
        if let Some((path, prop)) = split_prop(name) {
            let target = Value::Str(norm(path));
            self.prop_set(prop, &target, v);
            return;
        }
        let k = self.var_key(name);
        self.vars.insert(k, v);
    }

    /// The path of a clip created inside `target`.
    fn child_path(&self, target: &str, name: &str) -> String {
        let t = norm(target);
        if t.is_empty() || t == "_root" || t == "_level0" {
            name.to_string()
        } else {
            format!("{t}.{name}")
        }
    }

    /// Creates an instance of a linked symbol inside `target` (a clip path).
    fn attach_clip(&mut self, link: &str, new_name: &str, depth: u16, target: &str) -> bool {
        let Some(&cid) = self.movie.exports.get(link) else {
            self.miss(format!("attachMovie({link})"));
            return false;
        };
        let sprite = build_sprite(&self.movie, cid, 0);
        self.insert_clip(cid, new_name, depth, target, sprite)
    }

    fn empty_clip(&mut self, new_name: &str, depth: u16, target: &str) -> bool {
        self.insert_clip(0, new_name, depth, target, Some(SpriteState::empty()))
    }

    fn insert_clip(
        &mut self,
        char_id: u16,
        new_name: &str,
        depth: u16,
        target: &str,
        sprite: Option<SpriteState>,
    ) -> bool {
        let inst = Instance {
            char_id,
            depth,
            name: if new_name.is_empty() {
                None
            } else {
                Some(new_name.to_string())
            },
            matrix: Mat::IDENTITY,
            cxform: CxForm::IDENTITY,
            ratio: 1.0,
            visible: true,
            sprite,
            clip_actions: Vec::new(),
            is_mask: false,
            clip_depth: None,
            loaded: false,
            init_done: false,
            placed_frame: None,
        };
        let owned = norm(target);
        let parts: Vec<&str> = owned.split('.').filter(|p| !p.is_empty()).collect();
        // A depth already in use is replaced, as Flash does for a fresh
        // attachMovie at the same depth.
        let root = parts.is_empty() || parts == ["_root"] || parts == ["_level0"];
        let list = if root {
            Some(&mut self.list)
        } else {
            nested_list_mut(&mut self.list, &parts)
        };
        match list {
            Some(list) => {
                list.insert(depth, inst);
                true
            }
            None => false,
        }
    }

    /// `duplicateMovieClip`: a copy of the target at a new depth, restarted at
    /// its first frame, which is how games spawn many copies of one symbol.
    fn duplicate_clip(&mut self, target: &str, new_name: &str, depth: u16) -> bool {
        let owned = norm(target);
        let parts: Vec<&str> = owned.split('.').filter(|p| !p.is_empty()).collect();
        let Some(inst) = find_instance(&self.list, &parts) else {
            return false;
        };
        let char_id = inst.char_id;
        let sprite = if inst.sprite.is_some() {
            build_sprite(&self.movie, char_id, 0)
        } else {
            None
        };
        // The new clip belongs where the original lives.
        let parent = parts[..parts.len().saturating_sub(1)].join(".");
        self.insert_clip(char_id, new_name, depth, &parent, sprite)
    }

    fn remove_clip(&mut self, target: &str) -> bool {
        let owned = norm(target);
        let parts: Vec<&str> = owned.split('.').filter(|p| !p.is_empty()).collect();
        let Some((last, parent)) = parts.split_last() else {
            return false;
        };
        let list = if parent.is_empty() {
            Some(&mut self.list)
        } else {
            nested_list_mut(&mut self.list, parent)
        };
        let Some(list) = list else { return false };
        let depth = list
            .iter()
            .find(|(_, i)| i.answers_to(last))
            .map(|(d, _)| *d);
        match depth {
            Some(d) => list.remove(&d).is_some(),
            None => false,
        }
    }

    /// `DefineLocal` always defines in the running function, which is what makes
    /// `var x` inside a function private to that call.
    fn define_local(&mut self, name: &str, v: Value) {
        let bare = norm(name);
        if !bare.contains('.') && !self.frames.is_empty() {
            self.local_define(&bare, v);
            return;
        }
        self.set_var(name, v);
    }

    // -----------------------------------------------------------------------
    // Objects and arrays
    // -----------------------------------------------------------------------

    fn obj_new(&mut self, proto: Option<usize>, items: Option<Vec<Value>>) -> Value {
        // A slot the collector emptied is indistinguishable from a fresh one.
        if let Some(i) = self.free_objects.pop() {
            self.objects[i] = Object {
                props: Vec::new(),
                proto,
                items,
                accessors: Vec::new(),
            };
            return Value::Object(i);
        }
        if self.objects.len() >= MAX_OBJECTS {
            if self.log.len() < 200 {
                self.log.push("object heap full".to_string());
            }
            return Value::Undefined;
        }
        self.objects.push(Object {
            props: Vec::new(),
            proto,
            items,
            accessors: Vec::new(),
        });
        Value::Object(self.objects.len() - 1)
    }

    fn array(&mut self, items: Vec<Value>) -> Value {
        self.obj_new(None, Some(items))
    }

    /// `mx.utils.Delegate`: the class every Flash 6+ game uses to attach a
    /// method as an event handler.
    ///
    /// Without it `clip.onPress = mx.utils.Delegate.create(td, "OnClick")`
    /// assigns undefined, so the handler is silently dropped and the movie never
    /// receives a click - a game that draws perfectly is then unplayable.
    pub fn install_builtins(&mut self) {
        let delegate = self.obj_new(None, None);
        let Value::Object(d) = delegate else {
            return;
        };
        self.obj_set(d, "__delegateClass", Value::Bool(true));
        let utils = self.obj_new(None, None);
        let Value::Object(u) = utils else {
            return;
        };
        self.obj_set(u, "Delegate", Value::Object(d));
        let mx = self.obj_new(None, None);
        let Value::Object(m) = mx else {
            return;
        };
        self.obj_set(m, "utils", Value::Object(u));
        self.vars.insert(String::from("mx"), Value::Object(m));
    }

    /// A bound method: the target object plus the method to run on it.
    fn make_delegate(&mut self, target: Value, method: Value) -> Value {
        let v = self.obj_new(None, None);
        if let Value::Object(i) = v {
            self.obj_set(i, "__delegateTarget", target);
            self.obj_set(i, "__delegateMethod", method);
        }
        v
    }

    fn delegate_parts(&self, idx: usize) -> Option<(Value, Value)> {
        let o = self.objects.get(idx)?;
        let target = o
            .props
            .iter()
            .find(|(k, _)| k == "__delegateTarget")
            .map(|(_, v)| v.clone())?;
        let method = o
            .props
            .iter()
            .find(|(k, _)| k == "__delegateMethod")
            .map(|(_, v)| v.clone())?;
        Some((target, method))
    }

    fn is_delegate(&self, v: &Value) -> bool {
        match v {
            Value::Object(i) => self.delegate_parts(*i).is_some(),
            _ => false,
        }
    }

    /// Calls a value as a function: a plain function, or a bound method. Event
    /// handlers are frequently delegates, so handler dispatch goes through here.
    fn call_value(&mut self, v: &Value, args: Vec<Value>, depth: u32) -> Value {
        match v {
            Value::Function(id) => {
                let this = self.current_this();
                self.invoke(*id, this, args, depth)
            }
            Value::Object(i) => match self.delegate_parts(*i) {
                Some((target, method)) => {
                    let f = match &method {
                        Value::Str(name) => {
                            let name = name.clone();
                            self.get_member(&target, &name)
                        }
                        other => other.clone(),
                    };
                    if let Value::Function(id) = f {
                        self.invoke(id, target, args, depth)
                    } else {
                        self.miss(format!("delegate target has no {}", self.value_str(&method)));
                        Value::Undefined
                    }
                }
                None => Value::Undefined,
            },
            _ => Value::Undefined,
        }
    }

    /// How much of the object heap is still free. The frame loop collections
    /// before a game can hit the ceiling.
    pub(crate) fn heap_pressure(&self) -> f32 {
        self.objects.len() as f32 / MAX_OBJECTS as f32
    }

    /// Mark and sweep over the object heap.
    ///
    /// AS2 games allocate arrays and scratch objects every frame and never say
    /// when they are done with them, exactly as they did under the real player,
    /// which collected for them. Without a collector the heap filled in seconds
    /// and every later `new Array()` quietly returned undefined, which is how a
    /// running game turns into a frozen one.
    ///
    /// This only runs between frames, when no interpreter stack or activation is
    /// live, so every reachable object is reachable from a root the timeline
    /// itself owns.
    pub fn collect_garbage(&mut self) {
        fn root(v: &Value, stack: &mut Vec<usize>) {
            if let Value::Object(i) = v {
                stack.push(*i);
            }
        }
        let len = self.objects.len();
        let mut mark = vec![false; len];
        let mut stack: Vec<usize> = Vec::new();
        for v in self.vars.values() {
            root(v, &mut stack);
        }
        for p in &self.fn_protos {
            if let Some(i) = p {
                stack.push(*i);
            }
        }
        for c in self.classes.values() {
            root(&c.ctor, &mut stack);
            if let Some(i) = c.proto {
                stack.push(i);
            }
        }
        root(&self.this_value, &mut stack);
        // A timer holds its callback and arguments across frames, so they are
        // live even while no script is running.
        for t in &self.timers {
            root(&t.func, &mut stack);
            for v in &t.args {
                root(v, &mut stack);
            }
        }
        for v in &self.script_regs {
            root(v, &mut stack);
        }
        for f in &self.frames {
            for (_, v) in &f.locals {
                root(v, &mut stack);
            }
            for v in &f.registers {
                root(v, &mut stack);
            }
            root(&f.ret, &mut stack);
        }
        while let Some(i) = stack.pop() {
            if i >= len || mark[i] {
                continue;
            }
            mark[i] = true;
            let obj = &self.objects[i];
            if let Some(p) = obj.proto {
                stack.push(p);
            }
            for (_, v) in &obj.props {
                root(v, &mut stack);
            }
            for (_, g, s) in &obj.accessors {
                if let Some(v) = g {
                    root(v, &mut stack);
                }
                if let Some(v) = s {
                    root(v, &mut stack);
                }
            }
            if let Some(items) = &obj.items {
                for v in items {
                    root(v, &mut stack);
                }
            }
        }
        let mut freed = 0u32;
        for i in 0..len {
            if !mark[i] {
                // Emptied rather than removed: indexes are the only handle the
                // movie has on an object, so the slot has to stay addressable.
                self.objects[i] = Object {
                    props: Vec::new(),
                    proto: None,
                    items: None,
                    accessors: Vec::new(),
                };
                self.free_objects.push(i);
                freed += 1;
            }
        }
        if std::env::var_os("FLASH_GC").is_some() {
            eprintln!("gc: {} live, {freed} freed", len - freed as usize);
        }
    }

    fn obj_get_own(&self, idx: usize, name: &str) -> Option<Value> {
        let o = self.objects.get(idx)?;
        if let Some(items) = o.items.as_ref() {
            if name == "length" {
                return Some(Value::Num(items.len() as f64));
            }
            if let Ok(n) = name.parse::<usize>() {
                return Some(items.get(n).cloned().unwrap_or(Value::Undefined));
            }
        }
        o.props
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }

    /// Own property, then the prototype chain, as AS2 property lookup does.
    fn obj_get(&self, idx: usize, name: &str) -> Value {
        let mut cur = Some(idx);
        let mut hops = 0;
        while let Some(i) = cur {
            if let Some(v) = self.obj_get_own(i, name) {
                return v;
            }
            cur = self.objects.get(i).and_then(|o| o.proto);
            hops += 1;
            if hops > 32 {
                break;
            }
        }
        Value::Undefined
    }

    fn obj_set(&mut self, idx: usize, name: &str, v: Value) {
        if let Ok(want) = std::env::var("FLASH_SETMEMBER") {
            if name == want {
                eprintln!("OBJSET {name} on object {idx} = {}", self.value_str(&v));
            }
        }
        let Some(o) = self.objects.get_mut(idx) else {
            return;
        };
        if o.items.is_some() {
            if name == "length" {
                let n = v.as_num();
                let n = if n.is_finite() && n > 0.0 {
                    (n as usize).min(MAX_ARRAY_LEN)
                } else {
                    0
                };
                if let Some(items) = o.items.as_mut() {
                    items.resize(n, Value::Undefined);
                }
                return;
            }
            if let Ok(n) = name.parse::<usize>() {
                if n >= MAX_ARRAY_LEN {
                    self.log.push(format!("array index {n} out of range"));
                    return;
                }
                let items = o.items.as_mut().expect("array");
                if items.len() <= n {
                    items.resize(n + 1, Value::Undefined);
                }
                items[n] = v;
                return;
            }
        }
        if let Some(slot) = o.props.iter_mut().find(|(k, _)| k == name) {
            slot.1 = v;
        } else {
            o.props.push((name.to_string(), v));
        }
    }

    /// The prototype object of a function, created the first time it is read so
    /// `F.prototype.m = ...` has somewhere to go.
    fn fn_proto(&mut self, id: usize) -> Value {
        while self.fn_protos.len() <= id {
            self.fn_protos.push(None);
        }
        if let Some(p) = self.fn_protos[id] {
            return Value::Object(p);
        }
        let v = self.obj_new(None, None);
        if let Value::Object(i) = v {
            self.fn_protos[id] = Some(i);
        }
        v
    }

    /// Anything an AS2 script can stringify: arrays join, objects fall back to
    /// their class name, clips to their path.
    fn value_str(&self, v: &Value) -> String {
        match v {
            Value::Object(i) => match self.objects.get(*i) {
                Some(o) if o.items.is_some() => self.array_join(*i, ","),
                Some(o) => o
                    .props
                    .iter()
                    .find(|(k, _)| k == "toString")
                    .and_then(|(_, f)| match f {
                        Value::Function(_) => None,
                        _ => Some(f.as_str()),
                    })
                    .unwrap_or_else(|| "[object Object]".to_string()),
                None => "[object Object]".into(),
            },
            other => other.as_str(),
        }
    }

    fn array_join(&self, idx: usize, sep: &str) -> String {
        match self.objects.get(idx).and_then(|o| o.items.as_ref()) {
            Some(items) => items
                .iter()
                .map(|v| self.value_str(v))
                .collect::<Vec<_>>()
                .join(sep),
            None => String::new(),
        }
    }

    /// Own and inherited property names, for `for...in`.
    fn obj_keys(&self, idx: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut cur = Some(idx);
        let mut hops = 0;
        while let Some(i) = cur {
            let Some(o) = self.objects.get(i) else { break };
            if let Some(items) = o.items.as_ref() {
                for n in 0..items.len() {
                    let k = n.to_string();
                    if !out.contains(&k) {
                        out.push(k);
                    }
                }
            }
            for (k, _) in &o.props {
                if !out.contains(k) {
                    out.push(k.clone());
                }
            }
            cur = o.proto;
            hops += 1;
            if hops > 32 {
                break;
            }
        }
        out
    }

    /// `for...in` stack setup: the end sentinel sits at the bottom and the
    /// first key ends up on top, so `StoreRegister` takes them in order.
    fn push_keys(&mut self, stack: &mut Vec<Value>, value: &Value) {
        stack.push(Value::Undefined);
        if let Value::Object(i) = value {
            let keys = self.obj_keys(*i);
            for k in keys.iter().rev() {
                stack.push(Value::Str(k.clone()));
            }
        }
    }

    /// The `addProperty` descriptor for `name`, walking the prototype chain the
    /// way a property lookup does.
    fn accessor_lookup(&self, idx: usize, name: &str) -> Option<(Option<Value>, Option<Value>)> {
        let mut cur = Some(idx);
        let mut hops = 0;
        while let Some(i) = cur {
            let Some(o) = self.objects.get(i) else {
                break;
            };
            if let Some((_, g, s)) = o.accessors.iter().find(|(k, _, _)| k == name) {
                return Some((g.clone(), s.clone()));
            }
            cur = o.proto;
            hops += 1;
            if hops > 32 {
                break;
            }
        }
        None
    }

    /// Runs a registered getter for `receiver.name`, if there is one.
    fn accessor_get(&mut self, receiver: &Value, name: &str) -> Option<Value> {
        let Value::Object(idx) = receiver else {
            return None;
        };
        let (get, _) = self.accessor_lookup(*idx, name)?;
        let get = get?;
        if let Value::Function(id) = get {
            return Some(self.invoke(id, receiver.clone(), Vec::new(), 0));
        }
        None
    }

    /// Runs a registered setter for `receiver.name`; true means the write is
    /// done and must not also land as a plain property.
    fn accessor_set(&mut self, receiver: &Value, name: &str, v: Value) -> bool {
        let Value::Object(idx) = receiver else {
            return false;
        };
        let Some((_, Some(set))) = self.accessor_lookup(*idx, name) else {
            return false;
        };
        if let Value::Function(id) = set {
            self.invoke(id, receiver.clone(), vec![v], 0);
            return true;
        }
        false
    }

    fn get_member(&mut self, obj: &Value, name: &str) -> Value {
        // FLASH_GETMEMBER=<name> reports the receiver of a member read, which is
        // how a lookup that answers undefined names the expression it came from.
        if std::env::var("FLASH_GETMEMBER").ok().is_some_and(|w| w == name) {
            eprintln!(
                "GETMEMBER {name} on {obj:?} in {:?}",
                if self.target_path.is_empty() {
                    "_root"
                } else {
                    &self.target_path
                }
            );
        }
        if let Some(v) = self.accessor_get(obj, name) {
            return v;
        }
        match obj {
            Value::Object(i) => return self.obj_get(*i, name),
            Value::Function(id) => {
                if name == "prototype" {
                    return self.fn_proto(*id);
                }
                return Value::Undefined;
            }
            Value::Str(s) if s == "Array" && name == "prototype" => return self.array(Vec::new()),
            Value::Str(_) => {}
            _ => return Value::Undefined,
        }
        if let Value::Str(o) = obj {
            if name == "length" {
                return Value::Num(o.chars().count() as f64);
            }
            match (o.as_str(), name) {
                ("Key", "LEFT") => return Value::Num(37.0),
                ("Key", "UP") => return Value::Num(38.0),
                ("Key", "RIGHT") => return Value::Num(39.0),
                ("Key", "DOWN") => return Value::Num(40.0),
                ("Key", "SPACE") => return Value::Num(32.0),
                ("Key", "ENTER") => return Value::Num(13.0),
                ("Key", "SHIFT") => return Value::Num(16.0),
                ("Key", "CONTROL") => return Value::Num(17.0),
                ("Key", "ESCAPE") => return Value::Num(27.0),
                ("Math", "PI") => return Value::Num(std::f64::consts::PI),
                ("Math", "E") => return Value::Num(std::f64::consts::E),
                ("Mouse", "_x") => return Value::Num(self.mouse.0 as f64),
                ("Mouse", "_y") => return Value::Num(self.mouse.1 as f64),
                _ => {}
            }
            if let Some(prop) = prop_id(name) {
                let v = self.prop_get(prop, &Value::Str(o.clone()));
                if v != Value::Undefined {
                    return v;
                }
            }
            if let Some(v) = self.member_var(o, name) {
                return v;
            }
            // A child clip *is* a property of its parent: `this.menu` after
            // `attachMovie("menu", "menu", 1)` has to answer the clip, or a
            // game that guards on it attaches a fresh copy every frame and its
            // intro never finishes.
            if let Some(child) = self.child_clip_path(o, name) {
                return Value::Str(child);
            }
            // A clip created from a registered symbol inherits its class's
            // methods through the prototype chain.
            if let Some(v) = self.class_member(o, name) {
                return v;
            }
        }
        Value::Undefined
    }

    /// The path of `obj.name` when that names a clip on the display list.
    fn child_clip_path(&self, obj: &str, name: &str) -> Option<String> {
        if name.is_empty() || prop_id(name).is_some() || name.starts_with('_') {
            return None;
        }
        let base = norm(obj);
        let path = if base.is_empty() || base == "_root" || base == "_level0" {
            name.to_string()
        } else {
            format!("{base}.{name}")
        };
        let parts: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
        find_instance(&self.list, &parts).map(|_| path)
    }

    /// A member lookup: the object's own property first, then the enclosing
    /// scopes, which is how a clip sees functions defined on its timeline.
    fn member_var(&self, obj: &str, name: &str) -> Option<Value> {
        // `_global.name` is a plain global variable, so `_global.BloonsTD = ...`
        // has to be visible as `BloonsTD` everywhere; keeping it under a
        // `_global.` prefix made every `new BloonsTD()` construct nothing.
        if norm(obj) == "_global" {
            if let Some(v) = self.vars.get(name) {
                return Some(v.clone());
            }
        }
        let key = norm(&format!("{obj}.{name}"));
        if let Some(v) = self.vars.get(&key) {
            return Some(v.clone());
        }
        for scope in self.var_scopes() {
            let k = if scope.is_empty() {
                name.to_string()
            } else {
                format!("{scope}.{name}")
            };
            if let Some(v) = self.vars.get(&k) {
                return Some(v.clone());
            }
        }
        None
    }

    fn set_member(&mut self, obj: &Value, name: &str, v: Value) {
        // Debug: follow one member name through every assignment
        // (FLASH_SETMEMBER=Init), which is how a class method that never lands
        // on its prototype is found.
        if let Ok(want) = std::env::var("FLASH_SETMEMBER") {
            if name == want {
                eprintln!(
                    "SETMEMBER {name} on {:?} = {} at {:.0}ms in {:?} after {:?}",
                    obj,
                    self.value_str(&v),
                    self.time_ms,
                    if self.target_path.is_empty() {
                        "_root"
                    } else {
                        &self.target_path
                    },
                    self.recent_ops
                );
                if let Some(f) = self.frames.last() {
                    eprintln!("   registers {:?}", f.registers);
                }
            }
        }
        if std::env::var_os("FLASH_VIS").is_some() {
            if let Value::Str(path) = obj {
                if path.contains("pathhit") {
                    eprintln!("PATHHIT SET {path}.{name} = {}", self.value_str(&v));
                }
            }
        }
        if let Value::Str(path) = obj {
            let full = if norm(path).is_empty() {
                name.to_string()
            } else {
                format!("{path}.{name}")
            };
            self.note_event_assign(&full, &v);
            self.note_event_assign_on(&full, &v, &format!("clip {path:?}"));
        } else {
            let on = match obj {
                Value::Object(i) => format!("object {i}"),
                other => format!("value {:?}", other.as_str()),
            };
            self.note_event_assign_on(name, &v, &on);
            if obj == &Value::Undefined && std::env::var_os("FLASH_EVENTS").is_some() {
                eprintln!(
                    "  receiver undefined in {:?} this {:?} frames {}; last ops {:?}",
                    self.target_path,
                    self.this_value,
                    self.frames.len(),
                    self.recent_ops
                );
            }
        }
        if self.accessor_set(obj, name, v.clone()) {
            return;
        }
        match obj {
            Value::Object(i) => self.obj_set(*i, name, v),
            // A bare path such as "hero" addressed a clip, whose variables live
            // in the variable table rather than an object.
            Value::Str(o) => {
                if let Some(prop) = prop_id(name) {
                    let target = Value::Str(o.clone());
                    self.prop_set(prop, &target, v);
                    return;
                }
                if norm(o) == "_global" {
                    self.vars.insert(name.to_string(), v);
                    return;
                }
                let key = norm(&format!("{o}.{name}"));
                self.vars.insert(key, v);
            }
            Value::Function(id) if name == "prototype" => {
                if let Value::Object(p) = v {
                    while self.fn_protos.len() <= *id {
                        self.fn_protos.push(None);
                    }
                    self.fn_protos[*id] = Some(p);
                }
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // User-defined functions
    // -----------------------------------------------------------------------

    /// Registers a decoded function. A named definition becomes a local where it
    /// appears; an anonymous one leaves its value on the stack for the caller.
    fn define_function(&mut self, f: avm1::Function) -> Option<Value> {
        let name = f.name.clone();
        let id = self.functions.len();
        self.functions.push(f);
        // The body's `Constant(n)` operands address the pool in force where the
        // function was written. Sharing one pool across the whole runtime made
        // every later call resolve names from an unrelated script, so a class
        // constructor assigned the wrong members and its real work vanished.
        self.fn_pools.push(self.pool.clone());
        if name.is_empty() {
            Some(Value::Function(id))
        } else {
            self.define_local(&name, Value::Function(id));
            None
        }
    }

    /// The receiver an unqualified call runs against: the current object when a
    /// method is executing, otherwise the clip path.
    fn current_this(&mut self) -> Value {
        match &self.this_value {
            Value::Object(_) => self.this_value.clone(),
            _ => Value::Str(if self.target_path.is_empty() {
                "_root".into()
            } else {
                self.target_path.clone()
            }),
        }
    }

    /// Runs a function body addressed by clip path.
    fn exec_function(&mut self, id: usize, this: &str, args: Vec<Value>, depth: u32) -> Value {
        let n = norm(this);
        let v = if n.is_empty() || n == "_root" || n == "_level0" {
            Value::Str("_root".into())
        } else {
            Value::Str(n)
        };
        self.invoke(id, v, args, depth)
    }

    /// Runs a function body with its own locals and registers. `this` is the
    /// clip whose timeline the call belongs to or the object a method was
    /// called on, so unqualified variables and `this._x` address that target.
    fn invoke(&mut self, id: usize, this: Value, args: Vec<Value>, depth: u32) -> Value {
        let Some(def) = self.functions.get(id).cloned() else {
            return Value::Undefined;
        };
        if std::env::var_os("FLASH_CALLS").is_some() {
            // Which of a movie's own functions actually run: a game that never
            // calls its own init or update method never gets past its title
            // screen, and no amount of pixel inspection shows that.
            eprintln!(
                "CALL {:?} @{} this {} args {} at {:.0}ms",
                if def.name.is_empty() { "[anon]" } else { &def.name },
                def.code_start,
                match &this {
                    Value::Object(i) => format!("object {i}"),
                    other => self.value_str(other),
                },
                args.len(),
                self.time_ms
            );
        }
        if self.frames.len() >= MAX_CALL_DEPTH {
            let name = if def.name.is_empty() {
                "[anonymous]"
            } else {
                &def.name
            };
            self.log.push(format!("call depth limit reached in {name}"));
            return Value::Undefined;
        }
        // The clip path drives variable scoping; an object receiver has none.
        // `_root` normalises to the empty path that means the main timeline.
        let this_path = match &this {
            Value::Str(s) => {
                let n = norm(s);
                if n == "_root" || n == "_level0" {
                    String::new()
                } else {
                    n
                }
            }
            _ => String::new(),
        };
        let mut frame = Frame {
            locals: Vec::new(),
            // `register_count` covers what the body may use; older bodies grow
            // the list on demand.
            registers: vec![Value::Undefined; def.register_count as usize],
            ret: Value::Undefined,
        };
        // Preloaded registers are numbered from 1 in the order Flash uses;
        // register 0 is never allocated.
        let mut r = 1usize;
        if def.flags & avm1::PRELOAD_THIS != 0 {
            let v = if def.flags & avm1::SUPPRESS_THIS != 0 {
                Value::Undefined
            } else {
                this.clone()
            };
            set_reg(&mut frame, r, v);
            r += 1;
        }
        // `arguments` is a real array holding the call's arguments, but only a
        // body that mentions it (or asks for it in a preload register) gets one:
        // a game calls its own helpers thousands of times a second, and building
        // an array per call filled the heap on its own.
        let wants_args = def.flags & avm1::PRELOAD_ARGUMENTS != 0
            || (def.flags & avm1::SUPPRESS_ARGUMENTS == 0 && def.uses_arguments);
        let args_array = if wants_args {
            self.array(args.clone())
        } else {
            Value::Undefined
        };
        if def.flags & avm1::PRELOAD_ARGUMENTS != 0 {
            set_reg(&mut frame, r, args_array.clone());
            r += 1;
        }
        if def.flags & avm1::PRELOAD_SUPER != 0 {
            set_reg(&mut frame, r, Value::Undefined);
            r += 1;
        }
        if def.flags & avm1::PRELOAD_ROOT != 0 {
            set_reg(&mut frame, r, Value::Str("_root".into()));
            r += 1;
        }
        if def.flags & avm1::PRELOAD_PARENT != 0 {
            // Flash skips this register on a root timeline (no _parent), which
            // shifts _global down by one; matching that keeps indexes right.
            if let Some((parent, _)) = this_path.rsplit_once('.') {
                set_reg(&mut frame, r, Value::Str(parent.to_string()));
                r += 1;
            }
        }
        if def.flags & avm1::PRELOAD_GLOBAL != 0 {
            set_reg(&mut frame, r, Value::Str("_global".into()));
        }
        if def.flags & avm1::SUPPRESS_ARGUMENTS == 0 && def.uses_arguments {
            frame.locals.push(("arguments".into(), args_array));
        }
        // Missing arguments read as undefined, and a register parameter has no
        // named local - only its register.
        for (i, p) in def.params.iter().enumerate() {
            let v = args.get(i).cloned().unwrap_or(Value::Undefined);
            if p.register != 0 {
                set_reg(&mut frame, p.register as usize, v);
            } else if !p.name.is_empty() {
                frame.locals.push((p.name.clone(), v));
            }
        }
        self.frames.push(frame);
        let saved_unwind = std::mem::replace(&mut self.unwinding, false);
        let saved_target = std::mem::replace(&mut self.target_path, this_path);
        let saved_this = std::mem::replace(&mut self.this_value, this);
        let saved_pool = std::mem::replace(
            &mut self.pool,
            self.fn_pools.get(id).cloned().unwrap_or_default(),
        );
        let data = self.movie.data.clone();
        // The body ends where its declared length says: a fallthrough past it
        // is the next function in the file, not part of this one.
        let end = def.code_start + def.code.len();
        self.exec_program(&data, def.code_start, end, depth + 1);
        let ret = self.frames.pop().map(|f| f.ret).unwrap_or(Value::Undefined);
        self.pool = saved_pool;
        self.unwinding = saved_unwind;
        self.target_path = saved_target;
        self.this_value = saved_this;
        ret
    }

    /// `new Ctor(args)`: an object whose prototype is `Ctor.prototype`, built by
    /// running the constructor with that object as `this`.
    fn construct(&mut self, name: &str, args: Vec<Value>, depth: u32) -> Value {
        if let Value::Function(id) = self.get_var(name) {
            return self.run_constructor_fn(id, args, depth);
        }
        match name {
            "Array" => self.array(args),
            "Object" => self.obj_new(None, None),
            "String" => Value::Str(args.first().cloned().unwrap_or(Value::Undefined).as_str()),
            "Number" => Value::Num(args.first().cloned().unwrap_or(Value::Undefined).as_num()),
            "Boolean" => Value::Bool(
                args.first().cloned().unwrap_or(Value::Undefined).as_bool(),
            ),
            // A sound object is only a handle: the host has no audio engine, so
            // volume and playback calls are accepted and dropped.
            "Sound" => Value::Str(String::from("Sound")),
            _ => {
                // A constructor name that resolves to nothing is a real gap:
                // the object it should have built is empty, and a game whose
                // controller lives there does nothing at all.
                self.miss(format!("new {name}()"));
                self.obj_new(None, None)
            }
        }
    }

    /// Builds an instance of a constructor function and runs its body with the
    /// new object as `this`. `ActionNewObject` pushes the *function value*, not
    /// its name, so resolving by name here produced an empty object and every
    /// `td = new BloonsTD()` in the movie lost its state.
    fn run_constructor_fn(&mut self, id: usize, args: Vec<Value>, depth: u32) -> Value {
        let proto = match self.fn_proto(id) {
            Value::Object(i) => Some(i),
            _ => None,
        };
        let obj = self.obj_new(proto, None);
        let ret = self.invoke(id, obj.clone(), args, depth);
        // A constructor that returns an object replaces the fresh one.
        match ret {
            Value::Object(_) => ret,
            _ => obj,
        }
    }

    /// `new` on a value: a function constructs directly, a name resolves first.
    fn construct_value(&mut self, ctor: Value, args: Vec<Value>, depth: u32) -> Value {
        match ctor {
            Value::Function(id) => self.run_constructor_fn(id, args, depth),
            Value::Object(i) => {
                // `new someObject()` treats the object as the prototype's owner.
                let obj = self.obj_new(Some(i), None);
                obj
            }
            Value::Str(name) if !name.is_empty() => self.construct(&name, args, depth),
            _ => self.obj_new(None, None),
        }
    }

    /// `ActionCallFunction` names its callee instead of pushing a value, so the
    /// name is resolved through the scope chain before the builtin table.
    fn call_named(&mut self, name: &str, args: &[Value], depth: u32) -> Value {
        if let Value::Function(id) = self.get_var(name) {
            let this = self.current_this();
            return self.invoke(id, this, args.to_vec(), depth);
        }
        self.call_function(name, args)
    }

    /// `obj.method(...)`: a member that holds a user function runs with `this`
    /// bound to the object, everything else goes to the builtin table.
    fn call_member(&mut self, obj: &Value, name: &str, args: &[Value], depth: u32) -> Value {
        if let Value::Function(id) = self.get_member(obj, name) {
            let this = match obj {
                Value::Object(_) => obj.clone(),
                Value::Str(s) if !s.is_empty() && s != "_root" => Value::Str(norm(s)),
                _ => self.current_this(),
            };
            return self.invoke(id, this, args.to_vec(), depth);
        }
        // A method that resolves to nothing is dropped silently by AVM1, so a
        // missing prototype method looks exactly like one that does nothing.
        if let Ok(want) = std::env::var("FLASH_METHOD") {
            if name.eq_ignore_ascii_case(&want) {
                eprintln!(
                    "NOMETHOD {name} on {:?} in {:?} after {:?}",
                    obj,
                    if self.target_path.is_empty() {
                        "_root"
                    } else {
                        &self.target_path
                    },
                    self.recent_ops
                );
            }
        }
        self.call_method(obj, name, args, depth)
    }

    /// Runs the timers whose delay has elapsed.
    ///
    /// They follow the movie clock rather than the frame loop: a preloader that
    /// polls for its data keeps ticking while the playhead is stopped, so a
    /// frame-driven timer would never fire.
    pub fn run_timers(&mut self) {
        if self.timers.is_empty() {
            return;
        }
        let now = self.time_ms;
        let due: Vec<usize> = self
            .timers
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.canceled && t.due <= now)
            .map(|(i, _)| i)
            .collect();
        for i in due {
            let Some(t) = self.timers.get(i).cloned() else {
                continue;
            };
            if t.canceled {
                continue;
            }
            // Reschedule before running, so a one-shot is gone by the time its
            // own body looks at the list, and an interval that clears itself
            // from inside does not run again.
            if let Some(slot) = self.timers.get_mut(i) {
                match t.period {
                    Some(period) => slot.due = now + period.max(1.0),
                    None => slot.canceled = true,
                }
            }
            self.call_timer(&t);
        }
        self.timers.retain(|t| !t.canceled);
    }

    /// Invokes one timer callback with the scheduling timeline as `this`.
    fn call_timer(&mut self, t: &Timer) {
        let saved_path = std::mem::take(&mut self.target_path);
        let saved_this = self.this_value.clone();
        let this = if t.path.is_empty() {
            Value::Str("_root".into())
        } else {
            Value::Str(t.path.clone())
        };
        self.target_path = t.path.clone();
        self.this_value = this.clone();
        match &t.func {
            Value::Function(id) => {
                self.invoke(*id, this, t.args.clone(), 1);
            }
            // `setInterval("tick()", 100)` names code to run; the name is also
            // accepted with the call parentheses stripped.
            Value::Str(s) => {
                let base = s.trim();
                let bare = base.trim_end_matches(')').rsplit('(').next().unwrap_or(base);
                match self.get_var(bare.trim()) {
                    Value::Function(id) => {
                        self.invoke(id, this, t.args.clone(), 1);
                    }
                    _ => self.miss(format!("timer target {s:?}")),
                }
            }
            other => self.miss(format!("timer target {other:?}")),
        }
        self.target_path = saved_path;
        self.this_value = saved_this;
    }

    fn call_function(&mut self, name: &str, args: &[Value]) -> Value {
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Undefined);
        match name {
            "random" => {
                let max = arg(0).as_num().max(1.0);
                Value::Num((self.next_rand() * max).floor())
            }
            "getTimer" => Value::Num(self.time_ms),
            // AVM1 timers. A movie polls through these while its playhead sits
            // still - a preloader waiting for its data never continues when
            // they are missing, because the function call is simply dropped.
            "setInterval" | "setTimeout" => {
                let ms = arg(1).as_num();
                // Flash treats a missing or nonsense delay as "as soon as it can".
                let ms = if ms > 0.0 { ms } else { 10.0 };
                let id = self.next_timer_id;
                self.next_timer_id += 1;
                self.timers.push(Timer {
                    id,
                    due: self.time_ms + ms,
                    period: if name == "setInterval" { Some(ms) } else { None },
                    func: arg(0),
                    args: if args.len() > 2 {
                        args[2..].to_vec()
                    } else {
                        Vec::new()
                    },
                    path: self.target_path.clone(),
                    canceled: false,
                });
                Value::Num(id as f64)
            }
            "clearInterval" | "clearTimeout" => {
                let id = arg(0).as_num();
                for t in self.timers.iter_mut() {
                    if t.id as f64 == id {
                        t.canceled = true;
                    }
                }
                Value::Undefined
            }
            "trace" => {
                let text = args
                    .iter()
                    .map(|v| self.value_str(v))
                    .collect::<Vec<_>>()
                    .join(" ");
                if self.log.len() < 200 {
                    self.log.push(text);
                }
                Value::Undefined
            }
            "Number" => Value::Num(arg(0).as_num()),
            "String" => Value::Str(self.value_str(&arg(0))),
            "Boolean" => Value::Bool(arg(0).as_bool()),
            "Array" => {
                let items = match args.first() {
                    // Array(3) is a length, Array(1, 2) is two elements.
                    Some(Value::Num(n)) if args.len() == 1 && *n >= 0.0 && *n < MAX_ARRAY_LEN as f64 => {
                        vec![Value::Undefined; *n as usize]
                    }
                    _ => args.to_vec(),
                };
                self.array(items)
            }
            "Object" => self.obj_new(None, None),
            // Function(f) returns f when it is already a function.
            "Function" => match arg(0) {
                Value::Function(_) => arg(0),
                _ => Value::Undefined,
            },
            // The URL codecs are part of AS2's globals and movies use them to
            // hide and rebuild strings, so they have to be exact.
            "escape" => Value::Str(escape_str(&self.value_str(&arg(0)))),
            "unescape" => Value::Str(unescape_str(&self.value_str(&arg(0)))),
            "int" => Value::Num(arg(0).as_num().trunc()),
            "parseInt" => Value::Num(arg(0).as_str().trim().parse::<f64>().unwrap_or(0.0).trunc()),
            "parseFloat" => Value::Num(arg(0).as_str().trim().parse::<f64>().unwrap_or(0.0)),
            "isNaN" => Value::Bool(arg(0).as_num().is_nan()),
            "chr" => Value::Str(
                char::from_u32(arg(0).as_num() as u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            ),
            "ord" => Value::Num(
                arg(0)
                    .as_str()
                    .chars()
                    .next()
                    .map(|c| c as u32 as f64)
                    .unwrap_or(0.0),
            ),
            // AS2 class plumbing that only matters inside Flash itself.
            "ASSetPropFlags" | "ASSetPropFlags2" | "ASConstructor" => Value::Undefined,
            "classConstruct" => Value::Undefined,
            // Navigations and sound control are deliberately inert: the app
            // has no browser to navigate and audio output is not wired up.
            "getURL" | "stopAllSounds" | "fscommand" | "updateAfterEvent" => Value::Undefined,
            // The whole movie is already in memory, so a preloader asking for
            // progress is always done.
            "getBytesLoaded" | "getBytesTotal" => Value::Num(1.0),
            "getSWFVersion" => Value::Num(8.0),
            "Object.registerClass" => {
                self.register_class(&arg(0).as_str(), &arg(1));
                Value::Bool(true)
            }
            "attachMovie" => {
                let link = arg(0).as_str();
                let new_name = arg(1).as_str();
                let depth = arg(2).as_num().max(0.0) as u16;
                let target = self.target_path.clone();
                self.attach_clip(&link, &new_name, depth, &target);
                // An `initObj` seeds the new clip's variables.
                let init = arg(3);
                if let Value::Object(i) = init {
                    let path = if target.is_empty() {
                        new_name.clone()
                    } else {
                        format!("{target}.{new_name}")
                    };
                    let props = self
                        .objects
                        .get(i)
                        .map(|o| o.props.clone())
                        .unwrap_or_default();
                    for (k, v) in props {
                        let key = norm(&format!("{path}.{k}"));
                        self.vars.insert(key, v);
                    }
                }
                // The new clip is the result. Compiled ActionScript uses
                // `var t = holder.attachMovie(...)` and then sets `t._x`, so
                // returning nothing left the object it had just created at the
                // origin and in its "can't place" state.
                Value::Str(self.child_path(&target, &new_name))
            }
            "createEmptyMovieClip" => {
                let new_name = arg(0).as_str();
                let depth = arg(1).as_num().max(0.0) as u16;
                let target = self.target_path.clone();
                self.empty_clip(&new_name, depth, &target);
                Value::Str(self.child_path(&target, &new_name))
            }
            "duplicateMovieClip" => {
                let target = arg(0).as_str();
                let new_name = arg(1).as_str();
                let depth = arg(2).as_num().max(0.0) as u16;
                self.duplicate_clip(&target, &new_name, depth);
                Value::Undefined
            }
            "removeMovieClip" => {
                let target = arg(0).as_str();
                self.remove_clip(&target);
                Value::Undefined
            }
            "unloadMovie" => Value::Undefined,
            _ => {
                self.miss(format!("{name}()"));
                Value::Undefined
            }
        }
    }

    // -----------------------------------------------------------------------
    // MovieClip methods
    // -----------------------------------------------------------------------

    /// Runs `clip.method(...)`. Games call the methods far more often than the
    /// equivalent actions (`balloon.gotoAndStop(2)`, `this.swapDepths(n)`), and
    /// every one of them drives the same display-list state the actions do.
    fn clip_method(&mut self, path: &str, name: &str, args: &[Value], depth: u32) -> Option<Value> {
        const NAMES: [&str; 17] = [
            "play",
            "stop",
            "nextFrame",
            "prevFrame",
            "gotoAndPlay",
            "gotoAndStop",
            "getDepth",
            "getNextHighestDepth",
            "swapDepths",
            "attachMovie",
            "createEmptyMovieClip",
            "duplicateMovieClip",
            "removeMovieClip",
            "getBytesLoaded",
            "getBytesTotal",
            "hitTest",
            "getBounds",
        ];
        if !NAMES.contains(&name) {
            return None;
        }
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Undefined);
        let owned = self.prop_parts(&Value::Str(path.to_string()));
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let full = refs.join(".");
        let is_root = refs.is_empty();
        // Debug: FLASH_CLIPLOG=winpanel follows transport calls on one clip
        // (path substring), which is how a panel that opens on its own is
        // traced back to the code that started it.
        if let Ok(want) = std::env::var("FLASH_CLIPLOG") {
            if full.contains(&want) {
                eprintln!(
                    "CLIP {full} {name}({:?}) at {:.0}ms from {:?} after {:?}",
                    args.iter().map(|v| self.value_str(v)).collect::<Vec<_>>(),
                    self.time_ms,
                    if self.target_path.is_empty() {
                        "_root"
                    } else {
                        &self.target_path
                    },
                    self.recent_ops
                );
            }
        }
        match name {
            "play" | "stop" => {
                let playing = name == "play";
                if is_root {
                    self.playing = playing;
                } else {
                    with_instance_mut(&mut self.list, &refs, |inst| {
                        if let Some(sp) = inst.sprite.as_mut() {
                            sp.playing = playing;
                        }
                    });
                }
                Some(Value::Undefined)
            }
            "nextFrame" | "prevFrame" => {
                let delta = if name == "nextFrame" { 1 } else { -1 };
                self.jump_clip(&refs, Jump::Delta(delta), true, depth);
                Some(Value::Undefined)
            }
            "gotoAndPlay" | "gotoAndStop" => {
                let play = name == "gotoAndPlay";
                let t = arg(0);
                if std::env::var_os("FLASH_NAV").is_some() {
                    eprintln!(
                        "NAV {name}({}) on {:?}",
                        self.value_str(&t),
                        full
                    );
                }
                // A numeric argument is an absolute 1-based frame, so
                // `gotoAndStop(1)` is index 0 *whatever* frame the clip is on.
                // Treating it as a delta from the current frame made
                // `gotoAndStop(1)` a no-op on any clip that had moved, which is
                // how a "you can build here" indicator stayed on "can't build"
                // and every click refused to place a tower.
                let jump = match &t {
                    Value::Str(_) => Jump::Label(t.as_str()),
                    _ => Jump::Abs((t.as_num() as i32 - 1).max(0) as usize),
                };
                self.jump_clip(&refs, jump, play, depth);
                Some(Value::Undefined)
            }
            "getDepth" => Some(Value::Num(
                find_instance(&self.list, &refs)
                    .map(|i| i.depth as f64)
                    .unwrap_or(0.0),
            )),
            "getNextHighestDepth" => {
                let top = if is_root {
                    self.list.keys().next_back().copied()
                } else {
                    find_instance(&self.list, &refs)
                        .and_then(|i| i.sprite.as_ref())
                        .and_then(|sp| sp.list.keys().next_back().copied())
                };
                Some(Value::Num(top.map(|d| d as f64 + 1.0).unwrap_or(1.0)))
            }
            "swapDepths" => {
                let other = arg(0);
                let other_depth = match &other {
                    Value::Str(_) => {
                        let op = self.prop_parts(&other);
                        let oref: Vec<&str> = op.iter().map(|s| s.as_str()).collect();
                        find_instance(&self.list, &oref).map(|i| i.depth)
                    }
                    _ => Some(other.as_num().max(0.0) as u16),
                };
                let Some(my_depth) = find_instance(&self.list, &refs).map(|i| i.depth) else {
                    return Some(Value::Undefined);
                };
                let Some(od) = other_depth else {
                    return Some(Value::Undefined);
                };
                if od != my_depth {
                    let parent = &refs[..refs.len().saturating_sub(1)];
                    if let Some(list) = nested_list_mut(&mut self.list, parent) {
                        let a = list.remove(&my_depth);
                        let b = list.remove(&od);
                        if let Some(mut a) = a {
                            a.depth = od;
                            list.insert(od, a);
                        }
                        if let Some(mut b) = b {
                            b.depth = my_depth;
                            list.insert(my_depth, b);
                        }
                    }
                }
                Some(Value::Undefined)
            }
            "attachMovie" => {
                let link = arg(0).as_str();
                let new_name = arg(1).as_str();
                let d = arg(2).as_num().max(0.0) as u16;
                self.attach_clip(&link, &new_name, d, &full);
                Some(Value::Str(self.child_path(&full, &new_name)))
            }
            "createEmptyMovieClip" => {
                let new_name = arg(0).as_str();
                let d = arg(1).as_num().max(0.0) as u16;
                self.empty_clip(&new_name, d, &full);
                Some(Value::Str(self.child_path(&full, &new_name)))
            }
            "duplicateMovieClip" => {
                if is_root {
                    return Some(Value::Undefined);
                }
                let new_name = arg(0).as_str();
                let d = arg(1).as_num().max(0.0) as u16;
                self.duplicate_clip(&full, &new_name, d);
                Some(Value::Str(self.child_path(&full, &new_name)))
            }
            "removeMovieClip" => {
                if is_root {
                    return Some(Value::Undefined);
                }
                self.remove_clip(&full);
                Some(Value::Undefined)
            }
            "getBytesLoaded" | "getBytesTotal" => Some(Value::Num(1.0)),
            "hitTest" => {
                let b = path_bounds(&self.movie, &self.list, &refs, &Mat::IDENTITY);
                let Some(b) = b else {
                    return Some(Value::Bool(false));
                };
                let first = arg(0);
                if let Value::Str(_) = first {
                    let op = self.prop_parts(&first);
                    let oref: Vec<&str> = op.iter().map(|s| s.as_str()).collect();
                    let ob = path_bounds(&self.movie, &self.list, &oref, &Mat::IDENTITY);
                    return Some(Value::Bool(match ob {
                        Some(o) => !(b.2 < o.0 || o.2 < b.0 || b.3 < o.1 || o.3 < b.1),
                        None => false,
                    }));
                }
                let x = arg(0).as_num();
                let y = arg(1).as_num();
                Some(Value::Bool(
                    x >= b.0 as f64 && x <= b.2 as f64 && y >= b.1 as f64 && y <= b.3 as f64,
                ))
            }
            "getBounds" => {
                let b = path_bounds(&self.movie, &self.list, &refs, &Mat::IDENTITY);
                let obj = self.obj_new(None, None);
                if let (Value::Object(idx), Some(b)) = (obj.clone(), b) {
                    self.obj_set(idx, "xMin", Value::Num(b.0 as f64));
                    self.obj_set(idx, "yMin", Value::Num(b.1 as f64));
                    self.obj_set(idx, "xMax", Value::Num(b.2 as f64));
                    self.obj_set(idx, "yMax", Value::Num(b.3 as f64));
                }
                Some(obj)
            }
            _ => None,
        }
    }

    /// Moves a clip's playhead: `Jump::Abs` is a 0-based frame, `Label` names
    /// one. Re-entering a frame rebuilds that clip's display list from the
    /// frames before it, which is what Flash shows after a jump backwards.
    fn jump_clip(&mut self, parts: &[&str], jump: Jump, play: bool, depth: u32) {
        if parts.is_empty() {
            let n = self.frame_count();
            let idx = match jump {
                Jump::Abs(i) => i % n,
                Jump::Delta(d) => (self.frame as i32 + d).rem_euclid(n as i32) as usize,
                Jump::Label(name) => match self.movie.root.labels.get(&name) {
                    Some(&i) => i % n,
                    None => return,
                },
            };
            // Move the playhead; do not rebuild the world. Clearing the list
            // and replaying every frame from the start recreated every clip
            // from its first frame, so a script that sends the timeline to the
            // frame it is already on (a common "hold here until done" loop)
            // restarted the animation it was waiting for, forever.
            self.playing = play;
            self.enter_frame(idx, depth + 1);
            return;
        }
        let movie = &self.movie;
        let mut codes: Vec<Code> = Vec::new();
        with_instance_mut(&mut self.list, parts, |inst| {
            let Some(sp) = inst.sprite.as_mut() else {
                return;
            };
            let Some(tl) = movie.sprites.get(&sp.timeline) else {
                return;
            };
            let total = tl.frames.len().max(1);
            let idx = match jump {
                Jump::Abs(i) => i % total,
                Jump::Delta(d) => (sp.frame as i32 + d).rem_euclid(total as i32) as usize,
                Jump::Label(name) => match tl.labels.get(&name) {
                    Some(&i) => i % total,
                    None => return,
                },
            };
            // Move this clip's playhead the way Flash does: keep the objects
            // that the target frame also places (so their own playheads and
            // nested clips survive), drop only what belongs to later frames,
            // and apply the frames being passed over.
            let from = sp.frame;
            if idx <= from {
                sp.list
                    .retain(|_, i| i.placed_frame.map_or(true, |f| f <= idx));
            }
            let first = if idx > from { from + 1 } else { idx };
            for f in first..=idx {
                apply_ops(movie, &mut sp.list, &tl.frames[f].ops, depth + 1, f);
            }
            sp.frame = idx;
            sp.playing = play;
            codes = tl.frames[idx].actions.clone();
        });
        if !codes.is_empty() {
            let saved = std::mem::replace(&mut self.target_path, parts.join("."));
            for code in codes {
                self.exec_block(&code, depth + 1);
            }
            self.target_path = saved;
        }
    }

    fn call_method(&mut self, obj: &Value, name: &str, args: &[Value], depth: u32) -> Value {
        match obj {
            Value::Object(i) => return self.object_method(*i, name, args, depth),
            Value::Function(id) => {
                return match name {
                    // f.call(thisArg, ...args)
                    "call" => {
                        let this = args.first().cloned().unwrap_or(Value::Undefined);
                        let rest = args.get(1..).unwrap_or(&[]).to_vec();
                        self.invoke(*id, this, rest, depth)
                    }
                    // f.apply(thisArg, argArray)
                    "apply" => {
                        let this = args.first().cloned().unwrap_or(Value::Undefined);
                        let list = match args.get(1) {
                            Some(Value::Object(a)) => self.array_items(*a),
                            _ => Vec::new(),
                        };
                        self.invoke(*id, this, list, depth)
                    }
                    "toString" => Value::Str("[type Function]".into()),
                    _ => Value::Undefined,
                };
            }
            Value::Str(s) => {
                let text = s.clone();
                if let Some(v) = self.clip_method(&text, name, args, depth) {
                    return v;
                }
                if let Some(v) = self.string_method(&text, name, args) {
                    return v;
                }
            }
            _ => {}
        }
        let o = obj.as_str();
        // `_global.parseInt(...)`, `_global.ASSetPropFlags(...)` and the rest of
        // the global functions are ordinary members of the global object, so
        // anything called through it is just a builtin call by name.
        if o == "_global" {
            return self.call_named(name, args, depth);
        }
        let a0 = args.first().cloned().unwrap_or(Value::Undefined);
        let a1 = args.get(1).cloned().unwrap_or(Value::Undefined);
        if o == "Object" && name == "registerClass" {
            self.register_class(&a0.as_str(), &a1);
            return Value::Bool(true);
        }
        // A method the clip inherits from its registered class.
        if !o.is_empty() && o != "undefined" {
            if let Some(Value::Function(id)) = self.class_member(&o, name) {
                let this = Value::Str(norm(&o));
                return self.invoke(id, this, args.to_vec(), depth);
            }
        }
        match (o.as_str(), name) {
            // MochiAds preloader and inter-level ads: games wait for the ad
            // callback before revealing the stage, so reporting a finished ad
            // immediately is what lets a movie start. This is the same shim
            // Ruffle ships; without it a movie stays behind its ad cover.
            ("MochiAd", "showPreloaderAd")
            | ("MochiAd", "showPreGameAd")
            | ("MochiAd", "showInterLevelAd")
            | ("MochiAd", "showTimedAd") => {
                self.mochi_ad_finished(args, depth);
                Value::Undefined
            }
            ("MochiAd", "getVersion") => Value::Str(String::from("3.0")),
            ("MochiAd", "isRunning") => Value::Bool(false),
            ("MochiBot", _) => Value::Undefined,
            ("Key", "isDown") => {
                let code = a0.as_num() as usize;
                Value::Bool(self.keys.get(code).copied().unwrap_or(false))
            }
            ("Key", "getCode") => Value::Num(self.last_key as f64),
            ("Key", "isToggled") => Value::Bool(false),
            ("Mouse", "hide") | ("Mouse", "show") => Value::Undefined,
            ("Sound", "setVolume") | ("Sound", "getVolume") => Value::Num(100.0),
            ("Sound", "setPan") | ("Sound", "getPan") => Value::Num(0.0),
            ("Sound", "attachSound")
            | ("Sound", "start")
            | ("Sound", "stop")
            | ("Sound", "setTransform")
            | ("Sound", "loadSound") => Value::Undefined,
            ("Math", "random") => Value::Num(self.next_rand()),
            ("Math", "floor") => Value::Num(a0.as_num().floor()),
            ("Math", "ceil") => Value::Num(a0.as_num().ceil()),
            ("Math", "round") => Value::Num(a0.as_num().round()),
            ("Math", "abs") => Value::Num(a0.as_num().abs()),
            ("Math", "sqrt") => Value::Num(a0.as_num().sqrt()),
            ("Math", "sin") => Value::Num(a0.as_num().sin()),
            ("Math", "cos") => Value::Num(a0.as_num().cos()),
            ("Math", "tan") => Value::Num(a0.as_num().tan()),
            ("Math", "atan2") => Value::Num(a0.as_num().atan2(a1.as_num())),
            ("Math", "min") => Value::Num(a0.as_num().min(a1.as_num())),
            ("Math", "max") => Value::Num(a0.as_num().max(a1.as_num())),
            ("Math", "pow") => Value::Num(a0.as_num().powf(a1.as_num())),
            _ => {
                // `FLASH_UNDEF=1` names the call sites whose receiver resolved to
                // undefined; without it a game silently does nothing forever.
                if std::env::var_os("FLASH_UNDEF").is_some() {
                    eprintln!(
                        "UNDEFCALL in {}: {o}.{name}({})",
                        if self.target_path.is_empty() {
                            "_root"
                        } else {
                            &self.target_path
                        },
                        args.iter()
                            .map(|v| self.value_str(v))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                self.miss(format!("{o}.{name}()"));
                Value::Undefined
            }
        }
    }

    /// Reports a MochiAds slot as finished: the callback argument (the last
    /// function passed to `MochiAd.show*Ad`) runs straight away with a success
    /// object, which is what a movie waits on before it starts playing.
    fn mochi_ad_finished(&mut self, args: &[Value], depth: u32) {
        let callback = args.iter().rev().find_map(|a| match a {
            Value::Function(id) => Some(*id),
            _ => None,
        });
        let Some(id) = callback else {
            return;
        };
        let result = match self.obj_new(None, None) {
            Value::Object(i) => {
                self.obj_set(i, "success", Value::Bool(true));
                self.obj_set(i, "ad_failed", Value::Bool(false));
                self.obj_set(i, "ad_loaded", Value::Bool(true));
                self.obj_set(i, "ad_started", Value::Bool(true));
                Value::Object(i)
            }
            other => other,
        };
        let this = self.current_this();
        self.invoke(id, this, vec![result], depth + 1);
    }

    fn array_items(&self, idx: usize) -> Vec<Value> {
        self.objects
            .get(idx)
            .and_then(|o| o.items.clone())
            .unwrap_or_default()
    }

    /// Array and Object methods. Only a minimal but useful set: enough for
    /// list handling and for calling a function stored on an object.
    fn object_method(&mut self, idx: usize, name: &str, args: &[Value], _depth: u32) -> Value {
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Undefined);
        let is_array = self
            .objects
            .get(idx)
            .map(|o| o.items.is_some())
            .unwrap_or(false);
        if is_array {
            match name {
                "push" => {
                    if let Some(items) = self.objects[idx].items.as_mut() {
                        items.extend(args.iter().cloned());
                        return Value::Num(items.len() as f64);
                    }
                }
                "pop" => {
                    if let Some(items) = self.objects[idx].items.as_mut() {
                        return items.pop().unwrap_or(Value::Undefined);
                    }
                }
                "shift" => {
                    if let Some(items) = self.objects[idx].items.as_mut() {
                        return if items.is_empty() {
                            Value::Undefined
                        } else {
                            items.remove(0)
                        };
                    }
                }
                "unshift" => {
                    if let Some(items) = self.objects[idx].items.as_mut() {
                        for (k, v) in args.iter().enumerate() {
                            items.insert(k, v.clone());
                        }
                        return Value::Num(items.len() as f64);
                    }
                }
                "reverse" => {
                    if let Some(items) = self.objects[idx].items.as_mut() {
                        items.reverse();
                    }
                    return Value::Object(idx);
                }
                "join" => {
                    let sep = if args.is_empty() {
                        ",".to_string()
                    } else {
                        self.value_str(&arg(0))
                    };
                    return Value::Str(self.array_join(idx, &sep));
                }
                "toString" => return Value::Str(self.array_join(idx, ",")),
                "indexOf" => {
                    let needle = arg(0);
                    let items = self.array_items(idx);
                    for (i, v) in items.iter().enumerate() {
                        if self.values_equal(v, &needle) {
                            return Value::Num(i as f64);
                        }
                    }
                    return Value::Num(-1.0);
                }
                "slice" => {
                    let items = self.array_items(idx);
                    let n = items.len() as f64;
                    // A missing or undefined argument means "from the start" /
                    // "to the end", not zero.
                    let mut start = match args.first() {
                        Some(Value::Undefined) | None => 0.0,
                        Some(v) => v.as_num(),
                    };
                    if start < 0.0 {
                        start += n;
                    }
                    let mut end = match args.get(1) {
                        Some(Value::Undefined) | None => n,
                        Some(v) => v.as_num(),
                    };
                    if end < 0.0 {
                        end += n;
                    }
                    let s = start.max(0.0).min(n) as usize;
                    let e = end.max(0.0).min(n) as usize;
                    let out: Vec<Value> = if s < e {
                        items[s..e].to_vec()
                    } else {
                        Vec::new()
                    };
                    return self.array(out);
                }
                "concat" => {
                    let mut items = self.array_items(idx);
                    for a in args {
                        match a {
                            Value::Object(oi) => items.extend(self.array_items(*oi)),
                            other => items.push(other.clone()),
                        }
                    }
                    return self.array(items);
                }
                "splice" => {
                    // splice() with no arguments changes nothing and returns an
                    // empty array; splice(start) removes to the end.
                    if args.is_empty() {
                        return self.array(Vec::new());
                    }
                    let items = self.array_items(idx);
                    let n = items.len() as f64;
                    let mut start = arg(0).as_num();
                    if start < 0.0 {
                        start += n;
                    }
                    let s = start.max(0.0).min(n) as usize;
                    let count = match args.get(1) {
                        None => items.len().saturating_sub(s),
                        Some(v) => v.as_num().max(0.0) as usize,
                    };
                    let removed: Vec<Value> = items[s..(s + count).min(items.len())].to_vec();
                    if let Some(list) = self.objects[idx].items.as_mut() {
                        list.splice(s..(s + count).min(list.len()), args.iter().skip(2).cloned());
                    }
                    return self.array(removed);
                }
                _ => {}
            }
        }
        // `mx.utils.Delegate.create(obj, "method")` and its older spelling.
        if matches!(name, "create" | "createDelegate")
            && self
                .objects
                .get(idx)
                .is_some_and(|o| o.props.iter().any(|(k, _)| k == "__delegateClass"))
        {
            return self.make_delegate(arg(0), arg(1));
        }
        match name {
            // Property descriptors: a missing getter or setter means "no access
            // on this side", which Flash spells as passing null.
            "addProperty" => {
                let prop = arg(0).as_str();
                let get = arg(1);
                let set = arg(2);
                let pick = |v: &Value| match v {
                    Value::Function(_) => Some(v.clone()),
                    _ => None,
                };
                let entry = (prop.clone(), pick(&get), pick(&set));
                if let Some(o) = self.objects.get_mut(idx) {
                    if let Some(slot) = o.accessors.iter_mut().find(|(k, _, _)| *k == prop) {
                        *slot = entry;
                    } else {
                        o.accessors.push(entry);
                    }
                }
                Value::Bool(true)
            }
            "removeProperty" | "unwatch" => Value::Bool(true),
            "watch" => Value::Bool(true),
            "toString" => Value::Str(self.value_str(&Value::Object(idx))),
            "valueOf" => Value::Object(idx),
            "hasOwnProperty" => Value::Bool(self.obj_get_own(idx, &arg(0).as_str()).is_some()),
            _ => {
                self.miss(format!("object.{name}()"));
                Value::Undefined
            }
        }
    }

    fn values_equal(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => x == y,
            (Value::Function(x), Value::Function(y)) => x == y,
            (Value::Object(_), _) | (_, Value::Object(_)) => false,
            (Value::Num(x), Value::Num(y)) => x == y,
            _ => a.as_str() == b.as_str(),
        }
    }

    /// String methods, so `name.charAt(0)` and friends work on plain strings.
    fn string_method(&mut self, s: &str, name: &str, args: &[Value]) -> Option<Value> {
        let a0 = args.first().cloned().unwrap_or(Value::Undefined);
        let a1 = args.get(1).cloned().unwrap_or(Value::Undefined);
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len() as f64;
        let clamp = |v: f64, len: f64| -> usize {
            let mut i = v;
            if i < 0.0 {
                i += len;
            }
            i.max(0.0).min(len) as usize
        };
        let out = match name {
            "charAt" => Value::Str(
                chars
                    .get(a0.as_num() as usize)
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            ),
            "charCodeAt" => Value::Num(
                chars
                    .get(a0.as_num() as usize)
                    .map(|c| *c as u32 as f64)
                    .unwrap_or(f64::NAN),
            ),
            "indexOf" => {
                let needle = a0.as_str();
                Value::Num(
                    s.find(&needle)
                        .map(|i| s[..i].chars().count() as f64)
                        .unwrap_or(-1.0),
                )
            }
            "lastIndexOf" => {
                let needle = a0.as_str();
                Value::Num(
                    s.rfind(&needle)
                        .map(|i| s[..i].chars().count() as f64)
                        .unwrap_or(-1.0),
                )
            }
            "substring" => {
                let mut from = a0.as_num();
                let mut to = if args.len() > 1 { a1.as_num() } else { n };
                if from > to {
                    std::mem::swap(&mut from, &mut to);
                }
                let f = clamp(from, n);
                let t = clamp(to, n);
                Value::Str(chars[f..t].iter().collect())
            }
            "substr" => {
                let start = a0.as_num();
                let from = clamp(start, n);
                let len = if args.len() > 1 {
                    a1.as_num().max(0.0) as usize
                } else {
                    chars.len() - from
                };
                let to = (from + len).min(chars.len());
                Value::Str(chars[from..to].iter().collect())
            }
            "toUpperCase" => Value::Str(s.to_uppercase()),
            "toLowerCase" => Value::Str(s.to_lowercase()),
            "concat" => Value::Str(format!("{s}{}", self.value_str(&a0))),
            "split" => {
                let sep = a0.as_str();
                let parts: Vec<Value> = if sep.is_empty() {
                    chars.iter().map(|c| Value::Str(c.to_string())).collect()
                } else {
                    s.split(&sep).map(|p| Value::Str(p.to_string())).collect()
                };
                self.array(parts)
            }
            "toString" => Value::Str(s.to_string()),
            _ => return None,
        };
        Some(out)
    }

    // -----------------------------------------------------------------------
    // Built-in properties
    // -----------------------------------------------------------------------

    fn prop_get(&self, prop: u16, target: &Value) -> Value {
        let owned = self.prop_parts(target);
        let parts: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        if parts.is_empty() {
            return match prop {
                0 | 1 => Value::Num(0.0),
                2 | 3 | 6 => Value::Num(100.0),
                4 => Value::Num((self.frame + 1) as f64),
                5 | 12 => Value::Num(self.frame_count() as f64),
                7 => Value::Num(1.0),
                // Flash reports the main timeline's path-style name as "/".
                11 => Value::Str(String::new()),
                13 => Value::Str("/".into()),
                15 => Value::Str(String::new()),
                16 => Value::Num(2.0),
                17 => Value::Bool(true),
                18 => Value::Num(5.0),
                19 => Value::Str("HIGH".into()),
                20 => Value::Num(self.mouse.0 as f64),
                21 => Value::Num(self.mouse.1 as f64),
                _ => Value::Num(0.0),
            };
        }
        let Some(inst) = find_instance(&self.list, &parts) else {
            return Value::Undefined;
        };
        let frames = inst
            .sprite
            .as_ref()
            .and_then(|s| self.movie.sprites.get(&s.timeline))
            .map(|t| t.frames.len() as f64)
            .unwrap_or(1.0);
        match prop {
            0 => Value::Num(inst.matrix.tx as f64),
            1 => Value::Num(inst.matrix.ty as f64),
            2 => Value::Num(inst.matrix.scale_x as f64 * 100.0),
            3 => Value::Num(inst.matrix.scale_y as f64 * 100.0),
            4 => Value::Num(
                inst.sprite
                    .as_ref()
                    .map(|s| s.frame as f64 + 1.0)
                    .unwrap_or(1.0),
            ),
            5 | 12 => Value::Num(frames),
            6 => Value::Num(inst.cxform.mul[3] as f64 * 100.0),
            7 => Value::Num(if inst.visible { 1.0 } else { 0.0 }),
            10 => Value::Num(
                (inst.matrix.rot_skew_0 as f64)
                    .atan2(inst.matrix.scale_x as f64)
                    .to_degrees(),
            ),
            13 => Value::Str(inst.name.clone().unwrap_or_default()),
            // `_xmouse` is the pointer in *this clip's* coordinates, not on the
            // stage. Games hit-test with it, so reporting stage coordinates made
            // every spot look wrong: a tower preview stayed in its "can't place"
            // state and no click ever built a tower.
            20 | 21 => {
                let world = self.world_of(&parts).unwrap_or(inst.matrix);
                match world.invert() {
                    Some(inv) => {
                        let (lx, ly) = inv.apply(self.mouse.0, self.mouse.1);
                        Value::Num(if prop == 20 { lx as f64 } else { ly as f64 })
                    }
                    None => Value::Num(0.0),
                }
            }
            _ => Value::Num(0.0),
        }
    }

    /// The stage transform of the instance at `parts`, by multiplying down the
    /// path. Clip-local pointer properties depend on it.
    fn world_of(&self, parts: &[&str]) -> Option<Mat> {
        let mut m = Mat::IDENTITY;
        let mut list = &self.list;
        for (i, seg) in parts.iter().enumerate() {
            let inst = list.values().find(|ins| ins.answers_to(seg))?;
            m = m.mul(&inst.matrix);
            if i + 1 == parts.len() {
                return Some(m);
            }
            list = &inst.sprite.as_ref()?.list;
        }
        Some(m)
    }

    fn prop_set(&mut self, prop: u16, target: &Value, v: Value) {
        if std::env::var_os("FLASH_VIS").is_some()
            && matches!(prop, 6 | 7)
            && self.value_str(target).is_empty()
        {
            // A property write whose target resolved to nothing is lost, and
            // these are the writes that hide menus, so say where they came from.
            eprintln!(
                "VIS-EMPTY prop {prop} target {:?} = {} in {:?} this {:?} last {:?}",
                target,
                self.value_str(&v),
                self.target_path,
                self.this_value,
                self.recent_ops
            );
        }
        if (self.trace || std::env::var_os("FLASH_VIS").is_some()) && matches!(prop, 6 | 7) {
            // Who hides what: the display order only makes sense once the
            // scripts' visibility changes are visible too.
            eprintln!(
                "VIS prop {prop} {:?} -> {}",
                self.value_str(target),
                self.value_str(&v)
            );
        }
        let owned = self.prop_parts(target);
        let parts: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let num = v.as_num();
        if parts.is_empty() {
            return;
        }
        let hit = with_instance_mut(&mut self.list, &parts, |inst| match prop {
            0 => inst.matrix.tx = num as f32,
            1 => inst.matrix.ty = num as f32,
            2 => inst.matrix.scale_x = (num / 100.0) as f32,
            3 => inst.matrix.scale_y = (num / 100.0) as f32,
            6 => {
                // `_alpha` is the alpha multiplier of the colour transform, not
                // a tint: treating it as RGB turned transparent overlays (the
                // path hit areas, faded panels) into black and opaque ones.
                inst.cxform.mul[3] = (num / 100.0).clamp(0.0, 1.0) as f32;
            }
            7 => inst.visible = num != 0.0,
            10 => {
                // `_rotation` is absolute, so the current matrix has to be
                // decomposed first: scaling the existing entries by cos/sin
                // rotated an already-rotated clip by the wrong angle.
                let rad = (num as f32).to_radians();
                let sx = inst.matrix.scale_x.hypot(inst.matrix.rot_skew_0);
                let sy = inst.matrix.scale_y.hypot(inst.matrix.rot_skew_1);
                let (sn, cs) = rad.sin_cos();
                inst.matrix.scale_x = sx * cs;
                inst.matrix.rot_skew_1 = -sx * sn;
                inst.matrix.rot_skew_0 = sy * sn;
                inst.matrix.scale_y = sy * cs;
            }
            _ => {}
        });
        if std::env::var_os("FLASH_VIS").is_some() && matches!(prop, 6 | 7) && !hit {
            // A property write that resolves to no instance is silently lost,
            // which is how a menu that should hide stays on screen.
            eprintln!("VIS-MISS prop {prop} target {:?} parts {parts:?}", self.value_str(target));
        }
    }

    // -----------------------------------------------------------------------
    // Bytecode execution
    // -----------------------------------------------------------------------

    /// The clip a `goto` acts on: the current target when a clip's script is
    /// running, otherwise the main timeline. `gotoAndPlay` inside a clip's
    /// frame script moves that clip, not the whole movie.
    /// Starts or stops the current target's playhead (`play()` / `stop()`).
    fn play_or_stop(&mut self, play: bool) {
        match self.goto_target() {
            Some(path) => {
                let parts: Vec<&str> = path.split('.').collect();
                with_instance_mut(&mut self.list, &parts, |inst| {
                    if let Some(sp) = inst.sprite.as_mut() {
                        sp.playing = play;
                    }
                });
            }
            None => self.playing = play,
        }
    }

    /// The clip the running code belongs to, as a member target. Send it to
    /// `prop_get`/`prop_set` and a bare `_visible` addresses the right clip.
    fn clip_value(&self) -> Value {
        if self.target_path.is_empty() {
            Value::Str("_root".into())
        } else {
            Value::Str(self.target_path.clone())
        }
    }

    fn goto_target(&self) -> Option<String> {
        if self.target_path.is_empty() {
            None
        } else {
            Some(self.target_path.clone())
        }
    }

    fn exec_block(&mut self, code: &Code, depth: u32) {
        // Each action block gets its own constant pool; function bodies run
        // through `exec_program` directly so they inherit their caller's.
        let saved = std::mem::take(&mut self.pool);
        let data = self.movie.data.clone();
        self.exec_program(&data, code.off, code.off + code.bytes.len(), depth);
        self.pool = saved;
    }

    /// Runs AVM1 starting at `start` in `code`.
    ///
    /// `code` is the movie's whole decompressed byte stream, not the tag's
    /// bytes: AVM1 branch offsets are relative to the stream, and Flash lets a
    /// jump leave the block it started in. SWF Encrypt builds on exactly that -
    /// a decoy block jumps into a private tag holding the real program - so the
    /// cursor has to be free to run anywhere in the file.
    /// Runs the code at `start` and stops at `end`.
    ///
    /// The end matters: a function body is a slice of the movie's byte stream,
    /// and the instruction after it belongs to whatever was defined next. Without
    /// a boundary every call ran on into the following code, which re-ran class
    /// definitions and reset game state - a click handler would place nothing and
    /// then quietly clear the player's selection.
    fn exec_program(&mut self, code: &[u8], start: usize, end: usize, depth: u32) {
        let mut stack: Vec<Value> = Vec::new();
        // A byte cursor, not an instruction index: branch targets are byte
        // offsets, and a jump can land inside a record.
        let mut pc = start;
        let mut steps = 0u32;
        let mut recent: Vec<(usize, String)> = Vec::new();
        let mut recent_sets: Vec<String> = Vec::new();
        // A `return` sets `unwinding`, which ends this body but not the caller.
        // The step budget keeps a runaway script from hanging the console; when
        // it runs out the block is marked truncated so the host can say so.
        const STEP_BUDGET: u32 = 2_000_000;
        while pc < end && pc < code.len() && steps < STEP_BUDGET && !self.unwinding {
            steps += 1;
            self.steps += 1;
            let Some((act, len)) = avm1::decode_at(code, pc, &mut self.pool) else {
                // Debug: a block that stops early looks exactly like one that
                // ran to completion, so report the byte it died on (FLASH_STOP).
                if std::env::var_os("FLASH_STOP").is_some() {
                    eprintln!(
                        "STOP undecodable at {pc} (block {start}..{end}) after {:?}",
                        recent
                    );
                }
                break;
            };
            let watcher = self.trace_fn;
            // FLASH_TRACE_DEPTH widens the trace into nested calls, which is how a
            // handler that runs every frame but does nothing gets read.
            let trace_depth: u32 = std::env::var("FLASH_TRACE_DEPTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2);
            if (self.trace && depth < trace_depth) || (watcher.is_some() && watcher == Some(start)) {
                // Constants are resolved against the running pool, so a traced
                // body reads as `Push(td, "money")` instead of `Constant(31)`.
                let text = match &act {
                    Action::Push(items) => {
                        let parts: Vec<String> = items
                            .iter()
                            .map(|it| match it {
                                Item::Constant(i) => format!(
                                    "{:?}",
                                    self.pool.get(*i as usize).cloned().unwrap_or_default()
                                ),
                                other => format!("{other:?}"),
                            })
                            .collect();
                        format!("Push({})", parts.join(", "))
                    }
                    other => format!("{other:?}"),
                };
                let short: String = text.chars().take(70).collect();
                eprintln!(
                    "[{}] {} {}",
                    if self.target_path.is_empty() { "_root" } else { &self.target_path },
                    pc,
                    short
                );
            }
            let mut next = pc + len.max(1);
            if self.debugging() || self.miss_trace {
                // Constants are resolved against the running block's pool: a
                // name like `onRelease` is only readable this way, since the
                // code itself is obfuscated.
                let text = match &act {
                    Action::Push(items) => {
                        let parts: Vec<String> = items
                            .iter()
                            .map(|it| match it {
                                Item::Constant(i) => format!(
                                    "{:?}",
                                    self.pool.get(*i as usize).cloned().unwrap_or_default()
                                ),
                                other => format!("{other:?}"),
                            })
                            .collect();
                        format!("Push({})", parts.join(", "))
                    }
                    other => format!("{other:?}"),
                };
                let short: String = text.chars().take(90).collect();
                self.recent_ops.push(short);
                if self.recent_ops.len() > 10 {
                    self.recent_ops.remove(0);
                }
            }
            if std::env::var_os("FLASH_LOG").is_some() {
                // A filtered trace: only instructions whose operands are real
                // names. The obfuscated loader is noise; the game's own logic
                // uses readable member names, and those show its state machine.
                let text = match &act {
                    Action::Push(items) => {
                        let parts: Vec<String> = items
                            .iter()
                            .map(|it| match it {
                                Item::Constant(i) => format!(
                                    "{:?}",
                                    self.pool.get(*i as usize).cloned().unwrap_or_default()
                                ),
                                other => format!("{other:?}"),
                            })
                            .collect();
                        format!("Push({})", parts.join(", "))
                    }
                    other => format!("{other:?}"),
                };
                // Only names a human can read: a quoted run of letters with no
                // escapes. Everything else is the obfuscated loader's noise.
                let readable = text
                    .split('"')
                    .skip(1)
                    .step_by(2)
                    .any(|s| s.len() >= 3 && !s.contains('\\') && s.chars().all(|c| c.is_ascii_graphic() || c == ' '));
                if readable {
                    let short: String = text.chars().take(120).collect();
                    // The byte offset is what lets the same line be looked up in
                    // a disassembly, so a name that runs can be traced back to
                    // the instruction that pushed it.
                    eprintln!(
                        "[{:>6.0}ms][{}] {pc} {short}",
                        self.time_ms,
                        if self.target_path.is_empty() {
                            "_root"
                        } else {
                            &self.target_path
                        }
                    );
                }
            }
            if self.trace {
                let text = format!("{:?}", act);
                recent.push((pc, text.chars().take(24).collect()));
                if recent.len() > 12 {
                    recent.remove(0);
                }
            }
            match act {
                Action::End => break,
                Action::Push(items) => {
                    for item in items {
                        let v = match item {
                            Item::Value(v) => v,
                            Item::Constant(i) => Value::Str(
                                self.pool.get(i as usize).cloned().unwrap_or_default(),
                            ),
                            Item::Register(r) => self.reg_get(r),
                        };
                        stack.push(v);
                    }
                }
                Action::Pop => {
                    pop(&mut stack);
                }
                Action::PushDuplicate => {
                    let v = stack.last().cloned().unwrap_or(Value::Undefined);
                    stack.push(v);
                }
                Action::StackSwap => {
                    let a = pop(&mut stack);
                    let b = pop(&mut stack);
                    stack.push(a);
                    stack.push(b);
                }
                Action::Add => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num() + b.as_num()));
                }
                Action::StringAdd => {
                    // SWF 4 concatenation: both operands become strings.
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Str(format!(
                        "{}{}",
                        self.value_str(&a),
                        self.value_str(&b)
                    )));
                }
                Action::Add2 => {
                    // ECMA-262 `+`: concatenate only if an operand is a string,
                    // otherwise add. Obfuscated code depends on the difference.
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    let stringy = matches!(a, Value::Str(_)) || matches!(b, Value::Str(_));
                    let v = if stringy {
                        Value::Str(format!("{}{}", self.value_str(&a), self.value_str(&b)))
                    } else {
                        Value::Num(a.as_num() + b.as_num())
                    };
                    stack.push(v);
                }
                Action::Subtract => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num() - b.as_num()));
                }
                Action::Multiply => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num() * b.as_num()));
                }
                Action::Divide => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num() / b.as_num()));
                }
                Action::Modulo => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num() % b.as_num()));
                }
                Action::Equals => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    // Loose equality: null and undefined are equal, which is how
                    // a for...in loop detects its sentinel.
                    let nullish = |v: &Value| matches!(v, Value::Null | Value::Undefined);
                    let eq = self.values_equal(&a, &b) || (nullish(&a) && nullish(&b));
                    if self.trace && depth < 3 {
                        eprintln!(
                            "EQ @{} {:?}({}) vs {:?}({}) = {}",
                            pc,
                            a,
                            self.value_str(&a),
                            b,
                            self.value_str(&b),
                            eq
                        );
                    }
                    stack.push(Value::Bool(eq));
                }
                Action::StrictEquals => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    let eq = self.values_equal(&a, &b);
                    stack.push(Value::Bool(eq));
                }
                Action::Less => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Bool(a.as_num() < b.as_num()));
                }
                Action::StringLess => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Bool(a.as_str() < b.as_str()));
                }
                Action::Greater => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Bool(a.as_num() > b.as_num()));
                }
                Action::And => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Bool(a.as_bool() && b.as_bool()));
                }
                Action::Or => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Bool(a.as_bool() || b.as_bool()));
                }
                Action::Not => {
                    let a = pop(&mut stack);
                    stack.push(Value::Bool(!a.as_bool()));
                }
                Action::ToNumber => {
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num()));
                }
                Action::ToString => {
                    let a = pop(&mut stack);
                    stack.push(Value::Str(a.as_str()));
                }
                Action::ToInteger => {
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num().trunc()));
                }
                Action::Increment => {
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num() + 1.0));
                }
                Action::Decrement => {
                    let a = pop(&mut stack);
                    stack.push(Value::Num(a.as_num() - 1.0));
                }
                Action::TypeOf => {
                    let a = pop(&mut stack);
                    stack.push(Value::Str(a.type_of().to_string()));
                }
                Action::StringLength => {
                    let a = pop(&mut stack);
                    stack.push(Value::Num(self.value_str(&a).chars().count() as f64));
                }
                Action::StringExtract => {
                    let count = pop(&mut stack).as_num() as usize;
                    let start = pop(&mut stack).as_num() as usize;
                    let s = pop(&mut stack).as_str();
                    let v: String = s.chars().skip(start).take(count).collect();
                    stack.push(Value::Str(v));
                }
                Action::StringEquals => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    stack.push(Value::Bool(a.as_str() == b.as_str()));
                }
                Action::GetVariable => {
                    let name = pop(&mut stack).as_str();
                    let v = self.get_var(&name);
                    stack.push(v);
                }
                Action::SetVariable => {
                    let v = pop(&mut stack);
                    let name = pop(&mut stack).as_str();
                    if self.trace && depth < 3 {
                        // State-machine debugging: the obfuscated code stores its
                        // program counter in a variable, so the value matters
                        // more than the opcode.
                        let shown = self.value_str(&v);
                        eprintln!("SET {name:?} = {shown}");
                        recent_sets.push(shown);
                        if recent_sets.len() > 10 {
                            recent_sets.remove(0);
                        }
                    }
                    self.set_var(&name, v);
                }
                Action::DefineLocal => {
                    let v = pop(&mut stack);
                    let name = pop(&mut stack).as_str();
                    if self.trace && depth < 3 {
                        eprintln!("LOCAL {name:?} = {}", self.value_str(&v));
                    }
                    self.define_local(&name, v);
                }
                Action::DefineLocal2 => {
                    let name = pop(&mut stack).as_str();
                    self.define_local(&name, Value::Undefined);
                }
                Action::Delete | Action::Delete2 => {
                    let name = pop(&mut stack).as_str();
                    let k = self.var_key(&name);
                    self.vars.remove(&k);
                }
                Action::GetMember => {
                    let name = pop(&mut stack).as_str();
                    let obj = pop(&mut stack);
                    let v = self.get_member(&obj, &name);
                    stack.push(v);
                }
                Action::SetMember => {
                    let v = pop(&mut stack);
                    let name = pop(&mut stack).as_str();
                    let obj = pop(&mut stack);
                    self.set_member(&obj, &name, v);
                }
                Action::GetProperty => {
                    // The property index is a stack operand, not action data.
                    let idx = pop(&mut stack).as_num();
                    let t = pop(&mut stack);
                    // Indexes at or below -1 are invalid; anything in between
                    // truncates to 0, which is what Flash does with -0.8.
                    let v = if idx.is_finite() && idx > -1.0 {
                        self.prop_get(idx as u16, &t)
                    } else {
                        Value::Undefined
                    };
                    stack.push(v);
                }
                Action::SetProperty => {
                    let v = pop(&mut stack);
                    let idx = pop(&mut stack).as_num();
                    let t = pop(&mut stack);
                    if idx.is_finite() && idx > -1.0 {
                        self.prop_set(idx as u16, &t, v);
                    }
                }
                Action::SetTarget(path) => {
                    if std::env::var_os("FLASH_NAV").is_some() {
                        eprintln!("NAV SetTarget({path:?})");
                    }
                    self.target_path = norm(&path);
                }
                Action::SetTarget2 => {
                    let t = pop(&mut stack);
                    self.target_path = norm(&t.as_str());
                }
                Action::Trace => {
                    let v = pop(&mut stack);
                    // trace() prints the word, unlike string coercion which
                    // turns undefined into an empty string.
                    let text = match v {
                        Value::Undefined => "undefined".to_string(),
                        other => self.value_str(&other),
                    };
                    if self.log.len() < 200 {
                        self.log.push(text);
                    }
                }
                Action::RandomNumber => {
                    let max = pop(&mut stack).as_num().max(1.0);
                    let r = (self.next_rand() * max).floor();
                    stack.push(Value::Num(r));
                }
                Action::GetTime => stack.push(Value::Num(self.time_ms)),
                Action::CallFunction => {
                    // Stack: args..., count, name.
                    let name = pop(&mut stack).as_str();
                    let count = pop(&mut stack).as_num().max(0.0) as usize;
                    let args = pop_args(&mut stack, count);
                    let v = self.call_named(&name, &args, depth);
                    stack.push(v);
                }
                Action::CallMethod => {
                    // Stack: args..., count, object, method name.
                    let name = pop(&mut stack).as_str();
                    let obj = pop(&mut stack);
                    let count = pop(&mut stack).as_num().max(0.0) as usize;
                    let args = pop_args(&mut stack, count);
                    let v = self.call_member(&obj, &name, &args, depth);
                    stack.push(v);
                }
                Action::Call => {
                    // Calls the function value on the stack with no arguments.
                    let f = pop(&mut stack);
                    let v = if let Value::Function(id) = f {
                        let this = self.current_this();
                        self.invoke(id, this, Vec::new(), depth)
                    } else {
                        Value::Undefined
                    };
                    stack.push(v);
                }
                Action::DefineFunction(f) => {
                    if let Some(v) = self.define_function(f) {
                        stack.push(v);
                    }
                }
                Action::Return => {
                    let v = pop(&mut stack);
                    if let Some(frame) = self.frames.last_mut() {
                        frame.ret = v;
                        self.unwinding = true;
                    }
                }
                Action::StoreRegister(r) => {
                    // The value stays on the stack; the for...in loop reads it
                    // again to test for the end sentinel.
                    let v = stack.last().cloned().unwrap_or(Value::Undefined);
                    self.reg_set(r, v);
                }
                Action::Enumerate => {
                    let name = pop(&mut stack).as_str();
                    let value = self.get_var(&name);
                    self.push_keys(&mut stack, &value);
                }
                Action::Enumerate2 => {
                    let value = pop(&mut stack);
                    self.push_keys(&mut stack, &value);
                }
                // The SWF 4-era GotoFrame is gotoAndStop.
                Action::GotoFrame(n) => match self.goto_target() {
                    Some(parts) => {
                        let parts: Vec<&str> = parts.split('.').collect();
                        self.jump_clip(&parts, Jump::Abs(n as usize - 1), false, depth + 1);
                    }
                    None => {
                        self.note_nav(format!("GotoFrame({n})"));
                        self.playing = false;
                        self.enter_frame(n as usize, depth + 1);
                    }
                },
                Action::GotoLabel(label) => match self.goto_target() {
                    Some(parts) => {
                        let parts: Vec<&str> = parts.split('.').collect();
                        self.jump_clip(&parts, Jump::Label(label), true, depth + 1);
                    }
                    None => {
                        self.note_nav(format!("GotoLabel({label})"));
                        self.playing = true;
                        if let Some(&i) = self.movie.root.labels.get(&label) {
                            self.enter_frame(i, depth + 1);
                        }
                    }
                },
                Action::GotoFrame2 { play } => {
                    let v = pop(&mut stack);
                    let target = self.goto_target();
                    match (&v, target) {
                        (Value::Str(s), Some(path)) if !s.is_empty() => {
                            let parts: Vec<&str> = path.split('.').collect();
                            self.jump_clip(&parts, Jump::Label(s.clone()), play, depth + 1);
                        }
                        (_, Some(path)) => {
                            let n = v.as_num();
                            if n >= 1.0 {
                                let parts: Vec<&str> = path.split('.').collect();
                                self.jump_clip(&parts, Jump::Abs(n as usize - 1), play, depth + 1);
                            }
                        }
                        (Value::Str(s), None) if !s.is_empty() => {
                            if let Some(&i) = self.movie.root.labels.get(s) {
                                self.enter_frame(i, depth + 1);
                            }
                            self.playing = play;
                        }
                        (other, None) => {
                            let n = other.as_num();
                            if n >= 1.0 {
                                self.enter_frame(n as usize - 1, depth + 1);
                            }
                            // gotoAndStop has to stop: the flag is authoritative.
                            self.playing = play;
                        }
                    }
                }
                Action::NextFrame => {
                    match self.goto_target() {
                        Some(path) => {
                            let parts: Vec<&str> = path.split('.').collect();
                            self.jump_clip(&parts, Jump::Delta(1), true, depth + 1);
                        }
                        None => {
                            let n = self.frame + 1;
                            self.enter_frame(n, depth + 1);
                        }
                    }
                }
                Action::PrevFrame => {
                    match self.goto_target() {
                        Some(path) => {
                            let parts: Vec<&str> = path.split('.').collect();
                            self.jump_clip(&parts, Jump::Delta(-1), true, depth + 1);
                        }
                        None => {
                            let n = self.frame + self.frame_count() - 1;
                            self.enter_frame(n, depth + 1);
                        }
                    }
                }
                // Play/Stop act on the current target: a clip's frame script
                // stops that clip, not the main timeline. Stopping the root
                // instead froze the whole movie while the clip that called it
                // carried on looping.
                Action::Play => self.play_or_stop(true),
                Action::Stop => self.play_or_stop(false),
                Action::StopSounds | Action::ToggleQuality | Action::GetUrl => {}
                Action::JumpTo(target) => next = target,
                Action::IfJump(target) => {
                    // ActionIf carries its own offset and jumps when true.
                    if pop(&mut stack).as_bool() {
                        next = target;
                    }
                }
                Action::ConstantPool(p) => self.pool = p,
                Action::InitArray => {
                    // Count comes from the stack, then the elements in order.
                    let n = pop(&mut stack).as_num();
                    let n = if n.is_finite() && n >= 0.0 {
                        n as usize
                    } else {
                        0
                    };
                    let items = pop_args(&mut stack, n);
                    let v = self.array(items);
                    stack.push(v);
                }
                Action::NewMethod => {
                    // Stack: args..., count, object, method name.
                    let name = pop(&mut stack).as_str();
                    let obj = pop(&mut stack);
                    let count = pop(&mut stack).as_num().max(0.0) as usize;
                    let args = pop_args(&mut stack, count);
                    let method = self.get_member(&obj, &name);
                    let v = match method {
                        Value::Function(id) => self.run_constructor_fn(id, args, depth),
                        _ => Value::Undefined,
                    };
                    stack.push(v);
                }
                Action::Extends => {
                    // AS2's `class B extends A`: link B's prototype to A's and
                    // B itself to A, so inherited methods resolve.
                    let superclass = pop(&mut stack);
                    let subclass = pop(&mut stack);
                    let super_proto = match &superclass {
                        Value::Function(id) => match self.fn_proto(*id) {
                            Value::Object(i) => Some(i),
                            _ => None,
                        },
                        Value::Object(i) => Some(*i),
                        _ => None,
                    };
                    if let Value::Function(sub) = &subclass {
                        if let Value::Object(sub_proto) = self.fn_proto(*sub) {
                            if let Some(sp) = super_proto {
                                if let Some(o) = self.objects.get_mut(sub_proto) {
                                    o.proto = Some(sp);
                                }
                            }
                        }
                    }
                    // `extends` is a statement, not an expression.
                    stack.push(Value::Undefined);
                }
                Action::InitObject => {
                    let n = pop(&mut stack).as_num();
                    let n = if n.is_finite() && n >= 0.0 {
                        n as usize
                    } else {
                        0
                    };
                    let obj = self.obj_new(None, None);
                    for _ in 0..n {
                        let value = pop(&mut stack);
                        let name = pop(&mut stack).as_str();
                        if let Value::Object(i) = obj {
                            self.obj_set(i, &name, value);
                        }
                    }
                    stack.push(obj);
                }
                Action::NewObject => {
                    // Flash pushes the arguments, then the count, then the
                    // constructor, so the count sits below the constructor on
                    // the stack. Re-emitted and obfuscated movies (SWF Encrypt
                    // and friends) emit the two the other way round, so take the
                    // order the stack actually has: a number on top means the
                    // count is there.
                    let top = pop(&mut stack);
                    let next = pop(&mut stack);
                    let (ctor, count) = match (&top, &next) {
                        (Value::Num(n), other)
                            if !matches!(other, Value::Num(_)) && *n >= 0.0 =>
                        {
                            (next, *n as usize)
                        }
                        _ => (top, next.as_num().max(0.0) as usize),
                    };
                    let args = pop_args(&mut stack, count);
                    let v = self.construct_value(ctor, args, depth);
                    stack.push(v);
                }
                Action::Unsupported(op) => {
                    self.miss(format!("action 0x{op:02x}"));
                }
            }
            pc = next;
        }
        if steps >= STEP_BUDGET && pc < code.len() {
            self.truncated += 1;
            if self.trace {
                // What a livelocked block was doing when the budget ran out.
                let tail: Vec<String> = recent
                    .iter()
                    .rev()
                    .take(8)
                    .rev()
                    .map(|(o, a)| format!("{o}:{a}"))
                    .collect();
                let head: Vec<String> = code
                    .iter()
                    .take(6)
                    .map(|b| format!("{b:02x}"))
                    .collect();
                eprintln!(
                    "TRUNC {} len {} steps {steps} at {pc} tail {:?} head {:?} sets {:?}",
                    self.target_path,
                    code.len(),
                    tail,
                    head,
                    recent_sets
                );
            }
        }
    }
}
