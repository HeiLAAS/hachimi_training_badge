#![allow(static_mut_refs)]

// Probe v4 - sampling is driven by a hook, so our code runs on the game's own
// calling thread (the Unity main thread) instead of a thread we fabricate.
//
// WHY v3 CRASHED (evidence, 19:44):
//   worker thread -> GetCurrentViewController() -> 0xe06d7363 (MSVC C++ throw)
//   Player.log stack: hachimi_training_badge x4 -> KERNEL32 BaseThreadInitThunk
//   A background thread called managed game code; IL2CPP raised an exception
//   with no handler on that thread -> the process was terminated.
//
// WHY NOT il2cpp_schedule_on_thread:
//   Measured twice in the live game. The posts landed on a ThreadPool thread
//   (TICKS froze at 31 right after the first real IL2CPP call, while main-thread
//   log lines kept coming). Its internals (GetMutableExecutionContext /
//   create_delegate / ctx.Post) are managed calls, so it is not a safe
//   cross-thread entry point either.
//
// v4 design - every IL2CPP call happens on the game's own thread:
//   1. init() (main thread) resolves umamusume.dll -> Gallop.SceneManager and
//      locates the non-generic GetCurrentViewController MethodInfo.
//   2. We install a hook on that method through Hachimi's interceptor
//      (plugin_api.rs:805 interceptor_hook -> Interceptor::hook,
//       core/interceptor.rs:35-49). Hachimi itself only hooks SceneManager
//      .ChangeView, and hook_map is keyed by *hook* address, so this method is
//      free.
//   3. Our hook forwards to the trampoline, then reads the returned view
//      controller and dumps its class / methods / fields. Being on the game's
//      own thread at its own call site, this is exactly as safe as game code -
//      no thread attach, no runtime_invoke, no SynchronizationContext.
//   4. The hook is read-only and de-duplicates by class pointer, so after the
//      first dump the per-call cost is one pointer compare.
//
// Layout evidence (Hachimi-Edge/src/il2cpp/types.rs):
//   Il2CppObject: union { klass, vtable } @0            (:1984-1993)
//   Il2CppClass : image@0 gc_desc@8 name@16 namespaze@24 (:1436-1440)
//   MethodInfo  : methodPointer@0 .. name@24             (:1325-1333)
//   FieldInfo   : name@0 type_@8 parent@16 offset@24     (:1195-1201)
//
// Field values are only read for reference-typed fields, and only when
// il2cpp_field_get_type + il2cpp_type_get_type are both available, so a value
// type (int/float/struct) is never misinterpreted as a pointer.

use crate::{ai, badge, log_i, try_resolve, GetApiFn};
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

#[repr(C)]
struct MethodInfoHead {
    method_pointer: usize,
    virtual_method_pointer: usize,
    invoker: usize,
    name: *const c_char,
    klass: *mut c_void,
    return_type: *const c_void,
    parameters: *const c_void,
    anon1: usize,
    anon2: usize,
    token: u32,
    flags: u16,
    iflags: u16,
    slot: u16,
    parameters_count: u8,
    bitfield: u8, // types.rs:1358 - is_generic is bit0 of this byte (offset 83)
}
#[repr(C)]
struct FieldInfo {
    name: *const c_char,
    type_: *const c_void,
    parent: *mut c_void,
    offset: i32,
    token: u32,
}
#[repr(C)]
struct ClassHead {
    image: *const c_void,
    gc_desc: *mut c_void,
    name: *const c_char,
    namespaze: *const c_char,
}

type PresentCallback = unsafe extern "C" fn(*mut c_void, *mut c_void);
type ResolveSymbolFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type GetImageFn = unsafe extern "C" fn(*const c_char) -> *const c_void;
type GetClassFn = unsafe extern "C" fn(*const c_void, *const c_char, *const c_char) -> *mut c_void;
type GetMethodsFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> *const MethodInfoHead;
type GetFieldsFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> *const FieldInfo;
type ObjGetClassFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FieldGetTypeFn = unsafe extern "C" fn(*mut FieldInfo) -> *const c_void;
type TypeGetTypeFn = unsafe extern "C" fn(*const c_void) -> i32;
type ClassGetParentFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type ClassFromTypeFn = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type GetMethodAddrFn = unsafe extern "C" fn(*mut c_void, *const c_char, i32) -> *mut c_void;
type StrLenFn = unsafe extern "C" fn(*mut c_void) -> i32;
type StrCharsFn = unsafe extern "C" fn(*mut c_void) -> *mut u16;
type ObjGetNameFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type GetFieldByNameFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut FieldInfo;
type GetMethodOverloadAddrFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize) -> *mut c_void;
type DomainGetFn = unsafe extern "C" fn() -> *mut c_void;
type DomainAssembliesFn = unsafe extern "C" fn(*const c_void, *mut usize) -> *mut *const c_void;
type AssemblyGetImageFn = unsafe extern "C" fn(*const c_void) -> *const c_void;
type ImageNameFn = unsafe extern "C" fn(*const c_void) -> *const c_char;
type ImageClassCountFn = unsafe extern "C" fn(*const c_void) -> usize;
type ImageGetClassFn = unsafe extern "C" fn(*const c_void, usize) -> *const c_void;
type RegisterPresentFn = unsafe extern "C" fn(Option<PresentCallback>, *mut c_void) -> bool;

// Hachimi interceptor surface (plugin_api.rs:802-813).
type HachimiInstanceFn = unsafe extern "C" fn() -> *const c_void;
type GetInterceptorFn = unsafe extern "C" fn(*const c_void) -> *const c_void;
type InterceptorHookFn =
    unsafe extern "C" fn(*const c_void, *mut c_void, *mut c_void) -> *mut c_void;

// GetCurrentViewController(this) -> view controller
type GcvFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;

struct Api {
    get_methods: GetMethodsFn,
    get_fields: Option<GetFieldsFn>,
    obj_class: Option<ObjGetClassFn>,
    field_type: Option<FieldGetTypeFn>,
    type_kind: Option<TypeGetTypeFn>,
    class_parent: Option<ClassGetParentFn>,
    class_from_type: Option<ClassFromTypeFn>,
    str_len: Option<StrLenFn>,
    str_chars: Option<StrCharsFn>,
    // Resolved at init from UnityEngine.CoreModule.dll. Stored as usize so the
    // Api struct stays Sync.
    component_klass: usize,
    go_addr: usize,        // Component.get_gameObject()
    name_addr: usize,      // Object.get_name()
    get_parent_addr: usize, // Transform.get_parent()
    child_count_addr: usize, // Transform.get_childCount()
    get_child_addr: usize,  // Transform.GetChild(int)
    get_transform_addr: usize, // Component.get_transform()
    cached_ptr_off: i32,    // UnityEngine.Object.m_CachedPtr
    find_objects_addr: usize, // UnityEngine.Object.FindObjectsOfType(Type)
    button_klass: usize,    // UnityEngine.UI.Button
    class_get_type_addr: usize,
    type_get_object_addr: usize,
    get_component_addr: usize, // Component.GetComponent(Type)  (unresolved on this build)
    get_component_str_addr: usize, // Component.GetComponent(string)
    string_new_addr: usize,        // il2cpp_string_new
    mask_klass: usize,         // UnityEngine.UI.Mask
    rectmask_klass: usize,     // UnityEngine.UI.RectMask2D
    sibling_index_addr: usize, // Transform.GetSiblingIndex()
}

/// Type layouts we need before we can touch the UI. All of these are pure
/// metadata reads (no instance), so they are safe to run during init().
/// (assembly, full type name)
const NAMED_TYPES: &[(&str, &str)] = &[
    ("umamusume.dll", "Gallop.TrainingViewController"),
    ("umamusume.dll", "Gallop.TrainingButton"),
    ("umamusume.dll", "Gallop.TextCommon"),
    ("umamusume.dll", "Gallop.RawImageCommon"),
    ("umamusume.dll", "Gallop.ViewBase"),
    ("umamusume.dll", "Gallop.GameViewControllerBase"),
    ("umamusume.dll", "Gallop.HubViewControllerBase"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.Object"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.Component"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.Behaviour"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.GameObject"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.Transform"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.RectTransform"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.Sprite"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.Texture"),
    ("UnityEngine.CoreModule.dll", "UnityEngine.Texture2D"),
    ("UnityEngine.ImageConversionModule.dll", "UnityEngine.ImageConversion"),
    ("UnityEngine.UI.dll", "UnityEngine.UI.Graphic"),
    ("UnityEngine.UI.dll", "UnityEngine.UI.MaskableGraphic"),
    ("UnityEngine.UI.dll", "UnityEngine.UI.Image"),
    ("UnityEngine.UI.dll", "UnityEngine.UI.RawImage"),
    ("UnityEngine.UIModule.dll", "UnityEngine.CanvasRenderer"),
];

static API: OnceLock<Api> = OnceLock::new();
static SEEN: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static LAST_KLASS: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static LINES: AtomicU32 = AtomicU32::new(0);
static MAX_SCENES: AtomicU32 = AtomicU32::new(12);
static DUMPED: AtomicU32 = AtomicU32::new(0);
static FRAMES: AtomicUsize = AtomicUsize::new(0);
static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
static STOPPED: AtomicBool = AtomicBool::new(false);
static ATTACH_KLASS: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static ATTACH_TRIES: AtomicU32 = AtomicU32::new(0);
static TC_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static TC_CALLS: AtomicU32 = AtomicU32::new(0);
static LAST_SCAN: AtomicU32 = AtomicU32::new(0);
static DEEP_LAST: AtomicU32 = AtomicU32::new(0);
static DEEP_SIG: AtomicU64 = AtomicU64::new(0);
static BTN_PASS: AtomicU32 = AtomicU32::new(0);
// Current AI state, published by the high-frequency poll in text_common_tick
// and consumed by the (rarer) button scan. 255 / "" makes the very first poll
// count as a state change so the badge is attached right away.
static CUR_TARGET_SLOT: AtomicU32 = AtomicU32::new(255);
static mut CUR_ICON_KEY: &'static str = "";
static AI_POLLS: AtomicU32 = AtomicU32::new(0);
static NUMBER_ALL: AtomicBool = AtomicBool::new(false);
static TRN_LOGGED: AtomicBool = AtomicBool::new(false);

/// Diagnostic switch: number every control on the screen instead of showing the
/// recommendation icon.
pub fn set_number_all(on: bool) {
    NUMBER_ALL.store(on, Ordering::Relaxed);
    log_i(&format!("[PROBE4] number_all={}", on));
}
static DIAG_N: AtomicU32 = AtomicU32::new(0);
static mut PREV_HOSTS: [usize; 8] = [0; 8];
static mut PREV_N: usize = 0;

const MAX_LINES: u32 = 12000;
const MAX_NAMES: i32 = 60;
const MAX_DEPTH: u32 = 2;

// Il2CppTypeEnum values that denote a reference (heap) type.
fn is_ref_type(t: i32) -> bool {
    matches!(t, 0x0e | 0x12 | 0x14 | 0x15 | 0x1c | 0x1d)
}

fn emit(s: String) {
    if LINES.fetch_add(1, Ordering::Relaxed) < MAX_LINES {
        log_i(&s);
    }
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return "?".to_string();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

unsafe fn class_key(klass: *mut c_void) -> String {
    if klass.is_null() {
        return "?".to_string();
    }
    let h = &*(klass as *const ClassHead);
    let ns = cstr(h.namespaze);
    let n = cstr(h.name);
    if ns.is_empty() { n } else { format!("{}.{}", ns, n) }
}

// Il2CppObject's first word is the klass pointer (types.rs:1984-1993).
unsafe fn obj_class_of(a: &Api, obj: *mut c_void) -> *mut c_void {
    if obj.is_null() {
        return ptr::null_mut();
    }
    let k = *(obj as *const *mut c_void);
    if !k.is_null() {
        return k;
    }
    match a.obj_class {
        Some(f) => f(obj),
        None => ptr::null_mut(),
    }
}

/// Read the object reference stored at `obj + off`.
/// unaligned: IL2CPP does not guarantee 8-byte alignment for every field slot.
unsafe fn read_ptr_slot(obj: *mut c_void, off: i32) -> *mut c_void {
    if obj.is_null() || off <= 0 {
        return ptr::null_mut();
    }
    ptr::read_unaligned((obj as *const u8).add(off as usize) as *const *mut c_void)
}

fn interesting(key: &str) -> bool {
    let l = key.to_ascii_lowercase();
    ["button", "training", "icon", "card", "status", "parts", "view", "menu"]
        .iter()
        .any(|w| l.contains(w))
}

/// True when `klass` is `ancestor` or derives from it. Used to decide whether it
/// is safe to call Component.get_gameObject on an object of that class.
unsafe fn is_subclass(a: &Api, klass: *mut c_void, ancestor: usize) -> bool {
    if ancestor == 0 || klass.is_null() {
        return false;
    }
    let Some(parent) = a.class_parent else { return false };
    let mut cur = klass;
    let mut d = 0;
    while !cur.is_null() && d < 16 {
        if cur as usize == ancestor {
            return true;
        }
        cur = parent(cur);
        d += 1;
    }
    false
}

/// Unity "fake null": a destroyed managed object keeps its address but its
/// m_CachedPtr becomes 0. Calling a native method on such an object is a hard
/// access violation inside UnityPlayer, which is what killed the process at
/// 12:24 on a screen transition. Every node is checked before we touch it.
unsafe fn alive(a: &Api, obj: *mut c_void) -> bool {
    if obj.is_null() || a.cached_ptr_off <= 0 {
        return false;
    }
    let p = ptr::read_unaligned((obj as *const u8).add(a.cached_ptr_off as usize) as *const usize);
    p != 0
}

unsafe fn read_string(a: &Api, s: *mut c_void) -> String {
    let (Some(l), Some(c)) = (a.str_len, a.str_chars) else {
        return "?".to_string();
    };
    if s.is_null() {
        return "<null>".to_string();
    }
    let n = l(s);
    if n <= 0 || n > 200 {
        return format!("<len {}>", n);
    }
    let p = c(s);
    if p.is_null() {
        return "?".to_string();
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n as usize))
}

/// Component.get_gameObject() -> Object.get_name(). Both are plain property
/// getters, and we are on the game's own thread, so this is as safe as the game
/// reading itself.
unsafe fn go_name_of(a: &Api, comp: *mut c_void) -> String {
    if a.go_addr == 0 || a.name_addr == 0 || comp.is_null() {
        return "?".to_string();
    }
    let f: ObjGetNameFn = std::mem::transmute(a.go_addr);
    let go = f(comp);
    if go.is_null() {
        return "<no-go>".to_string();
    }
    let g: ObjGetNameFn = std::mem::transmute(a.name_addr);
    read_string(a, g(go))
}

unsafe fn dump_methods(a: &Api, klass: *mut c_void) {
    let mut iter: *mut c_void = ptr::null_mut();
    let mut n: i32 = 0;
    loop {
        let m = (a.get_methods)(klass, &mut iter);
        if m.is_null() || n >= MAX_NAMES {
            break;
        }
        emit(format!("  [M] {}", cstr((*m).name)));
        n += 1;
    }
    emit(format!("  [M] count={}", n));
}

unsafe fn echo_go_name(a: &Api, pad: &str, fname: &str, off: i32, vkey: &str, obj: *mut c_void) {
    let go = go_name_of(a, obj);
    emit(format!(
        "{}[F] {} off={} -> {} go=\"{}\"",
        pad, fname, off, vkey, go
    ));
}

unsafe fn dump_fields(a: &Api, obj: *mut c_void, klass: *mut c_void, depth: u32, force: bool) {
    let Some(gf) = a.get_fields else {
        emit("  [F] il2cpp_class_get_fields unavailable".to_string());
        return;
    };
    let maxd = if force { 2 } else { MAX_DEPTH };
    let pad = "  ".repeat(depth as usize + 1);
    let mut iter: *mut c_void = ptr::null_mut();
    let mut n: i32 = 0;
    loop {
        let f = gf(klass, &mut iter);
        if f.is_null() || n >= MAX_NAMES {
            break;
        }
        let fname = cstr((*f).name);
        let off = (*f).offset;

        // Only walk the field slot when we can prove it holds a reference.
        let mut fval: *mut c_void = ptr::null_mut();
        let mut vk: *mut c_void = ptr::null_mut();
        let mut vkey = String::new();
        if off > 0 {
            let kind = match (a.field_type, a.type_kind) {
                (Some(ft), Some(tk)) => {
                    let ty = ft(f as *mut FieldInfo);
                    if ty.is_null() { -1 } else { tk(ty) }
                }
                _ => -1,
            };
            if is_ref_type(kind) {
                fval = read_ptr_slot(obj, off);
                if !fval.is_null() {
                    vk = obj_class_of(a, fval);
                    if !vk.is_null() {
                        vkey = class_key(vk);
                    }
                }
            }
        }

        if vk.is_null() {
            emit(format!("{}[F] {} off={}", pad, fname, off));
        } else if force && is_subclass(a, vk, a.component_klass) {
            // Unity component: report the GameObject name so the 5 training
            // buttons can be identified semantically instead of by guesswork.
            echo_go_name(a, &pad, &fname, off, &vkey, fval);
        } else {
            emit(format!("{}[F] {} off={} -> {}", pad, fname, off, vkey));
        }

        if !vk.is_null() && depth < maxd && (force || interesting(&vkey)) {
            dump_fields(a, fval, vk, depth + 1, force);
        }
        n += 1;
    }
    emit(format!("{}[F] count={}", pad, n));
}

/// Declared field list of a class (no instance involved).
unsafe fn dump_decl_fields(a: &Api, klass: *mut c_void, pad: &str, max: i32) {
    let Some(gf) = a.get_fields else {
        emit(format!("{}[D] il2cpp_class_get_fields unavailable", pad));
        return;
    };
    let mut iter: *mut c_void = ptr::null_mut();
    let mut n: i32 = 0;
    loop {
        let f = gf(klass, &mut iter);
        if f.is_null() || n >= max {
            break;
        }
        let tn = match (a.field_type, a.class_from_type) {
            (Some(ft), Some(cft)) => {
                let ty = ft(f as *mut FieldInfo);
                if ty.is_null() {
                    "?".to_string()
                } else {
                    let k = cft(ty);
                    if k.is_null() {
                        format!("(type 0x{:x})", ty as usize)
                    } else {
                        class_key(k)
                    }
                }
            }
            _ => "?".to_string(),
        };
        emit(format!("{}[D] {} off={} : {}", pad, cstr((*f).name), (*f).offset, tn));
        n += 1;
    }
    emit(format!("{}[D] count={}", pad, n));
}

/// Inheritance chain + declared fields of one class.
unsafe fn dump_chain(a: &Api, k: *mut c_void) {
    let mut cur = k;
    let mut d = 0;
    while !cur.is_null() && d < 10 {
        emit(format!("  [C] {} klass={:p}", class_key(cur), cur));
        dump_decl_fields(a, cur, "    ", 80);
        cur = match a.class_parent {
            Some(f) => f(cur),
            None => ptr::null_mut(),
        };
        d += 1;
    }
}

fn split_ns(full: &str) -> (&str, &str) {
    match full.rfind('.') {
        Some(i) => (&full[..i], &full[i + 1..]),
        None => ("", full),
    }
}

/// Print the full inheritance chain + declared fields of every type we will
/// need. Pure metadata: no instance, no static init, so this is safe to call
/// from init() on the plugin-load thread.
unsafe fn dump_named_types(a: &Api, get_image: GetImageFn, get_class: GetClassFn) {
    log_i("[PROBE4] ===== type layouts =====");
    for (asm, full) in NAMED_TYPES {
        let (ns, name) = split_ns(full);
        let (Ok(c_asm), Ok(c_ns), Ok(c_name)) = (
            CString::new(*asm),
            CString::new(ns),
            CString::new(name),
        ) else {
            continue;
        };
        let img = get_image(c_asm.as_ptr());
        if img.is_null() {
            emit(format!("[TYPE] {} -> assembly image NULL", asm));
            continue;
        }
        let k = get_class(img, c_ns.as_ptr(), c_name.as_ptr());
        if k.is_null() {
            emit(format!("[TYPE] {} -> NOT FOUND", full));
            continue;
        }
        emit(format!("[TYPE] {} -> {} klass={:p}", full, class_key(k), k));
        dump_chain(a, k);
    }
    log_i("[PROBE4] ===== end type layouts =====");
}

/// Scan every loaded assembly for classes whose full name ends with one of
/// `want`. Some game types are not in `umamusume.dll` (TrainingViewController
/// and TrainingButton are two of them), so a fixed assembly guess is not enough.
/// Walk every class of every loaded assembly. Returns (assemblies, classes) or
/// None when the domain/image symbols are unavailable.
unsafe fn walk_classes(
    resolve: ResolveSymbolFn,
    mut f: impl FnMut(*mut c_void, &str, &str),
) -> Option<(usize, usize)> {
    let (Some(dom_get), Some(dom_asms), Some(asm_img), Some(img_name), Some(img_cnt), Some(img_cls)) = (
        symbol::<DomainGetFn>(resolve, "il2cpp_domain_get"),
        symbol::<DomainAssembliesFn>(resolve, "il2cpp_domain_get_assemblies"),
        symbol::<AssemblyGetImageFn>(resolve, "il2cpp_assembly_get_image"),
        symbol::<ImageNameFn>(resolve, "il2cpp_image_get_name"),
        symbol::<ImageClassCountFn>(resolve, "il2cpp_image_get_class_count"),
        symbol::<ImageGetClassFn>(resolve, "il2cpp_image_get_class"),
    ) else {
        log_i("[PROBE4] class scan unavailable (missing domain/image symbols)");
        return None;
    };
    let domain = dom_get();
    if domain.is_null() {
        log_i("[PROBE4] il2cpp_domain_get() = NULL -> no class scan");
        return None;
    }
    let mut n: usize = 0;
    let arr = dom_asms(domain, &mut n);
    if arr.is_null() || n == 0 || n > 512 {
        log_i(&format!("[PROBE4] assemblies = {} (unusable)", n));
        return None;
    }
    let asms = std::slice::from_raw_parts(arr, n);
    let mut total: usize = 0;
    for asm in asms {
        let img = asm_img(*asm);
        if img.is_null() {
            continue;
        }
        let iname = cstr(img_name(img));
        let c = img_cnt(img);
        total += c;
        if c == 0 || c > 40000 {
            continue;
        }
        for k in 0..c {
            let cl = img_cls(img, k) as *mut c_void;
            if cl.is_null() {
                continue;
            }
            let key = class_key(cl);
            f(cl, &key, &iname);
        }
    }
    Some((n, total))
}

unsafe fn find_classes_global(
    a: &Api,
    resolve: ResolveSymbolFn,
    want: &[&str],
) -> Vec<*mut c_void> {
    let mut hits: Vec<*mut c_void> = Vec::new();
    let Some((nasms, total)) = walk_classes(resolve, |cl, key, iname| {
        for w in want {
            if key.ends_with(w) {
                emit(format!("[HIT] {} @ {} klass={:p}", key, iname, cl));
                hits.push(cl);
                break;
            }
        }
    }) else {
        return hits;
    };
    log_i(&format!(
        "[PROBE4] scanned assemblies={} classes={} hits={}",
        nasms,
        total,
        hits.len()
    ));
    for cl in &hits {
        emit(format!("[CHAIN] {}", class_key(*cl)));
        dump_chain(a, *cl);
    }
    hits
}

/// Names are only half the story: the training screen's controller and its five
/// buttons are not called *Training*(ViewController|Button) in this build, so a
/// suffix match misses them. Log every class whose name merely *contains* one of
/// the patterns, and dump the chain for anything button-like.
unsafe fn list_classes_containing(
    a: &Api,
    resolve: ResolveSymbolFn,
    patterns: &[&str],
) {
    let mut shown = 0u32;
    let mut chained = 0u32;
    let Some((_n, _t)) = walk_classes(resolve, |cl, key, iname| {
        let lower = key.to_ascii_lowercase();
        if !patterns.iter().any(|p| lower.contains(&p.to_ascii_lowercase())) {
            return;
        }
        if shown >= 250 {
            return;
        }
        shown += 1;
        emit(format!("[SUB] {} @ {}", key, iname));
        let is_button = lower.contains("button") || lower.contains("menu") || lower.contains("view");
        if is_button && chained < 14 {
            chained += 1;
            emit(format!("[CHAIN] {}", key));
            dump_chain(a, cl);
        }
    }) else {
        return;
    };
    log_i(&format!("[PROBE4] substring scan done: {} classes listed", shown));
}

/// Find the 5 training buttons by shape, not by name: any reference field on the
/// view controller (or its bases) whose runtime class is a TrainingButton.
unsafe fn attach_buttons(a: &Api, vc: *mut c_void, klass: *mut c_void) {
    let Some(gf) = a.get_fields else { return };
    let mut cur = klass;
    let mut d = 0;
    let mut n = 0u32;
    while !cur.is_null() && d < 10 && n < 5 {
        let mut iter: *mut c_void = ptr::null_mut();
        let mut i = 0;
        loop {
            let f = gf(cur, &mut iter);
            if f.is_null() || i >= 200 {
                break;
            }
            i += 1;
            let off = (*f).offset;
            if off <= 0 {
                continue;
            }
            let kind = match (a.field_type, a.type_kind) {
                (Some(ft), Some(tk)) => {
                    let ty = ft(f as *mut FieldInfo);
                    if ty.is_null() { -1 } else { tk(ty) }
                }
                _ => -1,
            };
            if !is_ref_type(kind) {
                continue;
            }
            let val = read_ptr_slot(vc, off);
            if val.is_null() {
                continue;
            }
            let vk = obj_class_of(a, val);
            if vk.is_null() {
                continue;
            }
            let vkey = class_key(vk);
            if vkey.ends_with("TrainingButton") {
                let fname = cstr((*f).name);
                if badge::attach(val, &fname, 0) {
                    n += 1;
                }
            }
        }
        cur = match a.class_parent {
            Some(f) => f(cur),
            None => ptr::null_mut(),
        };
        d += 1;
    }
    log_i(&format!("[BADGE] scene attach done: {} badge(s)", n));
}


/// Node -> GameObject -> name, with the liveness guard on the way in.
unsafe fn node_name(a: &Api, tr: *mut c_void) -> Option<String> {
    if a.go_addr == 0 || a.name_addr == 0 {
        return None;
    }
    let get_go: ObjGetNameFn = std::mem::transmute(a.go_addr);
    let go = get_go(tr);
    if !alive(a, go) {
        return None;
    }
    let get_name: ObjGetNameFn = std::mem::transmute(a.name_addr);
    Some(read_string(a, get_name(go)))
}

unsafe fn children_of(a: &Api, tr: *mut c_void) -> i32 {
    if a.child_count_addr == 0 {
        return 0;
    }
    let f: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(a.child_count_addr);
    f(tr).max(0)
}

/// Collect every deep node (depth >= 6) under `tr`. The five training buttons
/// live well below the visible panels, so a depth-limited "interesting" filter
/// misses them; this captures the whole deep UI instead.
unsafe fn collect_deep(a: &Api, tr: *mut c_void, depth: u32, out: &mut Vec<String>, total: &mut u32) {
    if !alive(a, tr) || depth > 24 || *total > 900 {
        return;
    }
    *total += 1;
    if depth >= 6 {
        if let Some(n) = node_name(a, tr) {
            out.push(format!("{}|{}", depth, n));
        }
    }
    if a.get_child_addr == 0 {
        return;
    }
    let get_child: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void =
        std::mem::transmute(a.get_child_addr);
    let cc = children_of(a, tr);
    for i in 0..cc.min(80) {
        collect_deep(a, get_child(tr, i), depth + 1, out, total);
    }
}

unsafe fn root_of(a: &Api, this: *mut c_void) -> *mut c_void {
    if a.get_transform_addr == 0 || a.get_parent_addr == 0 || !alive(a, this) {
        return ptr::null_mut();
    }
    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_transform_addr);
    let get_parent: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_parent_addr);
    let mut tr = get_tr(this);
    if !alive(a, tr) {
        return ptr::null_mut();
    }
    for _ in 0..24 {
        let p = get_parent(tr);
        if !alive(a, p) {
            break;
        }
        tr = p;
    }
    tr
}

/// Dump the deep UI, but only when it actually changed: that way each screen
/// change (home -> training) yields exactly one snapshot without flooding.
unsafe fn deep_dump_if_changed(a: &Api, this: *mut c_void, tag: u32) {
    let root = root_of(a, this);
    if !alive(a, root) {
        return;
    }
    let mut out: Vec<String> = Vec::new();
    let mut total = 0u32;
    collect_deep(a, root, 0, &mut out, &mut total);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for s in &out {
        for b in s.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    if h == DEEP_SIG.load(Ordering::Relaxed) {
        return;
    }
    DEEP_SIG.store(h, Ordering::Relaxed);
    emit(format!(
        "[DEEP #{}] total={} deep={} sig={:x}",
        tag,
        total,
        out.len(),
        h
    ));
    for s in out.iter().take(500) {
        match s.split_once('|') {
            Some((d, n)) => emit(format!("[DEEP] d{} {}", d, n)),
            None => emit(format!("[DEEP] {}", s)),
        }
    }
}


/// Enumerate every live UnityEngine.UI.Button and log its node path.
/// This is name- and depth-independent: FindObjectsOfType returns the actual
/// instances, so the 5 training buttons cannot hide from it.
unsafe fn list_buttons(
    a: &Api,
    tag: u32,
    attach_rule: bool,
    target_slot: u8,
    icon_key: &str,
) {
    if a.find_objects_addr == 0
        || a.button_klass == 0
        || a.class_get_type_addr == 0
        || a.type_get_object_addr == 0
        || a.get_transform_addr == 0
        || a.get_parent_addr == 0
    {
        emit(format!("[BTN #{}] unavailable", tag));
        return;
    }
    let cgt: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.class_get_type_addr);
    let tgo: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.type_get_object_addr);
    let ty = tgo(cgt(a.button_klass as *mut c_void));
    if ty.is_null() {
        emit("[BTN] System.Type for Button is null".to_string());
        return;
    }
    let find: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.find_objects_addr);
    let arr = find(ty);
    if arr.is_null() {
        emit("[BTN] FindObjectsOfType returned null".to_string());
        return;
    }
    let count = ptr::read_unaligned((arr as *const u8).add(24) as *const usize);
    let data = (arr as *const u8).add(32) as *const *mut c_void;
    emit(format!("[BTN #{}] count={}", tag, count));

    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_transform_addr);
    let get_parent: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_parent_addr);

    let mut cur_hosts: Vec<usize> = Vec::new();
    let mut items: Vec<(*mut c_void, String)> = Vec::new();
    let mut all: Vec<(*mut c_void, String)> = Vec::new();
    let mut attached = 0u32;
    for i in 0..count.min(240) {
        let btn = ptr::read_unaligned(data.add(i));
        if !alive(a, btn) {
            continue;
        }
        let mut parts: Vec<String> = Vec::new();
        let mut tr = get_tr(btn);
        let mut d = 0;
        while alive(a, tr) && d < 8 {
            if let Some(n) = node_name(a, tr) {
                parts.push(n);
            }
            tr = get_parent(tr);
            d += 1;
        }
        parts.reverse();
        let path = parts.join("/");
        let low = path.to_ascii_lowercase();
        let hit = low.contains("trainingmenu") || low.contains("training_menu");
        // The five training items are the only buttons under ...TrainingFooterItem/
        let is_training_item = low.contains("trainingfooteritem");
        emit(format!(
            "[BTN] {}{}",
            if is_training_item { "* " } else if hit { ". " } else { "" },
            path
        ));
        if all.len() < 60 {
            all.push((btn, path.clone()));
        }
        if is_training_item {
            cur_hosts.push(btn as usize);
            items.push((btn, path.clone()));
            // Report what the button actually contains, using only the calls we
            // already trust (get_transform / get_childCount / GetChild / get_name).
            let btr = get_tr(btn);
            let bcc = children_of(a, btr);
            let mut cn: Vec<String> = Vec::new();
            if a.get_child_addr != 0 {
                let gc: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void =
                    std::mem::transmute(a.get_child_addr);
                for i in 0..bcc.min(10) {
                    let c = gc(btr, i);
                    cn.push(node_name(a, c).unwrap_or_else(|| "?".to_string()));
                }
            }
            emit(format!("[ITEM] children({}) = {}", bcc, cn.join(", ")));
        }
        // Diagnose the first five training items we ever see, whatever pass that
        // happens on. (Pass 1 often runs before the training screen exists.)


    }
    // stable == same host set as the previous pass and non-empty
    let stable = PREV_N == cur_hosts.len()
        && PREV_N > 0
        && (0..PREV_N).all(|i| PREV_HOSTS[i] == cur_hosts[i]);
    for i in 0..cur_hosts.len().min(8) {
        PREV_HOSTS[i] = cur_hosts[i];
    }
    PREV_N = cur_hosts.len().min(8);
    log_i(&format!(
        "[BTN] pass={} items={} stable={} total={}",
        tag,
        items.len(),
        stable,
        badge::count()
    ));
    // `target_slot` / `icon_key` are produced by the high-frequency AI poll in
    // text_common_tick; this scan only applies them to the live buttons.

    // Attach only once the footer has been stable for two consecutive passes:
    // attaching while the screen is still being rebuilt is what crashed the game
    // when a step transition happened.
    if attach_rule && stable {

        // Only the five training items; numbered 1..5 left-to-right using their
        // anchoredPosition.x, so the number does not depend on enumeration order.
        let mut ordered: Vec<(*mut c_void, String, f32)> = items
            .iter()
            .map(|(b, p)| {
                let x = badge::world_cx(*b).unwrap_or_else(|| badge::anchor_x(*b).unwrap_or(0.0));
                (*b, p.clone(), x)
            })
            .collect();
        // Log the training items in ENUMERATION order together with their world X.
        // The user's numbering gives the ground truth for that order:
        //   智力=#2, 根性=#3, 力量=#4, 耐力=#5, 速度=#6 (global enumeration index).
        // One-shot reference: the training items in ENUMERATION order with their
        // world X. Ground truth (user numbering): 智力=#2, 根性=#3, 力量=#4,
        // 耐力=#5, 速度=#6 -> enumeration runs right-to-left, and the world X
        // decreases from left to right, so ASCENDING X == screen order.
        if !TRN_LOGGED.swap(true, Ordering::Relaxed) {
            for (i, (_b, _p, x)) in ordered.iter().enumerate() {
                emit(format!(
                    "[TRN] enum={} wx={:.3} screen_left_to_right={}",
                    i,
                    x,
                    ["智力", "根性", "力量", "耐力", "速度"][i.min(4)]
                ));
            }
        }
        ordered.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        let _ = (&icon_key, target_slot);
        if NUMBER_ALL.load(Ordering::Relaxed) {
            // Diagnostic: number every control on the screen.
            for (i, (b, p)) in all.iter().enumerate() {
                let num = (i + 1).min(99) as u8;
                emit(format!("[NUM] {} = {}", num, p));
                badge::attach_number(*b, num);
            }
        } else {
            for (i, (_b, p, _x)) in ordered.iter().enumerate() {
                emit(format!("[NUM] {} = {}", i + 1, p));
            }
            let buttons: Vec<*mut c_void> = ordered.iter().map(|(b, _, _)| *b).collect();
            badge::sync_badges(&buttons, target_slot, icon_key);
        }
        attached = 1;
        log_i(&format!(
            "[BTN] attach done: {} (total {}), items={}",
            attached,
            badge::count(),
            items.len()
        ));
    }
}


/// One-shot diagnosis: what is actually covering our badge? Reported instead of
/// guessed - mask on the button (or an ancestor), the button's children, and the
/// sibling index we end up at.
unsafe fn diag_button(a: &Api, btn: *mut c_void, path: &str) {
    if a.get_transform_addr == 0 || a.get_parent_addr == 0 {
        emit("[DIAG] transform api missing".to_string());
        return;
    }
    let use_str = a.get_component_str_addr != 0 && a.string_new_addr != 0;
    let mask_ok = use_str;
    let getcs: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_component_str_addr);
    let strnew: unsafe extern "C" fn(*const c_char) -> *mut c_void =
        std::mem::transmute(a.string_new_addr);
    let mask_str = if use_str {
        let c = CString::new("UnityEngine.UI.Mask").unwrap();
        strnew(c.as_ptr())
    } else {
        ptr::null_mut()
    };
    let rectmask_str = if use_str {
        let c = CString::new("UnityEngine.UI.RectMask2D").unwrap();
        strnew(c.as_ptr())
    } else {
        ptr::null_mut()
    };
    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_transform_addr);
    let get_parent: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_parent_addr);

    emit(format!("[DIAG] ---- {} ----", path));
    if true {
        return;
    }

    // mask on the button itself or any ancestor within 8 levels
    let mut tr = get_tr(btn);
    let mut d = 0;
    while mask_ok && alive(a, tr) && d < 8 {
        for (label, ty) in [("Mask", mask_str), ("RectMask2D", rectmask_str)] {
            if ty.is_null() {
                continue;
            }
            let go = {
                let g: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
                    std::mem::transmute(a.go_addr);
                g(tr)
            };
            if !alive(a, go) {
                continue;
            }
            let comp = getcs(go, ty);
            if !comp.is_null() {
                let nm = node_name(a, tr).unwrap_or_else(|| "?".to_string());
                emit(format!("[DIAG] {} found at depth {} on '{}'", label, d, nm));
            }
        }
        tr = get_parent(tr);
        d += 1;
    }

    // what the button actually contains
    let host_tr = get_tr(btn);
    let cc = children_of(a, host_tr);
    let mut names: Vec<String> = Vec::new();
    if a.get_child_addr != 0 {
        let gc: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void =
            std::mem::transmute(a.get_child_addr);
        for i in 0..cc.min(12) {
            let c = gc(host_tr, i);
            names.push(node_name(a, c).unwrap_or_else(|| "?".to_string()));
        }
    }
    emit(format!("[DIAG] children({}) = {}", cc, names.join(", ")));

    if false && a.sibling_index_addr != 0 {
        let gsi: unsafe extern "C" fn(*mut c_void) -> i32 =
            std::mem::transmute(a.sibling_index_addr);
        emit(format!("[DIAG] host sibling index = {}", gsi(host_tr)));
        if a.get_child_addr != 0 {
            let gc: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void =
                std::mem::transmute(a.get_child_addr);
            for i in 0..cc.min(12) {
                let c = gc(host_tr, i);
                let nm = node_name(a, c).unwrap_or_else(|| "?".to_string());
                if nm.contains("Badge") || nm.contains("badge") || nm.contains("TB_") {
                    emit(format!("[DIAG] badge child at index {} '{}' of {}", i, nm, cc));
                }
            }
        }
    }
}

/// True when a node name is worth printing even deep in the tree.
fn name_is_interesting(n: &str) -> bool {
    let l = n.to_ascii_lowercase();
    [
        "train", "btn", "button", "speed", "stamina", "power", "guts", "wiz", "intel", "icon",
        "menu", "select", "status", "card",
    ]
    .iter()
    .any(|w| l.contains(w))
}

unsafe fn walk_tree(a: &Api, tr: *mut c_void, depth: u32, total: &mut u32) {
    if !alive(a, tr) || depth > 24 || *total > 900 {
        return;
    }
    *total += 1;

    let name = if a.go_addr == 0 || a.name_addr == 0 {
        "?".to_string()
    } else {
        let get_go: ObjGetNameFn = std::mem::transmute(a.go_addr);
        let go = get_go(tr);
        if go.is_null() {
            "<no-go>".to_string()
        } else {
            let get_name: ObjGetNameFn = std::mem::transmute(a.name_addr);
            read_string(a, get_name(go))
        }
    };
    let cc = if a.child_count_addr != 0 {
        let f: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(a.child_count_addr);
        f(tr).max(0)
    } else {
        0
    };
    let interesting = name_is_interesting(&name);
    if depth <= 3 || interesting {
        emit(format!(
            "[TREE] {}{} ch={}{}",
            "  ".repeat(depth as usize),
            name,
            cc,
            if interesting { " *" } else { "" }
        ));
    }
    if a.get_child_addr == 0 {
        return;
    }
    let get_child: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void =
        std::mem::transmute(a.get_child_addr);
    for i in 0..cc.min(80) {
        let c = get_child(tr, i);
        walk_tree(a, c, depth + 1, total);
    }
}

unsafe fn dump_tree_from(a: &Api, this: *mut c_void, tag: u32) {
    if a.get_transform_addr == 0 || a.get_parent_addr == 0 {
        log_i("[TREE] transform helpers missing -> skip");
        return;
    }
    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_transform_addr);
    let get_parent: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_parent_addr);
    if !alive(a, this) {
        return;
    }
    let mut tr = get_tr(this);
    if !alive(a, tr) {
        return;
    }
    for _ in 0..24 {
        let p = get_parent(tr);
        if !alive(a, p) {
            break;
        }
        tr = p;
    }
    let mut total = 0u32;
    log_i(&format!("[TREE #{}] root={:p}", tag, tr));
    walk_tree(a, tr, 0, &mut total);
    log_i(&format!("[TREE #{}] nodes={}", tag, total));
}

/// A training button node. Calibrated against the real tree (2026-11-08 20:24):
/// the button is `TrainingButton`, wrapped in `TrainingButtonRoot` /
/// `TrainingButtonTweenRoot`. Attaching to the wrappers too put three badges on
/// one button, so wrappers are excluded on purpose.
fn looks_like_training_button(n: &str) -> bool {
    let l = n.to_ascii_lowercase();
    l.contains("train")
        && (l.contains("button") || l.contains("btn"))
        && !l.contains("root")
        && !l.contains("collision")
        && !l.contains("effect")
}

unsafe fn walk_and_attach(a: &Api, tr: *mut c_void, depth: u32, total: &mut u32, hits: &mut u32) {
    if !alive(a, tr) || depth > 24 || *total > 900 {
        return;
    }
    *total += 1;

    let name = if a.go_addr == 0 || a.name_addr == 0 {
        String::new()
    } else {
        let get_go: ObjGetNameFn = std::mem::transmute(a.go_addr);
        let go = get_go(tr);
        if go.is_null() {
            String::new()
        } else {
            let get_name: ObjGetNameFn = std::mem::transmute(a.name_addr);
            read_string(a, get_name(go))
        }
    };
    if !name.is_empty() && looks_like_training_button(&name) {
        *hits += 1;
        if badge::attach(tr, &name, 0) {
            log_i(&format!("[BADGE] node match '{}' at depth {}", name, depth));
        }
    }

    let cc = if a.child_count_addr != 0 {
        let f: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(a.child_count_addr);
        f(tr).max(0)
    } else {
        0
    };
    if a.get_child_addr == 0 {
        return;
    }
    let get_child: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void =
        std::mem::transmute(a.get_child_addr);
    for i in 0..cc.min(80) {
        let c = get_child(tr, i);
        walk_and_attach(a, c, depth + 1, total, hits);
    }
}

unsafe fn scan_and_attach(a: &Api, this: *mut c_void) {
    if a.get_transform_addr == 0 || a.get_parent_addr == 0 || !badge::ready() {
        return;
    }
    let get_tr: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_transform_addr);
    let get_parent: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        std::mem::transmute(a.get_parent_addr);
    if !alive(a, this) {
        return;
    }
    let mut tr = get_tr(this);
    if !alive(a, tr) {
        return;
    }
    for _ in 0..24 {
        let p = get_parent(tr);
        if !alive(a, p) {
            break;
        }
        tr = p;
    }
    let mut total = 0u32;
    let mut hits = 0u32;
    walk_and_attach(a, tr, 0, &mut total, &mut hits);
    if hits > 0 {
        log_i(&format!(
            "[BADGE] scan: {} candidate node(s), {} badge(s) total",
            hits,
            badge::count()
        ));
    }
}


unsafe extern "C" fn text_common_tick(this: *mut c_void) {
    let t = TC_TRAMPOLINE.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(t);
        orig(this);
    }
    let n = TC_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let Some(a) = API.get() else { return; };
    if n == 3 {
        dump_tree_from(a, this, n);
    }
    // --- AI state: cheap file reads, polled often -------------------------
    // The gap between "the game wrote the next frame" and "matsuri published
    // its decision" is only ~1-2 s. Polling the AI state *inside* the button
    // scan (once every 20000 ticks, 24 s in one observed session) missed that
    // window completely: the previous icon stayed on screen and "Ai计算中"
    // was never seen. refresh_throttled() does no IL2CPP calls, so it is safe
    // to run far more often than the scan.
    let mut ai_changed = false;
    if let Some(slot_now) = ai::refresh_throttled(100) {
        let target_slot = slot_now.index();
        let icon_key = slot_now.icon_key();
        if CUR_TARGET_SLOT.load(Ordering::Relaxed) != target_slot as u32
            || CUR_ICON_KEY != icon_key
        {
            log_i(&format!(
                "[AI] icon -> slot {} ({}) icon={}",
                target_slot,
                slot_now.label(),
                icon_key
            ));
            CUR_TARGET_SLOT.store(target_slot as u32, Ordering::Relaxed);
            CUR_ICON_KEY = icon_key;
            ai_changed = true;
        }
        // Heartbeat: proves the poll is alive even when the state is stable.
        let polls = AI_POLLS.fetch_add(1, Ordering::Relaxed) + 1;
        if polls % 50 == 0 {
            log_i(&format!(
                "[AI] poll #{} state={} slot={} icon={}",
                polls,
                slot_now.label(),
                target_slot,
                icon_key
            ));
        }
    }

    // Button enumeration: name- and depth-independent discovery of real UI
    // buttons. Normally ~0.25 s, but *immediately* after an AI state change so
    // the badge is replaced in the same frame. A state change that lands on a
    // transient screen (stable=false, common right after a training/event
    // transition) is retried on this tighter cadence instead of ~1 s later.
    let gap = n.wrapping_sub(DEEP_LAST.load(Ordering::Relaxed));
    if n >= 4000 && (ai_changed || gap >= 1500) {
        DEEP_LAST.store(n, Ordering::Relaxed);
        let pass = BTN_PASS.fetch_add(1, Ordering::Relaxed) + 1;
        // raise_all() used cached Il2CppObject pointers and crashed at the
        // race screen; sync_badges now re-asserts sibling order on the live
        // transform it just resolved.
        // Attach only from the 3rd pass: by then the button has finished
        // instantiating its icon/text children, so our node really is last and
        // therefore drawn on top.
        list_buttons(
            a,
            pass,
            pass >= 3,
            CUR_TARGET_SLOT.load(Ordering::Relaxed) as u8,
            CUR_ICON_KEY,
        );
    }
    // Attach scan, throttled by tick count (all TextCommon instances share the
    // counter, so ~6000 ticks/s means roughly one scan every few seconds).
    let last = LAST_SCAN.load(Ordering::Relaxed);
    if n >= 2000 && n.wrapping_sub(last) >= 12000 {
        LAST_SCAN.store(n, Ordering::Relaxed);
        // scan_and_attach is kept for reference but disabled: its name heuristic
        // also matched the single-mode menu pieces (OBJ_btn_training_menu_*).
        // The FindObjectsOfType(Button) pass below is precise.
    }
}


unsafe fn observe(vc: *mut c_void) {
    if vc.is_null() {
        return;
    }
    let Some(a) = API.get() else { return; };
    let klass = obj_class_of(a, vc);
    if klass.is_null() {
        return;
    }

    // Badge attach runs on EVERY call while it has not succeeded yet: the view
    // controller can be handed out before its buttons are instantiated, so a
    // single-shot attempt would silently place nothing.
    if badge::ready() {
        let tries = ATTACH_TRIES.load(Ordering::Relaxed);
        let same = ATTACH_KLASS.load(Ordering::Relaxed) == klass;
        if tries < 30 && (!same || badge::count() == 0) {
            let probe_key = class_key(klass);
            if probe_key.contains("Training") {
                ATTACH_KLASS.store(klass, Ordering::Relaxed);
                ATTACH_TRIES.fetch_add(1, Ordering::Relaxed);
                attach_buttons(a, vc, klass);
            }
        }
    }

    // Dumping has its own budget and must not gate the badge work above.
    if STOPPED.load(Ordering::Relaxed) {
        return;
    }

    // Fast path: same class as last time -> one pointer compare and out.
    if LAST_KLASS.load(Ordering::Relaxed) == klass {
        return;
    }

    let key = class_key(klass);
    let idx = {
        let m = SEEN.get_or_init(|| Mutex::new(Vec::new()));
        let mut seen = match m.lock() {
            Ok(v) => v,
            Err(_) => return,
        };
        if seen.iter().any(|s| *s == key) {
            LAST_KLASS.store(klass, Ordering::Relaxed);
            return;
        }
        seen.push(key.clone());
        seen.len()
    };
    LAST_KLASS.store(klass, Ordering::Relaxed);

    emit(format!("[SCENE] #{} {} vc={:p} klass={:p}", idx, key, vc, klass));
    dump_methods(a, klass);
    // In a training scene we need the button -> transform chain, so drop the
    // keyword filter and walk two levels down unconditionally.
    let force = key.contains("Training") || key.contains("SingleMode");
    dump_fields(a, vc, klass, 0, force);

    let n = DUMPED.fetch_add(1, Ordering::Relaxed) + 1;
    let cap = MAX_SCENES.load(Ordering::Relaxed);
    if n >= cap {
        STOPPED.store(true, Ordering::Relaxed);
        emit(format!("[PROBE4] reached max_scenes={} -> stop dumping (hook stays)", cap));
    }
}

/// Hook body. Must never panic across the FFI boundary and must always return
/// exactly what the original method returned.
unsafe extern "C" fn gcv_hook(this: *mut c_void) -> *mut c_void {
    let t = TRAMPOLINE.load(Ordering::Relaxed);
    if t == 0 {
        return ptr::null_mut();
    }
    let orig: GcvFn = std::mem::transmute(t);
    let vc = orig(this);

    let c = HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    if c == 0 {
        log_i(&format!(
            "[PROBE4] hook fired (this={:p} vc={:p}) -> first call on game thread",
            this, vc
        ));
    }
    observe(vc);
    vc
}

/// Pure-C heartbeat. Never touches the IL2CPP runtime.
unsafe extern "C" fn on_present(_swapchain: *mut c_void, _userdata: *mut c_void) {
    let n = FRAMES.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        log_i("[PROBE4] present#0 (render thread alive; no il2cpp here)");
    }
}

macro_rules! need_v3 {
    ($g:expr, $n:expr, $t:ty) => {
        match try_resolve::<$t>($g, $n) {
            Some(f) => f,
            None => {
                log_i(&format!("[PROBE4] missing v3 api '{}' -> abort", $n));
                return;
            }
        }
    };
}

/// Resolve a raw il2cpp_* symbol through Hachimi's scanner.
/// NOTE: symbols_impl.rs:287 indexes SYMBOL_MAP[name], which panics on an
/// unknown name - so only names present in SYMBOL_LIST may be passed here.
unsafe fn symbol<T: Copy>(resolve: ResolveSymbolFn, name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    let p = resolve(c.as_ptr());
    if p.is_null() {
        log_i(&format!("[PROBE4] symbol '{}' unavailable", name));
        return None;
    }
    Some(std::mem::transmute_copy::<*mut c_void, T>(&p))
}

pub fn run(get_api: GetApiFn, max_scenes: u32) {
    unsafe {
        MAX_SCENES.store(max_scenes.max(1), Ordering::Relaxed);

        // --- v3 APIs, all null-checked (never transmute a null pointer) ---
        let resolve_symbol: ResolveSymbolFn =
            need_v3!(get_api, "il2cpp_resolve_symbol", ResolveSymbolFn);
        let get_image: GetImageFn = need_v3!(get_api, "il2cpp_get_assembly_image", GetImageFn);
        let get_class: GetClassFn = need_v3!(get_api, "il2cpp_get_class", GetClassFn);
        let get_methods: GetMethodsFn = need_v3!(get_api, "il2cpp_class_get_methods", GetMethodsFn);
        let register_present: RegisterPresentFn =
            need_v3!(get_api, "hachimi_register_present_callback", RegisterPresentFn);
        let hachimi_instance: HachimiInstanceFn =
            need_v3!(get_api, "hachimi_instance", HachimiInstanceFn);
        let get_interceptor: GetInterceptorFn =
            need_v3!(get_api, "hachimi_get_interceptor", GetInterceptorFn);
        let interceptor_hook: InterceptorHookFn =
            need_v3!(get_api, "interceptor_hook", InterceptorHookFn);

        // --- low-level il2cpp_* via Hachimi's symbol scanner ---
        let get_fields: Option<GetFieldsFn> = symbol(resolve_symbol, "il2cpp_class_get_fields");
        let obj_class: Option<ObjGetClassFn> = symbol(resolve_symbol, "il2cpp_object_get_class");
        let field_type: Option<FieldGetTypeFn> = symbol(resolve_symbol, "il2cpp_field_get_type");
        let type_kind: Option<TypeGetTypeFn> = symbol(resolve_symbol, "il2cpp_type_get_type");
        let class_parent: Option<ClassGetParentFn> = symbol(resolve_symbol, "il2cpp_class_get_parent");
        let class_from_type: Option<ClassFromTypeFn> =
            symbol(resolve_symbol, "il2cpp_class_from_type");

        // --- Gallop.SceneManager ---
        // Hachimi uses "umamusume.dll" (il2cpp/hook/umamusume/mod.rs:195),
        // NOT "Assembly-CSharp". Try several assemblies and log each result.
        let ns_gallop = CString::new("Gallop").unwrap();
        let cls_sm = CString::new("SceneManager").unwrap();
        let mut sm_class: *mut c_void = ptr::null_mut();
        let mut chosen = "";
        let mut img_um: *const c_void = ptr::null();
        for asm_name in [
            "umamusume.dll",
            "Assembly-CSharp",
            "umamusume",
            "Assembly-CSharp-firstpass",
        ] {
            let a = CString::new(asm_name).unwrap();
            let img = get_image(a.as_ptr());
            if img.is_null() {
                log_i(&format!("[PROBE4] asm '{}' -> image NULL", asm_name));
                continue;
            }
            let k = get_class(img, ns_gallop.as_ptr(), cls_sm.as_ptr());
            log_i(&format!(
                "[PROBE4] asm '{}' image={:p} Gallop.SceneManager={:p}",
                asm_name, img, k
            ));
            if !k.is_null() {
                sm_class = k;
                chosen = asm_name;
                img_um = img;
                break;
            }
        }
        if sm_class.is_null() {
            log_i("[PROBE4] SceneManager not found in any assembly -> abort");
            return;
        }
        log_i(&format!(
            "[PROBE4] using '{}' SceneManager klass={:p}",
            chosen, sm_class
        ));

        // Non-generic, parameterless overload only - mirrors SceneManager.rs:108
        // (`name == "GetCurrentViewController" && (*method).is_generic() == 0`).
        // A generic/inflated overload has a different calling convention
        // (extra MethodInfo*/rgctx argument), so hooking the wrong one would
        // corrupt the stack. If no candidate matches we abort instead: no data
        // is acceptable, a crash is not.
        let mut iter: *mut c_void = ptr::null_mut();
        let mut gcv_addr: *mut c_void = ptr::null_mut();
        loop {
            let m = get_methods(sm_class, &mut iter);
            if m.is_null() {
                break;
            }
            if cstr((*m).name) != "GetCurrentViewController" {
                continue;
            }
            let generic = (*m).bitfield & 1;
            let argc = (*m).parameters_count;
            log_i(&format!(
                "[PROBE4] candidate GetCurrentViewController ptr=0x{:x} generic={} argc={}",
                (*m).method_pointer, generic, argc
            ));
            if (*m).method_pointer != 0 && generic == 0 && argc == 0 {
                gcv_addr = (*m).method_pointer as *mut c_void;
                break;
            }
        }
        if gcv_addr.is_null() {
            log_i("[PROBE4] no non-generic parameterless GetCurrentViewController -> abort (game untouched)");
            return;
        }
        log_i(&format!("[PROBE4] GetCurrentViewController @ {:p}", gcv_addr));

        // --- UnityEngine reflection helpers (optional: only used for naming) ---
        let get_method_addr: Option<GetMethodAddrFn> =
            try_resolve(get_api, "il2cpp_get_method_addr");
        let str_len: Option<StrLenFn> = try_resolve(get_api, "il2cpp_string_length");
        let str_chars: Option<StrCharsFn> = try_resolve(get_api, "il2cpp_string_chars");
        let mut component_klass: usize = 0;
        let mut go_addr: usize = 0;
        let mut name_addr: usize = 0;
        let mut get_parent_addr: usize = 0;
        let mut child_count_addr: usize = 0;
        let mut get_child_addr: usize = 0;
        let mut get_transform_addr: usize = 0;
        let mut sibling_index_addr: usize = 0;
        let mut cached_ptr_off: i32 = 0;
        let mut find_objects_addr: usize = 0;
        {
            let ns_unity = CString::new("UnityEngine").unwrap();
            let asm_core = CString::new("UnityEngine.CoreModule.dll").unwrap();
            let img = get_image(asm_core.as_ptr());
            if img.is_null() {
                log_i("[PROBE4] UnityEngine.CoreModule.dll -> image NULL (no go names)");
            } else {
                let c_obj = CString::new("Object").unwrap();
                let c_comp = CString::new("Component").unwrap();
                let obj_k = get_class(img, ns_unity.as_ptr(), c_obj.as_ptr());
                let comp_k = get_class(img, ns_unity.as_ptr(), c_comp.as_ptr());
                log_i(&format!(
                    "[PROBE4] UnityEngine.Object={:p} Component={:p}",
                    obj_k, comp_k
                ));
                if let Some(gma) = get_method_addr {
                    if !obj_k.is_null() {
                        let n = CString::new("get_name").unwrap();
                        name_addr = gma(obj_k, n.as_ptr(), 0) as usize;
                    }
                    if !comp_k.is_null() {
                        let n = CString::new("get_gameObject").unwrap();
                        go_addr = gma(comp_k, n.as_ptr(), 0) as usize;
                    }
                }
                component_klass = comp_k as usize;
                if let Some(gma) = get_method_addr {
                    if !comp_k.is_null() {
                        let n = CString::new("get_transform").unwrap();
                        get_transform_addr = gma(comp_k, n.as_ptr(), 0) as usize;
                    }
                }
                // Transform helpers, used by the UI tree walk.
                let c_tr = CString::new("Transform").unwrap();
                let tr_k = get_class(img, ns_unity.as_ptr(), c_tr.as_ptr());
                if !tr_k.is_null() {
                    if let Some(gma) = get_method_addr {
                        let p1 = CString::new("get_parent").unwrap();
                        let p2 = CString::new("get_childCount").unwrap();
                        let p3 = CString::new("GetChild").unwrap();
                        get_parent_addr = gma(tr_k, p1.as_ptr(), 0) as usize;
                        child_count_addr = gma(tr_k, p2.as_ptr(), 0) as usize;
                        get_child_addr = gma(tr_k, p3.as_ptr(), 1) as usize;
                        let p4 = CString::new("GetSiblingIndex").unwrap();
                        sibling_index_addr = gma(tr_k, p4.as_ptr(), 0) as usize;
                    }
                }
                log_i(&format!(
                    "[PROBE4] get_name=0x{:x} get_gameObject=0x{:x} get_transform=0x{:x} \
get_parent=0x{:x} childCount=0x{:x} GetChild=0x{:x}",
                    name_addr, go_addr, get_transform_addr, get_parent_addr, child_count_addr, get_child_addr
                ));
                // m_CachedPtr is the liveness sentinel used by every tree walk.
                if let Some(gfb) = try_resolve::<GetFieldByNameFn>(get_api, "il2cpp_get_field_from_name") {
                    if !obj_k.is_null() {
                        let cn = CString::new("m_CachedPtr").unwrap();
                        let fi = gfb(obj_k, cn.as_ptr());
                        if !fi.is_null() {
                            cached_ptr_off = (*fi).offset;
                        }
                    }
                }
                log_i(&format!("[PROBE4] m_CachedPtr off={}", cached_ptr_off));
                if let Some(gma) = get_method_addr {
                    let fo = CString::new("FindObjectsOfType").unwrap();
                    find_objects_addr = gma(obj_k, fo.as_ptr(), 1) as usize;
                }
                log_i(&format!("[PROBE4] FindObjectsOfType @ 0x{:x}", find_objects_addr));
            }
        }

        let string_new_addr: usize = try_resolve::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(
            get_api,
            "il2cpp_string_new",
        )
        .map(|f| f as usize)
        .unwrap_or(0);
        let mut button_klass: usize = 0;
        let mut mask_klass: usize = 0;
        let mut rectmask_klass: usize = 0;
        let mut get_component_addr: usize = 0;
        let mut get_component_str_addr: usize = 0;
        {
            let a_ui = CString::new("UnityEngine.UI.dll").unwrap();
            let ui_img = get_image(a_ui.as_ptr());
            if !ui_img.is_null() {
                let ns_ui = CString::new("UnityEngine.UI").unwrap();
                let c_btn = CString::new("Button").unwrap();
                let b = get_class(ui_img, ns_ui.as_ptr(), c_btn.as_ptr());
                button_klass = b as usize;
                let c_mask = CString::new("Mask").unwrap();
                let c_rm = CString::new("RectMask2D").unwrap();
                mask_klass = get_class(ui_img, ns_ui.as_ptr(), c_mask.as_ptr()) as usize;
                rectmask_klass = get_class(ui_img, ns_ui.as_ptr(), c_rm.as_ptr()) as usize;
                if let Some(gma) = try_resolve::<GetMethodAddrFn>(get_api, "il2cpp_get_method_addr") {
                    let c_gc = CString::new("GetComponent").unwrap();
                    get_component_addr = 0;
                    if let Some(gmop) = try_resolve::<GetMethodOverloadAddrFn>(
                        get_api,
                        "il2cpp_get_method_overload_addr",
                    ) {
                        // System.Type is a reference type -> Il2CppTypeEnum 0x12.
                        let want: [u8; 1] = [0x12];
                        get_component_addr = gmop(
                            component_klass as *mut c_void,
                            c_gc.as_ptr(),
                            want.as_ptr(),
                            1,
                        ) as usize;
                        log_i(&format!(
                            "[PROBE4] GetComponent(Type) by signature = 0x{:x}",
                            get_component_addr
                        ));
                    }
                    // No fallback to the argc-based lookup: that one resolves
                    // GetComponent(string) and passing a Type into it crashes.
                    if get_component_addr == 0 {
                        // Fall back to the *string* overload, which is a different
                        // signature and therefore safe to address by argc.
                        get_component_str_addr =
                            gma(component_klass as *mut c_void, c_gc.as_ptr(), 1) as usize;
                        log_i(&format!(
                            "[PROBE4] GetComponent(string) = 0x{:x}",
                            get_component_str_addr
                        ));
                    }
                    log_i(&format!(
                        "[PROBE4] Button={:p} Mask=0x{:x} RectMask2D=0x{:x} GetComponent=0x{:x}",
                        b, mask_klass, rectmask_klass, get_component_addr
                    ));
                }
                log_i(&format!("[PROBE4] UnityEngine.UI.Button klass={:p}", b));
            }
        }

        let api = Api {
            get_methods,
            get_fields,
            obj_class,
            field_type,
            type_kind,
            class_parent,
            class_from_type,
            str_len,
            str_chars,
            component_klass,
            go_addr,
            name_addr,
            get_parent_addr,
            child_count_addr,
            get_child_addr,
            get_transform_addr,
            cached_ptr_off,
            find_objects_addr,
            button_klass,
            class_get_type_addr: symbol::<unsafe extern "C" fn(*mut c_void) -> *mut c_void>(
                resolve_symbol,
                "il2cpp_class_get_type",
            )
            .map(|f| f as usize)
            .unwrap_or(0),
            type_get_object_addr: symbol::<unsafe extern "C" fn(*mut c_void) -> *mut c_void>(
                resolve_symbol,
                "il2cpp_type_get_object",
            )
            .map(|f| f as usize)
            .unwrap_or(0),
            get_component_addr,
            get_component_str_addr,
            string_new_addr,
            mask_klass,
            rectmask_klass,
            sibling_index_addr,
        };

        // Type layouts first: this is the part that needs no navigation from
        // the user, so we get the evidence even on a run that stays on home.
        dump_named_types(&api, get_image, get_class);

        // Then a global class scan: TrainingViewController / TrainingButton are
        // NOT in umamusume.dll, so a fixed assembly list misses them.
        find_classes_global(
            &api,
            resolve_symbol,
            &["TrainingViewController", "TrainingButton", "TrainingResultViewController"],
        );
        list_classes_containing(&api, resolve_symbol, &["training", "singlemode", "singlemode"]);

        let _ = API.set(api);

        // Heartbeat first, so even if the hook fails we know the renderer is ok.
        let ok = register_present(Some(on_present), ptr::null_mut());

        // --- install the hook through Hachimi's interceptor ---
        let hachimi = hachimi_instance();
        if hachimi.is_null() {
            log_i("[PROBE4] hachimi_instance() = NULL -> abort");
            return;
        }
        let icp = get_interceptor(hachimi);
        if icp.is_null() {
            log_i("[PROBE4] hachimi_get_interceptor() = NULL -> abort");
            return;
        }
        let tramp = interceptor_hook(icp, gcv_addr, gcv_hook as *mut c_void);
        if tramp.is_null() {
            // Interceptor::hook logs the reason itself (core/interceptor.rs:35).
            log_i("[PROBE4] interceptor_hook failed -> abort (game untouched)");
            return;
        }
        TRAMPOLINE.store(tramp as usize, Ordering::Relaxed);

        log_i(&format!(
            "[PROBE4] hooked: trampoline={:p} present={} fields={} ftype={}",
            tramp,
            ok,
            get_fields.is_some(),
            field_type.is_some() && type_kind.is_some()
        ));
        log_i("[PROBE4] enter a training screen; the game calls GetCurrentViewController itself");

        // --- second hook: TextCommon.Update as a main-thread tick ---
        // GetCurrentViewController turned out to be called only on the game's own
        // transitions, so it never fires while a screen sits open. TextCommon is
        // on every loaded text label and its Update runs every frame.
        if !img_um.is_null() {
            if let Some(gma) = try_resolve::<GetMethodAddrFn>(get_api, "il2cpp_get_method_addr") {
                let ns_g = CString::new("Gallop").unwrap();
                let c_tc = CString::new("TextCommon").unwrap();
                let tc_klass = get_class(img_um, ns_g.as_ptr(), c_tc.as_ptr());
                log_i(&format!("[PROBE4] Gallop.TextCommon klass={:p}", tc_klass));
                if !tc_klass.is_null() {
                    let n_upd = CString::new("Update").unwrap();
                    let tc_upd = gma(tc_klass, n_upd.as_ptr(), 0);
                    log_i(&format!("[PROBE4] TextCommon.Update @ {:p}", tc_upd));
                    if !tc_upd.is_null() {
                        let t2 = interceptor_hook(icp, tc_upd, text_common_tick as *mut c_void);
                        if t2.is_null() {
                            log_i("[PROBE4] TextCommon.Update hook FAILED");
                        } else {
                            TC_TRAMPOLINE.store(t2 as usize, Ordering::Relaxed);
                            log_i(&format!("[PROBE4] tick hook ok tramp={:p}", t2));
                        }
                    }
                }
            }
        }
    }
}
