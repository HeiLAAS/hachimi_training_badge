#![allow(static_mut_refs)]

// In-game badge construction.
//
// Goal: put a small picture on top of a training button, using only the game's
// own runtime (no overlay, no external window).
//
// Route chosen from the measured type layouts (hachimi.log 12:05 type dump):
//   * RawImage.m_Texture          off=224  : UnityEngine.Texture   <- direct write
//   * Graphic.m_RaycastTarget     off=58   : System.Boolean        <- direct write
//   * RectTransform / Transform   no managed fields (purely native) -> method calls
//   * Texture2D                   no managed fields -> ctor / SetPixels32 / Apply
//
// Why RawImage and not Image: Image needs Sprite + Rect + Vector2 (structure
// arguments with ABI risk). RawImage only needs one object reference.
//
// Every step is validated before the next one runs, and any missing method
// address degrades to "skip and log" instead of a blind call - a wrong call on
// the game thread kills the process (measured three times already).

use crate::{log_i, try_resolve, GetApiFn};
use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::sync::OnceLock;

#[repr(C)]
struct FieldInfo {
    name: *const c_char,
    type_: *const c_void,
    parent: *mut c_void,
    offset: i32,
    token: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct V2 {
    pub x: f32,
    pub y: f32,
}

type GetImageFn = unsafe extern "C" fn(*const c_char) -> *const c_void;
type GetClassFn = unsafe extern "C" fn(*const c_void, *const c_char, *const c_char) -> *mut c_void;
type ObjectNewFn = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type ObjectInitFn = unsafe extern "C" fn(*mut c_void);
type ClassGetTypeFn = unsafe extern "C" fn(*const c_void) -> *const c_void;
type TypeGetObjectFn = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type CreateArrayFn = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
type GetMethodAddrFn = unsafe extern "C" fn(*mut c_void, *const c_char, i32) -> *mut c_void;
type GetFieldFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut FieldInfo;
type ResolveSymbolFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;

const TEX_SIZE: i32 = 64;
const BADGE_SIZE: f32 = 56.0;
static mut OFFSET_Y: f32 = -150.0;

/// Vertical offset of the badge inside the button's local space. Negative = down.
/// Exposed so it can be tuned from training_badge.json without a rebuild.
pub fn set_offset(y: f32) {
    unsafe {
        OFFSET_Y = y;
    }
}

struct Kit {
    go_klass: *mut c_void,
    rt_klass: *mut c_void,
    raw_klass: *mut c_void,
    tex_klass: *mut c_void,
    c32_klass: *mut c_void,
    canvas_klass: *mut c_void,
    object_new: ObjectNewFn,
    object_init: ObjectInitFn,
    class_get_type: ClassGetTypeFn,
    type_get_object: TypeGetObjectFn,
    create_array: CreateArrayFn,
    get_method: GetMethodAddrFn,
    get_field: GetFieldFn,
    m_add_component: usize, // GameObject.AddComponent(Type)
    m_tex_ctor: usize,      // Texture2D..ctor(int,int)
    m_set_pixels: usize,    // Texture2D.SetPixels32(Color32[])
    m_tex_apply: usize,     // Texture2D.Apply()
    m_get_transform: usize, // Component.get_transform()
    m_set_parent: usize,    // Transform.SetParent(Transform,bool)
    m_set_anchor_min: usize,
    m_set_anchor_max: usize,
    m_set_pivot: usize,
    m_set_size: usize,
    m_set_anchored: usize,
    m_set_texture: usize, // RawImage.set_texture(Texture)
    m_set_raycast: usize, // Graphic.set_raycastTarget(bool)
    m_get_pixels32: usize, // Texture2D.GetPixels32()  (diagnostic)
    m_set_all_dirty: usize, // Graphic.SetAllDirty()
    m_last_sibling: usize, // Transform.SetAsLastSibling()
    m_set_maskable: usize, // MaskableGraphic.set_maskable(bool)
    m_get_alpha: usize,    // CanvasRenderer.GetAlpha()
    m_set_alpha: usize,    // CanvasRenderer.SetAlpha(float)
    m_set_active: usize,   // GameObject.set_active(bool)
    m_load_image: usize,   // ImageConversion.LoadImage(Texture2D, byte[])  (static)
    byte_klass: *mut c_void, // System.Byte
    off_canvas_renderer: i32, // Graphic.m_CanvasRenderer
    m_get_anchor_min: usize,
    m_get_anchor_max: usize,
    m_get_pivot: usize,
    m_get_size: usize,
    m_get_anchored: usize,
    m_get_parent: usize,
    m_set_render_mode: usize, // Canvas.set_renderMode(int)
    m_get_world_corners: usize, // RectTransform.GetWorldCorners(Vector3[])
    v3_klass: *mut c_void,     // UnityEngine.Vector3
    m_get_active: usize, // GameObject.get_activeInHierarchy()
    m_get_go: usize,     // Component.get_gameObject()
    m_set_override_sorting: usize, // Canvas.set_overrideSorting(bool)
    m_get_child_count: usize, // Transform.get_childCount()
    m_get_child: usize,      // Transform.GetChild(int)
    m_get_name: usize,       // Object.get_name()
    m_set_name: usize,       // GameObject.set_name(string)
    m_string_new: usize,     // il2cpp_string_new(char*)
    m_str_len: usize,        // il2cpp_string_length()
    m_str_chars: usize,      // il2cpp_string_chars()
    m_destroy: usize,        // Object.Destroy(Object)
    m_set_hide_flags: usize,  // Object.set_hideFlags(HideFlags)
    m_set_sorting_order: usize, // Canvas.set_sortingOrder(int)
    off_graphic_color: i32, // Graphic.m_Color (UnityEngine.Color, 16 bytes)
    off_uv_rect: i32,       // RawImage.m_UVRect (UnityEngine.Rect)
    off_raw_texture: i32,
    off_raycast: i32,
    off_cached_ptr: i32,
}

unsafe impl Send for Kit {}
unsafe impl Sync for Kit {}

static KIT: OnceLock<Kit> = OnceLock::new();

struct State {
    texture: *mut c_void,
    count: u32,
}

static mut STATE: State = State {
    texture: ptr::null_mut(),
    count: 0,
};

// Hosts that already carry a badge. Base-class and derived-class fields can
// resolve to the same object, so de-duplicating by pointer is not optional.
static mut OVERLAY: usize = 0; // our own top-most ScreenSpaceOverlay canvas
static mut HOSTS: [usize; 512] = [0; 512];
static mut HOST_TAGS: [u64; 512] = [0; 512];
static mut BADGE_TRS: [usize; 64] = [0; 64];
static mut BADGE_RAW: [usize; 64] = [0; 64];
static mut BADGE_HOST: [usize; 64] = [0; 64];
static mut BADGE_TRS_N: usize = 0;
static mut LAST_SYNC_SIG: u64 = 0;
static mut HOST_N: usize = 0;

/// A host may legitimately carry one badge per icon (train / wait / race /
/// outing / rest) - they are separate GameObjects that we show/hide by name.
/// So the tag is part of the de-dup key, not just the host pointer.
fn tag_hash(tag: &str) -> u64 {
    tag.as_bytes().iter().fold(0xcbf2_9ce4_8422_2325u64, |a, b| {
        a.wrapping_mul(0x1000_0000_1b3u64) ^ (*b as u64)
    })
}

fn dedup_key(tag: &str) -> u64 {
    // Only the AI badges may coexist on one host (one per icon). Number/debug
    // markers keep the original "one per host" rule, otherwise a scene scan
    // would pile up duplicate markers.
    if tag.starts_with("TB_Badge_") {
        tag_hash(tag)
    } else {
        0
    }
}

unsafe fn already_hosted(host: *mut c_void, tag: &str) -> bool {
    let h = host as usize;
    let t = dedup_key(tag);
    for i in 0..HOST_N {
        if HOSTS[i] == h && HOST_TAGS[i] == t {
            return true;
        }
    }
    false
}

unsafe fn remember_host(host: *mut c_void, tag: &str) {
    if HOST_N < 512 {
        HOSTS[HOST_N] = host as usize;
        HOST_TAGS[HOST_N] = dedup_key(tag);
        HOST_N += 1;
    }
}

/// Hard ceiling: if something upstream keeps producing new hosts, stop instead
/// of piling up hundreds of GameObjects (that happened: 1052 badges in one run).
unsafe fn remember_badge(tr: *mut c_void, raw: *mut c_void, host: *mut c_void) {
    if BADGE_TRS_N < 64 {
        BADGE_TRS[BADGE_TRS_N] = tr as usize;
        BADGE_RAW[BADGE_TRS_N] = raw as usize;
        BADGE_HOST[BADGE_TRS_N] = host as usize;
        BADGE_TRS_N += 1;
    }
}

unsafe fn cr_of(k: &Kit, comp: *mut c_void) -> *mut c_void {
    if k.off_canvas_renderer <= 0 || !is_alive(k, comp) {
        return ptr::null_mut();
    }
    ptr::read_unaligned((comp as *const u8).add(k.off_canvas_renderer as usize) as *const *mut c_void)
}

/// Alpha of the host graphic, so the badge disappears together with the button
/// instead of floating over unrelated screens.
unsafe fn host_alpha(k: &Kit, comp: *mut c_void) -> Option<f32> {
    if k.m_get_alpha == 0 {
        return None;
    }
    let cr = cr_of(k, comp);
    if !is_alive(k, cr) {
        return None;
    }
    let f: unsafe extern "C" fn(*mut c_void) -> f32 = std::mem::transmute(k.m_get_alpha);
    Some(f(cr))
}

/// Re-assert draw order. The button keeps instantiating its icon/text children
/// after we attach, and those landed *after* our node, which is why the badge sat
/// behind the artwork. Called on every scan pass.
pub unsafe fn raise_all() {
    let Some(k) = KIT.get() else { return };
    if k.m_last_sibling == 0 {
        return;
    }
    let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(k.m_last_sibling);
    for i in 0..BADGE_TRS_N {
        let tr = BADGE_TRS[i] as *mut c_void;
        if is_alive(k, tr) {
            f(tr);
        }
    }
}


/// Our own ScreenSpaceOverlay canvas at sortingOrder 32000: nothing in the game's
/// hierarchy can draw over it, and no Mask can clip it.
unsafe fn overlay(k: &Kit) -> *mut c_void {
    if OVERLAY != 0 {
        return OVERLAY as *mut c_void;
    }
    if k.canvas_klass.is_null() {
        log_i("[BADGE] Canvas class missing -> no overlay");
        return ptr::null_mut();
    }
    let go = (k.object_new)(k.go_klass);
    if go.is_null() {
        return ptr::null_mut();
    }
    (k.object_init)(go);
    if !is_alive(k, go) {
        log_i("[BADGE] overlay GameObject cannot be created -> abort");
        return ptr::null_mut();
    }
    let add = |klass: *mut c_void| -> *mut c_void {
        let g: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(k.m_add_component);
        let t = (k.class_get_type)(klass);
        if t.is_null() {
            return ptr::null_mut();
        }
        let sys_type = (k.type_get_object)(t);
        if sys_type.is_null() {
            return ptr::null_mut();
        }
        g(go, sys_type)
    };
    let _rt = add(k.rt_klass);
    let cnv = add(k.canvas_klass);
    if !is_alive(k, cnv) {
        log_i("[BADGE] overlay AddComponent(Canvas) failed");
        return ptr::null_mut();
    }
    // A fresh Canvas is already ScreenSpaceOverlay with sortingOrder 0, and an
    // overlay canvas created last draws last - so the setters are nice-to-have,
    // not required. (set_renderMode does not resolve on this build.)
    if k.m_set_render_mode != 0 {
        let rm: unsafe extern "C" fn(*mut c_void, i32) = std::mem::transmute(k.m_set_render_mode);
        rm(cnv, 0);
    }
    if k.m_set_sorting_order != 0 {
        let so: unsafe extern "C" fn(*mut c_void, i32) = std::mem::transmute(k.m_set_sorting_order);
        so(cnv, 32000);
    }
    log_i(&format!(
        "[BADGE] overlay created (renderMode setter={} sortingOrder setter={})",
        k.m_set_render_mode != 0,
        k.m_set_sorting_order != 0
    ));
    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(k.m_get_transform);
    OVERLAY = get_tr(cnv) as usize;
    let ovrt = OVERLAY as *mut c_void;
    if k.m_get_world_corners != 0 && !k.v3_klass.is_null() {
        let arr = (k.create_array)(k.v3_klass, 4);
        if !arr.is_null() {
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(k.m_get_world_corners);
            f(ovrt, arr);
            let b = (arr as *const u8).add(32) as *const f32;
            let x0 = ptr::read_unaligned(b);
            let y0 = ptr::read_unaligned(b.add(1));
            let x2 = ptr::read_unaligned(b.add(6));
            let y2 = ptr::read_unaligned(b.add(7));
            log_i(&format!(
                "[BADGE] overlay rect = ({:.0},{:.0})-({:.0},{:.0}) => {:.0}x{:.0}",
                x0,
                y0,
                x2,
                y2,
                (x2 - x0).abs(),
                (y2 - y0).abs()
            ));
        }
    }
    log_i(&format!("[BADGE] overlay canvas created rt={:p}", ovrt));
    OVERLAY as *mut c_void
}

/// Centre of a RectTransform in screen pixels, via GetWorldCorners (array in,
/// nothing returned by value - no structure-return ABI involved).
unsafe fn screen_center(k: &Kit, rt: *mut c_void) -> Option<V2> {
    if k.m_get_world_corners == 0 || k.v3_klass.is_null() {
        return None;
    }
    let arr = (k.create_array)(k.v3_klass, 4);
    if arr.is_null() {
        return None;
    }
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(k.m_get_world_corners);
    f(rt, arr);
    let base = (arr as *const u8).add(32) as *const f32;
    let mut sx = 0.0f32;
    let mut sy = 0.0f32;
    for i in 0..4 {
        sx += ptr::read_unaligned(base.add(i * 3));
        sy += ptr::read_unaligned(base.add(i * 3 + 1));
    }
    Some(V2 { x: sx / 4.0, y: sy / 4.0 })
}


unsafe fn corners_rect(k: &Kit, rt: *mut c_void) -> Option<(V2, V2)> {
    if k.m_get_world_corners == 0 || k.v3_klass.is_null() {
        return None;
    }
    let arr = (k.create_array)(k.v3_klass, 4);
    if arr.is_null() {
        return None;
    }
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(k.m_get_world_corners);
    f(rt, arr);
    let b = (arr as *const u8).add(32) as *const f32;
    let mut minx = f32::MAX;
    let mut miny = f32::MAX;
    let mut maxx = -f32::MAX;
    let mut maxy = -f32::MAX;
    for i in 0..4 {
        let x = ptr::read_unaligned(b.add(i * 3));
        let y = ptr::read_unaligned(b.add(i * 3 + 1));
        if x < minx { minx = x; }
        if y < miny { miny = y; }
        if x > maxx { maxx = x; }
        if y > maxy { maxy = y; }
    }
    Some((V2 { x: minx, y: miny }, V2 { x: maxx - minx, y: maxy - miny }))
}

/// Map a host element's centre into overlay pixels by normalising against the
/// game canvas' own rect. The game's UI world space is ~14 units wide, so raw
/// world coordinates cannot be used as pixels.
unsafe fn map_to_overlay(k: &Kit, host_tr: *mut c_void) -> Option<V2> {
    let host = corners_rect(k, host_tr)?;
    if host.1.x <= 0.0 || host.1.y <= 0.0 {
        return None;
    }
    let get_parent: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(k.m_get_parent);
    let mut root = host_tr;
    for _ in 0..24 {
        let p = get_parent(root);
        if !is_alive(k, p) {
            break;
        }
        root = p;
    }
    let rr = corners_rect(k, root)?;
    if rr.1.x <= 0.0 || rr.1.y <= 0.0 {
        return None;
    }
    let ov = corners_rect(k, OVERLAY as *mut c_void)?;
    let sx = ov.1.x / rr.1.x;
    let sy = ov.1.y / rr.1.y;
    // centre of the host in canvas-normalised space, then scaled into overlay px
    let cx = host.0.x + host.1.x / 2.0;
    let cy = host.0.y + host.1.y / 2.0;
    Some(V2 {
        x: ov.0.x + (cx - rr.0.x) * sx,
        y: ov.0.y + (cy - rr.0.y) * sy,
    })
}

unsafe fn a_go_addr(k: &Kit) -> usize {
    k.m_get_go
}

unsafe fn budget_left() -> bool {
    // The footer is re-instantiated as turns advance, so hosts keep changing;
    // each badge dies with its parent, so a generous ceiling is still bounded.
    HOST_N < 200
}

unsafe fn st() -> *mut State {
    ptr::addr_of_mut!(STATE)
}

unsafe fn sym<T: Copy>(resolve: ResolveSymbolFn, name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    let p = resolve(c.as_ptr());
    if p.is_null() {
        log_i(&format!("[BADGE] symbol '{}' unavailable", name));
        return None;
    }
    Some(std::mem::transmute_copy::<*mut c_void, T>(&p))
}

unsafe fn field_offset(k: &Kit, klass: *mut c_void, name: &str) -> i32 {
    let Ok(c) = CString::new(name) else { return 0 };
    let f = (k.get_field)(klass, c.as_ptr());
    if f.is_null() {
        return 0;
    }
    (*f).offset
}

unsafe fn method(k: &Kit, klass: *mut c_void, name: &str, argc: i32) -> usize {
    let Ok(c) = CString::new(name) else { return 0 };
    (k.get_method)(klass, c.as_ptr(), argc) as usize
}

/// Unity's "destroyed / fake null" test: a live managed object wraps a native
/// object, so m_CachedPtr must be non-zero.
unsafe fn is_alive(k: &Kit, obj: *mut c_void) -> bool {
    if obj.is_null() || k.off_cached_ptr <= 0 {
        return false;
    }
    let p = ptr::read_unaligned((obj as *const u8).add(k.off_cached_ptr as usize) as *const usize);
    p != 0
}

unsafe fn image_of(get_api: GetApiFn, asm: &str) -> Option<*const c_void> {
    let f: GetImageFn = try_resolve(get_api, "il2cpp_get_assembly_image")?;
    let c = CString::new(asm).ok()?;
    let img = f(c.as_ptr());
    if img.is_null() {
        None
    } else {
        Some(img)
    }
}

pub fn init(get_api: GetApiFn) -> bool {
    unsafe {
        let Some(core_img) = image_of(get_api, "UnityEngine.CoreModule.dll") else {
            log_i("[BADGE] no UnityEngine.CoreModule.dll -> disabled");
            return false;
        };
        let Some(ui_img) = image_of(get_api, "UnityEngine.UI.dll") else {
            log_i("[BADGE] no UnityEngine.UI.dll -> disabled");
            return false;
        };
        let get_class: GetClassFn = match try_resolve(get_api, "il2cpp_get_class") {
            Some(f) => f,
            None => return false,
        };
        let (Some(object_new), Some(object_init), Some(create_array), Some(get_method), Some(get_field)) = (
            try_resolve::<ObjectNewFn>(get_api, "il2cpp_object_new"),
            try_resolve::<ObjectInitFn>(get_api, "il2cpp_runtime_object_init"),
            try_resolve::<CreateArrayFn>(get_api, "il2cpp_create_array"),
            try_resolve::<GetMethodAddrFn>(get_api, "il2cpp_get_method_addr"),
            try_resolve::<GetFieldFn>(get_api, "il2cpp_get_field_from_name"),
        ) else {
            log_i("[BADGE] core il2cpp api missing -> disabled");
            return false;
        };
        let resolve: ResolveSymbolFn = match try_resolve(get_api, "il2cpp_resolve_symbol") {
            Some(f) => f,
            None => return false,
        };
        let (Some(class_get_type), Some(type_get_object)) = (
            sym::<ClassGetTypeFn>(resolve, "il2cpp_class_get_type"),
            sym::<TypeGetObjectFn>(resolve, "il2cpp_type_get_object"),
        ) else {
            log_i("[BADGE] type reflection missing -> disabled");
            return false;
        };

        let cls = |img: *const c_void, ns: &str, name: &str| -> *mut c_void {
            let (Ok(ns), Ok(n)) = (CString::new(ns), CString::new(name)) else {
                return ptr::null_mut();
            };
            get_class(img, ns.as_ptr(), n.as_ptr())
        };

        let go_klass = cls(core_img, "UnityEngine", "GameObject");
        let rt_klass = cls(core_img, "UnityEngine", "RectTransform");
        let tex_klass = cls(core_img, "UnityEngine", "Texture2D");
        let c32_klass = cls(core_img, "UnityEngine", "Color32");
        let comp_klass = cls(core_img, "UnityEngine", "Component");
        let trans_klass = cls(core_img, "UnityEngine", "Transform");
        let obj_klass = cls(core_img, "UnityEngine", "Object");
        let raw_klass = cls(ui_img, "UnityEngine.UI", "RawImage");
        if [
            go_klass, rt_klass, tex_klass, c32_klass, comp_klass, trans_klass, obj_klass, raw_klass,
        ]
        .iter()
        .any(|p| p.is_null())
        {
            log_i("[BADGE] a Unity class is missing -> disabled");
            return false;
        }

        let uimod_img = image_of(get_api, "UnityEngine.UIModule.dll").unwrap_or(ptr::null());
        let canvas_klass = {
            let a = cls(core_img, "UnityEngine", "Canvas");
            if !a.is_null() {
                a
            } else if !uimod_img.is_null() {
                cls(uimod_img, "UnityEngine", "Canvas")
            } else {
                ptr::null_mut()
            }
        };
        log_i(&format!(
            "[BADGE] Canvas klass={:p} (UIModule image {:p})",
            canvas_klass, uimod_img
        ));

        let mut k = Kit {
            go_klass,
            rt_klass,
            raw_klass,
            tex_klass,
            c32_klass,
            canvas_klass,
            object_new,
            object_init,
            class_get_type,
            type_get_object,
            create_array,
            get_method,
            get_field,
            m_add_component: 0,
            m_tex_ctor: 0,
            m_set_pixels: 0,
            m_tex_apply: 0,
            m_get_transform: 0,
            m_set_parent: 0,
            m_set_anchor_min: 0,
            m_set_anchor_max: 0,
            m_set_pivot: 0,
            m_set_size: 0,
            m_set_anchored: 0,
            m_set_texture: 0,
            m_set_raycast: 0,
            m_get_pixels32: 0,
            m_set_all_dirty: 0,
            m_last_sibling: 0,
            m_set_maskable: 0,
            m_get_alpha: 0,
            m_set_alpha: 0,
            m_set_active: 0,
            m_load_image: 0,
            byte_klass: ptr::null_mut(),
            off_canvas_renderer: 0,
            m_get_anchor_min: 0,
            m_get_anchor_max: 0,
            m_get_pivot: 0,
            m_get_size: 0,
            m_get_anchored: 0,
            m_get_parent: 0,
            m_set_render_mode: 0,
            m_get_world_corners: 0,
            v3_klass: ptr::null_mut(),
            m_get_active: 0,
            m_get_go: 0,
            m_set_override_sorting: 0,
            m_get_child_count: 0,
            m_get_child: 0,
            m_get_name: 0,
            m_set_name: 0,
            m_string_new: 0,
            m_str_len: 0,
            m_str_chars: 0,
            m_destroy: 0,
            m_set_hide_flags: 0,
            m_set_sorting_order: 0,
            off_graphic_color: 0,
            off_uv_rect: 0,
            off_raw_texture: 0,
            off_raycast: 0,
            off_cached_ptr: 0,
        };

        k.m_add_component = method(&k, go_klass, "AddComponent", 1);
        k.m_tex_ctor = method(&k, tex_klass, ".ctor", 2);
        k.m_set_pixels = method(&k, tex_klass, "SetPixels32", 1);
        k.m_tex_apply = method(&k, tex_klass, "Apply", 0);
        k.m_get_transform = method(&k, comp_klass, "get_transform", 0);
        k.m_set_parent = method(&k, trans_klass, "SetParent", 2);
        k.m_set_anchor_min = method(&k, rt_klass, "set_anchorMin", 1);
        k.m_set_anchor_max = method(&k, rt_klass, "set_anchorMax", 1);
        k.m_set_pivot = method(&k, rt_klass, "set_pivot", 1);
        k.m_set_size = method(&k, rt_klass, "set_sizeDelta", 1);
        k.m_set_anchored = method(&k, rt_klass, "set_anchoredPosition", 1);
        // Prefer the real setters: they mark the mesh/material dirty. Writing the
        // backing field directly skipped that and left the CanvasRenderer with
        // stale state, which is the prime suspect for the 12:24 crash.
        let graphic_klass = cls(ui_img, "UnityEngine.UI", "Graphic");
        k.m_set_texture = method(&k, raw_klass, "set_texture", 1);
        if k.m_set_texture == 0 && !graphic_klass.is_null() {
            k.m_set_texture = method(&k, graphic_klass, "set_texture", 1);
        }
        k.m_set_raycast = method(&k, raw_klass, "set_raycastTarget", 1);
        if k.m_set_raycast == 0 && !graphic_klass.is_null() {
            k.m_set_raycast = method(&k, graphic_klass, "set_raycastTarget", 1);
        }
        k.m_get_pixels32 = method(&k, tex_klass, "GetPixels32", 0);
        k.off_graphic_color = if graphic_klass.is_null() {
            0
        } else {
            field_offset(&k, graphic_klass, "m_Color")
        };
        k.m_set_all_dirty = method(&k, graphic_klass, "SetAllDirty", 0);
        if k.m_set_all_dirty == 0 {
            // SetAllDirty is declared on Graphic; some builds expose it on the subclass only
            k.m_set_all_dirty = method(&k, raw_klass, "SetAllDirty", 0);
        }
        k.m_get_parent = method(&k, trans_klass, "get_parent", 0);
        k.m_last_sibling = method(&k, trans_klass, "SetAsLastSibling", 0);
        k.m_set_maskable = method(&k, raw_klass, "set_maskable", 1);
        if k.m_set_maskable == 0 && !graphic_klass.is_null() {
            k.m_set_maskable = method(&k, graphic_klass, "set_maskable", 1);
        }
        k.off_canvas_renderer = if graphic_klass.is_null() {
            0
        } else {
            field_offset(&k, graphic_klass, "m_CanvasRenderer")
        };
        if k.off_canvas_renderer > 0 {
            let cr_klass = {
                let a = cls(core_img, "UnityEngine", "CanvasRenderer");
                if !a.is_null() {
                    a
                } else if !uimod_img.is_null() {
                    cls(uimod_img, "UnityEngine", "CanvasRenderer")
                } else {
                    ptr::null_mut()
                }
            };
            if !cr_klass.is_null() {
                k.m_get_alpha = method(&k, cr_klass, "GetAlpha", 0);
                k.m_set_alpha = method(&k, cr_klass, "SetAlpha", 1);
                log_i(&format!(
                    "[BADGE] CanvasRenderer getAlpha=0x{:x} setAlpha=0x{:x}",
                    k.m_get_alpha, k.m_set_alpha
                ));
            }
        }
        log_i(&format!(
            "[BADGE] mask/alpha set_maskable=0x{:x} getAlpha=0x{:x} cr_off={}",
            k.m_set_maskable, k.m_get_alpha, k.off_canvas_renderer
        ));
        k.m_get_active = method(&k, go_klass, "get_activeInHierarchy", 0);
        k.m_get_go = method(&k, comp_klass, "get_gameObject", 0);
        k.m_set_active = method(&k, go_klass, "SetActive", 1);
        if k.m_set_active == 0 {
            k.m_set_active = method(&k, go_klass, "set_active", 1);
        }
        log_i(&format!("[BADGE] GameObject.set_active=0x{:x}", k.m_set_active));
        k.m_get_anchor_min = method(&k, rt_klass, "get_anchorMin", 0);
        k.m_get_anchor_max = method(&k, rt_klass, "get_anchorMax", 0);
        k.m_get_pivot = method(&k, rt_klass, "get_pivot", 0);
        k.m_get_size = method(&k, rt_klass, "get_sizeDelta", 0);
        k.m_get_anchored = method(&k, rt_klass, "get_anchoredPosition", 0);
        if !k.canvas_klass.is_null() {
            k.m_set_override_sorting = method(&k, k.canvas_klass, "set_overrideSorting", 1);
            k.m_set_sorting_order = method(&k, k.canvas_klass, "set_sortingOrder", 1);
            k.m_set_render_mode = method(&k, k.canvas_klass, "set_renderMode", 1);
        }
        // ImageConversion lives in its own module on this Unity version.
        let ic_klass = {
            let mut found = cls(core_img, "UnityEngine", "ImageConversion");
            if found.is_null() {
                if let Some(img) = image_of(get_api, "UnityEngine.ImageConversionModule.dll") {
                    found = cls(img, "UnityEngine", "ImageConversion");
                }
            }
            found
        };
        if !ic_klass.is_null() {
            k.m_load_image = method(&k, ic_klass, "LoadImage", 2);
        }
        if let Some(mscorlib) = image_of(get_api, "mscorlib.dll") {
            let ns_sys = CString::new("System").unwrap();
            let c_byte = CString::new("Byte").unwrap();
            k.byte_klass = get_class(mscorlib, ns_sys.as_ptr(), c_byte.as_ptr());
        }
        log_i(&format!(
            "[BADGE] LoadImage=0x{:x} Byte={:p}",
            k.m_load_image, k.byte_klass
        ));
        k.m_get_child_count = method(&k, trans_klass, "get_childCount", 0);
        k.m_get_child = method(&k, trans_klass, "GetChild", 1);
        let uobj_klass = cls(core_img, "UnityEngine", "Object");
        k.m_get_name = method(&k, uobj_klass, "get_name", 0);
        k.m_set_name = method(&k, go_klass, "set_name", 1);
        k.m_destroy = method(&k, uobj_klass, "Destroy", 1);
        k.m_set_hide_flags = method(&k, uobj_klass, "set_hideFlags", 1);
        k.m_str_len = try_resolve::<unsafe extern "C" fn(*mut c_void) -> i32>(
            get_api,
            "il2cpp_string_length",
        )
        .map(|f| f as usize)
        .unwrap_or(0);
        k.m_str_chars = try_resolve::<unsafe extern "C" fn(*mut c_void) -> *const u16>(
            get_api,
            "il2cpp_string_chars",
        )
        .map(|f| f as usize)
        .unwrap_or(0);
        k.m_string_new = try_resolve::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(
            get_api,
            "il2cpp_string_new",
        )
        .map(|f| f as usize)
        .unwrap_or(0);
        log_i(&format!(
            "[BADGE] names getName=0x{:x} setName=0x{:x} strNew=0x{:x} destroy=0x{:x} children=0x{:x}/0x{:x}",
            k.m_get_name, k.m_set_name, k.m_string_new, k.m_destroy, k.m_get_child_count, k.m_get_child
        ));
        k.m_get_world_corners = method(&k, rt_klass, "GetWorldCorners", 1);
        k.v3_klass = cls(core_img, "UnityEngine", "Vector3");
        log_i(&format!(
            "[BADGE] overlay setRenderMode=0x{:x} getWorldCorners=0x{:x} Vector3={:p}",
            k.m_set_render_mode, k.m_get_world_corners, k.v3_klass
        ));
        k.off_uv_rect = field_offset(&k, raw_klass, "m_UVRect");
        k.off_raw_texture = field_offset(&k, raw_klass, "m_Texture");
        log_i(&format!(
            "[BADGE] setters set_texture=0x{:x} set_raycast=0x{:x} setAllDirty=0x{:x} \
getPixels32=0x{:x} color_off={} uv_off={} tex_off={}",
            k.m_set_texture,
            k.m_set_raycast,
            k.m_set_all_dirty,
            k.m_get_pixels32,
            k.off_graphic_color,
            k.off_uv_rect,
            k.off_raw_texture
        ));
        k.off_raycast = field_offset(&k, raw_klass, "m_RaycastTarget");
        k.off_cached_ptr = field_offset(&k, obj_klass, "m_CachedPtr");

        log_i(&format!(
            "[BADGE] ready addComp=0x{:x} texCtor=0x{:x} setPixels=0x{:x} apply=0x{:x} \
getTr=0x{:x} setParent=0x{:x} anchor=0x{:x}/0x{:x} pivot=0x{:x} size=0x{:x} anchored=0x{:x} \
| m_Texture={} raycast={} cached={}",
            k.m_add_component,
            k.m_tex_ctor,
            k.m_set_pixels,
            k.m_tex_apply,
            k.m_get_transform,
            k.m_set_parent,
            k.m_set_anchor_min,
            k.m_set_anchor_max,
            k.m_set_pivot,
            k.m_set_size,
            k.m_set_anchored,
            k.off_raw_texture,
            k.off_raycast,
            k.off_cached_ptr
        ));

        if k.m_add_component == 0
            || k.m_tex_ctor == 0
            || k.m_set_pixels == 0
            || k.m_tex_apply == 0
            || k.m_get_transform == 0
            || k.m_set_parent == 0
            || k.off_raw_texture == 0
            || k.off_cached_ptr == 0
        {
            log_i("[BADGE] essential pieces missing -> disabled");
            return false;
        }

        let _ = KIT.set(k);
        true
    }
}

pub fn ready() -> bool {
    KIT.get().is_some()
}

/// Seven-segment digit so each badge can be told apart at a glance.
fn segments_for(d: u8) -> &'static [usize] {
    // A B C D E F G
    match d {
        0 => &[0, 1, 2, 3, 4, 5],
        1 => &[1, 2],
        2 => &[0, 1, 6, 4, 3],
        3 => &[0, 1, 6, 2, 3],
        4 => &[5, 6, 1, 2],
        5 => &[0, 5, 6, 2, 3],
        6 => &[0, 5, 6, 4, 2, 3],
        7 => &[0, 1, 2],
        8 => &[0, 1, 2, 3, 4, 5, 6],
        _ => &[0, 5, 6, 4, 2, 3],
    }
}

fn draw_digit(buf: &mut [u8], n: i32, x0: f32, y0: f32, w: f32, h: f32, d: u8) {
    let t = (w * 0.22).max(3.0);
    let hh = h / 2.0;
    let rects = [
        (x0 + t * 0.6, y0, w - t * 1.2, t),                       // A
        (x0 + w - t, y0 + t * 0.6, t, hh - t * 1.1),              // B
        (x0 + w - t, y0 + hh + t * 0.4, t, hh - t * 1.1),         // C
        (x0 + t * 0.6, y0 + h - t, w - t * 1.2, t),               // D
        (x0, y0 + hh + t * 0.4, t, hh - t * 1.1),                 // E
        (x0, y0 + t * 0.6, t, hh - t * 1.1),                      // F
        (x0 + t * 0.6, y0 + hh - t / 2.0, w - t * 1.2, t),        // G
    ];
    for seg in segments_for(d) {
        let (rx, ry, rw, rh) = rects[*seg];
        for y in (ry as i32).max(1)..((ry + rh) as i32).min(n - 1) {
            for x in (rx as i32).max(1)..((rx + rw) as i32).min(n - 1) {
                let i = (((n - 1 - y) * n + x) * 4) as usize;
                buf[i] = 255;
                buf[i + 1] = 255;
                buf[i + 2] = 255;
                buf[i + 3] = 255;
            }
        }
    }
}

/// Green disc, dark ring, white number (1..99) in the middle.
fn make_pixels(num: u8) -> Vec<u8> {
    let n = TEX_SIZE;
    let c = (n as f32 - 1.0) / 2.0;
    let r_out = c - 1.0;
    let r_in = r_out - 3.0;
    let mut buf = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d = (dx * dx + dy * dy).sqrt();
            let i = (((n - 1 - y) * n + x) * 4) as usize;
            if d <= r_in {
                // medium green, easy on the eyes
                buf[i] = 24;
                buf[i + 1] = 148;
                buf[i + 2] = 62;
                buf[i + 3] = 235;
            } else if d <= r_out {
                buf[i] = 16;
                buf[i + 1] = 96;
                buf[i + 2] = 40;
                buf[i + 3] = 255;
            }
        }
    }

    let v = num.min(99) as u32;
    if v < 10 {
        draw_digit(&mut buf, n, 24.0, 16.0, 16.0, 32.0, v as u8);
    } else {
        let a = (v / 10) as u8;
        let b = (v % 10) as u8;
        draw_digit(&mut buf, n, 8.0, 20.0, 13.0, 26.0, a);
        draw_digit(&mut buf, n, 24.0, 20.0, 13.0, 26.0, b);
        let _ = 0;
    }
    buf
}

static mut ICONS: Vec<(&'static str, Vec<u8>)> = Vec::new();
static mut ICON_TEX: Vec<(&'static str, usize)> = Vec::new();

/// Register an icon PNG (decoded later by the engine via LoadImage).
pub fn set_icon_png(key: &'static str, bytes: Vec<u8>) {
    unsafe {
        ICONS.retain(|(k, _)| *k != key);
        ICONS.push((key, bytes));
    }
}

fn icon_bytes(key: &str) -> Option<Vec<u8>> {
    unsafe {
        ICONS.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, b)| b.clone())
    }
}

/// Natural pixel size of a PNG, read from its IHDR header (width/height at
/// offset 16). The badge is displayed at 1:1 so the artwork keeps its shape.
fn png_size(bytes: &[u8]) -> Option<(f32, f32)> {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as f32;
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as f32;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((w, h))
}

/// Displayed size per icon: the PNG's own pixel size.
fn icon_size(key: &str) -> (f32, f32) {
    if let Some(bytes) = icon_bytes(key) {
        if let Some((w, h)) = png_size(&bytes) {
            return (w, h);
        }
    }
    (BADGE_SIZE, BADGE_SIZE)
}

static mut TEX_BY_NUM: [usize; 100] = [0; 100];
static mut SLOT_RAW: [usize; 6] = [0; 6];
static mut SLOT_GO: [usize; 6] = [0; 6];
static mut CURRENT_ICON: Option<String> = None;

/// Decode the supplied PNG into a Texture2D via ImageConversion.LoadImage.
unsafe fn icon_texture(k: &Kit, key: &str) -> *mut c_void {
    for (k, t) in ICON_TEX.iter() {
        if *k == key {
            return *t as *mut c_void;
        }
    }
    let Some(bytes) = icon_bytes(key) else {
        return ptr::null_mut();
    };
    if k.m_load_image == 0 || k.byte_klass.is_null() || k.m_tex_ctor == 0 {
        log_i("[BADGE] no LoadImage/Byte -> falling back to generated digits");
        return ptr::null_mut();
    }
    let tex = (k.object_new)(k.tex_klass);
    if tex.is_null() {
        return ptr::null_mut();
    }
    let ctor: unsafe extern "C" fn(*mut c_void, i32, i32) = std::mem::transmute(k.m_tex_ctor);
    ctor(tex, 2, 2);
    if !is_alive(k, tex) {
        log_i("[BADGE] icon Texture2D alloc failed");
        return ptr::null_mut();
    }
    let arr = (k.create_array)(k.byte_klass, bytes.len());
    if arr.is_null() {
        log_i("[BADGE] icon byte[] alloc failed");
        return ptr::null_mut();
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), (arr as *mut u8).add(32), bytes.len());
    let load: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool =
        std::mem::transmute(k.m_load_image);
    let ok = load(tex, arr);
    if ok && k.m_set_hide_flags != 0 {
        // HideFlags.DontSave (52) + DontUnloadUnusedAsset: without this the game
        // can unload our runtime texture (Resources.UnloadUnusedAssets on scene
        // changes), which turns the badge into a white square and then crashes.
        let hf: unsafe extern "C" fn(*mut c_void, i32) = std::mem::transmute(k.m_set_hide_flags);
        hf(tex, 52);
    }
    log_i(&format!(
        "[BADGE] icon decoded bytes={} ok={} tex={:p}",
        bytes.len(),
        ok,
        tex
    ));
    if !ok {
        return ptr::null_mut();
    }
    ICON_TEX.push((Box::leak(key.to_string().into_boxed_str()), tex as usize));
    tex
}

unsafe fn texture_for(k: &Kit, num: u8) -> *mut c_void {
    let d = (num as usize).min(99);
    if TEX_BY_NUM[d] != 0 {
        return TEX_BY_NUM[d] as *mut c_void;
    }
    let tex = build_texture(k, num);
    if !tex.is_null() {
        TEX_BY_NUM[d] = tex as usize;
    }
    tex
}

unsafe fn build_texture(k: &Kit, num: u8) -> *mut c_void {

    let tex = (k.object_new)(k.tex_klass);
    if tex.is_null() {
        log_i("[BADGE] Texture2D alloc failed");
        return ptr::null_mut();
    }
    let ctor: unsafe extern "C" fn(*mut c_void, i32, i32) = std::mem::transmute(k.m_tex_ctor);
    ctor(tex, TEX_SIZE, TEX_SIZE);
    if !is_alive(k, tex) {
        log_i("[BADGE] Texture2D..ctor(int,int) produced no native texture -> abort");
        return ptr::null_mut();
    }

    let total = (TEX_SIZE * TEX_SIZE) as usize;
    let arr = (k.create_array)(k.c32_klass, total);
    if arr.is_null() {
        log_i("[BADGE] Color32[] alloc failed");
        return ptr::null_mut();
    }
    // Il2CppArray: obj(16) + bounds(8) + max_length(8) = 32, elements follow.
    let dst = (arr as *mut u8).add(32);
    let px = make_pixels(num);
    ptr::copy_nonoverlapping(px.as_ptr(), dst, px.len());

    let setp: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(k.m_set_pixels);
    setp(tex, arr);
    let apply: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(k.m_tex_apply);
    apply(tex);

    if k.m_get_pixels32 != 0 {
        let gp: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
            std::mem::transmute(k.m_get_pixels32);
        let back = gp(tex);
        if back.is_null() {
            log_i("[BADGE] readback: GetPixels32 -> null");
        } else {
            let n = ptr::read_unaligned((back as *const u8).add(24) as *const usize);
            let px = (back as *const u8).add(32);
            let b = [*px, *px.add(1), *px.add(2), *px.add(3)];
            let mid = 32 + (TEX_SIZE as usize / 2) * (TEX_SIZE as usize) * 4;
            let m = [
                *px.add(mid),
                *px.add(mid + 1),
                *px.add(mid + 2),
                *px.add(mid + 3),
            ];
            log_i(&format!(
                "[BADGE] readback len={} corner={:?} mid={:?}",
                n, b, m
            ));
        }
    }

    log_i(&format!(
        "[BADGE] texture {}x{} num={} ready {:p}",
        TEX_SIZE, TEX_SIZE, num, tex
    ));
    tex
}

/// Create one badge GameObject parented under `host` (any Component).
pub unsafe fn attach(host: *mut c_void, tag: &str, num: u8) -> bool {
    attach_ex(host, tag, num, "", OFFSET_Y)
}

/// Attach a badge. When `icon_key` resolves to a registered PNG the badge uses
/// that texture and the icon's own size; otherwise it falls back to the
/// generated digit (debug aid).
pub unsafe fn attach_ex(
    host: *mut c_void,
    tag: &str,
    num: u8,
    icon_key: &str,
    offset_y: f32,
) -> bool {
    let Some(k) = KIT.get() else {
        return false;
    };
    if host.is_null() {
        return false;
    }
    if already_hosted(host, tag) {
        return false;
    }
    if !budget_left() {
        return false;
    }
    if !is_alive(k, host) {
        log_i("[BADGE] host already destroyed -> skip");
        return false;
    }
    if k.m_get_active != 0 && a_go_addr(k) != 0 {
        let get_go: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
            std::mem::transmute(a_go_addr(k));
        let go = get_go(host);
        if !go.is_null() {
            let act: unsafe extern "C" fn(*mut c_void) -> bool =
                std::mem::transmute(k.m_get_active);
            if !act(go) {
                return false;
            }
        }
    }
    let icon = if icon_key.is_empty() {
        ptr::null_mut()
    } else {
        icon_texture(k, icon_key)
    };
    let tex = if !icon.is_null() {
        icon
    } else {
        texture_for(k, num)
    };
    if tex.is_null() {
        return false;
    }

    // 1. GameObject - validate that the ctor really produced a native object.
    let go = (k.object_new)(k.go_klass);
    if go.is_null() {
        log_i("[BADGE] GameObject alloc failed");
        return false;
    }
    (k.object_init)(go);
    if !is_alive(k, go) {
        log_i("[BADGE] GameObject() ctor produced no native object (stripped?) -> abort");
        return false;
    }
    // Without this the node stays "New Game Object" and no later pass can tell
    // it apart from the button's own children.
    set_game_object_name(k, go, tag);

    // 2. RectTransform, then RawImage.
    let add = |klass: *mut c_void| -> *mut c_void {
        let g: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(k.m_add_component);
        let t = (k.class_get_type)(klass);
        if t.is_null() {
            return ptr::null_mut();
        }
        let sys_type = (k.type_get_object)(t);
        if sys_type.is_null() {
            return ptr::null_mut();
        }
        g(go, sys_type)
    };

    let rt = add(k.rt_klass);
    if !is_alive(k, rt) {
        log_i("[BADGE] AddComponent(RectTransform) failed -> abort");
        return false;
    }
    let raw = add(k.raw_klass);
    if !is_alive(k, raw) {
        log_i("[BADGE] AddComponent(RawImage) failed -> abort");
        return false;
    }

    // 3. texture + no raycast: use the managed setters when available so the
    // graphic is marked dirty the way the game expects.
    if k.m_set_texture != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(k.m_set_texture);
        f(raw, tex);
    } else if k.off_raw_texture > 0 {
        ptr::write_unaligned(
            (raw as *mut u8).add(k.off_raw_texture as usize) as *mut *mut c_void,
            tex,
        );
    }
    if k.m_set_raycast != 0 {
        let f: unsafe extern "C" fn(*mut c_void, bool) = std::mem::transmute(k.m_set_raycast);
        f(raw, false);
    } else if k.off_raycast > 0 {
        *((raw as *mut u8).add(k.off_raycast as usize) as *mut u8) = 0;
    }

    // 3a-mask. Opt out of any Mask on the button: a masked child is clipped to the
    // button's own silhouette, which made the badge look "covered".
    if k.m_set_maskable != 0 {
        let f: unsafe extern "C" fn(*mut c_void, bool) = std::mem::transmute(k.m_set_maskable);
        f(raw, false);
    }

    // 3a. UV rect (0,0,1,1): a default rect of zero size collapses every UV to
    // one texel, which is how a textured RawImage can still come out flat.
    if k.off_uv_rect > 0 {
        let p = (raw as *mut u8).add(k.off_uv_rect as usize) as *mut f32;
        ptr::write_unaligned(p, 0.0);
        ptr::write_unaligned(p.add(1), 0.0);
        ptr::write_unaligned(p.add(2), 1.0);
        ptr::write_unaligned(p.add(3), 1.0);
    }

    // 3b. Tint must stay white: the colour lives in the texture, and a red tint
    // multiplied the green badge into near-black.
    if k.off_graphic_color > 0 {
        let p = (raw as *mut u8).add(k.off_graphic_color as usize) as *mut f32;
        ptr::write_unaligned(p, 1.0);
        ptr::write_unaligned(p.add(1), 1.0);
        ptr::write_unaligned(p.add(2), 1.0);
        ptr::write_unaligned(p.add(3), 1.0);
    }

    // 4. parent it under the button.
    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(k.m_get_transform);
    let host_tr = get_tr(host);
    let my_tr = get_tr(rt);
    if host_tr.is_null() || my_tr.is_null() {
        log_i("[BADGE] transform lookup failed -> abort");
        return false;
    }
    let set_parent: unsafe extern "C" fn(*mut c_void, *mut c_void, bool) =
        std::mem::transmute(k.m_set_parent);
    let get_parent: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(k.m_get_parent);
    // Step 1: become a child of the button so the anchors give the exact spot.
    let _ = get_parent;
    set_parent(my_tr, host_tr, false);
    if k.m_last_sibling != 0 {
        let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(k.m_last_sibling);
        f(my_tr);
    }

    // 5. geometry: centred on the button, shifted by OFFSET_Y so the button's own
    //    artwork cannot cover it. anchors (0.5,0.5) means sizeDelta IS the size.
    if k.m_set_anchor_min != 0 && k.m_set_anchor_max != 0 {
        let half = V2 { x: 0.5, y: 0.5 };
        let a: unsafe extern "C" fn(*mut c_void, V2) = std::mem::transmute(k.m_set_anchor_min);
        a(rt, half);
        let b: unsafe extern "C" fn(*mut c_void, V2) = std::mem::transmute(k.m_set_anchor_max);
        b(rt, half);
        if k.m_set_pivot != 0 {
            let p: unsafe extern "C" fn(*mut c_void, V2) = std::mem::transmute(k.m_set_pivot);
            p(rt, half);
        }
        if k.m_set_size != 0 {
            let sz: unsafe extern "C" fn(*mut c_void, V2) = std::mem::transmute(k.m_set_size);
            let (w, h) = if icon.is_null() {
                (BADGE_SIZE, BADGE_SIZE)
            } else {
                icon_size(icon_key)
            };
            sz(rt, V2 { x: w, y: h });
        }
        if k.m_set_anchored != 0 {
            let an: unsafe extern "C" fn(*mut c_void, V2) = std::mem::transmute(k.m_set_anchored);
            an(rt, V2 { x: 0.0, y: offset_y });
        }
    } else {
        log_i("[BADGE] RectTransform setters missing -> badge may be invisible");
    }

    (*st()).count += 1;
    remember_host(host, tag);
    // Deliberately NOT remember_badge(): cached Il2CppObject pointers get
    // collected on scene teardown and calling into them crashes at the race
    // screen. Every pointer sync_badges uses is resolved fresh from live
    // buttons instead.
    log_i(&format!(
        "[BADGE] attached #{} num={} tag={} go={:p} raw={:p} host={:p}",
        (*st()).count,
        num,
        tag,
        go,
        raw,
        host
    ));
    true
}

/// World-space centre X of a host. The absolute value is unitless, but the
/// *ordering* is what we need to sort the five buttons left-to-right.
pub unsafe fn world_cx(host: *mut c_void) -> Option<f32> {
    let k = KIT.get()?;
    if !is_alive(k, host) {
        return None;
    }
    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(k.m_get_transform);
    let rt = get_tr(host);
    let (min, size) = corners_rect(k, rt)?;
    if size.x <= 0.0 {
        return None;
    }
    Some(min.x + size.x / 2.0)
}

/// Horizontal anchored position of a host, used to order the five training
/// buttons left-to-right instead of trusting enumeration order.
pub unsafe fn anchor_x(host: *mut c_void) -> Option<f32> {
    let k = KIT.get()?;
    if k.m_get_anchored == 0 || !is_alive(k, host) {
        return None;
    }
    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(k.m_get_transform);
    let rt = get_tr(host);
    if !is_alive(k, rt) {
        return None;
    }
    let g: unsafe extern "C" fn(*mut c_void) -> V2 = std::mem::transmute(k.m_get_anchored);
    let v = g(rt);
    if v.x.is_finite() {
        Some(v.x)
    } else {
        None
    }
}

/// Show the badge of `slot` (1..5) and hide the other four. Only field writes, so
/// no managed call and no canvas rebuild ordering involved.


/// Read an Il2CppString (UTF-16) into a Rust String.
unsafe fn managed_string(k: &Kit, s: *mut c_void) -> String {
    if s.is_null() || k.m_str_len == 0 || k.m_str_chars == 0 {
        return String::new();
    }
    let len: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(k.m_str_len);
    let chars: unsafe extern "C" fn(*mut c_void) -> *const u16 = std::mem::transmute(k.m_str_chars);
    let n = len(s);
    if n <= 0 {
        return String::new();
    }
    let p = chars(s);
    if p.is_null() {
        return String::new();
    }
    let mut out = String::with_capacity(n as usize);
    for i in 0..n as usize {
        out.push(char::from_u32(*p.add(i) as u32).unwrap_or('?'));
    }
    out
}

unsafe fn game_object_name(k: &Kit, go: *mut c_void) -> String {
    if k.m_get_name == 0 {
        return String::new();
    }
    let f: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(k.m_get_name);
    managed_string(k, f(go))
}

unsafe fn set_game_object_name(k: &Kit, go: *mut c_void, name: &str) {
    if k.m_set_name == 0 || k.m_string_new == 0 {
        return;
    }
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    let mk: unsafe extern "C" fn(*const c_char) -> *mut c_void = std::mem::transmute(k.m_string_new);
    let managed = mk(bytes.as_ptr() as *const c_char);
    if managed.is_null() {
        return;
    }
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(k.m_set_name);
    f(go, managed);
}

/// Texture currently shown by a RawImage (m_Texture, direct field read).
unsafe fn raw_texture(k: &Kit, raw: *mut c_void) -> *mut c_void {
    if k.off_raw_texture <= 0 || !is_alive(k, raw) {
        return ptr::null_mut();
    }
    ptr::read_unaligned((raw as *const u8).add(k.off_raw_texture as usize) as *const *mut c_void)
}

/// Keep exactly one badge on screen: the one on `target_slot` carrying `icon_key`.
///
/// SAFETY: every pointer used here is resolved FRESH from the live buttons on
/// this call (FindObjectsOfType -> Transform.GetChild -> Component.get_gameObject).
/// We never call into a cached managed wrapper: after a scene teardown those
/// wrappers can be collected by the GC, and calling into a collected object
/// crashes the process (that is exactly what happened at the race screen).
/// Badges are recognised by their GameObject name, which attach_ex() now sets.
pub unsafe fn sync_badges(buttons: &[*mut c_void], target_slot: u8, icon_key: &str) {
    let Some(k) = KIT.get() else { return };
    if k.m_get_child_count == 0
        || k.m_get_child == 0
        || k.m_get_go == 0
        || k.m_set_active == 0
        || k.m_get_transform == 0
    {
        return;
    }
    // An empty icon_key means "no recommendation right now". We still walk
    // the live buttons below to hide every badge, but we never load a texture
    // and never attach a new one.
    let show = !icon_key.is_empty();
    let want_name = format!("TB_Badge_{icon_key}");
    let mut icon = if show {
        icon_texture(k, icon_key)
    } else {
        ptr::null_mut()
    };
    if show && !icon.is_null() && !is_alive(k, icon) {
        log_i("[BADGE] icon texture was unloaded -> rebuilding");
        ICON_TEX.clear();
        icon = icon_texture(k, icon_key);
    }
    // A dead texture (or the empty "no hint" state) must still hide every
    // badge - otherwise the old recommendation would stay on screen.
    let icon_ok = show && !icon.is_null() && is_alive(k, icon);

    let sig = icon_key
        .as_bytes()
        .iter()
        .fold(target_slot as u64, |a, b| a.wrapping_mul(131).wrapping_add(*b as u64));
    if LAST_SYNC_SIG != sig {
        log_i(&format!(
            "[BADGE] sync -> slot={} icon={} (live scan)",
            target_slot,
            if show { icon_key } else { "(none)" }
        ));
        LAST_SYNC_SIG = sig;
    }

    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(k.m_get_transform);
    let cc: unsafe extern "C" fn(*mut c_void) -> i32 =
        std::mem::transmute(k.m_get_child_count);
    let gc: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void =
        std::mem::transmute(k.m_get_child);
    let get_go: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(k.m_get_go);
    let set_act: unsafe extern "C" fn(*mut c_void, bool) =
        std::mem::transmute(k.m_set_active);

    for (i, btn) in buttons.iter().enumerate() {
        let slot = (i + 1) as u8;
        if btn.is_null() || !is_alive(k, *btn) {
            continue;
        }
        let tr = get_tr(*btn);
        if tr.is_null() || !is_alive(k, tr) {
            continue;
        }
        let mut have_want = false;
        for ci in 0..cc(tr) {
            let child = gc(tr, ci);
            if child.is_null() || !is_alive(k, child) {
                continue;
            }
            let go = get_go(child);
            if go.is_null() || !is_alive(k, go) {
                continue;
            }
            let name = game_object_name(k, go);
            if !name.starts_with("TB_Badge") {
                continue;
            }
            if icon_ok && name == want_name && slot == target_slot {
                set_act(go, true);
                // Keep the badge on top using the transform we just fetched
                // from the live hierarchy - never a cached wrapper.
                if k.m_last_sibling != 0 {
                    let last: unsafe extern "C" fn(*mut c_void) =
                        std::mem::transmute(k.m_last_sibling);
                    last(child);
                }
                have_want = true;
            } else {
                // This is what removes the previous recommendation.
                set_act(go, false);
            }
        }
        if icon_ok && slot == target_slot && !have_want {
            attach_ex(*btn, &want_name, slot, icon_key, OFFSET_Y);
        }
    }
}

/// Attach a numbered marker to a button (diagnostic mode: every control on the
/// screen gets one, so the user can simply say "number N").
pub unsafe fn attach_number(host: *mut c_void, num: u8) -> bool {
    let tag = format!("TB_Num_{num}");
    // centred on the button, drawn last -> visible on top of the artwork.
    // `num` is the digit to draw, so it must reach the texture builder.
    attach_ex(host, &tag, num, "", 0.0)
}

pub unsafe fn count() -> u32 {
    (*st()).count
}

