import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bmesh
import bpy
from common import *
from mathutils import Vector

MONO = os.path.join(bpy.utils.system_resource("DATAFILES", path="fonts"), "DejaVuSansMono.woff2")
LOOP = 97
EXIT = 145
END = 160
PERIOD = 48
GREEN = (0.18, 1.0, 0.32)
HOT = (0.85, 1.0, 0.85)

SCREEN_Z = 0.15
SCREEN_Y = -0.55
OPEN_W, OPEN_H = 2.4, 1.5
RASTER_W, RASTER_H = 2.2, 1.32

s = reset(samples=32, view="Standard")


def empty(name, parent=None, location=(0, 0, 0)):
    e = link(bpy.data.objects.new(name, None))
    e.parent = parent
    e.location = location
    return e


def box(name, size, location, bevel=0.0, segments=4, parent=None, mat=None):
    me = bpy.data.meshes.new(name)
    bm = bmesh.new()
    bmesh.ops.create_cube(bm, size=1.0)
    for v in bm.verts:
        v.co = Vector((v.co.x * size[0], v.co.y * size[1], v.co.z * size[2]))
    bm.to_mesh(me)
    bm.free()
    o = link(bpy.data.objects.new(name, me))
    o.location = location
    o.parent = parent
    if bevel:
        b = o.modifiers.new("bevel", "BEVEL")
        b.width = bevel
        b.segments = segments
        b.harden_normals = True
        for p in me.polygons:
            p.use_smooth = True
    if mat:
        me.materials.append(mat)
    return o


def node_math(nt, op, a, b=None):
    n = nt.nodes.new("ShaderNodeMath")
    n.operation = op
    for i, v in enumerate((a, b)):
        if v is None:
            continue
        if isinstance(v, (int, float)):
            n.inputs[i].default_value = v
        else:
            nt.links.new(v, n.inputs[i])
    return n.outputs[0]


def sock(sockets, name, kind):
    return next(x for x in sockets if x.name == name and x.type == kind)


def value(nt, name, v):
    n = nt.nodes.new("ShaderNodeValue")
    n.name = name
    n.outputs[0].default_value = v
    return n.outputs[0]


def phosphor(name, strength, root, vignette=False, band_gain=1.2):
    m = bpy.data.materials.new(name)
    nt = m.node_tree
    nt.nodes.clear()
    out = nt.nodes.new("ShaderNodeOutputMaterial")
    em = nt.nodes.new("ShaderNodeEmission")
    power = value(nt, "power", 0.0)
    heat = value(nt, "heat", 0.0)
    band = value(nt, "band", 2.0)
    tc = nt.nodes.new("ShaderNodeTexCoord")
    tc.object = root
    sep = nt.nodes.new("ShaderNodeSeparateXYZ")
    nt.links.new(tc.outputs["Object"], sep.inputs[0])
    d = node_math(nt, "DIVIDE", node_math(nt, "SUBTRACT", sep.outputs["Z"], band), 0.14)
    g = node_math(nt, "EXPONENT", node_math(nt, "MULTIPLY", node_math(nt, "MULTIPLY", d, d), -1.0))
    k = node_math(nt, "ADD", node_math(nt, "MULTIPLY", g, band_gain), 1.0)
    st = node_math(nt, "MULTIPLY", node_math(nt, "MULTIPLY", power, k), strength)
    if vignette:
        gen = nt.nodes.new("ShaderNodeSeparateXYZ")
        nt.links.new(tc.outputs["Generated"], gen.inputs[0])

        def falloff(axis):
            c = node_math(nt, "ABSOLUTE", node_math(nt, "SUBTRACT", node_math(nt, "MULTIPLY", gen.outputs[axis], 2.0), 1.0))
            return node_math(nt, "SUBTRACT", 1.0, node_math(nt, "POWER", c, 6.0))

        st = node_math(nt, "MULTIPLY", st, node_math(nt, "MULTIPLY", falloff("X"), falloff("Z")))
    mix = nt.nodes.new("ShaderNodeMix")
    mix.data_type = "RGBA"
    sock(mix.inputs, "A", "RGBA").default_value = (*GREEN, 1)
    sock(mix.inputs, "B", "RGBA").default_value = (*HOT, 1)
    nt.links.new(heat, sock(mix.inputs, "Factor", "VALUE"))
    nt.links.new(sock(mix.outputs, "Result", "RGBA"), em.inputs["Color"])
    nt.links.new(st, em.inputs["Strength"])
    nt.links.new(em.outputs[0], out.inputs[0])
    return m


def glass_material():
    m = bpy.data.materials.new("glass")
    m.surface_render_method = "BLENDED"
    nt = m.node_tree
    nt.nodes.clear()
    out = nt.nodes.new("ShaderNodeOutputMaterial")
    mix = nt.nodes.new("ShaderNodeMixShader")
    tr = nt.nodes.new("ShaderNodeBsdfTransparent")
    gl = nt.nodes.new("ShaderNodeBsdfGlossy")
    gl.inputs["Roughness"].default_value = 0.12
    fr = nt.nodes.new("ShaderNodeFresnel")
    fr.inputs["IOR"].default_value = 1.6
    fac = node_math(nt, "ADD", node_math(nt, "MULTIPLY", fr.outputs[0], 0.7), 0.0)
    nt.links.new(fac, mix.inputs[0])
    nt.links.new(tr.outputs[0], mix.inputs[1])
    nt.links.new(gl.outputs[0], mix.inputs[2])
    nt.links.new(mix.outputs[0], out.inputs[0])
    return m


beige = material("case", (0.56, 0.47, 0.34), roughness=0.42)
beige_dark = material("plinth", (0.3, 0.26, 0.2), roughness=0.5)
tube = material("tube", (0.012, 0.02, 0.016), roughness=0.25)
slot = material("slot", (0.02, 0.018, 0.015), roughness=0.8)
led_mat = emission("led", (0.4, 1.0, 0.2), 0.0)

terminal = empty("terminal")
body = empty("body", terminal)

case = box("case", (3.0, 1.6, 2.3), (0, 0, 0), bevel=0.16, segments=5, parent=body, mat=beige)
housing = box("housing", (2.5, 1.0, 1.85), (0, 1.15, 0.08), bevel=0.22, segments=5, parent=body, mat=beige)
plinth = box("plinth", (2.5, 1.3, 0.16), (0, 0.15, -1.22), bevel=0.05, segments=3, parent=body, mat=beige_dark)

cutters = bpy.data.collections.new("cutters")
s.collection.children.link(cutters)


def cutter(name, size, location, bevel=0.0):
    o = box(name, size, location, bevel=bevel, segments=4, parent=body)
    s.collection.objects.unlink(o)
    cutters.objects.link(o)
    o.display_type = "WIRE"
    o.hide_render = True
    return o


cutter("recess", (OPEN_W, 0.6, OPEN_H), (0, -0.8, SCREEN_Z), bevel=0.12)
cutter("floppy", (0.62, 0.3, 0.05), (0.72, -0.8, -0.8))
for side in (-1, 1):
    for i in range(6):
        cutter(f"vent{side}{i}", (0.2, 0.05, 0.62), (side * 1.5, 0.05 + i * 0.13, 0.35))
cutter("handle", (1.2, 0.18, 0.2), (0, 0.55, 1.15), bevel=0.06)

boolean = case.modifiers.new("cut", "BOOLEAN")
boolean.operation = "DIFFERENCE"
boolean.operand_type = "COLLECTION"
boolean.collection = cutters
boolean.solver = "EXACT"

tube_face = box("tube_face", (OPEN_W + 0.1, 0.02, OPEN_H + 0.1), (0, SCREEN_Y + 0.04, SCREEN_Z), parent=body, mat=tube)

led = box("led", (0.07, 0.04, 0.07), (-1.08, -0.81, -0.8), bevel=0.03, segments=3, parent=body, mat=led_mat)

display = empty("display", body, (0, SCREEN_Y, SCREEN_Z))
panel_mat = phosphor("raster", 0.065, display, vignette=True, band_gain=2.5)
mark_mat = phosphor("wordmark", 1.1, display, band_gain=0.6)
prompt_mat = phosphor("prompt", 1.0, display, band_gain=0.6)
beam_mat = emission("beam", HOT, 0.0)

raster = bpy.data.objects.new("raster", bpy.data.meshes.new("raster"))
link(raster)
bm = bmesh.new()
bmesh.ops.create_grid(bm, x_segments=1, y_segments=1, size=0.5)
for v in bm.verts:
    v.co = Vector((v.co.x * RASTER_W, 0, v.co.y * RASTER_H))
bm.to_mesh(raster.data)
bm.free()
raster.parent = display
raster.data.materials.append(panel_mat)
raster.data.uv_layers.new()


def glyphs(body_text, font, size, name, mat):
    t = text(body_text, font=font, size=size, name=name)
    t.data.materials.append(mat)
    t.parent = display
    t.rotation_euler = (math.radians(90), 0, 0)
    return t


def bounds(obj):
    bpy.context.view_layer.update()
    xs = [v[0] for v in obj.bound_box]
    ys = [v[1] for v in obj.bound_box]
    return min(xs), max(xs), min(ys), max(ys)


mark = glyphs("facetty", "/System/Library/Fonts/Supplemental/Arial Black.ttf", 1.0, "wordmark", mark_mat)
x0, x1, y0, y1 = bounds(mark)
size = 1.98 / (x1 - x0)
mark.data.size = size
x0, x1, y0, y1 = bounds(mark)
mark.location = (-(x0 + x1) / 2, -0.012, 0.2 - (y0 + y1) / 2)
mark_left = mark.location.x + x0

PROMPT_SIZE = 0.26
prompt_chars = []
advance = PROMPT_SIZE * 0.602
for i, ch in enumerate("> join"):
    if ch == " ":
        prompt_chars.append(None)
        continue
    c = glyphs(ch, MONO, PROMPT_SIZE, f"prompt{i}", prompt_mat)
    c.data.align_x = "LEFT"
    c.data.align_y = "BOTTOM_BASELINE"
    c.data.offset = 0.012
    c.location = (mark_left + i * advance, -0.012, -0.44)
    prompt_chars.append(c)
cursor = box("cursor", (advance * 0.92, 0.01, PROMPT_SIZE * 0.78), (0, -0.012, -0.44 + PROMPT_SIZE * 0.36), parent=display, mat=prompt_mat)

beam = box("beam", (1.0, 0.005, 1.0), (0, -0.02, 0), parent=display, mat=beam_mat)

glass = bpy.data.objects.new("glass", bpy.data.meshes.new("glass"))
link(glass)
bm = bmesh.new()
bmesh.ops.create_grid(bm, x_segments=40, y_segments=28, size=0.5)
a, b = OPEN_W / 2, OPEN_H / 2
for v in bm.verts:
    x, z = v.co.x * 2 * a, v.co.y * 2 * b
    bulge = 0.07 * (1 - (x / a) ** 2) * (1 - (z / b) ** 2)
    v.co = Vector((x, -bulge, z))
for f in bm.faces:
    f.smooth = True
bm.to_mesh(glass.data)
bm.free()
glass.parent = body
glass.location = (0, SCREEN_Y - 0.04, SCREEN_Z)
glass.data.materials.append(glass_material())

spill = light("AREA", (0, 0, 0), 0.0, color=GREEN)
spill.parent = display
spill.location = (0, -0.06, 0)
spill.rotation_euler = (math.radians(-90), 0, 0)
spill.data.shape = "RECTANGLE"
spill.data.size = RASTER_W
spill.data.size_y = RASTER_H

key = light("AREA", (4.5, -6, 5.5), 1100, color=(1.0, 0.9, 0.78), target=(0, 0, 0))
key.data.size = 5
rim = light("AREA", (-5, 4.5, 4.5), 5200, color=(0.45, 0.68, 1.0), target=(0, 0.4, 0.4))
rim.data.size = 3
rim2 = light("AREA", (5.5, 5, -0.5), 3200, color=(0.35, 0.85, 1.0), target=(0, 0.4, 0))
rim2.data.size = 2.5
fill = light("AREA", (-4, -6, -1), 260, color=(0.75, 0.8, 1.0), target=(0, 0, 0))
fill.data.size = 6
fill.data.specular_factor = 0.0
key.data.specular_factor = 0.35

cam = camera((0, -8, 1), (0, 0, 0), lens=50)
cam.data.sensor_width = 36


def phase(f):
    return ((f - LOOP) % PERIOD) / PERIOD


def wave(f, k, offset=0.0):
    return math.sin(2 * math.pi * (k * phase(f) + offset))


def ease_out_back(t, c1=2.2):
    t = min(max(t, 0.0), 1.0)
    return 1 + (c1 + 1) * (t - 1) ** 3 + c1 * (t - 1) ** 2


def drift_amp(f):
    return smooth((f - 56) / 40)


def screen(f):
    """Returns (on, sx, sz, power, heat, beam_w, beam_h, beam_power)."""
    if f < 34:
        return False, 1, 1, 0.0, 1.0, 0, 0, 0.0
    if f < 37:
        t = (f - 33) / 4
        r = 0.045 * ease_out(t)
        return False, 1, 1, 0.0, 1.0, r, r, 40 * ease_out(t)
    if f < 43:
        t = ease_out((f - 37) / 6, 4)
        return False, 1, 1, 0.0, 1.0, lerp(0.05, RASTER_W, t), lerp(0.04, 0.012, t), 40
    if f < 52:
        t = (f - 43) / 8
        sz = max(0.012, ease_out_back(t))
        return True, 1, sz, lerp(3.0, 1.6, t), 1.0, RASTER_W, 0.012, 40 * (1 - ease_out(t * 2.5))
    t = (f - 52) / 14
    power = lerp(1.6, 1.0, ease_out(t))
    dips = {53: 0.45, 54: 0.8, 57: 0.6, 61: 0.85}
    power *= dips.get(f, 1.0)
    return True, 1, 1, power, max(0.0, 1 - ease_out(t, 2)), 0, 0, 0.0


TYPE_AT = [66, 70, 74, 78]


def cursor_x(f):
    typed = sum(1 for t in TYPE_AT if f >= t)
    return mark_left + (2 + typed) * advance + advance * 0.46


def decay(f, at, tau, freq):
    if f < at:
        return 0.0
    t = f - at
    return math.exp(-t / tau) * math.sin(t * freq)


EXIT_FLARE = {
    "prompt": ([4.0, 4.5, 3.5, 2.8, 2.4, 2.2, 2.0], [1.0, 1.0, 0.8, 0.6, 0.5, 0.45, 0.4]),
    "raster": ([1.6, 6.0, 4.5, 3.6, 3.2, 3.0], [0.3, 1.0, 0.8, 0.6, 0.5, 0.5]),
    "wordmark": ([1.15, 2.2, 1.9, 1.7, 1.6, 1.5], [0.2, 0.9, 0.7, 0.5, 0.4, 0.35]),
}
AIM = Vector((0.054, 0, 0.253))
NEAR = 0.15


def exit_fade(f):
    return (1 - smooth((f - EXIT - 8) / 5)) ** 2


def blackout(f):
    return f >= END - 2


def flare(name, f):
    powers, heats = EXIT_FLARE[name]
    k = min(f - EXIT, len(powers) - 1)
    return powers[k] * exit_fade(f), heats[k]


def exit_camera(f, pos, target):
    bpy.context.view_layer.update()
    m = display.matrix_world
    aim_at = m @ AIM
    normal = (m.to_3x3() @ Vector((0, -1, 0))).normalized()
    s = min(max((f - EXIT - 1) / 10, 0.0), 1.0)
    away = pos - aim_at
    dist = away.length * (NEAR / away.length) ** (s**1.8)
    direction = away.normalized().lerp(normal, smooth(s)).normalized()
    return aim_at + direction * dist, target.lerp(aim_at, smooth(s * 2.5)), lerp(50, 24, smooth(s)), dist


LIGHT_IN = [(key, key.data.energy, 6, 22), (rim, rim.data.energy, 1, 12), (rim2, rim2.data.energy, 2, 14), (fill, fill.data.energy, 10, 20)]


def key_all(f):
    for lamp, energy, at, length in LIGHT_IN:
        lamp.data.energy = energy * smooth((f - at) / length)
        lamp.data.keyframe_insert("energy", frame=f)

    yaw = lerp(math.radians(-42), 0.0, smooth((f - 1) / 50) ** 0.9)
    tilt = lerp(math.radians(-7), 0.0, smooth((f - 1) / 56))
    terminal.rotation_euler = (tilt, 0, yaw)
    terminal.location = (0, 0, 0.035 * wave(f, 1))
    body.rotation_euler = (0.006 * wave(f, 1, 0.25) * drift_amp(f), 0, 0.008 * wave(f, 1, 0.6) * drift_amp(f))
    terminal.keyframe_insert("rotation_euler", frame=f)
    terminal.keyframe_insert("location", frame=f)
    body.keyframe_insert("rotation_euler", frame=f)

    u = 0.16 * smooth(f / 48) + 0.84 * smooth((f - 38) / 58)
    start = Vector((-0.6, -8.6, 2.0))
    end = Vector((0, SCREEN_Y - 4.0, SCREEN_Z))
    pos = start.lerp(end, u)
    target = Vector((0, 0.2, 0.0)).lerp(Vector((0, SCREEN_Y, SCREEN_Z)), smooth(u * 1.15))
    pos += Vector((0.05 * wave(f, 1, 0.1), 0.0, 0.03 * wave(f, 2, 0.3))) * drift_amp(f)
    pos.z += 0.035 * wave(f, 1)
    target.z += 0.035 * wave(f, 1)
    pos.z += 0.012 * decay(f, 43, 4, 1.9)
    target += Vector((0.025 * decay(f, 43, 5, 2.3), 0, 0.045 * decay(f, 43, 4, 1.9)))
    lens, clip = 50, 0.1
    if f >= EXIT:
        pos, target, lens, dist = exit_camera(f, pos, target)
        clip = 0.1 if dist > 0.6 else 0.005
    aim(cam, pos, target, roll=0.01 * wave(f, 1, 0.4) * drift_amp(f))
    cam.data.lens = lens
    cam.data.clip_start = clip
    cam.data.keyframe_insert("clip_start", frame=f)
    cam.keyframe_insert("location", frame=f)
    cam.keyframe_insert("rotation_euler", frame=f)
    cam.data.keyframe_insert("lens", frame=f)

    on, sx, sz, power, heat, bw, bh, bp = screen(f)
    flicker = 1 + 0.03 * wave(f, 3) + 0.02 * wave(f, 7, 0.37)
    display.scale = (sx * (1 + 0.04 * decay(f, 50, 6, 1.4)), 1, sz)
    display.keyframe_insert("scale", frame=f)
    band_z = lerp(1.15, -1.15, phase(f))
    for mat in (panel_mat, mark_mat, prompt_mat):
        nodes = mat.node_tree.nodes
        p, h = (power * flicker if on else 0.0), heat
        if f >= EXIT:
            k, h = flare(mat.name, f)
            p *= k
        for name, v in (("power", p), ("heat", h), ("band", band_z)):
            nodes[name].outputs[0].default_value = v
            nodes[name].outputs[0].keyframe_insert("default_value", frame=f)
    beam.scale = (max(bw, 1e-4), 1, max(bh, 1e-4))
    beam.hide_render = bp <= 0
    beam.keyframe_insert("scale", frame=f)
    beam.keyframe_insert("hide_render", frame=f)
    beam_mat.node_tree.nodes["Emission"].inputs["Strength"].default_value = bp
    beam_mat.node_tree.nodes["Emission"].inputs["Strength"].keyframe_insert("default_value", frame=f)

    spill.data.energy = power * flicker * 60 if on else bp * 1.5
    if f >= EXIT:
        spill.data.energy *= flare("raster", f)[0]
    spill.data.keyframe_insert("energy", frame=f)

    led_on = 1.0 if f >= 26 else 0.0
    if f in (26, 27):
        led_on = 3.0
    led_mat.node_tree.nodes["Emission"].inputs["Strength"].default_value = 6.0 * led_on
    led_mat.node_tree.nodes["Emission"].inputs["Strength"].keyframe_insert("default_value", frame=f)

    for i, c in enumerate(prompt_chars):
        if c is None:
            continue
        visible = on and (i < 2 or f >= TYPE_AT[i - 2]) and not blackout(f)
        c.hide_render = not visible
        c.keyframe_insert("hide_render", frame=f)
    for o in (raster, mark):
        o.hide_render = not on or blackout(f)
        o.keyframe_insert("hide_render", frame=f)
    for o in (case, housing, plinth, tube_face, led, glass):
        o.hide_render = blackout(f)
        o.keyframe_insert("hide_render", frame=f)
    cursor.location.x = cursor_x(f)
    blink = f < LOOP or (f - LOOP) % 24 < 12
    cursor.hide_render = not (on and blink) or f >= EXIT
    cursor.keyframe_insert("location", frame=f)
    cursor.keyframe_insert("hide_render", frame=f)


s.frame_start, s.frame_end = 1, END
for f in range(1, END + 1):
    s.frame_set(f)
    key_all(f)

argv = sys.argv[sys.argv.index("--") + 1 :]
out = argv[0]
os.makedirs(out, exist_ok=True)
bpy.ops.wm.save_as_mainfile(filepath=os.path.abspath(f"{out}/phosphor.blend"))
frames = range(1, END + 1)
if len(argv) > 1:
    frames = []
    for part in argv[1].split(","):
        lo, _, hi = part.partition("-")
        frames += range(int(lo), int(hi or lo) + 1)
render(out, frames)
