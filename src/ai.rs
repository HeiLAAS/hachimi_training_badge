//! Reads the decision stream produced by umaai-matsuri (`matsuri online --json`)
//! and turns it into "which of the five training buttons should carry the icon".
//!
//! The CLI writes one JSON object per line; we follow the last usable line of
//! that file. See crates/matsuri-ai/src/output.rs for the schema.

use crate::log_i;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Which training the icon should sit on. `Other` = 等待 / 非训练状态.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Speed,
    Stamina,
    Power,
    Guts,
    Wisdom,
    /// 等待计算结果
    Waiting,
    /// 推荐比赛
    Race,
    /// 推荐友人出行 / 普通出行
    Outing,
    /// 推荐休息
    Rest,
    /// 其它非训练（治病 / 教诲分配 …）：按等待态处理
    Other,
}

impl Slot {
    /// 1..5 = the five buttons left-to-right, matching the plugin's numbering.
    pub fn index(self) -> u8 {
        match self {
            Slot::Speed => 1,
            Slot::Stamina => 2,
            Slot::Power => 3,
            Slot::Guts => 4,
            Slot::Wisdom => 5,
            // 等待 / 非训练：都落在力量上
            Slot::Waiting | Slot::Race | Slot::Outing | Slot::Rest | Slot::Other => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Slot::Speed => "speed",
            Slot::Stamina => "stamina",
            Slot::Power => "power",
            Slot::Guts => "guts",
            Slot::Wisdom => "wisdom",
            Slot::Waiting => "waiting",
            Slot::Race => "race",
            Slot::Outing => "outing",
            Slot::Rest => "rest",
            Slot::Other => "other",
        }
    }

    /// Which icon file to show for this state.
    ///
    /// An empty key means "show no badge at all": while the AI is computing
    /// (or the decision is stale) we simply clear the old hint instead of
    /// putting up a separate "waiting" icon. The next decision is then shown
    /// directly, with no intermediate visual state.
    pub fn icon_key(self) -> &'static str {
        match self {
            Slot::Speed | Slot::Stamina | Slot::Power | Slot::Guts | Slot::Wisdom => "train",
            Slot::Race => "race",
            Slot::Outing => "outing",
            Slot::Rest => "rest",
            Slot::Waiting | Slot::Other => "",
        }
    }
}

static mut PATH: Option<String> = None;
static mut FRAME_DIR: Option<String> = None;
static mut STALE_SECS: u64 = 300;
static LAST_SIG: AtomicU64 = AtomicU64::new(0);
static CUR_SLOT: AtomicU32 = AtomicU32::new(255);
static HAS_DATA: AtomicBool = AtomicBool::new(false);
static LAST_DEC_TURN: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

/// Directory the frame writer (URA) drops `game<id>_turn<N>.json` into.
pub fn set_frame_dir(path: &str) {
    unsafe {
        FRAME_DIR = if path.is_empty() {
            None
        } else {
            Some(path.to_string())
        };
    }
    log_i(&format!(
        "[AI] frame_dir={}",
        if path.is_empty() { "<none>" } else { path }
    ));
}

pub fn init(path: &str, stale_secs: u64) {
    unsafe {
        PATH = if path.is_empty() {
            None
        } else {
            Some(path.to_string())
        };
        STALE_SECS = stale_secs.max(5);
        CUR_SLOT.store(255, Ordering::Relaxed);
        HAS_DATA.store(false, Ordering::Relaxed);
    }
    log_i(&format!(
        "[AI] decision_path={} stale_secs={}",
        if path.is_empty() { "<none>" } else { path },
        stale_secs
    ));
}

fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Minimal scanner for `"key": <int>` - avoids pulling in a JSON crate.
fn int_field(line: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{}\"", key);
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    let colon = rest.find(':')?;
    let tail = rest[colon + 1..].trim_start();
    let mut num = String::new();
    for ch in tail.chars() {
        if ch == '-' || ch.is_ascii_digit() {
            num.push(ch);
        } else {
            break;
        }
    }
    num.parse().ok()
}

/// Extract a JSON array of strings starting at `"<key>":[`.
///
/// Works on raw bytes and decodes once at the end: casting each byte to `char`
/// turns UTF-8 sequences into Latin-1 mojibake, which silently broke every
/// Chinese/Japanese label comparison.
fn str_array(line: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{}\"", key);
    let Some(i) = line.find(&pat) else {
        return Vec::new();
    };
    let rest = &line[i + pat.len()..];
    let Some(colon) = rest.find(':') else {
        return Vec::new();
    };
    let rest = &rest[colon + 1..];
    let Some(start) = rest.find('[') else {
        return Vec::new();
    };

    let bytes = rest.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if esc {
                cur.push(match b {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    b'"' => b'"',
                    b'\\' => b'\\',
                    other => other,
                });
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                out.push(String::from_utf8_lossy(&cur).into_owned());
                cur.clear();
                in_str = false;
            } else {
                cur.push(b);
            }
        } else if b == b'"' {
            in_str = true;
        } else if b == b']' {
            break;
        }
        i += 1;
    }
    out
}

fn text_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{}\"", key);
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    let colon = rest.find(':')?;
    let tail = rest[colon + 1..].trim_start();
    if !tail.starts_with('"') {
        return None;
    }
    let tail = &tail[1..];
    let end = tail.find('"')?;
    Some(&tail[..end])
}

/// Classify the recommended action text into a training slot.
///
/// The upstream labels are fixed by matsuri-core (`game/action.rs`):
///   AXIS_LABELS      = ["速","耐","力","根","智"]         + "训练"
///   ACT_SHORT_LABELS = ["流镝","神旗","纳舞","大曳","飞悬"] + "稽古"
/// and the docs state 演目下标 == 训练位下标, so the tables line up:
///   流镝=速 神旗=耐 纳舞=力 大曳=根 飞悬=智
pub fn classify(desc: &str) -> Slot {
    let d = desc.trim();

    // 演目稽古（本剧本把训练改名为稽古）
    if d.contains("稽古") {
        if d.contains("流镝") {
            return Slot::Speed;
        }
        if d.contains("神旗") {
            return Slot::Stamina;
        }
        if d.contains("纳舞") {
            return Slot::Power;
        }
        if d.contains("大曳") {
            return Slot::Guts;
        }
        if d.contains("飞悬") {
            return Slot::Wisdom;
        }
        // 繁体 / 异体写法兜底
        if d.contains("納舞") {
            return Slot::Power;
        }
        if d.contains("飛懸") {
            return Slot::Wisdom;
        }
    }

    // 普通训练：速训练 / 耐训练 / 力训练 / 根训练 / 智训练
    if d.contains("训练") || d.contains("トレーニング") {
        if d.contains('速') {
            return Slot::Speed;
        }
        if d.contains('耐') {
            return Slot::Stamina;
        }
        if d.contains('力') {
            return Slot::Power;
        }
        if d.contains('根') {
            return Slot::Guts;
        }
        if d.contains('智') || d.contains("賢") {
            return Slot::Wisdom;
        }
    }

    // 属性关键词兜底（英文 / 日文全称）
    let low = d.to_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| low.contains(k));
    if has(&["スピード", "speed", "速度"]) {
        return Slot::Speed;
    }
    if has(&["スタミナ", "stamina", "耐力"]) {
        return Slot::Stamina;
    }
    if has(&["パワー", "power", "力量"]) {
        return Slot::Power;
    }
    if has(&["根性", "guts", "毅力"]) {
        return Slot::Guts;
    }
    if has(&["賢さ", "智力", "wisdom", "intelligence"]) {
        return Slot::Wisdom;
    }

    // 非训练的具体类型：比赛 / 友人出行 / 普通出行 / 休息
    if d.contains("比赛") || d.contains("レース") || d.contains("race") {
        return Slot::Race;
    }
    if d.contains("友人出行") || d.contains("普通出行") || d.contains("お出かけ") {
        return Slot::Outing;
    }
    if d.contains("休息") || d.contains("休み") || d.contains("rest") {
        return Slot::Rest;
    }
    // 其它（治病 / 教诲分配 …）
    Slot::Other
}

/// Is this decision line an *action* decision (training / outing / race / rest)?
/// Event decisions list dialog options instead, and must be skipped.
fn is_action_decision(line: &str) -> bool {
    let descs = str_array(line, "candidate_descriptions");
    if !descs.iter().any(|d| d.contains("训练") || d.contains("稽古")) {
        return false;
    }
    let Some(idx) = int_field(line, "action_index") else {
        return false;
    };
    if idx < 0 {
        return false;
    }
    match descs.get(idx as usize) {
        Some(d) => !matches!(classify(d), Slot::Other),
        None => false,
    }
}

/// The turn the game is on right now.
///
/// Read it from `thisTurn.json` (the frame writer rewrites it every turn) and
/// take `baseGame.turn` from it. Do NOT use the highest `game<id>_turn<N>.json`
/// number: the directory keeps the files of previous runs, so that maximum
/// belongs to an old game and makes every decision look stale forever.
fn absolute_turn() -> Option<i64> {
    let dir = unsafe { FRAME_DIR.as_ref() }?;
    let text = std::fs::read_to_string(std::path::Path::new(dir).join("thisTurn.json")).ok()?;
    let base = text.find("baseGame")?;
    let turn_key = text[base..].find("\"turn\"")?;
    let after = base + turn_key + "\"turn\"".len();
    let tail = &text[after..];
    let colon = tail.find(':')?;
    let num: String = tail[colon + 1..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num.parse().ok()
}

/// Is the decision we are holding from an earlier turn?
///
/// The decision JSON's `turn` and the frame's `baseGame.turn` are the **same**
/// counter (both count turns of the current run, starting at 0), so this is a
/// plain comparison: a decision for turn N is stale as soon as the game moved
/// on to turn N+1 - that is exactly the window while the AI is still searching.
fn decision_is_stale(dec_turn: Option<i64>) -> bool {
    let Some(dt) = dec_turn else { return false };
    let Some(now) = absolute_turn() else { return false };
    dt < now
}


/// The last usable line of the decision file, if the file is fresh enough.
///
/// The stream interleaves decision lines with handshake/info lines ("connected"
/// is re-emitted repeatedly), so the *last line* is often not the newest decision.
/// Walk backwards and take the most recent action decision; fall back to the last
/// non-empty line (which means "waiting").
unsafe fn last_line() -> Option<String> {
    let path = PATH.as_ref()?;
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if let Ok(age) = SystemTime::now().duration_since(modified) {
        if age > Duration::from_secs(STALE_SECS) {
            return None;
        }
    }
    let text = std::fs::read_to_string(path).ok()?;
    // keep memory bounded on a long session
    let slice = if text.len() > 256 * 1024 {
        &text[text.len() - 256 * 1024..]
    } else {
        &text[..]
    };
    let mut fallback: Option<String> = None;
    for l in slice.lines().rev() {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if t.contains("\"decision_kind\"") || t.contains("\"action_index\"") {
            if is_action_decision(t) {
                return Some(t.to_string());
            }
            continue;
        }
        if fallback.is_none() {
            fallback = Some(t.to_string());
        }
    }
    fallback
}

/// Decode a cached slot. The arm order **must** match the `Slot` enum order:
/// Speed, Stamina, Power, Guts, Wisdom, Waiting, Race, Outing, Rest, Other.
/// (A stale mapping here silently turned 友人出行 into 休息.)
fn slot_from_u32(v: u32) -> Slot {
    match v {
        0 => Slot::Speed,
        1 => Slot::Stamina,
        2 => Slot::Power,
        3 => Slot::Guts,
        4 => Slot::Wisdom,
        5 => Slot::Waiting,
        6 => Slot::Race,
        7 => Slot::Outing,
        8 => Slot::Rest,
        _ => Slot::Other,
    }
}

/// Re-read the decision file. Returns the slot that should currently carry the icon.
pub fn refresh() -> Slot {
    unsafe {
        let Some(line) = last_line() else {
            if HAS_DATA.swap(false, Ordering::Relaxed) {
                log_i("[AI] no fresh decision file -> waiting state (icon on 力量)");
                CUR_SLOT.store(255, Ordering::Relaxed);
                LAST_DEC_TURN.store(-1, Ordering::Relaxed);
            }
            return Slot::Waiting;
        };
        let sig = hash(&line);
        if sig == LAST_SIG.load(Ordering::Relaxed) {
            // Same decision line - but the game may have moved on since (the AI
            // is still searching). Re-check freshness on every pass, otherwise a
            // stale recommendation would stay on screen until the next decision.
            let cached = slot_from_u32(CUR_SLOT.load(Ordering::Relaxed));
            let dec_turn = LAST_DEC_TURN.load(Ordering::Relaxed);
            if dec_turn >= 0 && decision_is_stale(Some(dec_turn)) {
                log_i("[AI] decision is older than the latest frame -> waiting");
                return Slot::Waiting;
            }
            return cached;
        }
        LAST_SIG.store(sig, Ordering::Relaxed);
        HAS_DATA.store(true, Ordering::Relaxed);

        let kind = text_field(&line, "type").unwrap_or("?");
        let slot = if kind == "decision" {
            let idx = int_field(&line, "action_index").unwrap_or(-1);
            let descs = str_array(&line, "candidate_descriptions");
            let desc = if idx >= 0 && (idx as usize) < descs.len() {
                descs[idx as usize].clone()
            } else {
                String::new()
            };
            let dk = text_field(&line, "decision_kind").unwrap_or("?");
            let mut s = classify(&desc);
            LAST_DEC_TURN.store(int_field(&line, "turn").unwrap_or(-1), Ordering::Relaxed);
            log_i(&format!(
                "[AI] decision turn={} kind={} action_index={} desc='{}' -> {} (icon on slot {})",
                int_field(&line, "turn").unwrap_or(-1),
                dk,
                idx,
                desc,
                s.label(),
                s.index()
            ));
            log_i(&format!("[AI] candidates = {:?}", descs));
            if decision_is_stale(int_field(&line, "turn")) {
                log_i("[AI] decision is for an earlier turn -> waiting");
                s = Slot::Waiting;
            }
            // A fresh decision line is checked against the current turn below.
            s
        } else {
            let ev = text_field(&line, "event").unwrap_or("");
            log_i(&format!(
                "[AI] non-decision line type={} event={} -> icon on 力量",
                kind, ev
            ));
            Slot::Waiting
        };
        CUR_SLOT.store(slot as u32, Ordering::Relaxed);
        slot
    }
}

static mut TICK: Option<Instant> = None;

/// Cheap throttle so the file is not re-read on every frame.
pub fn refresh_throttled(min_ms: u64) -> Option<Slot> {
    unsafe {
        let now = Instant::now();
        match TICK {
            Some(t) if now.duration_since(t) < Duration::from_millis(min_ms) => None,
            _ => {
                TICK = Some(now);
                Some(refresh())
            }
        }
    }
}
