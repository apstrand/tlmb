#!/usr/bin/env python3
"""Build a colored glTF/GLB of the Mailbox app icon: letters in a wooden tray."""

from __future__ import annotations

import base64
import json
import math
import os
import struct
from collections import defaultdict

OUT_DIR = os.path.dirname(os.path.abspath(__file__))

# Millimetres. glTF export scales to metres.
TRAY_W, TRAY_D, TRAY_H = 158.0, 198.0, 36.0
WALL = 12.0
BOTTOM = 8.0
SLAT_H = 2.6
SLAT_COUNT = 0
SLAT_GAP = 3.0

# #10 envelope proportions, standing on the long edge, packed into the tray.
ENV_W, ENV_H, ENV_T = 128.0, 70.0, 7.5
ENV_TILT_DEG = 15.0
ENV_COUNT = 6
ENV_SPACING = 23.0
ENV_X_OFFSET = 20.0
ENV_Z_OFFSET = 50
FLAP_PROUD = 1.1
FLAP_V = 0.44  # V-point as a fraction of envelope height from the bottom

MM = 0.001

WOOD = (226, 170, 66, 255)
WOOD_TOP = (236, 188, 78, 255)
WOOD_IN = (148, 92, 28, 255)
WOOD_SLAT = (196, 136, 42, 255)
PAPER = (243, 232, 211, 255)
PAPER_SIDE = (186, 160, 124, 255)
PAPER_BACK = (220, 204, 178, 255)
PAPER_FLAP = (250, 244, 230, 255)
PAPER_EDGE = (148, 124, 92, 255)


def sub(a, b):
    return (a[0] - b[0], a[1] - b[1], a[2] - b[2])


def add(a, b):
    return (a[0] + b[0], a[1] + b[1], a[2] + b[2])


def mul(a, s):
    return (a[0] * s, a[1] * s, a[2] * s)


def cross(a, b):
    return (
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    )


def dot(a, b):
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def length(a):
    return math.sqrt(dot(a, a))


def normalize(a):
    l = length(a)
    if l < 1e-12:
        return (0.0, 1.0, 0.0)
    return mul(a, 1.0 / l)


def rotate_x(p, rad):
    x, y, z = p
    c, s = math.cos(rad), math.sin(rad)
    return (x, y * c - z * s, y * s + z * c)


def srgb_to_lin(c):
    x = c / 255.0
    if x <= 0.04045:
        return x / 12.92
    return ((x + 0.055) / 1.055) ** 2.4


def to_lin(rgba):
    r, g, b, a = rgba
    return (srgb_to_lin(r), srgb_to_lin(g), srgb_to_lin(b), a / 255.0)


def pad4(b: bytes, pad: bytes = b"\x00") -> bytes:
    n = (4 - (len(b) % 4)) % 4
    return b + (pad * n)


class Mesh:
    def __init__(self):
        self.pos: list[tuple[float, float, float]] = []
        self.nrm: list[tuple[float, float, float]] = []
        self.col: list[tuple[float, float, float, float]] = []
        self.idx: list[int] = []
        # (verts, color, from_box). Winding matches the outward normal.
        self.faces: list[tuple[list[tuple[float, float, float]], tuple, bool]] = []

    def tri(self, a, b, c, color):
        n = normalize(cross(sub(b, a), sub(c, a)))
        lin = to_lin(color)
        i = len(self.pos)
        for v in (a, b, c):
            self.pos.append(v)
            self.nrm.append(n)
            self.col.append(lin)
        self.idx.extend((i, i + 1, i + 2))
        return n

    def tri_out(self, a, b, c, color, outward):
        n = cross(sub(b, a), sub(c, a))
        if dot(n, outward) < 0:
            b, c = c, b
        self.tri(a, b, c, color)

    def face(self, verts, color, outward, from_box=False):
        verts = [tuple(v) for v in verts]
        n = cross(sub(verts[1], verts[0]), sub(verts[2], verts[0]))
        if dot(n, outward) < 0:
            verts = [verts[0], *reversed(verts[1:])]
        self.faces.append((verts, color, from_box))
        if len(verts) == 3:
            self.tri_out(verts[0], verts[1], verts[2], color, outward)
            return
        origin = verts[0]
        for i in range(1, len(verts) - 1):
            self.tri_out(origin, verts[i], verts[i + 1], color, outward)

    def map_positions(self, fn):
        self.pos = [fn(p) for p in self.pos]
        self.faces = [
            ([fn(p) for p in verts], color, from_box)
            for verts, color, from_box in self.faces
        ]

    def recompute_normals(self):
        for i in range(0, len(self.idx), 3):
            ia, ib, ic = self.idx[i], self.idx[i + 1], self.idx[i + 2]
            a, b, c = self.pos[ia], self.pos[ib], self.pos[ic]
            n = normalize(cross(sub(b, a), sub(c, a)))
            self.nrm[ia] = self.nrm[ib] = self.nrm[ic] = n

    def scale(self, s):
        self.map_positions(lambda p: mul(p, s))

    def triangles_world(self):
        tris = []
        for i in range(0, len(self.idx), 3):
            ia, ib, ic = self.idx[i : i + 3]
            a, b, c = self.pos[ia], self.pos[ib], self.pos[ic]
            n = self.nrm[ia]
            tris.append((n, a, b, c))
        return tris


def add_box(mesh: Mesh, x0, y0, z0, x1, y1, z1, color, faces=None):
    if x0 > x1:
        x0, x1 = x1, x0
    if y0 > y1:
        y0, y1 = y1, y0
    if z0 > z1:
        z0, z1 = z1, z0
    fc = faces or {}

    def c(key):
        return fc.get(key, color)

    mesh.face(
        [(x0, y0, z0), (x1, y0, z0), (x1, y1, z0), (x0, y1, z0)],
        c("nz"),
        (0, 0, -1),
        from_box=True,
    )
    mesh.face(
        [(x1, y0, z1), (x0, y0, z1), (x0, y1, z1), (x1, y1, z1)],
        c("pz"),
        (0, 0, 1),
        from_box=True,
    )
    mesh.face(
        [(x0, y0, z1), (x0, y0, z0), (x0, y1, z0), (x0, y1, z1)],
        c("nx"),
        (-1, 0, 0),
        from_box=True,
    )
    mesh.face(
        [(x1, y0, z0), (x1, y0, z1), (x1, y1, z1), (x1, y1, z0)],
        c("px"),
        (1, 0, 0),
        from_box=True,
    )
    mesh.face(
        [(x0, y0, z0), (x0, y0, z1), (x1, y0, z1), (x1, y0, z0)],
        c("ny"),
        (0, -1, 0),
        from_box=True,
    )
    mesh.face(
        [(x0, y1, z0), (x1, y1, z0), (x1, y1, z1), (x0, y1, z1)],
        c("py"),
        (0, 1, 0),
        from_box=True,
    )


def build_tray() -> Mesh:
    m = Mesh()
    hw, hd = TRAY_W / 2, TRAY_D / 2
    inner_x0, inner_x1 = -hw + WALL, hw - WALL
    inner_z0, inner_z1 = -hd + WALL, hd - WALL

    add_box(
        m,
        -hw,
        0,
        -hd,
        hw,
        BOTTOM,
        hd,
        WOOD,
        faces={"py": WOOD_IN, "ny": WOOD, "px": WOOD, "nx": WOOD, "pz": WOOD, "nz": WOOD},
    )
    # Front / back / left / right walls sit on the bottom slab.
    add_box(
        m,
        -hw,
        BOTTOM,
        -hd,
        hw,
        TRAY_H,
        inner_z0,
        WOOD,
        faces={"py": WOOD_TOP, "pz": WOOD_IN, "nz": WOOD},
    )
    add_box(
        m,
        -hw,
        BOTTOM,
        inner_z1,
        hw,
        TRAY_H,
        hd,
        WOOD,
        faces={"py": WOOD_TOP, "nz": WOOD_IN, "pz": WOOD},
    )
    add_box(
        m,
        -hw,
        BOTTOM,
        inner_z0,
        inner_x0,
        TRAY_H,
        inner_z1,
        WOOD,
        faces={"py": WOOD_TOP, "px": WOOD_IN, "nx": WOOD},
    )
    add_box(
        m,
        inner_x1,
        BOTTOM,
        inner_z0,
        hw,
        TRAY_H,
        inner_z1,
        WOOD,
        faces={"py": WOOD_TOP, "nx": WOOD_IN, "px": WOOD},
    )

    inner_w = inner_x1 - inner_x0
    slat_w = (inner_w - (SLAT_COUNT + 1) * SLAT_GAP) / SLAT_COUNT if SLAT_COUNT else 0
    x = inner_x0 + SLAT_GAP
    for _ in range(SLAT_COUNT):
        add_box(
            m,
            x,
            BOTTOM,
            inner_z0 + 1.2,
            x + slat_w,
            BOTTOM + SLAT_H,
            inner_z1 - 1.2,
            WOOD_SLAT,
            faces={"py": WOOD_TOP, "px": WOOD_IN, "nx": WOOD_IN},
        )
        x += slat_w + SLAT_GAP
    return m


def build_envelope() -> Mesh:
    m = Mesh()
    w, h, t = ENV_W, ENV_H, ENV_T
    x0, x1 = -w / 2, w / 2
    y0, y1 = 0.0, h
    z0, z1 = 0.0, t

    add_box(
        m,
        x0,
        y0,
        z0,
        x1,
        y1,
        z1,
        PAPER,
        faces={
            "nz": PAPER,
            "pz": PAPER_BACK,
            "nx": PAPER_SIDE,
            "px": PAPER_SIDE,
            "ny": PAPER_EDGE,
            "py": PAPER_EDGE,
        },
    )

    v_y = h * FLAP_V
    proud = -FLAP_PROUD
    inset = 0.4
    tl, tr, vp = (x0 + inset, y1 - inset, z0), (x1 - inset, y1 - inset, z0), (0.0, v_y, z0)
    tl2, tr2, vp2 = (x0 + inset, y1 - inset, proud), (x1 - inset, y1 - inset, proud), (0.0, v_y, proud)

    m.face([tl2, tr2, vp2], PAPER_FLAP, (0, 0, -1))
    m.face([tl, tl2, vp2, vp], PAPER_EDGE, (-0.35, 0.1, -0.9))
    m.face([tr, vp, vp2, tr2], PAPER_EDGE, (0.35, 0.1, -0.9))
    m.face([tl, tr, tr2, tl2], PAPER_EDGE, (0, 1, 0))

    tilt = math.radians(ENV_TILT_DEG)
    m.map_positions(lambda p: rotate_x(p, tilt))
    min_y = min(p[1] for p in m.pos)
    m.map_positions(lambda p: (p[0], p[1] - min_y, p[2]))
    m.recompute_normals()
    return m


def envelope_translations():
    hd = TRAY_D / 2
    inner_z0 = -hd + WALL
    floor = BOTTOM + SLAT_H + 0.15
    # Sit the stack a little forward of the back wall, with a lip at the front.
    z0 = inner_z0 + ENV_Z_OFFSET
    xs = ENV_X_OFFSET
    return [(xs, floor, z0 + i * ENV_SPACING) for i in range(ENV_COUNT)]


def translate_mesh(mesh: Mesh, t):
    out = Mesh()
    out.pos = [add(p, t) for p in mesh.pos]
    out.nrm = list(mesh.nrm)
    out.col = list(mesh.col)
    out.idx = list(mesh.idx)
    out.faces = [
        ([add(p, t) for p in verts], color, from_box)
        for verts, color, from_box in mesh.faces
    ]
    return out


def write_stl(path: str, meshes: list[Mesh]):
    tris = []
    for mesh in meshes:
        tris.extend(mesh.triangles_world())
    with open(path, "wb") as f:
        f.write(b"Mailbox AppIcon 3D".ljust(80, b"\0"))
        f.write(struct.pack("<I", len(tris)))
        for n, a, b, c in tris:
            f.write(struct.pack("<12fH", *n, *a, *b, *c, 0))


def _f32_blob(vecs):
    return b"".join(struct.pack("<3f", *v) for v in vecs)


def _u16_blob(idxs):
    return b"".join(struct.pack("<H", i) for i in idxs)


def _f32_4_blob(cols):
    return b"".join(struct.pack("<4f", *c) for c in cols)


def mesh_minmax(vecs, n):
    mins = [min(v[i] for v in vecs) for i in range(n)]
    maxs = [max(v[i] for v in vecs) for i in range(n)]
    return mins, maxs


def write_glb(path: str, named_meshes: list[tuple[str, Mesh, list[float] | None]]):
    """named_meshes: (name, mesh in metres, translation or None)."""
    bin_blob = bytearray()
    buffer_views = []
    accessors = []
    meshes = []
    nodes = []
    child_ids = []

    def add_view(blob: bytes, target: int) -> int:
        blob = pad4(blob)
        i = len(buffer_views)
        buffer_views.append(
            {
                "buffer": 0,
                "byteOffset": len(bin_blob),
                "byteLength": len(blob),
                "target": target,
            }
        )
        bin_blob.extend(blob)
        return i

    for name, mesh, translation in named_meshes:
        pos_view = add_view(_f32_blob(mesh.pos), 34962)
        nrm_view = add_view(_f32_blob(mesh.nrm), 34962)
        col_view = add_view(_f32_4_blob(mesh.col), 34962)
        idx_view = add_view(_u16_blob(mesh.idx), 34963)

        pmin, pmax = mesh_minmax(mesh.pos, 3)
        nmin, nmax = mesh_minmax(mesh.nrm, 3)
        cmin, cmax = mesh_minmax(mesh.col, 4)
        ia = len(accessors)
        accessors.extend(
            [
                {
                    "bufferView": pos_view,
                    "componentType": 5126,
                    "count": len(mesh.pos),
                    "type": "VEC3",
                    "min": [float(x) for x in pmin],
                    "max": [float(x) for x in pmax],
                },
                {
                    "bufferView": nrm_view,
                    "componentType": 5126,
                    "count": len(mesh.nrm),
                    "type": "VEC3",
                    "min": [float(x) for x in nmin],
                    "max": [float(x) for x in nmax],
                },
                {
                    "bufferView": col_view,
                    "componentType": 5126,
                    "count": len(mesh.col),
                    "type": "VEC4",
                    "min": [float(x) for x in cmin],
                    "max": [float(x) for x in cmax],
                },
                {
                    "bufferView": idx_view,
                    "componentType": 5123,
                    "count": len(mesh.idx),
                    "type": "SCALAR",
                    "max": [max(mesh.idx)],
                    "min": [0],
                },
            ]
        )
        mesh_i = len(meshes)
        meshes.append(
            {
                "name": name,
                "primitives": [
                    {
                        "attributes": {
                            "POSITION": ia,
                            "NORMAL": ia + 1,
                            "COLOR_0": ia + 2,
                        },
                        "indices": ia + 3,
                        "material": 0,
                    }
                ],
            }
        )
        node = {"name": name, "mesh": mesh_i}
        if translation is not None:
            node["translation"] = [float(x) for x in translation]
        nodes.append(node)
        child_ids.append(len(nodes) - 1)

    nodes.insert(
        0,
        {
            "name": "MailboxIcon",
            "children": [i + 1 for i in range(len(child_ids))],
        },
    )

    bin_blob = pad4(bytes(bin_blob))
    gltf = {
        "asset": {
            "version": "2.0",
            "generator": "mailbox generate_appicon_3d.py",
        },
        "scene": 0,
        "scenes": [{"name": "Scene", "nodes": [0]}],
        "nodes": nodes,
        "meshes": meshes,
        "materials": [
            {
                "name": "VertexColor",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
                    "metallicFactor": 0.0,
                    "roughnessFactor": 0.74,
                },
            }
        ],
        "accessors": accessors,
        "bufferViews": buffer_views,
        "buffers": [{"byteLength": len(bin_blob)}],
    }

    json_blob = pad4(json.dumps(gltf, separators=(",", ":")).encode("utf-8"), b" ")
    total = 12 + 8 + len(json_blob) + 8 + len(bin_blob)
    with open(path, "wb") as f:
        f.write(struct.pack("<4sII", b"glTF", 2, total))
        f.write(struct.pack("<I4s", len(json_blob), b"JSON"))
        f.write(json_blob)
        f.write(struct.pack("<I4s", len(bin_blob), b"BIN\x00"))
        f.write(bin_blob)


HTML_TEMPLATE = r"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Mailbox icon — 3D</title>
  <link rel="icon" href="data:,">
  <style>
    html, body { margin: 0; height: 100%; background: #1a5d7a; overflow: hidden; }
    canvas { display: block; }
    #hint, #status {
      position: fixed; left: 16px; bottom: 16px;
      color: #f3e8d3; font: 13px/1.4 ui-sans-serif, system-ui, sans-serif;
      opacity: 0.85; pointer-events: none; max-width: 36em;
    }
    #status { bottom: auto; top: 16px; }
    #status.err { color: #ffd2a8; }
    body.ready #status { display: none; }
    body.shot #hint, body.shot #status { display: none; }
  </style>
</head>
<body>
  <div id="status">Loading 3D model…</div>
  <div id="hint">drag to orbit · scroll to zoom</div>
  <script src="https://unpkg.com/three@0.147.0/build/three.min.js"></script>
  <script src="https://unpkg.com/three@0.147.0/examples/js/controls/OrbitControls.js"></script>
  <script src="https://unpkg.com/three@0.147.0/examples/js/loaders/GLTFLoader.js"></script>
  <script>
    const GLB_B64 = "__GLB_BASE64__";

    function fail(msg, err) {
      const el = document.getElementById('status');
      el.className = 'err';
      el.textContent = msg;
      console.error(msg, err || '');
      window.__MODEL_READY__ = false;
    }

    if (typeof THREE === 'undefined' || !THREE.OrbitControls || !THREE.GLTFLoader) {
      fail('Could not load three.js. Open this file while online, or use a local server.');
    } else {
      const shot = new URLSearchParams(location.search).has('shot');
      if (shot) document.body.classList.add('shot');

      const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: false });
      renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
      renderer.setSize(innerWidth, innerHeight);
      renderer.setClearColor(0x1a5d7a, 1);
      renderer.outputEncoding = THREE.sRGBEncoding;
      renderer.shadowMap.enabled = true;
      renderer.shadowMap.type = THREE.PCFSoftShadowMap;
      document.body.appendChild(renderer.domElement);

      const scene = new THREE.Scene();
      scene.background = new THREE.Color(0x1a5d7a);

      const camera = new THREE.PerspectiveCamera(34, innerWidth / innerHeight, 0.01, 20);
      camera.position.set(-0.146, 0.128, -0.224);

      const controls = new THREE.OrbitControls(camera, renderer.domElement);
      controls.target.set(0.0, 0.050, -0.050);
      controls.enableDamping = true;
      controls.dampingFactor = 0.06;
      controls.maxPolarAngle = Math.PI * 0.48;
      controls.minDistance = 0.18;
      controls.maxDistance = 1.2;
      controls.update();

      scene.add(new THREE.AmbientLight(0xffffff, 0.58));
      scene.add(new THREE.HemisphereLight(0xf3f7fb, 0xe8c98a, 0.22));
      const key = new THREE.DirectionalLight(0xfff8ee, 1.15);
      key.position.set(-0.4, 0.7, -0.35);
      key.castShadow = true;
      key.shadow.mapSize.set(2048, 2048);
      key.shadow.camera.near = 0.05;
      key.shadow.camera.far = 2;
      key.shadow.camera.left = -0.4;
      key.shadow.camera.right = 0.4;
      key.shadow.camera.top = 0.4;
      key.shadow.camera.bottom = -0.4;
      key.shadow.bias = -0.0004;
      scene.add(key);
      const fill = new THREE.DirectionalLight(0x8ec4d8, 0.45);
      fill.position.set(0.5, 0.25, 0.2);
      scene.add(fill);

      function onModel(gltf) {
        const root = gltf.scene;
        root.traverse(function (obj) {
          if (!obj.isMesh) return;
          obj.castShadow = true;
          obj.receiveShadow = true;
          const mats = Array.isArray(obj.material) ? obj.material : [obj.material];
          mats.forEach(function (mat) {
            mat.vertexColors = true;
            mat.roughness = obj.name.indexOf('Envelope') === 0 ? 0.82 : 0.68;
            mat.metalness = 0;
          });
        });
        scene.add(root);
        document.body.classList.add('ready');
        window.__MODEL_READY__ = true;
      }

      const binary = Uint8Array.from(atob(GLB_B64), function (c) { return c.charCodeAt(0); }).buffer;
      const loader = new THREE.GLTFLoader();
      loader.parse(binary, '', onModel, function (err) {
        fail('Could not parse the embedded model.', err);
      });

      addEventListener('resize', function () {
        camera.aspect = innerWidth / innerHeight;
        camera.updateProjectionMatrix();
        renderer.setSize(innerWidth, innerHeight);
      });

      (function tick() {
        controls.update();
        renderer.render(scene, camera);
        requestAnimationFrame(tick);
      })();
    }
  </script>
</body>
</html>
"""


def hex_rgb(rgba) -> str:
    r, g, b = (int(rgba[0]), int(rgba[1]), int(rgba[2]))
    return f"#{r:02x}{g:02x}{b:02x}"


def collect_world_meshes() -> list[tuple[str, int, Mesh]]:
    """Return (kind, envelope_index_or_-1, mesh). Envelope 0 is nearest the front."""
    tray = build_tray()
    env = build_envelope()
    meshes = [("tray", -1, tray)]
    for i, t in enumerate(envelope_translations()):
        meshes.append(("env", i, translate_mesh(env, t)))
    return meshes


def camera_eye(az_deg: float, el_deg: float, dist: float, target):
    az = math.radians(az_deg)
    el = math.radians(el_deg)
    ce, se = math.cos(el), math.sin(el)
    ca, sa = math.cos(az), math.sin(az)
    # az 0 = from -Z (front of tray), positive = toward -X (left)
    return (
        target[0] - dist * ce * sa,
        target[1] + dist * se,
        target[2] - dist * ce * ca,
    )


def look_basis(eye, target, up=(0.0, 1.0, 0.0)):
    fwd = normalize(sub(target, eye))
    right = normalize(cross(fwd, up))
    if length(right) < 1e-8:
        right = (1.0, 0.0, 0.0)
    cam_up = cross(right, fwd)
    return right, cam_up, fwd


def project_point(p, eye, right, cam_up, fwd, fl):
    rel = sub(p, eye)
    vx = dot(rel, right)
    vy = dot(rel, cam_up)
    vz = dot(rel, fwd)  # positive in front of camera
    if vz < 1e-6:
        return None
    return (fl * vx / vz, fl * vy / vz, vz)


# Icon camera: 3/4 from the front-left, matching screenshot-2026-09-10_21-15-27.png
ICON_AZ = 40.0
ICON_EL = 17.0
ICON_DIST = 300.0
ICON_FOV = 34.0
ICON_TARGET = (0.0, 40.0, -50.0)
ICON_SCALE = 1.1

def write_svg(
    path: str,
    az=ICON_AZ,
    el=ICON_EL,
    dist=ICON_DIST,
    fov_deg=ICON_FOV,
    size=1024,
    pad=0.08,
    bg="#1a5d7a",
    target=None,
):
    """Project the tray + letters to a square SVG. Distances are millimetres."""
    if target is None:
        target = ICON_TARGET
    eye = camera_eye(az, el, dist, target)
    right, cam_up, fwd = look_basis(eye, target)
    fl = 1.0 / math.tan(math.radians(fov_deg) * 0.5)

    hw, hd = TRAY_W / 2, TRAY_D / 2

    def outer_wall(verts):
        xs = [p[0] for p in verts]
        zs = [p[2] for p in verts]
        return (
            all(x <= -hw + 0.25 for x in xs)
            or all(x >= hw - 0.25 for x in xs)
            or all(z <= -hd + 0.25 for z in zs)
            or all(z >= hd - 0.25 for z in zs)
        )

    def vkey(p):
        return (round(p[0], 2), round(p[1], 2), round(p[2], 2))

    def ekey(p, q):
        a, b = vkey(p), vkey(q)
        return (a, b) if a < b else (b, a)

    faces = []
    edge_map = defaultdict(list)
    for kind, env_i, mesh in collect_world_meshes():
        for verts, color, from_box in mesh.faces:
            n = normalize(cross(sub(verts[1], verts[0]), sub(verts[2], verts[0])))
            if from_box and dot(n, sub(eye, verts[0])) <= 0:
                continue
            proj = []
            clipped = False
            for v in verts:
                p = project_point(v, eye, right, cam_up, fwd, fl)
                if p is None:
                    clipped = True
                    break
                proj.append(p)
            if clipped:
                continue
            depth = sum(p[2] for p in proj) / len(proj)
            # Interior, then letters, then the outer rim — rim quads are large
            # enough that centroid z-sort alone cannot occlude letter bottoms.
            if kind == "tray" and outer_wall(verts):
                group = 20
            elif kind == "tray":
                group = 0
            else:
                group = 10 + (ENV_COUNT - 1 - env_i)
            faces.append((group, depth, proj, color, n, verts))
            nv = len(verts)
            for i in range(nv):
                wa, wb = verts[i], verts[(i + 1) % nv]
                sa, sb = proj[i], proj[(i + 1) % nv]
                edge_map[ekey(wa, wb)].append((group, depth, n, sa, sb))

    # Painter's algorithm: later group in front, far (larger z) first within a group.
    faces.sort(key=lambda f: (f[0], -f[1]))
    xs = [p[0] for f in faces for p in f[2]]
    ys = [p[1] for f in faces for p in f[2]]
    minx, maxx = min(xs), max(xs)
    miny, maxy = min(ys), max(ys)
    w, h = maxx - minx, maxy - miny
    span = max(w, h)
    # Fit the longer axis; center on both.
    scale = ICON_SCALE * (size - 2 * (size * pad)) / span
    # Recenter using pixel pad so the object sits in the square.
    cx, cy = (minx + maxx) / 2.0, (miny + maxy) / 2.0
    px0, py0 = size / 2.0, size / 2.0

    def to_svg(p):
        x = px0 + (p[0] - cx) * scale
        y = py0 - (p[1] - cy) * scale  # SVG y-down
        return (x, y)

    crease_lim = math.cos(math.radians(26))
    lines = []
    for recs in edge_map.values():
        if len(recs) == 1:
            group, depth, _n, sa, sb = recs[0]
            lines.append((group, depth, sa, sb, "sil"))
            continue
        recs = sorted(recs, key=lambda r: (-r[0], r[1]))
        n0, n1 = recs[0][2], recs[1][2]
        if dot(n0, n1) < crease_lim:
            group, depth = recs[0][0], recs[0][1]
            sa, sb = recs[0][3], recs[0][4]
            lines.append((group, depth, sa, sb, "crease"))
    lines.sort(key=lambda L: (L[0], -L[1]))

    def poly_elem(pts, col):
        svg_pts = [to_svg(p) for p in pts]
        fill = hex_rgb(col)
        pts_s = " ".join(f"{x:.2f},{y:.2f}" for x, y in svg_pts)
        return (
            f'<polygon points="{pts_s}" fill="{fill}" stroke="{fill}" '
            f'stroke-width="0.8" stroke-linejoin="round"/>'
        )

    def line_elem(sa, sb, color, width):
        a2, b2 = to_svg(sa), to_svg(sb)
        return (
            f'<line x1="{a2[0]:.2f}" y1="{a2[1]:.2f}" x2="{b2[0]:.2f}" y2="{b2[1]:.2f}" '
            f'stroke="{color}" stroke-width="{width}" stroke-linecap="round"/>'
        )

    items = []
    for group, depth, proj, col, _n, _verts in faces:
        items.append((group, depth, 0, poly_elem(proj, col)))
    for group, depth, sa, sb, kind in lines:
        color, width = ("#1a120c", 9.0) if kind == "sil" else ("#2a1c12", 5.0)
        items.append((group, depth, 1, line_elem(sa, sb, color, width)))
    items.sort(key=lambda it: (it[0], -it[1], it[2]))
    chunks = [it[3] for it in items]

    svg = (
        f'<?xml version="1.0" encoding="UTF-8"?>\n'
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" '
        f'width="{size}" height="{size}">\n'
        f'<rect width="{size}" height="{size}" fill="{bg}"/>\n'
        + "\n".join(chunks)
        + "\n</svg>\n"
    )
    with open(path, "w", encoding="utf-8") as f:
        f.write(svg)
    return eye


def write_html(path: str, glb_path: str):
    with open(glb_path, "rb") as f:
        b64 = base64.b64encode(f.read()).decode("ascii")
    html = HTML_TEMPLATE.replace("__GLB_BASE64__", b64)
    with open(path, "w", encoding="utf-8") as f:
        f.write(html)


def main():
    tray = build_tray()
    envelope = build_envelope()
    translations = envelope_translations()

    stl_meshes = [tray]
    glb_tray = build_tray()
    glb_env = build_envelope()
    glb_tray.scale(MM)
    glb_env.scale(MM)

    glb_entries = [("Tray", glb_tray, None)]
    for i, t in enumerate(translations, start=1):
        baked = translate_mesh(envelope, t)
        stl_meshes.append(baked)
        glb_entries.append(
            (f"Envelope_{i}", glb_env, (t[0] * MM, t[1] * MM, t[2] * MM))
        )

    glb_path = os.path.join(OUT_DIR, "AppIcon.glb")
    stl_path = os.path.join(OUT_DIR, "AppIcon.stl")
    html_path = os.path.join(OUT_DIR, "AppIcon3d.html")
    svg_path = os.path.join(OUT_DIR, "AppIcon.svg")
    write_glb(glb_path, glb_entries)
    write_stl(stl_path, stl_meshes)
    write_html(html_path, glb_path)
    write_svg(svg_path)
    print(f"wrote {glb_path} ({os.path.getsize(glb_path)} bytes)")
    print(f"wrote {stl_path} ({os.path.getsize(stl_path)} bytes)")
    print(f"wrote {html_path} ({os.path.getsize(html_path)} bytes)")
    print(f"wrote {svg_path} ({os.path.getsize(svg_path)} bytes)")
    print(f"tray tris={len(tray.idx)//3} envelope tris={len(envelope.idx)//3}")


if __name__ == "__main__":
    main()
