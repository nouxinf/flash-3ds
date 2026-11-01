# flash3ds

A Flash (SWF) player for homebrew Nintendo 3DS. It opens a `.swf` from the SD
card, renders the timeline in software on the top screen, and keeps controls and
status on the bottom screen.

Built in Rust with `ctru-rs` against devkitARM/libctru, packaged as both a
`.3dsx` for the Homebrew Launcher and a `.cia` for installation with FBI.

## Status — read this first

This is **v0.1: a working renderer, not yet a complete Flash runtime.**

| Area | State |
|---|---|
| SWF container (FWS/CWS), stage size, frame rate | works |
| Shapes: solid fills, strokes, curves | works |
| Gradients (linear/radial) | works |
| Bitmaps: `DefineBitsLossless`/`Lossless2` (formats 3, 4, 5) | works |
| Timeline: place/remove, depth ordering, transformations, colour transforms | works |
| Nested sprites (`DefineSprite`) | works, advanced in lockstep with the parent |
| ActionScript (AVM1/AS2) | interpreter: variables, arithmetic, branches, functions, `gotoAndPlay`/`gotoAndStop`, `Key`, properties |
| Objects, arrays, prototypes | `new`, `this`, `arguments`, `for...in`, prototype chains, array and string methods |
| Clip events | `onClipEvent(load/enterFrame/keyDown/keyUp/keyPress/mouseDown/mouseUp)` |
| Audio | **not implemented in v1** (deferred until `dspfirm.cdc` is available) |
| Static/dynamic text, JPEG bitmaps, filters, Stage3D, video | not supported |

Classic ActionScript 1/2 (AVM1) is executed by an interpreter in
`src/avm1.rs` (bytecode decoder) and `src/player_avm1.rs` (execution), so games
that drive themselves with frame scripts respond: `stop()`, `play()`,
`gotoAndPlay`/`gotoAndStop`, variables, arithmetic, comparisons, `if`, `trace`,
`Key.isDown`, `Math`, and `on(press)` / `on(release)` / `on(keyPress)` button
handlers all run.

User-defined functions execute, whether the compiler emitted `DefineFunction`
or `DefineFunction2`: parameters (by name or register), `var` locals, `return`
values, recursion and register preloads all work, and a call resolves through
the scope chain so a clip can call a function declared on its parent timeline.
`this` is bound for method-style calls, so `hero.moveBy(50)` writes the clip's
own properties, and a function body that assigns a bare name writes to that
clip.

`onClipEvent(enterFrame)` handlers fire on the timeline clock even after a
script has stopped the root, which is how most AS2 games drive themselves
without a frame loop. `load` runs once, key events fire on press/release edges,
and mouse down/up are delivered to every handler (clip-level `press`/`release`
and `rollOver`/`rollOut` are not dispatched; buttons still get `on(press)`).

Objects and arrays are real values: object and array literals, dynamic member
access (`o[key]`), `new` with a constructor whose `this` is the fresh object,
`F.prototype.m = ...` inherited by instances, `arguments` as an array, and
`Function.call`/`apply`. `for...in` enumerates property names. Arrays support
`length`, `push`, `pop`, `shift`, `unshift`, `splice`, `slice`, `join`,
`concat`, `reverse` and `indexOf`; strings support `length`, `charAt`,
`charCodeAt`, `indexOf`, `substr`, `substring`, `split`, `toUpperCase`,
`toLowerCase` and `concat`.

Still outside the interpreter: sparse array semantics. Arrays are dense, capped
at 65536 elements, so an index beyond that is ignored (and logged) instead of
growing `length` the way Flash's sparse arrays do; there is no `Array.sort`,
`Date`, `XML` or `Sound`; `delete` works on variables but not object
properties; `gotoAndStop` only moves the main timeline, not a nested clip;
`setInterval` is not implemented. AVM2 (SWF 9+ / AS3) is not supported at all.

Objects live for the whole movie because there is no garbage collector, so the
heap is capped at 4096 objects; a script that allocates every frame stops
allocating and logs it rather than slowly running the console out of memory.

Unsupported content is reported on the bottom screen and in the SD log instead
of failing silently.

## Requirements

- devkitPro with the `3ds-dev` group (`C:\devkitPro`), providing devkitARM,
  libctru, `3dsxtool`, `smdhtool` and `3dslink`
- Rust nightly with `rust-src` and [`cargo-3ds`](https://github.com/rust3ds/cargo-3ds)
- LLVM/libclang on PATH (bindgen builds the libctru bindings)
- Python 3 (asset and test-SWF generation)
- Optional for the CIA: `makerom` and `bannertool`

`tools/env.sh` sets `DEVKITPRO`, `DEVKITARM`, `LIBCLANG_PATH` and PATH
explicitly, so builds do not depend on a stale shell environment.

## Build

```sh
bash tools/build.sh
```

Outputs:

- `dist/flash3ds.3dsx` — copy to `sdmc:/3ds/flash3ds/` and start from hbmenu
- `dist/flash3ds.cia` — install with FBI (test-signed, no Nintendo certificates)
- `dist/sample.swf` — the generated test movie
- `dist/keys.swf` — the D-pad test movie: hold an arrow and the box moves that
  way, release and it stops

Deploy over the network with hbmenu in netloader mode (press Y):

```sh
3dslink dist/flash3ds.3dsx -a <console-ip>
```

## Using it

Put movies anywhere on the SD card; `sdmc:/3ds/flash3ds/` is created for you as
a sensible default. Launch the app and browse with the D-pad.

| Button | Browser | Player |
|---|---|---|
| D-pad | move / page | arrow keys for the movie (step frames while paused) |
| A | open folder or movie | play / pause |
| B | parent folder | back to browser |
| X | rescan folder | restart movie |
| Y | jump to SD root | spacebar |
| START | quit | pause menu |
| SELECT | settings | toggle FPS counter |
| Touch | — | mouse position, clicks and `on(press)` buttons; dragging a named clip moves it |

The D-pad and the circle pad are *different bits* in libctru, and this crate's
`KeyPad::LEFT` is the union of both (`KEY_DLEFT | KEY_CPAD_LEFT`). `bitflags::contains`
requires every bit of its argument, so `held.contains(KeyPad::LEFT)` only matches
when the D-pad *and* the circle pad are pushed together - it silently matched
nothing, which made the arrow keys dead while A/B/X worked. Direction keys are
tested with `intersects`, and the D-pad feeds the movie's arrow keys even while a
clip's own `stop()` holds the timeline.

## How it works

- `src/swf.rs` — SWF container, tag decoder, shapes, bitmaps, sprites, buttons,
  timeline
- `src/avm1.rs` — AVM1 bytecode decoder (ActionPush/Push, jumps, calls, ...)
- `src/player.rs` — display list, affine transforms and colour transforms,
  even-odd scanline rasterizer, bitmap sampling, input plumbing
- `src/player_avm1.rs` — the AVM1 interpreter: variables, properties, builtins,
  `gotoAndPlay`, button condition dispatch
- `src/gfx.rs` — RGBA canvas, text, scaling, and the rotated framebuffer blit
- `src/fs.rs` — SD access through libctru FS calls (no `std::fs`)
- `src/ui.rs` — browser, player chrome, settings
- `tools/hostcheck` — builds the decoder, renderer and interpreter on a desktop;
  writes every frame to PNG and runs the scripted-behaviour checks

The renderer is deliberately pure Rust with no GPU dependency: the 3DS
framebuffer is written directly through the `gfx` service, which keeps the same
code path on hardware and in the host harness.

### Vendored patch

`vendor/ctru-sys` is the upstream crate at the current revision with fixes for
four build failures that make `ctru-sys` unbuildable on Windows today: the
missing static-function wrapper file, allowlist regexes built from Windows paths
(which silently produced empty bindings), the missing `-I` and target triple for
clang, and the bindgen 0.72 opaque-struct bug that required bindgen 0.73.
`Cargo.lock` pins everything else.

## Verification

- `tools/hostcheck` renders the generated test movie to PNG on the host; the
  checked-in `assets/sample.swf` exercises fills, outlines, transforms and a
  per-frame move.
- `tools/gen_swf.py` writes that movie from the SWF specification independently
  of the Rust decoder, so a decoder regression shows up as a broken image.
- `tools/gen_avm1.py` writes ten movies that encode hand-assembled AVM1, and
  `tools/hostcheck --avm1 <dir>` asserts their behaviour: `stop()` halting the
  playhead, `gotoAndPlay(3)`, arithmetic plus a branch into `gotoAndStop(3)`,
  `Key.isDown` moving a named clip through `GetProperty`/`SetProperty` in all
  four directions on a looping timeline, a
  `DefineButton2` with press, release and keyPress handlers, and a clip whose
  `load`/`enterFrame`/`keyDown`/`keyUp`/`keyPress` handlers keep running after
  the root stopped, plus object literals, arrays, prototypes, `new`,
  `arguments`, `Function.apply` and a `for...in` loop, and every MovieClip
  method the game layer needs (`gotoAndStop`, `getDepth`,
  `getNextHighestDepth`, `swapDepths`, `hitTest` with expected geometry).
  All twelve checks pass.
- The same movies are also checked against real compiled ActionScript from
  Ruffle's test suite: `--trace` output matches the expected files line for line
  on `define_function2` (8/8), `timeline_function_def` (7/7),
  `define_function_case_sensitive` (2/2), `array_slice` (36/36) and
  `array_concat` (99/99), and nearly so on `array_trivial` (200/210),
  `array_splice` (190/209) and `object_function` (25/33). That comparison is
  what caught three engine bugs the hand-written movies missed: call arguments
  popped in reverse (`define_function2` traced `3 2 1` instead of `1 2 3`),
  `StoreRegister` consuming the value it stores (which broke every `for...in`
  loop), and an unbounded array index, where `a[302231454903659441160191] = 1`
  asked the allocator for 48 GB and aborted. It also showed that `trace()` of an
  array printed `[object Object]`, and that Flash renders `undefined` inside a
  string as the word rather than nothing.
- `tools/hostcheck --run <file.swf> [seconds]` runs any real movie headlessly
  and reports what the engine made of it: decoder warnings, `trace()` output,
  the top of the display list, and a count of every call or opcode the runtime
  has no implementation for. It is the fastest way to see why a game does not
  work, and it needs no emulator or input at all.
- `tools/hostcheck` also has inspection modes that made the Bloons work
  possible: `--ops <movie.swf> [sprite]` dumps a timeline's placements and
  action blocks, `--shapes`, `--bits`, `--fonts` (which also lists every text and edit-text
  field), `--button <id>` and `--shape <id>` dump the decoded character tables, and
  `--run <movie.swf> <seconds> --click X Y` presses a stage point headlessly so
  button handlers can be exercised without a mouse.
- `--timeline <movie.swf> [frames] [sprite]` dumps a timeline frame by frame
  with its decoded ActionScript (function bodies are summarised, or they would
  swamp the output). It is how the preloader and the symbol scripts were read.
- `--audit <movie.swf>` counts what every script in a movie actually does:
  blocks, instructions, constant pools, references to those pools, `SetMember`
  /`GetMember` and calls. It is how "the code is present" is told apart from
  "the code was stripped": `keys.swf` pushes 15 named strings and works, while
  `bloons.swf` pushes **none**, references **none** of its 279 pools and never
  touches a property.
- `tools/hostcheck/src/bin/swfaudit.rs` (`cargo run --bin swfaudit --
`movie.swf`) decodes a movie with **Ruffle's** `swf` crate instead of ours:
every script's instructions, pools, named strings, member accesses, calls, and
every unknown tag code with its size. `PAYLOADS=1` decodes each tag-253 payload
as ActionScript, `TAGS=1` lists unknown tags. It is how "the scripts are a
decoy" was separated from "our decoder is wrong" - both decoders agreed.
- `--run <movie.swf> <secs> --frame N` jumps the root timeline to a frame after
  running, so a scene behind a gate (Bloons' `main`) can be inspected without
  playing up to it.
- `--draw <movie.swf> <id>` rasterises one character alone and reports its lit
  pixels and centre colour, and `--layers <movie.swf> [seconds]` renders each
  top-level depth separately. Together they answer "what is painting over the
  screen" without looking at a screenshot.
- `--jpeg <file|movie.swf> [out]` decodes JPEG data to PNG; `.tmp/cmpjpeg.py`
  diffs those against Pillow and is how the two JPEG bugs above were found.
- `--bin refdump <movie.swf> [frames]` decodes the same movie with Ruffle's
  `swf` crate so the two decoders can be compared instruction for instruction.
  `FLASH_TRACE=1` prints every AVM1 instruction the root clip executes, which
  is how the protected dispatcher's state values were observed.
- The app builds as a release `.3dsx` with the pinned toolchain.

```sh
python tools/gen_avm1.py .tmp/avm1
cargo run --manifest-path tools/hostcheck/Cargo.toml -- --avm1 .tmp/avm1
```

The bytecode decoder was also checked against compiled ActionScript rather than
only against these hand-written streams: `--trace <file.swf>` prints a movie's
`trace()` output, and on Ruffle's `getproperty` test movie 19 of its 22 expected
`trace()` lines match in order. The three that differ depend on calling a
user-defined `valueOf` function, which is the documented gap rather than a
decoding error.

### Bloons TD 1 status

`--run bloons.swf` loads the movie, runs its ActionScript and draws its map.
The following were all found by running the real game and are fixed:

- **Shape `MoveTo` width.** Only edge records carry the `+2` bias; reading
  `MoveTo` that way shifted every later coordinate in spec-encoded files.
- **Signed bit reads.** `read_signed(31)` overflowed `i32` and aborted the parse.
- **8-bit bitmap colour tables.** For `DefineBitsLossless` format 3 the colour
  table lives inside the zlib stream, so all 16 of the game's images were
  reported corrupt.
- **`DefineFont2/3` offset table.** It holds one entry per glyph (the code table
  offset ends the last glyph), and `DefineFont3` geometry is 20480 units per em.
- **`TEXTRECORD` field order.** Font, colour, XOffset, YOffset, TextHeight - the
  height follows the offsets, not the font id.
- **`DefineEditText` flags.** `HasFont` is `0x0080` and `HasLayout` is `0x0400`.
- **`DefineFunction` framing.** The name is a null-terminated string, the
  parameter count is a `UI16`, and the code sits *after* the record's declared
  length. Reading the name as a two-byte constant-pool index consumed one byte
  too few, so every function body in the game started with `End` and did
  nothing. This one was confirmed against Ruffle's `swf` crate, which decodes
  the same bytes the same way.
- **`Add2` versus `StringAdd`.** `0x47` (`Add2`) concatenates only when an
  operand is a string and otherwise adds numerically; `0x21` (`StringAdd`)
  always concatenates. Treating both as `StringAdd` turned every obfuscated
  arithmetic expression in the game into a string.
- **Clip properties through dot syntax.** `clip._visible = false` compiled to
  `SetMember`, which wrote a variable instead of touching the display list, so
  hidden hit areas and panels stayed on screen.
- **Fill alpha.** `DefineShape4` fills lost their alpha, so transparent hit
  areas painted as opaque blocks.
- **Two-point subpaths.** Flash expresses a frame as four lone edges around a
  rectangle; closing them cancelled the crossing and filled the whole stage.
- **Nested buttons.** Hit-testing only looked at the root list, so every button
  inside a menu sprite was dead.
- **Display list on a backwards jump.** Entering a frame only ever *applied*
  that frame's records, so looping back to frame 1 left the objects that later
  frames had placed sitting on the list. A looping preloader therefore piled
  its last frames on top of its first one - which is how the game's full-stage
  field sprite ended up painting over every menu. Each instance now remembers
  the frame that placed it, and jumping back drops the ones the target frame
  does not place while keeping the runtime state of those it does, which is why
  a clip that loops keeps its `_x`.
- **Branches that leave their tag (SWF Encrypt).** Action jumps used to be
  clamped to the block they started in, which the format does not require:
  Flash resolves them against the movie's byte stream. Clamping turned every
  protected movie into an endless decoy loop. `Movie` now keeps the
  decompressed stream (an `Rc`) and every action block records its offset, so
  execution - including function bodies and `DoInitAction` - can leave the tag
  it began in. This is what makes the attached Bloons TD copy run its real code
  at all.
- **Clip nesting depth.** Sprite construction stopped at four levels and
  returned *no sprite at all* below that, so every clip five levels down lost
  its whole subtree silently; the intro's balloons were five deep. The cap is a
  recursion guard now, not a content limit.
- **Nested clip playheads.** Only clips placed directly on the root advanced,
  so anything nested kept its first frame forever. Advancement walks the whole
  display tree, one frame per clip, as Flash does.
- **Re-entering a frame.** Placing the same character at a depth it already
  occupies now keeps that instance (and its playhead and children) instead of
  rebuilding it, and the root's place/remove tags are applied only when the
  playhead *changes* frame. A script that sends the timeline to the frame it is
  already on - the usual "hold here until done" loop - used to restart the very
  animation it was waiting for, every tick.
- **`gotoAndPlay` on the main timeline.** It cleared the display list and
  replayed every frame from zero, recreating every clip from its first frame;
  it now moves the playhead. The same goes for a nested clip, which keeps the
  objects the target frame also places.
- **`goto` targets.** `GotoFrame`/`GotoLabel`/`GotoFrame2`/`NextFrame`/
  `PrevFrame` acted on the root even when a clip's script was running; they now
  act on the current target, which is what Flash does.
- **`onClipEvent(press)` / `(release)`.** A clip can be its own button, and
  this is how a title screen says "click to continue" without a
  `DefineButton2`. Presses and releases are delivered to the clip under the
  pointer.
- **MovieClip methods.** `clip.gotoAndPlay(2)`, `swapDepths`, `getDepth`,
  `getNextHighestDepth`, `hitTest`, `getBounds`, `attachMovie` and friends were
  not implemented at all: only the equivalent *actions* were, so any game that
  addressed a clip through its name silently did nothing. They all drive the
  same display-list state now, including frame labels and a per-clip playhead
  that `clip.stop()` halts.
- **`_url` and `System.security.sandboxType`.** A protected movie's domain lock
  reads them, so a player without a browser must still answer: `_url` defaults
  to a `file://` path and the sandbox to `localWithFile`, both overridable with
  `FLASH_URL` / `FLASH_SANDBOX` to test which answer a given movie wants.
- **`DoInitAction` (tag 59).** It was not implemented at all, so the script that
  belongs to every symbol - the code that wires up a balloon, a tower or a
  panel the moment the clip exists - never ran. It now runs once per instance,
  before that instance's `onClipEvent(load)`.
- **JPEG bitmaps.** A baseline decoder in `src/jpeg.rs` handles
  `DefineBitsJPEG2`: Huffman and progressive-free baseline scans, any
  subsampling (4:4:4, 4:2:2, 4:2:0), restart markers and grayscale. Two bugs
  were caught by diffing against Pillow: `ZRL` (`0xF0`) advanced the coefficient
  index by one instead of sixteen, which corrupted every busy block, and chroma
  was expanded nearest-neighbour instead of with the 3:1 triangle filter
  libjpeg calls "fancy upsampling". All eight fixtures now decode within a mean
  error of 1.4/255 of Pillow.

**The movie is obfuscated with SWF Encrypt, and it runs.** The visible scripts
really are a decoy: 157 blocks, 43,025 instructions, only four distinct pushed
strings (`\x01`, `\x01\x02`, `\x0f`, `\x09`), zero `GetMember`/`SetMember`, and
junk function bodies. The real program sits in the unknown tag 253 (144
payloads, 82 KB, `swfaudit` reports them and so does Ruffle's crate).

It is not encrypted, and nothing has to be spliced: the decoy's `Jump` leaves
its own tag and lands inside the payload, and Flash resolves AVM1 branch
offsets against the **whole decompressed movie**, not the block. `escape.py`
shows the jump at 343762 leaving its `DoAction` tag to land 189 bytes into a
payload. That is the entire trick, and it is why the same file plays in Ruffle.
`Player` now keeps the decompressed byte stream and executes against it, so a
branch (and a function body) can leave its block; the recovered code is plain
AS2 (`GetMember`, `GotoLabel("intro")`, `MochiAd`) and it executes.

What that run now reports: the preloader completes (`prog = 100`), the intro
clip is built with all its nested parts, and forcing the "main" frame draws the
real `Bloons TD` title art on the green field. Still blocking play:

- **The intro does not release.** `_root` holds on frame 6 while
  `_root.introclip.done` is false, and that flag is only set by the intro clip's
  own frame 122; the root's hold re-enters frame 6 each pass, whose `Remove`
  tags recreate the clip, so the clip never reaches it. What a real player does
differently here is not yet established.
- Clip-depth masking is approximated by hiding the mask object; content is not
  clipped to it.
- Sound is not decoded, and the game drives much of its flow from `Sound`
  callbacks.

### AVM1 format notes

Three details decide whether real SWF bytecode decodes at all, and all three are
easy to get wrong:

- only opcodes `>= 0x80` have a 2-byte length field; `Stop`, `Trace`,
  `SetVariable` and friends are a single byte,
- `ActionIf` carries its own branch offset and jumps when the condition is
  *true* (`ActionJump` offsets are relative to the end of the record),
- property indexes, call names and argument counts are pushed on the stack, not
  stored in the action body, and `DefineFunction` embeds its code after the
  declared length.

The built `.3dsx` has been booted in Azahar (the Citra fork), where both screens
render, the SD browser lists the card's folders, and the file browser's own
D-pad navigation moves the selection. Real-console checks (framebuffer
orientation, actual frame times) still need hardware.

`keys.swf` is the movie to check input with: it polls `Key.isDown` for all four
arrows on a looping timeline and moves its box 10 px per frame. Two mistakes in
an earlier version made it look dead on hardware even with correct input - it
looped with `gotoAndStop` (flag `0x02`, which in `ActionGotoFrame2` means a
scene bias follows *and* stop), and its speed was 4 twips, a fifth of a pixel.

## What a real game needs (measured on Bloons TD 1)

Bloons TD 1 (518 KB, SWF 8, 640x480 at 40 fps) is the reference for "can it run
a real game". It now boots unattended into its main game screen: the intro runs,
the title clip finishes and hides itself, and the map, track, tower panel, balloon
counter and HUD text fields are all live. `--run <file> 10` prints the state:

| Need | State |
|---|---|
| SWF parsing, shapes, sprites, buttons | works (138 shapes, 129 sprites, 22 buttons) |
| Colour-mapped bitmaps (16) | works |
| `DefineText` / `DefineEditText` | works (37 static texts, 14 fields) |
| `DefineBitsJPEG2` (18 bitmaps) | works, verified against Pillow |
| Library symbols (`attachMovie`/`duplicateMovieClip`) | works (`ExportAssets` linkage) |
| Symbol init scripts (`DoInitAction`) | works (one-shot per instance) |
| Audio (`DefineSound`/`StartSound`, 8 sounds) | **missing**, as documented |
| Object lifetime | 4096-object cap, no GC: a frame-loop allocator fills it |
| SWF Encrypt code (tag 253, branches out of the block) | works: the real program executes |
| MochiAds preloader (`MochiAd.showPreloaderAd`) | works: the shim reports a finished ad, so the movie starts |
| Reaching "main" | works unattended: intro -> title -> game screen at frame 11 |
| Clicking a tower in the panel | handler runs, but nothing is selected yet |
| Text glyph shape | letters draw as fragments: the glyph fill matrix is wrong |

Rendering is also a problem: 640x480 at 40 fps is 12 M pixels/s of software
blending, which the 268 MHz ARM11 will not manage. Playable Bloons needs an
internal render size of about 320x240 scaled to the screen, or the GPU path.

The decoder bugs this movie exposed are listed above, and the last one was not
a decoder bug at all: the movie is SWF Encrypt protected, and what a player
needs is AVM1's rule that a branch may leave the block it started in. With that
implemented the real program runs, the preloader completes and the title screen
renders.

The intro's hand-off to the main timeline was the last thing gated on the
interpreter, and it is solved: the title clip plays its 131 frames, then sets
itself invisible and leaves `playarea`, the tower panel and the HUD on stage.
What still blocks play is rendering and input, not scripting:

* static and dynamic text draw their glyph outlines with the wrong matrix, so
  labels read as letter fragments;
* the preloader's cover shape (depth 548) is still on stage under the game;
* pressing a tower button in `toweroptions` runs its action block but selects
  nothing, so no tower can be placed and `startrnd_btn` never appears.

Diagnostics added for this work: `--order` composes the display list one depth
at a time and samples stage points, `--upto N` writes the composite of depths up
to `N`, `--clicks "x,y x,y"` replays a tap sequence, and the display dump now
marks masks (`MASK->depth`) and cxform alpha.

## Roadmap

1. Audio: SWF ADPCM/MP3 decoding, mixer, NDSP output (needs `dspfirm.cdc`)
2. Real masks: clip-depth regions currently hide the mask object but do not clip
   what sits under it
3. Performance pass and per-console presets (New 3DS 804 MHz + L2)

## Licence

MIT for this project. Font data: `font8x8` by Daniel Hepper (public domain).
Ruffle is not currently linked; the parser and renderer here are original.
