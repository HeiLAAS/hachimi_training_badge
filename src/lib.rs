#![allow(static_mut_refs)]

// Hachimi Edge v3 independent plugin (B1).
// v3 entry: hachimi_init_v3(get_api, version) -> resolve functions by name.
// See Hachimi-Edge/src/core/plugin_api.rs:14-16, 1047-1124, 1143-1144.

mod ai;
mod badge;
mod demo;
mod probe;

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::OnceLock;

pub type GetApiFn = extern "C" fn(*const c_char) -> *mut c_void;
pub type LogFn = unsafe extern "C" fn(i32, *const c_char, *const c_char);
type PathFn = unsafe extern "C" fn() -> *const c_char;

static LOG: OnceLock<LogFn> = OnceLock::new();

pub unsafe fn api(get_api: GetApiFn, name: &str) -> *mut c_void {
    let c = CString::new(name).unwrap();
    get_api(c.as_ptr())
}

/// Resolve an API by name. Returns None when Hachimi does not export it,
/// so a null pointer is never transmuted into a callable function pointer
/// (which would crash the whole game process on an API/version mismatch).
pub unsafe fn try_resolve<T: Copy>(get_api: GetApiFn, name: &str) -> Option<T> {
    let p = api(get_api, name);
    if p.is_null() {
        return None;
    }
    Some(std::mem::transmute_copy::<*mut c_void, T>(&p))
}

pub fn log_i(msg: &str) {
    if let Some(f) = LOG.get() {
        let tag = b"TrainingBadge\0";
        let clean = msg.replace('\0', "?");
        if let Ok(m) = CString::new(clean) {
            unsafe { f(3, tag.as_ptr() as *const c_char, m.as_ptr()) };
        }
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn path_string(f: PathFn) -> String {
    let p = f();
    if p.is_null() {
        return String::new();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

// Minimal JSON reader (no external crates). Handles "key": value / "key": "value".
fn json_value(cfg: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\"", key);
    let i = cfg.find(&pat)?;
    let rest = &cfg[i + pat.len()..];
    let colon = rest.find(':')?;
    let mut s = rest[colon + 1..].trim_start();
    if s.starts_with('"') {
        s = &s[1..];
        let end = s.find('"')?;
        Some(s[..end].to_string())
    } else {
        let end = s
            .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
            .unwrap_or(s.len());
        Some(s[..end].to_string())
    }
}

fn json_str(cfg: &str, key: &str, d: &str) -> String {
    json_value(cfg, key).unwrap_or_else(|| d.to_string())
}
fn json_u32(cfg: &str, key: &str, d: u32) -> u32 {
    json_value(cfg, key).and_then(|v| v.parse().ok()).unwrap_or(d)
}
fn json_i32(cfg: &str, key: &str, d: i32) -> i32 {
    json_value(cfg, key).and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn find_config(bd: &str, dp: &str) -> Option<(String, String)> {
    for base in [bd, dp] {
        if base.is_empty() {
            continue;
        }
        let b = base.trim_end_matches(|c| c == '/' || c == '\\');
        for p in [
            format!("{}/training_badge.json", b),
            format!("{}/hachimi/training_badge.json", b),
        ] {
            if let Ok(s) = std::fs::read_to_string(&p) {
                let dir = p
                    .rsplit_once(|c| c == '/' || c == '\\')
                    .map(|x| x.0)
                    .unwrap_or(b)
                    .to_string();
                log_i(&format!("[CFG] loaded {}", p));
                return Some((dir, s));
            }
        }
    }
    None
}

#[unsafe(no_mangle)]
pub extern "C" fn hachimi_init_v3(get_api: GetApiFn, version: i32) -> i32 {
    // InitResult: Error = 0, Ok = 1
    if version < 3 {
        return 0;
    }

    unsafe {
        // Every API pointer is null-checked before use. A missing export
        // must fail safely (InitResult::Error = 0), never crash the game.
        let log_fn: LogFn = match try_resolve(get_api, "log") {
            Some(f) => f,
            None => return 0,
        };
        let _ = LOG.set(log_fn);

        let base_dir_fn: PathFn = match try_resolve(get_api, "hachimi_get_base_dir") {
            Some(f) => f,
            None => { log_i("[INIT] missing api hachimi_get_base_dir -> abort"); return 0; }
        };
        let data_path_fn: PathFn = match try_resolve(get_api, "hachimi_get_data_path") {
            Some(f) => f,
            None => { log_i("[INIT] missing api hachimi_get_data_path -> abort"); return 0; }
        };
        let bd = path_string(base_dir_fn);
        let dp = path_string(data_path_fn);
        log_i(&format!("[INIT] api v{} base_dir={} data_path={}", version, bd, dp));

        // Safety gate: do nothing unless a config file explicitly enables it.
        let (dir, cfg) = match find_config(&bd, &dp) {
            Some(v) => v,
            None => {
                log_i("[INIT] no training_badge.json found -> idle");
                return 1;
            }
        };
            ai::init(
            &json_str(&cfg, "decision_path", ""),
            json_u32(&cfg, "stale_secs", 300) as u64,
        );
        probe::set_number_all(json_str(&cfg, "number_all", "false") == "true");
        ai::set_frame_dir(&json_str(
            &cfg,
            "frame_dir",
            "C:/Ai/masuri-ai/.portable/PluginData/SendGameStatusPlugin",
        ));
    if json_str(&cfg, "enabled", "false") != "true" {
            log_i("[INIT] enabled != true -> idle");
            return 1;
        }

        match json_str(&cfg, "mode", "idle").as_str() {
            "probe" => {
                log_i("[INIT] mode=probe");
                if json_str(&cfg, "attach", "true") == "true" {
                    badge::set_offset(json_i32(&cfg, "offset_y", -150) as f32);
                    // Icons live next to the config: <hachimi dir>/icons/<key>.png
                    for (key, file) in [
                        ("train", "train.png"),
                        ("wait", "wait.png"),
                        ("race", "race.png"),
                        ("outing", "outing.png"),
                        ("rest", "rest.png"),
                    ] {
                        let path = format!("{}/icons/{}", dir, file);
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                log_i(&format!(
                                    "[INIT] icon {} <- {} ({} bytes)",
                                    key,
                                    path,
                                    bytes.len()
                                ));
                                badge::set_icon_png(key, bytes);
                            }
                            Err(_) => {
                                log_i(&format!("[INIT] icon {} missing: {}", key, path));
                            }
                        }
                    }
                    log_i(&format!(
                        "[INIT] badge offset_y={}",
                        json_i32(&cfg, "offset_y", -150)
                    ));
                    if badge::init(get_api) {
                        log_i("[INIT] badge kit ready");
                    } else {
                        log_i("[INIT] badge kit NOT ready (attach will be skipped)");
                    }
                } else {
                    log_i("[INIT] attach=false -> no badge");
                }
                probe::run(get_api, json_u32(&cfg, "max_scenes", 12));
            }
            "demo" => {
                log_i("[INIT] mode=demo");
                let rel = json_str(&cfg, "badge_bmp", "badge_demo.bmp");
                let full = if rel.contains(':') || rel.starts_with('/') || rel.starts_with('\\') {
                    rel
                } else {
                    format!("{}/{}", dir, rel)
                };
                demo::run(demo::Config {
                    bmp_path: wide(&full),
                    interval_ms: json_u32(&cfg, "interval_ms", 2000),
                    offset_x: json_i32(&cfg, "offset_x", 24),
                    offset_y: json_i32(&cfg, "offset_y", 24),
                });
            }
            other => log_i(&format!("[INIT] unknown mode '{}' -> idle", other)),
        }
    }

    1
}
