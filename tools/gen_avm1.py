#!/usr/bin/env python3
"""Generates SWF movies that exercise the AVM1 interpreter.

The action bytecode is assembled here by hand from the SWF action format, so a
decoder or interpreter bug shows up as a failing assertion in
`tools/hostcheck --avm1 <dir>` rather than as a silent no-op on hardware.

Movies:
  stop.swf    -> frame 1 calls stop(), so the playhead must not advance
  goto.swf    -> frame 1 calls gotoAndPlay(3)
  vars.swf    -> arithmetic, comparison, branch, gotoAndStop(3)
  keys.swf    -> Key.isDown() moves a named clip with GetProperty/SetProperty
  button.swf  -> DefineButton2 on(press), on(release) and on(keyPress)
  clipevent.swf -> onClipEvent(load/enterFrame/keyDown/keyUp/keyPress)
  functions.swf -> user functions: parameters, return, recursion, locals,
                   registers and a method's `this`
  fnevent.swf -> clip-event handlers calling user functions after stop()
  objects.swf -> object/array literals, prototypes, new, arguments, apply,
                 string methods and a for...in loop

Usage: python tools/gen_avm1.py <out-dir>
"""
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gen_swf import BitWriter, nbits_for, rect, tag, shape_tag

W, H = 320, 240


# ---------------------------------------------------------------------------
# Action assembler
# ---------------------------------------------------------------------------

def act(op, body=b""):
    """One action record. Only opcodes >= 0x80 carry a length field."""
    if op >= 0x80:
        return bytes([op]) + struct.pack("<H", len(body)) + body
    return bytes([op]) + body


def push(*items):
    body = b""
    for it in items:
        if isinstance(it, bool):
            body += b"\x05" + bytes([1 if it else 0])
        elif isinstance(it, str):
            body += b"\x00" + it.encode() + b"\x00"
        elif isinstance(it, int):
            body += b"\x07" + struct.pack("<i", it)
        else:
            body += b"\x01" + struct.pack("<f", it)
    return act(0x96, body)


OP_STOP = 0x07
OP_PLAY = 0x06
OP_ADD = 0x0A
OP_SUB = 0x0B
OP_MUL = 0x0C
OP_EQUALS = 0x0E
OP_NOT = 0x12
OP_POP = 0x17
OP_GET_VAR = 0x1C
OP_SET_VAR = 0x1D
OP_GET_PROP = 0x22
OP_SET_PROP = 0x23
OP_LESS = 0x0F
OP_CALL_METHOD = 0x52
OP_INIT_ARRAY = 0x42
OP_INIT_OBJECT = 0x43
OP_NEW_OBJECT = 0x40
OP_ENUMERATE2 = 0x55
OP_STORE_REGISTER = 0x87
OP_GET_MEMBER = 0x4E
OP_SET_MEMBER = 0x4F
OP_CALL_FUNCTION = 0x3D
OP_RETURN = 0x3E
OP_DEFINE_LOCAL = 0x3C
OP_IF = 0x9D
OP_JUMP = 0x99
OP_GOTO_FRAME2 = 0x9F

PROP_X = 0
PROP_Y = 1

# Pixels per frame for the keys.swf box, in twips (20 per pixel).
SPEED_TWIPS = 200

EV_LOAD = 0x0001
EV_ENTER_FRAME = 0x0002
EV_KEY_DOWN = 0x0040
EV_KEY_UP = 0x0080
EV_KEY_PRESS = 0x00020000


class Code:
    """A tiny label-free assembler for ActionIf/ActionJump pairs.

    Parts are either byte blobs or ('jump', target_index). The target index is
    a part index, which keeps conditional blocks readable.
    """

    def __init__(self):
        self.parts = []

    def emit(self, blob):
        self.parts.append(blob)
        return self

    def mark(self):
        return len(self.parts)

    OPCODES = {"jump": OP_JUMP, "if": OP_IF}

    def build(self, end=True):
        """Returns the bytecode with branch offsets resolved. `end` appends
        ActionEnd, which standalone blocks need but spliced blocks must not."""
        sizes = [5 if isinstance(p, tuple) else len(p) for p in self.parts]
        starts = []
        pos = 0
        for size in sizes:
            starts.append(pos)
            pos += size
        starts.append(pos)  # one past the end, a valid branch target
        out = bytearray()
        for i, part in enumerate(self.parts):
            if isinstance(part, tuple):
                kind, target = part
                offset = starts[target] - (starts[i] + 5)
                out += bytes([self.OPCODES[kind]]) + struct.pack("<H", 2) + struct.pack("<h", offset)
            else:
                out += part
        if end:
            out += act(0x00)
        return bytes(out)


def place2(depth, char, tx, ty, name=None, move=False, clip_actions=None):
    bw = BitWriter()
    flags = 0x04 | (0x00 if move else 0x02)
    if name:
        flags |= 0x20
    if clip_actions:
        flags |= 0x80  # HasClipActions
    bw.ub(flags, 8)
    bw.le16(depth)
    if not move:
        bw.le16(char)
    bw.bit(0)  # no scale
    bw.bit(0)  # no rotate
    tn = nbits_for([tx, ty])
    bw.ub(tn, 5)
    bw.sb(tx, tn)
    bw.sb(ty, tn)
    if name or clip_actions:
        # Fields after MATRIX are byte-aligned; skipping this shifts the name
        # into the middle of the matrix's trailing bits.
        bw.align()
    if name:
        for b in name.encode() + b"\x00":
            bw.ub(b, 8)
    if clip_actions:
        # ClipActions: reserved word, union of all enabled events, then one
        # record per handler (event mask, byte size, optional key code, code).
        bw.le16(0)
        all_events = 0
        for events, _key, _code in clip_actions:
            all_events |= events
        for i in range(4):
            bw.ub((all_events >> (8 * i)) & 0xFF, 8)
        for events, key, code in clip_actions:
            for i in range(4):
                bw.ub((events >> (8 * i)) & 0xFF, 8)
            size = len(code) + (1 if events & EV_KEY_PRESS else 0)
            for i in range(4):
                bw.ub((size >> (8 * i)) & 0xFF, 8)
            if events & EV_KEY_PRESS:
                bw.ub(key, 8)
            for b in code:
                bw.ub(b, 8)
        for _ in range(4):
            bw.ub(0, 8)  # end of records
    return tag(26, bw.take())


def push_reg(n):
    """ActionPush with a register operand (type 4), used by DefineFunction2."""
    return act(0x96, b"\x04" + bytes([n]))


def constant_pool(strings):
    """ActionConstantPool (0x88): DefineFunction names and parameter names are
    indices into this table, never inline strings."""
    payload = struct.pack("<H", len(strings))
    for s in strings:
        payload += s.encode() + b"\x00"
    return act(0x88, payload)


def define_function(name, params, body):
    """ActionDefineFunction (0x9B). The body follows the record, outside the
    length field, exactly as the decoder expects it; the function name and
    each parameter name are constant-pool indices, so the pool comes first."""
    names = [name] + list(params)
    payload = struct.pack("<H", 0) + struct.pack("<H", len(params))
    for i in range(len(params)):
        payload += struct.pack("<H", i + 1)
    payload += struct.pack("<H", len(body))
    return constant_pool(names) + act(0x9B, payload) + body


def define_function2(name, params, body, registers=0, flags=0):
    """ActionDefineFunction2 (0x8E): params are (register, name) pairs; a zero
    register means the argument is bound by name instead of by register."""
    names = [name] + [pname for _, pname in params]
    payload = struct.pack("<H", 0) + struct.pack("<H", len(params))
    payload += bytes([registers]) + struct.pack("<H", flags)
    for i, (reg, _pname) in enumerate(params):
        payload += bytes([reg]) + struct.pack("<H", i + 1)
    payload += struct.pack("<H", len(body))
    return constant_pool(names) + act(0x8E, payload) + body


def call(name, count=0):
    """ActionCallFunction with `count` arguments already on the stack."""
    return push(count) + push(name) + act(OP_CALL_FUNCTION)


# ActionPush with the null constant (type 2).
PUSH_NULL = act(0x96, b"\x02")


def call_method(obj_code, name, arg_codes=()):
    """ActionCallMethod: stack is [args..., count, object, name]."""
    out = b""
    for code in reversed(arg_codes):
        out += code
    out += push(len(arg_codes)) + obj_code + push(name) + act(OP_CALL_METHOD)
    return out


def get_member(obj_code, name):
    """ActionGetMember: stack is [object, name]."""
    return obj_code + (push(name) if isinstance(name, str) else name) + act(OP_GET_MEMBER)


def set_member(obj_code, name, value_code):
    """ActionSetMember: stack is [object, name, value]."""
    return obj_code + push(name) + value_code + act(OP_SET_MEMBER)


def keepalive():
    """A frame whose only content is an empty DoAction.

    The decoder drops trailing frames that contain nothing, so a timeline that
    depends on blank frames needs one real record in each of them.
    """
    return tag(12, act(0x00))


def button2(button_id, records, conds):
    """records: [(state_flags, char_id, depth, tx, ty)]; conds: [(cond, key, code)]."""
    body = bytearray(struct.pack("<H", button_id))
    body += b"\x00"  # reserved flags
    body += b"\x00\x00"  # action offset, patched below
    for states, char_id, depth, tx, ty in records:
        bw = BitWriter()
        bw.ub(states, 8)
        bw.le16(char_id)
        bw.le16(depth)
        bw.bit(0)
        bw.bit(0)
        tn = nbits_for([tx, ty])
        bw.ub(tn, 5)
        bw.sb(tx, tn)
        bw.sb(ty, tn)
        bw.align()  # MATRIX is followed by byte-aligned fields
        # No colour transform: hasAdd, hasMul, nbits.
        bw.bit(0)
        bw.bit(0)
        bw.ub(0, 4)
        bw.align()
        body += bw.take()
    body += b"\x00"  # end of records
    offset = len(body)
    for cond, key, code in conds:
        body += struct.pack("<HH", 4 + len(code), (key << 9) | cond)
        body += code
    # Layout is ButtonId (2), ReservedFlags (1), ActionOffset (2).
    body[3:5] = struct.pack("<H", offset)
    return tag(34, body)


def movie(body, frames):
    bw = BitWriter()
    rect(W * 20, H * 20, bw)
    out = bytearray(b"FWS")
    out.append(6)
    out += b"\x00\x00\x00\x00"
    out += bw.take()
    out += struct.pack("<H", 12 << 8)
    out += struct.pack("<H", frames)
    out += body
    out = bytes(out)
    out = out[:4] + struct.pack("<I", len(out)) + out[8:]
    return out


def rect_shape(shape_id, w=2400, h=1800, color=(0xE5, 0x50, 0x4A)):
    poly = [(0, 0), (w, 0), (w, h), (0, h)]
    return shape_tag(32, shape_id, w, h, color, (0x20, 0x10, 0x10), poly)


def show_frame():
    return tag(1, b"")


def do_action(code):
    return tag(12, code)


# ---------------------------------------------------------------------------
# Movies
# ---------------------------------------------------------------------------

def gen_stop():
    body = bytearray()
    body += rect_shape(1)
    body += place2(1, 1, 2000, 2000)
    body += do_action(Code().emit(act(OP_STOP)).build())
    body += show_frame()
    for _ in range(2):
        body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 3)


def gen_goto():
    body = bytearray()
    body += rect_shape(1)
    body += place2(1, 1, 1000, 1000)
    # GotoFrame2 flag bit 0 is "play".
    code = Code().emit(push(3)).emit(act(OP_GOTO_FRAME2, bytes([0x01])))
    body += do_action(code.build())
    body += show_frame()
    for _ in range(3):
        body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 4)


def gen_vars():
    c = Code()
    # y = 10 * 3 + 2 (SetVariable pops the value, so the name goes first)
    c.emit(push("y"))
    c.emit(push(10)).emit(push(3)).emit(act(OP_MUL)).emit(push(2)).emit(act(OP_ADD))
    c.emit(act(OP_SET_VAR))
    # if (y == 32) gotoAndStop(3)
    c.emit(push("y")).emit(act(OP_GET_VAR)).emit(push(32)).emit(act(OP_EQUALS))
    cond = c.mark()
    c.parts.append(("if", 0))
    skip = c.mark()
    c.parts.append(("jump", 0))
    body_idx = c.mark()
    c.parts[cond] = ("if", body_idx)
    c.emit(push(3)).emit(act(OP_GOTO_FRAME2, bytes([0x00])))
    c.parts[skip] = ("jump", c.mark())

    body = bytearray()
    body += rect_shape(1)
    body += place2(1, 1, 500, 500)
    body += do_action(c.build())
    body += show_frame()
    for _ in range(2):
        body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 3)


def key_is_down(code):
    """Key.isDown(code): args, count, object, method name - the VM pops the
    method name, the object and the count from the stack."""
    c = Code()
    c.emit(push(code))
    c.emit(push(1))
    c.emit(push("Key"))
    c.emit(push("isDown"))
    c.emit(act(OP_CALL_METHOD))
    return c.build(end=False)


def if_then(cond, body):
    """`if (cond) { body }` as one spliced block (ActionIf jumps when true)."""
    c = Code()
    c.emit(cond)
    cond_idx = c.mark()
    c.parts.append(("if", 0))
    skip_idx = c.mark()
    c.parts.append(("jump", 0))
    body_idx = c.mark()
    c.parts[cond_idx] = ("if", body_idx)
    c.emit(body)
    c.parts[skip_idx] = ("jump", c.mark())
    return c


def gen_keys():
    """Key.isDown() moves the box one step per frame while a direction is held.

    Two details decide whether this movie does anything on a console:

    - the loop must be gotoAndPlay, because a stopped timeline never polls the
      keys again. ActionGotoFrame2 flags are bit 0 = play and bit 1 = a scene
      bias follows, so 0x02 both stops the movie and makes the decoder read two
      bytes that are not there;
    - the step must be large. 4 twips is a fifth of a pixel per frame, which
      looks like a frozen box even when the interpreter is right.
    """
    setup = Code().emit(push("speed")).emit(push(SPEED_TWIPS)).emit(act(OP_SET_VAR)).build()
    loop = Code().emit(push(2)).emit(act(OP_GOTO_FRAME2, bytes([0x01]))).build()

    def nudge(prop, code, op):
        # _root.hero.<prop> = _root.hero.<prop> (+|-) speed
        # SetProperty pops the value, then the property index, then the target.
        body = Code()
        body.emit(push("_root.hero")).emit(push(prop))
        body.emit(push("_root.hero")).emit(push(prop)).emit(act(OP_GET_PROP))
        body.emit(push("speed")).emit(act(OP_GET_VAR))
        body.emit(act(op))
        body.emit(act(OP_SET_PROP))
        return if_then(key_is_down(code), body.build(end=False))

    poll = Code()
    for prop, code, op in (
        (PROP_X, 39, OP_ADD),   # right
        (PROP_X, 37, OP_SUB),   # left
        (PROP_Y, 40, OP_ADD),   # down
        (PROP_Y, 38, OP_SUB),   # up
    ):
        poll.emit(nudge(prop, code, op).build(end=False))
    poll.emit(push("pos"))
    poll.emit(push("_root.hero")).emit(push(PROP_X)).emit(act(OP_GET_PROP))
    poll.emit(act(OP_SET_VAR))
    poll.emit(push("top"))
    poll.emit(push("_root.hero")).emit(push(PROP_Y)).emit(act(OP_GET_PROP))
    poll.emit(act(OP_SET_VAR))

    # The clip is placed once, on frame 1. Frames 2 and 3 alternate without a
    # placement record, so the clip survives and the script can move it; that is
    # how AS2 games loop their update code.
    body = bytearray()
    body += rect_shape(1, 800, 800)
    body += place2(1, 1, 1000, 1000, name="hero")
    body += do_action(setup)
    body += show_frame()
    body += do_action(poll.build())
    body += show_frame()
    body += do_action(loop)
    body += show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 3)


def gen_clipevent():
    # onClipEvent(enterFrame) keeps running after the root stops, which is the
    # pattern AS2 games use instead of a frame loop.
    enter = Code()
    enter.emit(push("n"))
    enter.emit(push("n")).emit(act(OP_GET_VAR)).emit(push(1)).emit(act(OP_ADD))
    enter.emit(act(OP_SET_VAR))
    enter.emit(push("_root.ticks"))
    enter.emit(push("_root.ticks")).emit(act(OP_GET_VAR)).emit(push(1)).emit(act(OP_ADD))
    enter.emit(act(OP_SET_VAR))

    load = Code().emit(push("loaded")).emit(push(1)).emit(act(OP_SET_VAR))
    keydown = Code()
    keydown.emit(push("keydowns"))
    keydown.emit(push("keydowns")).emit(act(OP_GET_VAR)).emit(push(1)).emit(act(OP_ADD))
    keydown.emit(act(OP_SET_VAR))
    keypress = Code()
    keypress.emit(push("spaces"))
    keypress.emit(push("spaces")).emit(act(OP_GET_VAR)).emit(push(1)).emit(act(OP_ADD))
    keypress.emit(act(OP_SET_VAR))
    keyup = Code().emit(push("keyups")).emit(push(1)).emit(act(OP_SET_VAR))

    clip_actions = [
        (EV_LOAD, 0, load.build()),
        (EV_ENTER_FRAME, 0, enter.build()),
        (EV_KEY_DOWN, 0, keydown.build()),
        (EV_KEY_UP, 0, keyup.build()),
        (EV_KEY_PRESS, 32, keypress.build()),
    ]

    body = bytearray()
    body += rect_shape(1, 800, 800)
    body += place2(1, 1, 1000, 1000, name="hero", clip_actions=clip_actions)
    body += do_action(Code().emit(act(OP_STOP)).build())
    body += show_frame()
    body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 2)


def gen_button():
    press = Code().emit(push("pressed")).emit(push(1)).emit(act(OP_SET_VAR)).build()
    release = Code().emit(push(2)).emit(act(OP_GOTO_FRAME2, bytes([0x00]))).build()
    keypress = (
        Code()
        .emit(push("space_hits"))
        .emit(push("space_hits"))
        .emit(act(OP_GET_VAR))
        .emit(push(1))
        .emit(act(OP_ADD))
        .emit(act(OP_SET_VAR))
        .build()
    )
    conds = [
        (0x04, 0, press),      # on(press)
        (0x08, 0, release),    # on(release)
        (0x00, 32, keypress),  # on(keyPress " "): the key code is bits 9..15
    ]
    btn = button2(10, [(0x09, 1, 1, 0, 0)], conds)  # up + hit-test states

    body = bytearray()
    body += rect_shape(1, 2400, 800, (0x4F, 0xD1, 0xC5))
    body += btn
    body += place2(1, 10, 2000, 1200)
    body += show_frame()
    body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 2)


def gen_functions():
    # function double(v) { return v * 2; }
    double = (
        Code()
        .emit(push("v"))
        .emit(act(OP_GET_VAR))
        .emit(push(2))
        .emit(act(OP_MUL))
        .emit(act(OP_RETURN))
        .build()
    )
    # function fact(n) { if (n < 2) return 1; return n * fact(n - 1); }
    c = Code()
    c.emit(push("n")).emit(act(OP_GET_VAR)).emit(push(2)).emit(act(OP_LESS))
    cond = c.mark()
    c.parts.append(("if", 0))
    c.emit(push("n")).emit(act(OP_GET_VAR))
    c.emit(push("n")).emit(act(OP_GET_VAR)).emit(push(1)).emit(act(OP_SUB))
    c.emit(call("fact", 1))
    c.emit(act(OP_MUL))
    c.emit(act(OP_RETURN))
    skip = c.mark()
    c.parts.append(("jump", 0))
    base = c.mark()
    c.parts[cond] = ("if", base)
    c.emit(push(1)).emit(act(OP_RETURN))
    c.parts[skip] = ("jump", c.mark())
    fact = c.build()

    # function shadow() { var t = 99; return t; }
    shadow = (
        Code()
        .emit(push("t"))
        .emit(push(99))
        .emit(act(OP_DEFINE_LOCAL))
        .emit(push("t"))
        .emit(act(OP_GET_VAR))
        .emit(act(OP_RETURN))
        .build()
    )

    # function diff(a, b) { return a - b; } - order-sensitive, unlike a sum.
    diff = (
        Code()
        .emit(push("a"))
        .emit(act(OP_GET_VAR))
        .emit(push("b"))
        .emit(act(OP_GET_VAR))
        .emit(act(OP_SUB))
        .emit(act(OP_RETURN))
        .build()
    )

    # function mark() { marker = 77; } - a bare assignment inside a root-level
    # function must land on the main timeline, not on "_root.marker".
    mark = (
        Code()
        .emit(push("marker"))
        .emit(push(77))
        .emit(act(OP_SET_VAR))
        .build()
    )

    # DefineFunction2 add3(a, b) with both parameters in registers.
    add3 = (
        Code()
        .emit(push_reg(1))
        .emit(push_reg(2))
        .emit(act(OP_ADD))
        .emit(act(OP_RETURN))
        .build()
    )

    # function moveBy(dx) { this._x = this._x + dx; }
    move = Code()
    move.emit(push("this")).emit(push(PROP_X))
    move.emit(push("this")).emit(push(PROP_X)).emit(act(OP_GET_PROP))
    move.emit(push("dx")).emit(act(OP_GET_VAR))
    move.emit(act(OP_ADD))
    move.emit(act(OP_SET_PROP))
    move_by = move.build()

    # DefineFunction2 getx() with PRELOAD_THIS: register 1 is `this`.
    getx = (
        Code()
        .emit(push_reg(1))
        .emit(push(PROP_X))
        .emit(act(OP_GET_PROP))
        .emit(act(OP_RETURN))
        .build()
    )

    setup = Code()
    setup.emit(define_function("double", ["v"], double))
    setup.emit(push("r"))
    setup.emit(push(21))
    setup.emit(call("double", 1))
    setup.emit(act(OP_SET_VAR))

    setup.emit(define_function("fact", ["n"], fact))
    setup.emit(push("f"))
    setup.emit(push(5))
    setup.emit(call("fact", 1))
    setup.emit(act(OP_SET_VAR))

    # A function-local `var` must not touch the clip variable of that name.
    setup.emit(push("t")).emit(push(1)).emit(act(OP_SET_VAR))
    setup.emit(define_function("shadow", [], shadow))
    setup.emit(push("loc"))
    setup.emit(call("shadow", 0))
    setup.emit(act(OP_SET_VAR))

    setup.emit(define_function("mark", [], mark))
    setup.emit(call("mark", 0))
    setup.emit(act(OP_POP))

    # Arguments arrive in declaration order: diff(10, 3) is 7, not -7.
    setup.emit(define_function("diff", ["a", "b"], diff))
    setup.emit(push("delta"))
    setup.emit(push(3)).emit(push(10))
    setup.emit(call("diff", 2))
    setup.emit(act(OP_SET_VAR))

    setup.emit(define_function2("add3", [(1, "a"), (2, "b")], add3, registers=3))
    setup.emit(push("sum"))
    setup.emit(push(7)).emit(push(8))
    setup.emit(call("add3", 2))
    setup.emit(act(OP_SET_VAR))

    # hero.moveBy = function(dx) { ... } and hero.getx = function() { ... }
    setup.emit(push("hero")).emit(push("moveBy"))
    setup.emit(define_function("", ["dx"], move_by))
    setup.emit(act(OP_SET_MEMBER))
    setup.emit(push("hero")).emit(push("getx"))
    setup.emit(define_function2("", [], getx, registers=2, flags=1))
    setup.emit(act(OP_SET_MEMBER))

    # x0 = hero.getx(); hero.moveBy(50);
    setup.emit(push("x0"))
    setup.emit(push(0)).emit(push("hero")).emit(push("getx"))
    setup.emit(act(OP_CALL_METHOD))
    setup.emit(act(OP_SET_VAR))
    setup.emit(push(50))
    setup.emit(push(1)).emit(push("hero")).emit(push("moveBy"))
    setup.emit(act(OP_CALL_METHOD))

    body = bytearray()
    body += rect_shape(1, 800, 800)
    body += place2(1, 1, 1000, 1000, name="hero")
    body += do_action(setup.build())
    body += show_frame()
    body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 2)


def gen_fnevent():
    # function bump() { _root.hits = _root.hits + 1; }
    bump = Code()
    bump.emit(push("_root.hits"))
    bump.emit(push("_root.hits")).emit(act(OP_GET_VAR)).emit(push(1)).emit(act(OP_ADD))
    bump.emit(act(OP_SET_VAR))
    # function step(dx) { _root.steps += dx; _root.hero._x += dx; }
    step = Code()
    step.emit(push("_root.steps"))
    step.emit(push("_root.steps")).emit(act(OP_GET_VAR))
    step.emit(push("dx")).emit(act(OP_GET_VAR))
    step.emit(act(OP_ADD))
    step.emit(act(OP_SET_VAR))
    step.emit(push("_root.hero")).emit(push(PROP_X))
    step.emit(push("_root.hero")).emit(push(PROP_X)).emit(act(OP_GET_PROP))
    step.emit(push("dx")).emit(act(OP_GET_VAR))
    step.emit(act(OP_ADD))
    step.emit(act(OP_SET_PROP))

    enter = Code().emit(call("bump", 0)).build()
    keydown = Code().emit(push(5)).emit(call("step", 1)).build()

    clip_actions = [
        (EV_ENTER_FRAME, 0, enter),
        (EV_KEY_DOWN, 0, keydown),
    ]

    setup = Code()
    setup.emit(push("hits")).emit(push(0)).emit(act(OP_SET_VAR))
    setup.emit(push("steps")).emit(push(0)).emit(act(OP_SET_VAR))
    setup.emit(define_function("bump", [], bump.build()))
    setup.emit(define_function("step", ["dx"], step.build()))
    setup.emit(act(OP_STOP))

    body = bytearray()
    body += rect_shape(1, 800, 800)
    body += place2(1, 1, 1000, 1000, name="hero", clip_actions=clip_actions)
    body += do_action(setup.build())
    body += show_frame()
    body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 2)


def assign(name, value_code):
    """SetVariable pops the value then the name, so the name goes first."""
    return push(name) + value_code + act(OP_SET_VAR)


def gen_clipapi():
    """MovieClip methods: gotoAndStop, getDepth, getNextHighestDepth,
    hitTest and swapDepths, driven through `box.<method>()` calls."""
    inner = bytearray()
    inner += rect_shape(2, 2400, 1800)
    inner += place2(1, 2, 0, 0)
    inner += show_frame()
    inner += keepalive() + show_frame()
    inner += keepalive() + show_frame()
    inner += tag(0, b"")

    body = bytearray()
    body += tag(39, struct.pack("<HH", 10, 3) + bytes(inner))
    # The clip sits at (100,100) and is 120x90, so (150,150) is inside it.
    body += place2(1, 10, 2000, 2000, name="box")

    box = push("_root.box")
    c = Code()
    c.emit(call_method(box, "gotoAndStop", [push(3)]))
    c.emit(assign("at_frame", get_member(box, "_currentframe")))
    c.emit(assign("depth_now", call_method(box, "getDepth")))
    c.emit(assign("next_depth", call_method(box, "getNextHighestDepth")))
    c.emit(assign("hit_inside", call_method(box, "hitTest", [push(150), push(150)])))
    c.emit(assign("hit_outside", call_method(box, "hitTest", [push(400), push(400)])))
    c.emit(call_method(box, "swapDepths", [push(5)]))
    c.emit(assign("depth_after", call_method(box, "getDepth")))
    c.emit(act(OP_STOP))
    body += do_action(c.build())
    body += show_frame()
    body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 3)


def gen_objects():
    this = push("this") + act(OP_GET_VAR)

    # function Pt(x, y) { this.x = x; this.y = y; }
    pt = Code()
    for prop in ("x", "y"):
        pt.emit(this)
        pt.emit(push(prop))
        pt.emit(push(prop)).emit(act(OP_GET_VAR))
        pt.emit(act(OP_SET_MEMBER))

    # function() { return this.x + this.y; }  (assigned to Pt.prototype.sum)
    summ = Code()
    summ.emit(get_member(this, "x"))
    summ.emit(get_member(this, "y"))
    summ.emit(act(OP_ADD))
    summ.emit(act(OP_RETURN))

    # function cnt() { return arguments.length; }
    cnt = (
        Code()
        .emit(get_member(push("arguments") + act(OP_GET_VAR), "length"))
        .emit(act(OP_RETURN))
    )
    # function arg2() { return arguments[1]; }
    arg2 = (
        Code()
        .emit(get_member(push("arguments") + act(OP_GET_VAR), push(1)))
        .emit(act(OP_RETURN))
    )
    # function addtwo(a, b) { return a + b; }
    addtwo = (
        Code()
        .emit(push("a"))
        .emit(act(OP_GET_VAR))
        .emit(push("b"))
        .emit(act(OP_GET_VAR))
        .emit(act(OP_ADD))
        .emit(act(OP_RETURN))
    )

    o = push("o") + act(OP_GET_VAR)
    a = push("a") + act(OP_GET_VAR)
    p = push("p") + act(OP_GET_VAR)

    s = Code()

    # o = { a: 1 }; o.b = 2; sum1 = o.a + o.b;
    s.emit(assign("o", push("a") + push(1) + push(1) + act(OP_INIT_OBJECT)))
    s.emit(set_member(o, "b", push(2)))
    s.emit(assign("sum1", get_member(o, "a") + get_member(o, "b") + act(OP_ADD)))

    # sum2 = o["b"];  dynamic property name
    s.emit(assign("sum2", get_member(o, push("b"))))

    # a = [1, 2, 3];  elements are pushed right-to-left
    s.emit(assign("a", push(3) + push(2) + push(1) + push(3) + act(OP_INIT_ARRAY)))
    s.emit(assign("n0", get_member(a, "length")))

    # n1 = a.push(4);
    s.emit(assign("n1", call_method(a, "push", [push(4)])))
    # last = a.pop();
    s.emit(assign("last", call_method(a, "pop")))
    # joined = a.join("-");
    s.emit(assign("joined", call_method(a, "join", [push("-")])))
    # first = a[0]; a[1] = 9; second = a[1]; idx = a.indexOf(9);
    s.emit(assign("first", get_member(a, push(0))))
    s.emit(set_member(a, "1", push(9)))
    s.emit(assign("second", get_member(a, push(1))))
    s.emit(assign("idx", call_method(a, "indexOf", [push(9)])))

    # function Pt(...) and Pt.prototype.sum = function() { ... }
    s.emit(define_function("Pt", ["x", "y"], pt.build()))
    s.emit(
        set_member(
            get_member(push("Pt") + act(OP_GET_VAR), "prototype"),
            "sum",
            define_function("", [], summ.build()),
        )
    )

    # p = new Pt(3, 4); total = p.sum(); px = p.x;
    s.emit(assign("p", push(4) + push(3) + push(2) + push("Pt") + act(OP_NEW_OBJECT)))
    s.emit(assign("total", call_method(p, "sum")))
    s.emit(assign("px", get_member(p, "x")))

    # k = cnt(7, 8, 9); argsecond = arg2(7, 8, 9);
    s.emit(define_function("cnt", [], cnt.build()))
    s.emit(define_function("arg2", [], arg2.build()))
    s.emit(assign("k", push(9) + push(8) + push(7) + call("cnt", 3)))
    s.emit(assign("argsecond", push(9) + push(8) + push(7) + call("arg2", 3)))

    # applied = addtwo.apply(null, [2, 3]);
    s.emit(define_function("addtwo", ["a", "b"], addtwo.build()))
    arr = push(3) + push(2) + push(2) + act(OP_INIT_ARRAY)
    s.emit(
        assign(
            "applied",
            call_method(
                push("addtwo") + act(OP_GET_VAR),
                "apply",
                [PUSH_NULL, arr],
            ),
        )
    )

    # for (k in o) { nkeys = nkeys + 1; }  - the loop protocol is
    # Enumerate2, then StoreRegister/compare-with-null/If per iteration.
    s.emit(assign("nkeys", push(0)))
    enum = Code()
    enum.emit(o + act(OP_ENUMERATE2))
    loop = enum.mark()
    # StoreRegister peeks the top of stack (the key) and keeps it there.
    enum.emit(act(OP_STORE_REGISTER, bytes([1])))
    enum.emit(PUSH_NULL)
    enum.emit(act(OP_EQUALS))
    done = enum.mark()
    enum.parts.append(("if", 0))
    enum.emit(
        assign(
            "nkeys",
            push("nkeys") + act(OP_GET_VAR) + push(1) + act(OP_ADD),
        )
    )
    enum.parts.append(("jump", loop))
    enum.parts[done] = ("if", enum.mark())
    # Spliced into the frame's block, so it must not append its own ActionEnd.
    s.emit(enum.build(end=False))

    # String methods on a plain string.
    s.emit(assign("up", call_method(push("hello"), "toUpperCase")))
    s.emit(assign("ch", call_method(push("hello"), "charAt", [push(1)])))
    s.emit(assign("sub", call_method(push("hello"), "substr", [push(1), push(3)])))
    s.emit(assign("slen", get_member(push("hello"), "length")))

    body = bytearray()
    body += rect_shape(1, 800, 800)
    body += place2(1, 1, 1000, 1000)
    body += do_action(s.build())
    body += show_frame()
    body += keepalive() + show_frame()
    body += tag(0, b"")
    return movie(bytes(body), 2)


def main(out_dir):
    os.makedirs(out_dir, exist_ok=True)
    for name, data in [
        ("stop.swf", gen_stop()),
        ("goto.swf", gen_goto()),
        ("vars.swf", gen_vars()),
        ("keys.swf", gen_keys()),
        ("button.swf", gen_button()),
        ("clipevent.swf", gen_clipevent()),
        ("functions.swf", gen_functions()),
        ("fnevent.swf", gen_fnevent()),
        ("objects.swf", gen_objects()),
        ("clipapi.swf", gen_clipapi()),
    ]:
        path = os.path.join(out_dir, name)
        with open(path, "wb") as f:
            f.write(data)
        print(f"wrote {path} ({len(data)} bytes)")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else ".tmp/avm1")
