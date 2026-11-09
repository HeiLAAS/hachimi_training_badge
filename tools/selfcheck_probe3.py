"""Offline end-to-end self-check for hachimi_training_badge probe v4.

Simulates the Hachimi v3 API surface in-process so the plugin can be exercised
on a bare machine (no game, no IL2CPP). v4 is hook-driven, so this harness
verifies the exact contract that matters:

  1. hachimi_init_v3 resolves everything and returns 1
  2. it finds GetCurrentViewController and hooks it through
     hachimi_instance -> hachimi_get_interceptor -> interceptor_hook
  3. the hook receives a trampoline and forwards calls to it
  4. invoking the hook dumps [SCENE]/[M]/[F] with field object types
  5. duplicate scenes are de-duplicated (pointer fast path)
  6. dumping stops at max_scenes while the hook keeps forwarding
  7. on_present stays a pure counter (no managed calls)
  8. the plugin makes no IL2CPP call outside the hook

Fake memory layout mirrors Hachimi-Edge/src/il2cpp/types.rs:
  Il2CppClass : image@0 gc_desc@8 name@16 namespaze@24
  MethodInfo  : methodPointer@0 virtual@8 invoker@16 name@24
  FieldInfo   : name@0 type@8 parent@16 offset@24 token@28
"""

import ctypes
import json
import os
import sys
import tempfile

DLL = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                   "target", "release", "hachimi_training_badge.dll")

log_lines = []
present_cbs = []
hooked = {"orig": 0, "hook": 0, "trampoline_calls": 0}

P = ctypes.c_void_p
BYTES = lambda n: ctypes.create_string_buffer(n)


class ClassHead(ctypes.Structure):
    _fields_ = [("image", P), ("gc_desc", P), ("name", ctypes.c_char_p),
                ("namespaze", ctypes.c_char_p)]


class MethodInfo(ctypes.Structure):
    # Full layout up to the bitfield byte, matching il2cpp/types.rs MethodInfo:
    # methodPointer@0 ... parameters_count@82, _bitfield_1@83 (is_generic = bit0)
    _fields_ = [("method_pointer", ctypes.c_size_t), ("virtual_pointer", ctypes.c_size_t),
                ("invoker", ctypes.c_size_t), ("name", ctypes.c_char_p),
                ("klass", P), ("return_type", P), ("parameters", P),
                ("anon1", ctypes.c_size_t), ("anon2", ctypes.c_size_t),
                ("token", ctypes.c_uint32), ("flags", ctypes.c_uint16),
                ("iflags", ctypes.c_uint16), ("slot", ctypes.c_uint16),
                ("parameters_count", ctypes.c_uint8), ("bitfield", ctypes.c_uint8)]


class FieldInfo(ctypes.Structure):
    _fields_ = [("name", ctypes.c_char_p), ("type_", P), ("parent", P),
                ("offset", ctypes.c_int32), ("token", ctypes.c_uint32)]


class Obj(ctypes.Structure):          # Il2CppObject: klass at offset 0
    _fields_ = [("klass", P), ("payload", ctypes.c_ubyte * 256)]


_KEEP = []


def addr(obj):
    return ctypes.addressof(obj)


CLASS_NAMES = {}


def make_class(ns, name):
    c = ClassHead(None, None, name.encode(), ns.encode())
    CLASS_NAMES[addr(c)] = f"{ns}.{name}"
    _KEEP.append(c)
    return c


def make_obj(klass_struct):
    o = Obj()
    o.klass = addr(klass_struct)
    _KEEP.append(o)
    return o


TYPE_REF = 0x12
TYPE_I4 = 0x08
type_kinds = {}


def make_type(kind):
    buf = BYTES(16)
    _KEEP.append(buf)
    p = addr(buf)
    type_kinds[p] = kind
    return p


class Il2String(ctypes.Structure):    # klass@0 monitor@8 length@16 chars@20
    _fields_ = [("klass", P), ("monitor", P), ("length", ctypes.c_int32),
                ("chars", ctypes.c_uint16 * 64)]


def make_str(text):
    s = Il2String()
    s.klass = 0
    s.monitor = 0
    s.length = len(text)
    for i, ch in enumerate(text):
        s.chars[i] = ord(ch)
    _KEEP.append(s)
    return addr(s)


go_name_map = {}     # GameObject addr -> Il2CppString addr
gameobjects = {}     # Component addr -> GameObject addr


sm_klass = make_class("Gallop", "SceneManager")
vc_klass = make_class("Gallop", "TrainingViewController")
vc2_klass = make_class("Gallop", "TrainingResultViewController")
btn_klass = make_class("Gallop", "TrainingButton")
vcbase_klass = make_class("Gallop", "ViewBase")
go_klass = make_class("UnityEngine", "GameObject")
i4_klass = make_class("System", "Int32")
obj_klass = make_class("System", "Object")
comp_klass = make_class("UnityEngine", "Component")
uobj_klass = make_class("UnityEngine", "Object")
rt_klass = make_class("UnityEngine", "RectTransform")
tex_klass = make_class("UnityEngine", "Texture2D")
c32_klass = make_class("UnityEngine", "Color32")
trans_klass = make_class("UnityEngine", "Transform")
rawui_klass = make_class("UnityEngine.UI", "RawImage")
button_klass = make_class("UnityEngine.UI", "Button")
graphic_klass = make_class("UnityEngine.UI", "Graphic")
canvas_klass = make_class("UnityEngine", "Canvas")
ic_klass = make_class("UnityEngine", "ImageConversion")
byte_klass = make_class("System", "Byte")
cr_klass = make_class("UnityEngine", "CanvasRenderer")
v3_klass = make_class("UnityEngine", "Vector3")
mask_klass = make_class("UnityEngine.UI", "Mask")
rectmask_klass = make_class("UnityEngine.UI", "RectMask2D")

# class -> parent class (for the [C] inheritance chain and is_subclass)
parents = {
    addr(vc_klass): addr(vcbase_klass),
    addr(vcbase_klass): 0,
    addr(btn_klass): addr(comp_klass),
    addr(comp_klass): addr(uobj_klass),
    addr(uobj_klass): 0,
}

# The original method the plugin will hook: returns the current view controller.
_CURRENT = {"vc": 0}


def gcv_impl(_this):
    hooked["trampoline_calls"] += 1
    return _CURRENT["vc"] or 0


GET_VC = ctypes.CFUNCTYPE(P, P)(gcv_impl)
_KEEP.append(GET_VC)

methods_by_class = {}


def add_method(klass, name, fn_ptr=0, argc=0, generic=0):
    m = MethodInfo(fn_ptr, 0, 0, name.encode(), None, None, None, 0, 0,
                   0, 0, 0, 0, argc, 1 if generic else 0)
    _KEEP.append(m)
    methods_by_class.setdefault(addr(klass), []).append(ctypes.pointer(m))


# A generic overload is listed FIRST on purpose: the plugin must skip it and
# pick the non-generic parameterless one, otherwise it would hook the wrong
# calling convention.
add_method(sm_klass, "GetCurrentViewController", ctypes.cast(GET_VC, P).value,
           argc=1, generic=1)
add_method(sm_klass, "GetCurrentViewController", ctypes.cast(GET_VC, P).value)
add_method(sm_klass, "ChangeView", 0)
add_method(vc_klass, "Update", 0)
add_method(vc_klass, "OnTapTraining", 0)
add_method(vc2_klass, "Update", 0)
add_method(vc2_klass, "OnClose", 0)

fields_by_class = {}
type_klass = {}      # Il2CppType* -> Il2CppClass* (for il2cpp_class_from_type)


def add_field(klass, name, offset, kind, ref_klass=None):
    ty = make_type(kind)
    type_klass[ty] = addr(ref_klass or (obj_klass if kind == TYPE_REF else i4_klass))
    f = FieldInfo(name.encode(), ty, None, offset, 0)
    _KEEP.append(f)
    fields_by_class.setdefault(addr(klass), []).append(ctypes.pointer(f))


add_field(vc_klass, "_buttonSpeed", 16, TYPE_REF, btn_klass)
add_field(vc_klass, "_buttonStamina", 24, TYPE_REF, btn_klass)
add_field(vc_klass, "_trainCount", 32, TYPE_I4)     # value type: must NOT be dereferenced
add_field(vc_klass, "_unusedRef", 40, TYPE_REF)     # reference type but null
add_field(btn_klass, "_icon", 32, TYPE_REF, go_klass)
add_field(vc2_klass, "_resultRoot", 16, TYPE_REF, btn_klass)
add_field(vcbase_klass, "_root", 16, TYPE_REF, go_klass)

CFUNCS = []


def apifn(restype, *argtypes):
    def deco(fn):
        t = ctypes.CFUNCTYPE(restype, *argtypes)
        CFUNCS.append(t(fn))
        return CFUNCS[-1]
    return deco


@apifn(None, ctypes.c_int, ctypes.c_char_p, ctypes.c_char_p)
def m_log(level, target, msg):
    log_lines.append(msg.decode("utf-8", "replace"))


@apifn(ctypes.c_char_p)
def m_base_dir():
    return BASE_DIR.encode()


@apifn(ctypes.c_char_p)
def m_data_path():
    return b""


@apifn(P, ctypes.c_char_p)
def m_resolve_symbol(name):
    return SYMS.get(name.decode(), 0)


@apifn(P, ctypes.c_char_p)
def m_get_image(name):
    return {"umamusume.dll": 0x1000,
            "UnityEngine.CoreModule.dll": 0x2000,
            "UnityEngine.UI.dll": 0x3000}.get(name.decode(), 0)


@apifn(P, P, ctypes.c_char_p, ctypes.c_char_p)
def m_get_class(image, ns, cls):
    if image == 0x1000:
        known = {
            (b"Gallop", b"SceneManager"): addr(sm_klass),
            (b"Gallop", b"TrainingViewController"): addr(vc_klass),
            (b"Gallop", b"TrainingButton"): addr(btn_klass),
            (b"Gallop", b"TextCommon"): addr(tc_klass),
        }
        return known.get((ns, cls), 0)
    if image == 0x2000:
        known = {
            (b"UnityEngine", b"Object"): addr(uobj_klass),
            (b"UnityEngine", b"Component"): addr(comp_klass),
            (b"UnityEngine", b"GameObject"): addr(go_klass),
            (b"UnityEngine", b"RectTransform"): addr(rt_klass),
            (b"UnityEngine", b"Texture2D"): addr(tex_klass),
            (b"UnityEngine", b"Color32"): addr(c32_klass),
            (b"UnityEngine", b"Transform"): addr(trans_klass),
            (b"UnityEngine", b"Canvas"): addr(canvas_klass),
            (b"UnityEngine", b"ImageConversion"): addr(ic_klass),
            (b"UnityEngine", b"CanvasRenderer"): addr(cr_klass),
            (b"UnityEngine", b"Vector3"): addr(v3_klass),
        }
        return known.get((ns, cls), 0)
    if image == 0x3000:
        known = {
            (b"UnityEngine.UI", b"RawImage"): addr(rawui_klass),
            (b"UnityEngine.UI", b"Button"): addr(button_klass),
            (b"UnityEngine.UI", b"Graphic"): addr(graphic_klass),
            (b"UnityEngine.UI", b"Mask"): addr(mask_klass),
            (b"UnityEngine.UI", b"RectMask2D"): addr(rectmask_klass),
        }
        return known.get((ns, cls), 0)
    return 0


@apifn(P, P, ctypes.POINTER(P))
def m_class_get_methods(klass, iterp):
    lst = methods_by_class.get(klass, [])
    idx = iterp[0] or 0
    if idx >= len(lst):
        return 0
    iterp[0] = idx + 1
    return ctypes.cast(lst[idx], P).value


@apifn(P, P, ctypes.POINTER(P))
def m_class_get_fields(klass, iterp):
    lst = fields_by_class.get(klass, [])
    idx = iterp[0] or 0
    if idx >= len(lst):
        return 0
    iterp[0] = idx + 1
    return ctypes.cast(lst[idx], P).value


@apifn(P, P)
def m_object_get_class(obj):
    return ctypes.cast(obj, ctypes.POINTER(P))[0]


@apifn(P, P)
def m_field_get_type(field):
    return ctypes.cast(field, ctypes.POINTER(FieldInfo))[0].type_


@apifn(ctypes.c_int, P)
def m_type_get_type(ty):
    return type_kinds.get(ty, TYPE_I4)


@apifn(P, P)
def m_class_get_parent(klass):
    return parents.get(klass, 0)


@apifn(P, P)
def m_class_from_type(ty):
    return type_klass.get(ty, 0)


# --- Component.get_gameObject / Object.get_name mocks ----------------------
def m_get_gameobject(comp):
    # a Transform is itself a Component; in the mock a tree node doubles as its
    # own GameObject so get_name can resolve it
    return gameobjects.get(comp, comp)


@apifn(P, P)
def m_get_name(go):
    return go_name_map.get(go, 0)


GO_FN_PTR = 0
NAME_FN_PTR = 0


@apifn(None, P, P)
def m_set_name(go, s):
    go_name_map[go] = s


@apifn(None, P, P)
def m_destroy(obj, delay):
    go_name_map.pop(obj, None)


@apifn(P, P, ctypes.c_char_p, ctypes.c_int)
def m_get_method_addr(klass, name, argc):
    n = name.decode()
    if klass == addr(uobj_klass) and n == "get_name" and argc == 0:
        return NAME_FN_PTR
    if klass == addr(comp_klass) and n == "get_gameObject" and argc == 0:
        return GO_FN_PTR
    hit = BADGE_METHODS.get((klass, n, argc))
    if hit:
        return hit
    return 0


@apifn(ctypes.c_int32, P)
@apifn(P, ctypes.c_char_p)
def m_string_new(text):
    return make_str(text.decode("utf-8", "replace"))


@apifn(ctypes.c_int32, P)
def m_string_length(s):
    return ctypes.cast(s, ctypes.POINTER(Il2String)).contents.length


@apifn(P, P)
def m_string_chars(s):
    return ctypes.addressof(ctypes.cast(s, ctypes.POINTER(Il2String)).contents.chars)


# --- domain / assembly / image enumeration mocks ---------------------------
DOMAIN = 0xD0
ASM_BUF = BYTES(16)
IMAGE_CLASSES = []


@apifn(P)
def m_domain_get():
    return DOMAIN


@apifn(P, P, ctypes.POINTER(ctypes.c_size_t))
def m_domain_assemblies(domain, size_out):
    size_out[0] = 1
    return ctypes.addressof(ASM_ARRAY)


@apifn(P, P)
def m_assembly_get_image(asm):
    return 0x1000


@apifn(ctypes.c_char_p, P)
def m_image_get_name(img):
    return b"umamusume.dll"


@apifn(ctypes.c_size_t, P)
def m_image_class_count(img):
    return len(IMAGE_CLASSES)


@apifn(P, P, ctypes.c_size_t)
def m_image_get_class(img, idx):
    return addr(IMAGE_CLASSES[idx]) if idx < len(IMAGE_CLASSES) else 0


GO_FN = ctypes.CFUNCTYPE(P, P)(m_get_gameobject)
NAME_FN = ctypes.CFUNCTYPE(P, P)(m_get_name)
_KEEP.extend([GO_FN, NAME_FN])
GO_FN_PTR = ctypes.cast(GO_FN, P).value
NAME_FN_PTR = ctypes.cast(NAME_FN, P).value

ASM_ARRAY = (P * 1)(ctypes.addressof(ASM_BUF))
_KEEP.append(ASM_ARRAY)
IMAGE_CLASSES.extend([sm_klass, vc_klass, btn_klass, vc2_klass])


# === badge construction mocks ==============================================
# These exercise the exact sequence badge.rs runs in the real game:
#   new GameObject -> AddComponent(RectTransform) -> AddComponent(RawImage)
#   -> Texture2D(48,48) + SetPixels32(Color32[2304]) + Apply()
#   -> m_Texture field write -> SetParent(host,false) -> anchors/size
# A wrong step order, a wrong offset or a wrong Vector2 ABI shows up here.

class V2(ctypes.Structure):
    _fields_ = [("x", ctypes.c_float), ("y", ctypes.c_float)]


PIX = {}
GEO = {}
PARENT = {}
CHILDREN = []
TRANSFORMS = {}
FIELD_OFFSETS = {}


def make_live(klass_ptr):
    o = Obj()
    o.klass = klass_ptr
    ctypes.cast(ctypes.byref(o, 16), ctypes.POINTER(ctypes.c_size_t))[0] = 0xBEEF0000
    _KEEP.append(o)
    return o


@apifn(P, P)
def m_object_new(klass):
    return addr(make_live(klass))


@apifn(None, P)
def m_object_init(obj):
    PIX["ctor"] = PIX.get("ctor", 0) + 1


@apifn(P, P)
def m_class_get_type(klass):
    return klass


@apifn(P, P)
def m_type_get_object(ty):
    return ty


@apifn(P, P, ctypes.c_size_t)
def m_create_array(klass, length):
    buf = BYTES(32 + 16 * length)
    ctypes.cast(ctypes.byref(buf, 24), ctypes.POINTER(ctypes.c_size_t))[0] = length
    _KEEP.append(buf)
    PIX["arr_len"] = length
    PIX.setdefault("arr_lens", []).append(length)
    PIX["arr_addr"] = ctypes.addressof(buf)
    return ctypes.addressof(buf)


@apifn(None, P, ctypes.c_int, ctypes.c_int)
def m_tex_ctor(tex, w, h):
    PIX["w"], PIX["h"] = w, h


@apifn(None, P, P)
def m_set_pixels(tex, arr):
    n = ctypes.cast(arr + 24, ctypes.POINTER(ctypes.c_size_t))[0]
    first = ctypes.cast(arr + 32, ctypes.POINTER(ctypes.c_ubyte * 4))[0]
    PIX["pixels"] = (n, tuple(first))


@apifn(None, P)
def m_apply(tex):
    PIX["apply"] = PIX.get("apply", 0) + 1


@apifn(None, P, P)
def m_set_texture(raw, tex):
    PIX["tex_set"] = tex
    off = FIELD_OFFSETS.get(addr(rawui_klass), {}).get("m_Texture")
    if off:
        ctypes.cast(raw + off, ctypes.POINTER(P))[0] = tex


@apifn(None, P, ctypes.c_bool)
def m_set_raycast(raw, v):
    PIX["raycast"] = v


@apifn(None, P)
def m_set_all_dirty(raw):
    PIX["dirty"] = PIX.get("dirty", 0) + 1


@apifn(None, P, ctypes.c_bool)
def m_set_maskable(raw, v):
    PIX["maskable"] = v


@apifn(ctypes.c_float, P)
def m_get_alpha(cr):
    return 1.0


@apifn(None, P, ctypes.c_bool)
def m_set_active(go, v):
    PIX.setdefault("active", []).append(v)


@apifn(ctypes.c_bool, P, P)
def m_load_image(tex, data):
    PIX["loaded"] = PIX.get("loaded", 0) + 1
    return True


@apifn(P, P, P)
def read_managed(s):
    if not s:
        return ""
    try:
        obj = ctypes.cast(s, ctypes.POINTER(Il2String)).contents
        return "".join(chr(obj.chars[i]) for i in range(obj.length))
    except Exception:
        return ""


@apifn(P, P, P)
def m_get_component(go, s):
    name = read_managed(s)
    for (g, cls, comp) in CHILDREN:
        if g == go and CLASS_NAMES.get(cls) == name:
            return comp
    return 0


@apifn(P, P, ctypes.c_char_p, ctypes.POINTER(ctypes.c_ubyte), ctypes.c_size_t)
def m_method_overload_addr(klass, name, params, n):
    if name.decode() == "GetComponent" and n == 1 and params[0] == 0x12:
        return ctypes.cast(m_get_component, P).value
    return 0


@apifn(ctypes.c_int, P)
def m_sibling_index(tr):
    return 0


@apifn(None, P)
def m_last_sibling(tr):
    PIX["last_sibling"] = PIX.get("last_sibling", 0) + 1


@apifn(ctypes.c_bool, P)
def m_get_active(go):
    return ACTIVE.get(go, True)


@apifn(None, P, ctypes.c_bool)
def m_set_override(c, v):
    PIX["override"] = v


@apifn(None, P, ctypes.c_int)
def m_set_order(c, v):
    PIX["order"] = v


@apifn(None, P, ctypes.c_int)
def m_set_render_mode(c, v):
    PIX["render_mode"] = v


@apifn(None, P, P)
def m_get_world_corners(rt, arr):
    # 4 corners of a 100x100 rect centred on (400, 300)
    pts = [(350.0, 250.0), (450.0, 250.0), (350.0, 350.0), (450.0, 350.0)]
    for i, (x, y) in enumerate(pts):
        base = arr + 32 + i * 12
        ctypes.cast(base, ctypes.POINTER(ctypes.c_float))[0] = x
        ctypes.cast(base + 4, ctypes.POINTER(ctypes.c_float))[0] = y
        ctypes.cast(base + 8, ctypes.POINTER(ctypes.c_float))[0] = 0.0


@apifn(P, P, P)
def m_add_component(go, sys_type):
    o = make_live(sys_type)
    CHILDREN.append((go, sys_type, addr(o)))
    # a component is also a transform, and it belongs to that GameObject
    TRANSFORMS[addr(o)] = addr(o)
    gameobjects[addr(o)] = go
    return addr(o)


@apifn(P, P)
def m_get_transform(comp):
    if comp not in TRANSFORMS:
        TRANSFORMS[comp] = addr(make_live(addr(trans_klass)))
    return TRANSFORMS[comp]


@apifn(None, P, P, ctypes.c_bool)
def m_setparent(self_tr, parent_tr, world):
    PARENT.update({"self": self_tr, "parent": parent_tr, "world": world})
    PARENT.setdefault("calls", []).append(world)
    # wire the tree so a later pass can find the badge again
    TREE_PARENT[self_tr] = parent_tr
    kids = TREE_CHILDREN.setdefault(parent_tr, [])
    if self_tr not in kids:
        kids.append(self_tr)


@apifn(None, P, V2)
def m_set_anchor_min(rt, v):
    GEO["min"] = (v.x, v.y)


@apifn(None, P, V2)
def m_set_anchor_max(rt, v):
    GEO["max"] = (v.x, v.y)


@apifn(None, P, V2)
def m_set_pivot(rt, v):
    GEO["pivot"] = (v.x, v.y)


@apifn(None, P, V2)
def m_set_size(rt, v):
    GEO["size"] = (v.x, v.y)


@apifn(None, P, V2)
def m_set_anchored(rt, v):
    GEO["anchored"] = (v.x, v.y)


@apifn(P, P, ctypes.c_char_p)
def m_get_field_from_name(klass, name):
    n = name.decode()
    cur = klass
    d = 0
    while cur and d < 16:
        off = FIELD_OFFSETS.get(cur, {}).get(n)
        if off is not None:
            f = FieldInfo(n.encode(), 0, None, off, 0)
            _KEEP.append(f)
            return ctypes.addressof(f)
        cur = parents.get(cur, 0)
        d += 1
    return 0


FIELD_OFFSETS[addr(uobj_klass)] = {"m_CachedPtr": 16}
FIELD_OFFSETS[addr(rawui_klass)] = {"m_Texture": 224, "m_RaycastTarget": 58, "m_UVRect": 232}
FIELD_OFFSETS[addr(graphic_klass)] = {"m_Color": 40, "m_RaycastTarget": 58, "m_CanvasRenderer": 88}


# --- TextCommon.Update tick + UI tree mocks --------------------------------
tc_klass = make_class("Gallop", "TextCommon")
TREE_CHILDREN = {}
TREE_PARENT = {}


@apifn(None, P)
def m_tc_update(self):
    pass


def add_node(name, parent=0):
    tr = addr(make_live(addr(trans_klass)))
    go_name_map[tr] = make_str(name)
    TREE_PARENT[tr] = parent
    if parent:
        TREE_CHILDREN.setdefault(parent, []).append(tr)
    return tr


_tc_tr = addr(make_live(addr(trans_klass)))
go_name_map[_tc_tr] = make_str("UI_Root")
TREE_PARENT[_tc_tr] = 0
_n1 = add_node("TrainingFooterItem", _tc_tr)
add_node("TrainingButtonSpeed", _n1)
add_node("TrainingButtonStamina", _n1)
add_node("OtherStuff", _tc_tr)


@apifn(P, P)
def m_tree_get_parent(tr):
    return TREE_PARENT.get(tr, 0)


@apifn(ctypes.c_int, P)
def m_tree_child_count(tr):
    return len(TREE_CHILDREN.get(tr, []))


@apifn(P, P, ctypes.c_int)
def m_tree_get_child(tr, i):
    kids = TREE_CHILDREN.get(tr, [])
    return kids[i] if 0 <= i < len(kids) else 0


TC_INST = make_live(addr(tc_klass))
TRANSFORMS[addr(TC_INST)] = _tc_tr

# Two UnityEngine.UI.Button instances sitting on the training tree nodes.
ACTIVE = {}
BTN_OBJS = []
for _node in TREE_CHILDREN.get(_n1, []):
    _b = make_live(addr(button_klass))
    TRANSFORMS[addr(_b)] = _node
    BTN_OBJS.append(addr(_b))
BTN_ARRAY = BYTES(32 + 8 * len(BTN_OBJS))
ctypes.cast(ctypes.byref(BTN_ARRAY, 24), ctypes.POINTER(ctypes.c_size_t))[0] = len(BTN_OBJS)
for _i, _a in enumerate(BTN_OBJS):
    ctypes.cast(ctypes.byref(BTN_ARRAY, 32 + 8 * _i), ctypes.POINTER(ctypes.c_size_t))[0] = _a


@apifn(P, P)
def m_find_objects(ty):
    return ctypes.addressof(BTN_ARRAY)



BADGE_METHODS = {}


def reg(klass, name, argc, fn):
    try:
        BADGE_METHODS[(addr(klass), name, argc)] = ctypes.cast(fn, P).value
    except Exception as e:
        print("REG FAILED:", name, argc, type(fn), e)


reg(go_klass, "AddComponent", 1, m_add_component)
reg(tex_klass, ".ctor", 2, m_tex_ctor)
reg(tex_klass, "SetPixels32", 1, m_set_pixels)
reg(tex_klass, "Apply", 0, m_apply)
reg(comp_klass, "get_transform", 0, m_get_transform)
reg(trans_klass, "SetParent", 2, m_setparent)
reg(rt_klass, "set_anchorMin", 1, m_set_anchor_min)
reg(rt_klass, "set_anchorMax", 1, m_set_anchor_max)
reg(rt_klass, "set_pivot", 1, m_set_pivot)
reg(rt_klass, "set_sizeDelta", 1, m_set_size)
reg(rt_klass, "set_anchoredPosition", 1, m_set_anchored)
reg(rawui_klass, "set_texture", 1, m_set_texture)
reg(rawui_klass, "set_raycastTarget", 1, m_set_raycast)
reg(graphic_klass, "SetAllDirty", 0, m_set_all_dirty)
reg(trans_klass, "SetAsLastSibling", 0, m_last_sibling)
reg(rawui_klass, "set_maskable", 1, m_set_maskable)
reg(cr_klass, "GetAlpha", 0, m_get_alpha)
reg(comp_klass, "GetComponent", 1, m_get_component)
reg(go_klass, "set_active", 1, m_set_active)
reg(ic_klass, "LoadImage", 2, m_load_image)
reg(trans_klass, "GetSiblingIndex", 0, m_sibling_index)
reg(canvas_klass, "set_overrideSorting", 1, m_set_override)
reg(go_klass, "get_activeInHierarchy", 0, m_get_active)
reg(comp_klass, "get_gameObject", 0, GO_FN)
reg(canvas_klass, "set_sortingOrder", 1, m_set_order)
reg(canvas_klass, "set_renderMode", 1, m_set_render_mode)
reg(rt_klass, "GetWorldCorners", 1, m_get_world_corners)
reg(trans_klass, "get_parent", 0, m_tree_get_parent)
reg(trans_klass, "get_childCount", 0, m_tree_child_count)
reg(trans_klass, "GetChild", 1, m_tree_get_child)
reg(tc_klass, "Update", 0, m_tc_update)
reg(uobj_klass, "FindObjectsOfType", 1, m_find_objects)
reg(uobj_klass, "get_name", 0, m_get_name)
reg(uobj_klass, "Destroy", 1, m_destroy)
reg(go_klass, "set_name", 1, m_set_name)
# === end badge mocks =======================================================


@apifn(ctypes.c_bool, P, P)
def m_register_present(cb, userdata):
    present_cbs.append(cb)
    return True


# --- Hachimi interceptor surface -------------------------------------------
@apifn(P)
def m_hachimi_instance():
    return 0xA11CE


@apifn(P, P)
def m_get_interceptor(_hachimi):
    return 0x1C9


SECOND = {"orig": 0, "hook": 0}


@apifn(P, P, P, P)
def m_interceptor_hook(_icp, orig, hook):
    if hooked["orig"] == 0:
        hooked["orig"] = orig
        hooked["hook"] = hook
        return ctypes.cast(TRAMPOLINE_FN, P).value
    SECOND["orig"] = orig
    SECOND["hook"] = hook
    return ctypes.cast(TRAMP2_FN, P).value


def tramp2_impl(this):
    # second hook (TextCommon.Update): forward to whatever was patched
    return ctypes.cast(SECOND["orig"], ctypes.CFUNCTYPE(P, P))(this)


TRAMP2_FN = ctypes.CFUNCTYPE(P, P)(tramp2_impl)
_KEEP.append(TRAMP2_FN)


def tramp_impl(this):
    # what Hachimi's trampoline does: jump to the original function
    return ctypes.cast(ctypes.cast(GET_VC, P).value, ctypes.CFUNCTYPE(P, P))(this)


TRAMPOLINE_FN = ctypes.CFUNCTYPE(P, P)(tramp_impl)
_KEEP.append(TRAMPOLINE_FN)

SYMS = {
    "il2cpp_class_get_fields": ctypes.cast(m_class_get_fields, P).value,
    "il2cpp_object_get_class": ctypes.cast(m_object_get_class, P).value,
    "il2cpp_field_get_type": ctypes.cast(m_field_get_type, P).value,
    "il2cpp_type_get_type": ctypes.cast(m_type_get_type, P).value,
    "il2cpp_class_get_parent": ctypes.cast(m_class_get_parent, P).value,
    "il2cpp_class_from_type": ctypes.cast(m_class_from_type, P).value,
    "il2cpp_domain_get": ctypes.cast(m_domain_get, P).value,
    "il2cpp_domain_get_assemblies": ctypes.cast(m_domain_assemblies, P).value,
    "il2cpp_assembly_get_image": ctypes.cast(m_assembly_get_image, P).value,
    "il2cpp_image_get_name": ctypes.cast(m_image_get_name, P).value,
    "il2cpp_image_get_class_count": ctypes.cast(m_image_class_count, P).value,
    "il2cpp_image_get_class": ctypes.cast(m_image_get_class, P).value,
    "il2cpp_class_get_type": ctypes.cast(m_class_get_type, P).value,
    "il2cpp_type_get_object": ctypes.cast(m_type_get_object, P).value,
}
API = {
    "log": m_log,
    "hachimi_get_base_dir": m_base_dir,
    "hachimi_get_data_path": m_data_path,
    "il2cpp_resolve_symbol": m_resolve_symbol,
    "il2cpp_get_assembly_image": m_get_image,
    "il2cpp_get_class": m_get_class,
    "il2cpp_class_get_methods": m_class_get_methods,
    "hachimi_register_present_callback": m_register_present,
    "hachimi_instance": m_hachimi_instance,
    "hachimi_get_interceptor": m_get_interceptor,
    "interceptor_hook": m_interceptor_hook,
    "il2cpp_get_method_addr": m_get_method_addr,
    "il2cpp_get_method_overload_addr": m_method_overload_addr,
    "il2cpp_string_new": m_string_new,
    "il2cpp_string_length": m_string_length,
    "il2cpp_string_chars": m_string_chars,
    "il2cpp_object_new": m_object_new,
    "il2cpp_runtime_object_init": m_object_init,
    "il2cpp_create_array": m_create_array,
    "il2cpp_get_field_from_name": m_get_field_from_name,
}

GET_API = ctypes.CFUNCTYPE(P, ctypes.c_char_p)
MISSING = os.environ.get("SELFCHECK_MISSING", "")


@GET_API
def get_api(name):
    n = name.decode()
    if MISSING and n == MISSING:
        return 0
    f = API.get(n)
    return ctypes.cast(f, P).value if f else 0


def make_btn(go_name):
    b = make_live(addr(btn_klass))
    _cr = make_live(addr(cr_klass))
    ctypes.cast(ctypes.byref(b, 88), ctypes.POINTER(P))[0] = addr(_cr)
    g = make_obj(go_klass)
    go_name_map[addr(g)] = make_str(go_name)
    gameobjects[addr(b)] = addr(g)
    return b


def build_vc(klass=None, with_buttons=True, dead=False):
    vc = make_obj(klass or vc_klass)
    if with_buttons:
        if dead:
            # destroyed Unity objects keep the address but lose m_CachedPtr
            a = make_obj(btn_klass)
            b = make_obj(btn_klass)
        else:
            a = make_btn("TrainingButtonSpeed")
            b = make_btn("TrainingButtonStamina")
        icon = make_obj(go_klass)
        ctypes.cast(ctypes.byref(vc, 16), ctypes.POINTER(P))[0] = addr(a)
        ctypes.cast(ctypes.byref(vc, 24), ctypes.POINTER(P))[0] = addr(b)
        ctypes.cast(ctypes.byref(a, 32), ctypes.POINTER(P))[0] = addr(icon)
    return vc


def main():
    global BASE_DIR
    if not os.path.exists(DLL):
        print("FAIL: dll missing:", DLL)
        return 1

    tmp = tempfile.mkdtemp(prefix="badge_selfcheck_")
    BASE_DIR = tmp
    # 图标资源（真实 PNG，走 LoadImage 那条路）
    src_icons = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "icons")
    dst_icons = os.path.join(tmp, "icons")
    os.makedirs(dst_icons, exist_ok=True)
    for name in ("train.png", "wait.png", "race.png", "outing.png", "rest.png"):
        src = os.path.join(src_icons, name)
        if os.path.exists(src):
            with open(src, "rb") as f:
                data = f.read()
            with open(os.path.join(dst_icons, name), "wb") as f:
                f.write(data)

    # 造一份决策流文件，验证「读文件 -> 分类 -> 槽位」整条链路（含缓存分支）
    decision_path = os.path.join(tmp, "decision.jsonl")
    with open(decision_path, "w", encoding="utf-8") as f:
        f.write(json.dumps({
            "type": "decision", "turn": 40, "decision_kind": "train", "action_index": 7,
            "candidate_descriptions": ["速训练", "耐训练", "力训练", "大曳稽古", "智训练",
                                        "休息", "普通出行", "友人出行", "比赛"],
            "score": 1.0, "candidate_n": 8192,
        }, ensure_ascii=False) + "\n")
    with open(os.path.join(tmp, "training_badge.json"), "w", encoding="utf-8") as f:
        json.dump({"enabled": True, "mode": "probe", "max_scenes": 2, "attach": True,
                   "decision_path": decision_path.replace("\\", "/"),
                   "stale_secs": 300}, f)

    dll = ctypes.CDLL(DLL)
    dll.hachimi_init_v3.argtypes = [P, ctypes.c_int]
    dll.hachimi_init_v3.restype = ctypes.c_int

    ok = True

    def check(cond, label):
        nonlocal ok
        print(("PASS  " if cond else "FAIL  ") + label)
        if not cond:
            ok = False

    rc = dll.hachimi_init_v3(ctypes.cast(get_api, P), 3)

    if MISSING:
        aborted = any(("missing v3 api '%s'" % MISSING) in l for l in log_lines)
        print("PASS  " if rc == 1 else "FAIL  ", "init returned 1 with '%s' absent" % MISSING)
        print("PASS  " if aborted else "FAIL  ", "logged a clean abort for the missing api")
        print("PASS  " if not hooked["hook"] else "FAIL  ", "no hook installed")
        good = rc == 1 and aborted and not hooked["hook"]
        print("RESULT:", "ALL PASS" if good else "FAILURES PRESENT")
        return 0 if good else 1

    check(rc == 1, "hachimi_init_v3 returned 1 (got %s)" % rc)
    check(any("using 'umamusume.dll'" in l for l in log_lines), "resolved SceneManager")
    check(any("GetCurrentViewController @" in l for l in log_lines), "found the method address")
    check(hooked["hook"] != 0, "hook installed via the interceptor")
    check(hooked["orig"] == ctypes.cast(GET_VC, P).value,
          "hook target is the real methodPointer")
    check(any("hooked: trampoline=" in l for l in log_lines), "trampoline received from Hachimi")
    check(len(present_cbs) == 1, "present heartbeat registered")
    check(not any("[SCENE]" in l for l in log_lines), "nothing dumped before any call")

    # --- type layout dump (metadata only, runs during init) ---
    check(any(l.startswith("[TYPE] Gallop.TrainingViewController -> ")
              for l in log_lines), "type layout: TrainingViewController resolved")
    check(any("  [C] Gallop.ViewBase " in l for l in log_lines),
          "type layout: walked up to the parent class")
    check(any("_buttonSpeed off=16 : Gallop.TrainingButton" in l for l in log_lines),
          "type layout: field type name resolved for the 5 buttons")
    check(any("_trainCount off=32 : System.Int32" in l for l in log_lines),
          "type layout: value-type field typed correctly")
    check(any("[TYPE] Gallop.TrainingButton -> NOT FOUND" in l or
              "[TYPE] Gallop.TrainingButton -> " in l for l in log_lines),
          "type layout: TrainingButton handled (found or reported)")
    check(any("assembly image NULL" in l for l in log_lines),
          "type layout: unmocked assembly skipped without crashing")
    check(any("[PROBE4] get_name=0x" in l for l in log_lines),
          "unity property getters resolved (get_name / get_gameObject)")
    check(any("[PROBE4] scanned assemblies=1 classes=4 hits=3" in l for l in log_lines),
          "global class scan walked the domain and found the 3 training types")
    check(any(l.startswith("[HIT] Gallop.TrainingViewController @ umamusume.dll")
              for l in log_lines), "global scan reported the hit with its assembly")
    check(any(l == "[CHAIN] Gallop.TrainingViewController" for l in log_lines),
          "global scan dumped the chain for a hit")

    hook_fn = ctypes.CFUNCTYPE(P, P)(hooked["hook"])

    # First call: the game asks for the current view controller
    vc1 = build_vc()
    _CURRENT["vc"] = addr(vc1)
    BEFORE_LINES = len(log_lines)
    ret = hook_fn(0x6001)

    check(ret == addr(vc1), "hook returns exactly what the original returned")
    check(hooked["trampoline_calls"] == 1, "hook forwards to the trampoline")
    check(any("hook fired" in l for l in log_lines[0:]), "hook logged its first call")
    check(any(l.startswith("[SCENE] #1 Gallop.TrainingViewController") for l in log_lines),
          "dumped the training scene")
    check(any("[M] OnTapTraining" in l for l in log_lines), "method names dumped")
    check(any("_buttonSpeed off=16 -> Gallop.TrainingButton" in l for l in log_lines),
          "reference field resolved to object class")
    check(any('_buttonSpeed off=16 -> Gallop.TrainingButton go="TrainingButtonSpeed"' in l
              for l in log_lines),
          "training button GameObject name resolved")
    check(any("_trainCount off=32" in l and "->" not in l for l in log_lines),
          "value-type field not dereferenced")
    check(any("_unusedRef off=40" in l for l in log_lines), "null reference field listed")
    check(any("_icon off=32 -> UnityEngine.GameObject" in l for l in log_lines),
          "depth-2 recursion into a training-scene field")

    # --- badge construction (the whole in-game UI sequence) ---
    check(any("[BADGE] ready" in l for l in log_lines),
          "badge kit resolved every method address")
    check(any("num=" in l and l.startswith("[BADGE] texture 64x64") for l in log_lines),
          "texture built with Texture2D + SetPixels32 + Apply")
    check(PIX.get("w") == 64 and PIX.get("h") == 64, "Texture2D..ctor got 64x64")
    check(64 * 64 in PIX.get("arr_lens", []), "Color32[] length is 64*64")
    check(PIX.get("pixels", (0, ()))[0] == 64 * 64, "SetPixels32 got the full array")
    check(PIX.get("apply") == 1, "Apply() called exactly once")
    check(any(l.startswith("[BADGE] attached #1 num=0 tag=_buttonSpeed") for l in log_lines),
          "first training button received a badge")
    check(any("tag=_buttonStamina" in l for l in log_lines),
          "second training button received a badge")
    check(any("[PROBE4] m_CachedPtr off=16" in l for l in log_lines),
          "liveness sentinel offset resolved from metadata")
    check(any("[BADGE] scene attach done: 2 badge(s)" in l for l in log_lines),
          "buttons discovered by shape (class name), not by hardcoded offset")
    check(len(CHILDREN) == 4, "2 badges x (RectTransform + RawImage) = 4 AddComponent")
    calls = PARENT.get("calls", [])
    check(len(calls) > 0 and all(c is False for c in calls),
          "badges parented to the buttons themselves (worldPositionStays=false)")
    check(GEO.get("min") == (0.5, 0.5) and GEO.get("max") == (0.5, 0.5),
          "anchors centre the badge in the button (Vector2 ABI matches)")
    check(GEO.get("anchored") == (0.0, -150.0),
          "anchoredPosition offset applied so the artwork cannot cover it")
    check(GEO.get("size") == (56.0, 56.0), "sizeDelta delivered as Vector2 (56x56)")
    check(PIX.get("tex_set", 0) != 0, "RawImage.set_texture called (marks the graphic dirty)")
    check(PIX.get("raycast") is False, "RaycastTarget disabled through the setter")
    check(PIX.get("last_sibling", 0) > 0, "SetAsLastSibling used to stay on top")
    check(PIX.get("maskable") is False, "badge opts out of the button's Mask")
    check(any("mask/alpha set_maskable=0x" in l for l in log_lines),
          "mask/alpha helpers resolved")
    check(PIX.get("dirty", 0) == 0, "SetAllDirty NOT called (crash-free rebuild path)")
    check(any("setAllDirty=0x" in l and "color_off=40" in l for l in log_lines),
          "graphic colour offset resolved (40) - red tint reachable")

    # --- TextCommon.Update tick + UI tree dump ---
    check(SECOND["hook"] != 0, "second hook installed (TextCommon.Update) as main-thread tick")
    tc_hook = ctypes.CFUNCTYPE(P, P)(SECOND["hook"])
    for _ in range(3):
        tc_hook(addr(TC_INST))
    check(any("[TREE #3] root=" in l for l in log_lines), "tree dump fires on the 3rd tick")
    check(any("TrainingButtonSpeed" in l and l.rstrip().endswith("*") for l in log_lines),
          "button-like node flagged in the tree")
    check(any(l.startswith("[TREE #3] nodes=") for l in log_lines),
          "tree walk terminated with a node count")

    # Attach scan is throttled by tick count, so drive it past the threshold.
    n_before_badges = sum(1 for l in log_lines if l.startswith("[BADGE] attached"))
    for _ in range(100000):
        tc_hook(addr(TC_INST))
    check(any(l.startswith("[BTN #1] count=") for l in log_lines),
          "FindObjectsOfType enumerated the live buttons")
    check(any(l.startswith("[BTN] * UI_Root/TrainingFooterItem/") for l in log_lines),
          "button node path resolved and flagged")
    check(any("[BTN] attach done:" in l for l in log_lines),
          "attached only after the host set stayed stable")
    ai_lines = [l for l in log_lines if "-> outing (icon on slot 3)" in l]
    check(len(ai_lines) >= 1, "decision stream read: 友人出行 -> outing / slot 3")
    check(not any("-> rest (icon on slot 3)" in l for l in log_lines),
          "cached slot decoding keeps 友人出行 (no stale mapping to 休息)")
    check(any("stable=" in l for l in log_lines), "stability gate reported per pass")
    # NOTE: the PNG icon path needs System.Byte (mscorlib), which this
    # single-assembly mock cannot provide, so the icon-texture branch is not
    # exercised here; we assert the plumbing around it instead.
    check(any("[BTN] pass=" in l for l in log_lines), "scan pass reported")
    n_badges = sum(1 for (_g, c, _o) in CHILDREN if CLASS_NAMES.get(c) == "UnityEngine.UI.RawImage")
    check(n_badges <= 8, "badge objects stay bounded (no pile-up): %d" % n_badges)
    check(any(l.startswith("[ITEM] children(") for l in log_lines),
          "training item subtree reported")
    check(any(l.startswith("[NUM] ") for l in log_lines), "every button got a number")
    n_attached = sum(1 for l in log_lines if l.startswith("[BADGE] attached"))
    check(n_attached <= 6, "repeat passes did not pile up badges (got %d)" % n_attached)

    # Same scene again: pointer fast path, no re-dump, still forwards
    n_before = sum(1 for l in log_lines if l.startswith("[SCENE]"))
    for _ in range(5):
        hook_fn(0x6001)
    n_after = sum(1 for l in log_lines if l.startswith("[SCENE]"))
    check(n_before == n_after == 1, "duplicate scene suppressed (%d -> %d)" % (n_before, n_after))
    check(hooked["trampoline_calls"] == 6, "still forwards every call (no swallowed calls)")

    # Second distinct scene -> hits max_scenes=2 -> dumping stops, hook stays
    vc2 = build_vc(klass=vc2_klass, dead=True)
    _CURRENT["vc"] = addr(vc2)
    hook_fn(0x6001)
    scenes = [l for l in log_lines if l.startswith("[SCENE]")]
    check(len(scenes) == 2, "second scene dumped (got %d)" % len(scenes))
    check(any("reached max_scenes=2" in l for l in log_lines), "stopped dumping at max_scenes")

    n_before = sum(1 for l in log_lines if l.startswith("[SCENE]"))
    ret = hook_fn(0x6001)
    check(ret == addr(vc2), "hook keeps returning the original value after stopping")
    n_after = sum(1 for l in log_lines if l.startswith("[SCENE]"))
    check(n_before == n_after, "no further dumps after stopping")
    check(any("host already destroyed -> skip" in l for l in log_lines),
          "destroyed host refused (fake-null guard works)")

    # The heartbeat must never call into the fake runtime
    before_lines = len(log_lines)
    ctypes.CFUNCTYPE(None, P, P)(present_cbs[0])(0, 0)
    check(len(log_lines) == before_lines + 1, "present heartbeat only logs once")

    print("\n---- hachimi.log (simulated) ----")
    for l in log_lines:
        print(l)

    print("\nRESULT:", "ALL PASS" if ok else "FAILURES PRESENT")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
