import os
import struct
import zlib

import bpy
import numpy as np
import OpenImageIO as oiio
from mathutils import Vector

W, H = 960, 540
FPS = 24


def reset(samples=24, view="AgX", look="None"):
    bpy.ops.wm.read_factory_settings(use_empty=True)
    s = bpy.context.scene
    s.render.engine = "BLENDER_EEVEE"
    s.render.resolution_x, s.render.resolution_y = W, H
    s.render.resolution_percentage = 100
    s.render.fps = FPS
    s.render.film_transparent = False
    s.eevee.taa_render_samples = samples
    s.eevee.use_raytracing = True
    s.view_settings.view_transform = view
    s.view_settings.look = look
    world = bpy.data.worlds.new("World")
    s.world = world
    world.node_tree.nodes["Background"].inputs["Color"].default_value = (0, 0, 0, 1)
    vl = s.view_layers[0]
    vl.use_pass_z = True
    vl.use_pass_normal = True
    return s


def material(name, color=(0.8, 0.8, 0.8), metallic=0.0, roughness=0.5, emission=None, strength=0.0):
    m = bpy.data.materials.new(name)
    bsdf = m.node_tree.nodes["Principled BSDF"]
    bsdf.inputs["Base Color"].default_value = (*color, 1)
    bsdf.inputs["Metallic"].default_value = metallic
    bsdf.inputs["Roughness"].default_value = roughness
    if emission is not None:
        bsdf.inputs["Emission Color"].default_value = (*emission, 1)
        bsdf.inputs["Emission Strength"].default_value = strength
    return m


def emission(name, color, strength):
    m = bpy.data.materials.new(name)
    nt = m.node_tree
    nt.nodes.clear()
    out = nt.nodes.new("ShaderNodeOutputMaterial")
    em = nt.nodes.new("ShaderNodeEmission")
    em.inputs["Color"].default_value = (*color, 1)
    em.inputs["Strength"].default_value = strength
    nt.links.new(em.outputs[0], out.inputs[0])
    return m


def camera(location, target, lens=50):
    cam = bpy.data.objects.new("Camera", bpy.data.cameras.new("Camera"))
    bpy.context.scene.collection.objects.link(cam)
    cam.data.lens = lens
    cam.data.clip_end = 200
    bpy.context.scene.camera = cam
    aim(cam, location, target)
    return cam


def aim(obj, location, target, roll=0.0):
    obj.location = Vector(location)
    d = Vector(target) - Vector(location)
    q = d.to_track_quat("-Z", "Y")
    obj.rotation_mode = "XYZ"
    obj.rotation_euler = q.to_euler()
    if roll:
        obj.rotation_euler.rotate_axis("Z", roll)


def light(kind, location, energy, color=(1, 1, 1), size=1.0, target=None):
    data = bpy.data.lights.new(kind.lower(), kind)
    data.energy = energy
    data.color = color
    if hasattr(data, "shadow_soft_size"):
        data.shadow_soft_size = size
    obj = bpy.data.objects.new(kind.lower(), data)
    bpy.context.scene.collection.objects.link(obj)
    obj.location = location
    if target is not None:
        aim(obj, location, target)
    return obj


def link(obj):
    bpy.context.scene.collection.objects.link(obj)
    return obj


def smooth(t):
    t = min(max(t, 0.0), 1.0)
    return t * t * (3 - 2 * t)


def ease_out(t, p=3):
    t = min(max(t, 0.0), 1.0)
    return 1 - (1 - t) ** p


def lerp(a, b, t):
    return a + (b - a) * t


def render(out, frames, update=None):
    """Renders each frame to NNNN.png (display referred) and NNNN.geo:
    u32 width, u32 height, f32 far, then zlib of u16 depth/far * 65535 [w*h]
    and i8 normal * 127 [w*h*3]."""
    os.makedirs(out, exist_ok=True)
    s = bpy.context.scene
    im = s.render.image_settings
    for f in frames:
        s.frame_set(f)
        if update:
            update(f)
        bpy.ops.render.render(write_still=False)
        rr = bpy.data.images["Render Result"]
        im.media_type = "IMAGE"
        im.file_format = "PNG"
        im.color_mode = "RGB"
        im.color_depth = "8"
        rr.save_render(f"{out}/{f:04d}.png", scene=s)
        im.media_type = "MULTI_LAYER_IMAGE"
        im.file_format = "OPEN_EXR_MULTILAYER"
        im.color_depth = "32"
        exr = f"{out}/.tmp.exr"
        rr.save_render(exr, scene=s)
        write_geo(exr, f"{out}/{f:04d}.geo")
        os.remove(exr)
        print(f"FRAME {f}", flush=True)


def text(body, font="/System/Library/Fonts/Supplemental/Arial Black.ttf", size=1.0, extrude=0.1, bevel=0.0, name="text"):
    cu = bpy.data.curves.new(name, "FONT")
    cu.body = body
    cu.font = bpy.data.fonts.load(font, check_existing=True)
    cu.size = size
    cu.extrude = extrude
    cu.bevel_depth = bevel
    cu.align_x = "CENTER"
    cu.align_y = "CENTER"
    return link(bpy.data.objects.new(name, cu))


def write_geo(exr, path):
    channels = {}
    inp = oiio.ImageInput.open(exr)
    sub = 0
    while inp.seek_subimage(sub, 0):
        spec = inp.spec()
        px = inp.read_image(sub, 0, 0, spec.nchannels, oiio.FLOAT)
        for i, n in enumerate(spec.channelnames):
            channels[n] = px[:, :, i]
        sub += 1
    inp.close()

    def chan(suffix):
        for n, v in channels.items():
            if n.endswith(suffix):
                return v
        raise KeyError(f"{suffix} not in {list(channels)}")

    far = bpy.context.scene.camera.data.clip_end
    depth = np.clip(chan("Depth.Z") / far, 0, 1)
    depth = np.round(depth * 65535).astype("<u2")
    normal = np.stack([chan("Normal.X"), chan("Normal.Y"), chan("Normal.Z")], axis=-1)
    normal = np.round(np.clip(normal, -1, 1) * 127).astype("i1")
    h, w = depth.shape
    with open(path, "wb") as f:
        f.write(struct.pack("<IIf", w, h, far))
        f.write(zlib.compress(depth.tobytes() + normal.tobytes(), 6))
