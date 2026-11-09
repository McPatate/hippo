//! `hippo statusline`: memory + backup state for harness status lines (Claude Code `statusLine`).
//! The last memory action stays visible until the next one.
//!
//! - Default: one line, `🦛 remembered prefers-rust 3s ago · ✓ backed up #12 · 73 memories`.
//! - `--pixel` / `HIPPO_PIXEL=1`: your hippo buddy (3 rows of half-block pixel art) with the info
//!   on its right. It reacts to what happens (chomps on writes, sniffs on reads, smiles when a
//!   backup is verified, sweats when something needs attention, naps when idle) and sits in the
//!   water when everything is safely backed up. Use `refreshInterval: 1` for the animations.
//!
//! Must be fast and never fail loudly: no git or network calls, only small local files.

use crate::config::{GlobalConfig, VaultConfig};
use crate::state::State;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::Path;

const ACTIVE_SECS: i64 = 6;
const HAPPY_SECS: i64 = 10;
const NAP_SECS: i64 = 30 * 60;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Write,
    Read,
    Forget,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Backup {
    Safe(u64),
    Pending,
    Failed,
    Conflict,
    LocalOnly,
}

pub struct Snapshot {
    pub name: Option<String>,
    pub color: String,
    pub last: Option<(Kind, String, DateTime<Utc>)>,
    pub backup: Backup,
    pub verified_at: Option<DateTime<Utc>>,
    pub count: usize,
    /// Agent changes waiting for `hippo review`.
    pub review: usize,
    pub remote: Option<String>,
}

fn describe(e: &Value) -> Option<(Kind, String)> {
    Some(match e["action"].as_str()? {
        "commit" => {
            let first = e["message"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string();
            if let Some(p) = first.strip_prefix("remember: ") {
                (Kind::Write, format!("remembered {}", p.trim_start_matches("memories/").trim_end_matches(".md")))
            } else if let Some(n) = first.strip_prefix("forget: ").or(first.strip_prefix("forget (pending review): ")) {
                (Kind::Forget, format!("forgot {n}"))
            } else if let Some(n) = first.strip_prefix("approve: ") {
                (Kind::Write, format!("approved {n}"))
            } else if let Some(n) = first.strip_prefix("reject: ") {
                (Kind::Forget, format!("rejected {n}"))
            } else if first.starts_with("import ") {
                (Kind::Write, first.replacen("import", "imported", 1))
            } else {
                (Kind::Write, "saved changes".into())
            }
        }
        "recall" => (Kind::Read, format!("recalled {}", e["name"].as_str().unwrap_or("?"))),
        "search" => (Kind::Read, format!("searched “{}”", e["query"].as_str().unwrap_or(""))),
        "restore" => (Kind::Other, "restored the vault".into()),
        "merge" => (Kind::Other, "merged another device".into()),
        "merge-conflict" => (Kind::Other, "hit a merge conflict".into()),
        _ => return None,
    })
}

fn read_ref(git_dir: &Path, name: &str) -> Option<String> {
    if let Ok(s) = std::fs::read_to_string(git_dir.join(name)) {
        return Some(s.trim().to_string());
    }
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    packed.lines().find(|l| l.ends_with(name)).map(|l| l[..40].to_string())
}

pub fn snapshot(vault: &Path) -> Option<Snapshot> {
    let cfg = VaultConfig::load(vault).ok()?;
    let state = State::load(vault).unwrap_or_default();
    let buddy = GlobalConfig::load().map(|g| g.buddy).unwrap_or_default();
    let last = crate::log::tail(vault, 40).iter().rev().find_map(|e| {
        let at = DateTime::parse_from_rfc3339(e["at"].as_str()?).ok()?.with_timezone(&Utc);
        let (k, what) = describe(e)?;
        Some((k, what, at))
    });
    let git_dir = vault.join(".git");
    let head = std::fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|h| read_ref(&git_dir, h.trim().strip_prefix("ref: ")?));
    let backup = if git_dir.join("MERGE_HEAD").exists() {
        Backup::Conflict
    } else if cfg.remote.is_none() {
        Backup::LocalOnly
    } else if state.last_error.is_some() {
        Backup::Failed
    } else if head.is_some() && head == read_ref(&git_dir, crate::backup::VERIFIED_REF) {
        Backup::Safe(state.verified_seq)
    } else {
        Backup::Pending
    };
    let files: Vec<_> = std::fs::read_dir(vault.join("memories"))
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect())
        .unwrap_or_default();
    let review = files
        .iter()
        .filter(|p| std::fs::read_to_string(p).is_ok_and(|t| crate::memory::Memory::parse("", &t).needs_review()))
        .count();
    Some(Snapshot {
        name: buddy.name,
        color: buddy.color.unwrap_or_else(|| "lavender".into()),
        last,
        backup,
        verified_at: state.verified_at,
        count: files.len(),
        review,
        remote: cfg.remote.map(|r| r.display()),
    })
}

fn paint(color: bool, code: &str, t: &str) -> String {
    if color { format!("\x1b[{code}m{t}\x1b[0m") } else { t.to_string() }
}

fn backup_text(b: &Backup, color: bool) -> String {
    match b {
        Backup::Safe(n) => paint(color, "32", &format!("✓ backed up #{n}")),
        Backup::Pending => paint(color, "33", "↻ backing up"),
        Backup::Failed => paint(color, "31", "✗ backup failed — `hippo status`"),
        Backup::Conflict => paint(color, "31", "⚠ merge conflict — ask your agent to resolve it"),
        Backup::LocalOnly => paint(color, "33", "⚠ local only — `hippo remote set`"),
    }
}

/// Default: one line.
pub fn line(s: &Snapshot, color: bool, now: DateTime<Utc>) -> String {
    let mut parts = vec![];
    if let Some((_, what, at)) = &s.last {
        let busy = (now - *at).num_seconds() < ACTIVE_SECS;
        let who = s.name.as_ref().map(|n| format!("{n} ")).unwrap_or_default();
        parts.push(format!("{} {}", paint(color, if busy { "1;35" } else { "35" }, &format!("{who}{what}")), paint(color, "2", &crate::util::ago(*at))));
    }
    parts.push(backup_text(&s.backup, color));
    if s.review > 0 {
        parts.push(paint(color, "33", &format!("✎ {} to review — `hippo review`", s.review)));
    }
    parts.push(paint(color, "2", &format!("{} memories", s.count)));
    format!("🦛 {}", parts.join(&paint(color, "2", " · ")))
}

// ───────────────────────────── the buddy ─────────────────────────────
// 10×6 pixels → 3 text rows of half blocks.
// B body · E eye · N nostril · M mouth · T tusk · P blush · D sweat drop · W water · . empty

const BASE: [&str; 6] = [".BB....BB.", "BBBBBBBBBB", "BBEBBBBEBB", "BBBBBBBBBB", "BNBBBBBBNB", ".BBBBBBBB."];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mood {
    Idle,
    Blink,
    Ears,
    Chomp,
    SniffLeft,
    SniffRight,
    Happy,
    Worried,
    Sleepy,
}

pub const MOODS: [Mood; 9] = [
    Mood::Idle,
    Mood::Blink,
    Mood::Ears,
    Mood::Chomp,
    Mood::SniffLeft,
    Mood::SniffRight,
    Mood::Happy,
    Mood::Worried,
    Mood::Sleepy,
];

pub fn pixels(mood: Mood, water: bool) -> [String; 6] {
    let mut f: [String; 6] = BASE.map(str::to_string);
    let set = |f: &mut [String; 6], r: usize, s: &str| f[r] = s.to_string();
    match mood {
        Mood::Idle => {}
        Mood::Blink | Mood::Sleepy => set(&mut f, 2, "BBBBBBBBBB"),
        Mood::Ears => set(&mut f, 0, "BB......BB"),
        Mood::Chomp => {
            set(&mut f, 1, "BBEBBBBEBB");
            set(&mut f, 2, "BNBBBBBBNB");
            set(&mut f, 3, "BTMMMMMMTB");
            set(&mut f, 4, "BMMMMMMMMB");
        }
        Mood::SniffLeft => {
            set(&mut f, 2, "BEBBBBEBBB");
            set(&mut f, 4, "NNBBBBBBNN");
        }
        Mood::SniffRight => {
            set(&mut f, 2, "BBBEBBBBEB");
            set(&mut f, 4, "NNBBBBBBNN");
        }
        Mood::Happy => {
            set(&mut f, 1, "BBEBBBBEBB");
            set(&mut f, 2, "BBBBBBBBBB");
            set(&mut f, 3, "BPBBBBBBPB");
        }
        Mood::Worried => {
            set(&mut f, 0, ".BB....BBD");
            set(&mut f, 1, "BBEBBBBEBB");
        }
    }
    if water {
        set(&mut f, 5, "WWWWWWWWWW");
    }
    f
}

fn palette(name: &str, busy: bool) -> (u8, u8, u8) {
    let (base, bright) = match name {
        "pink" => ((222, 150, 172), (242, 172, 192)),
        "gray" | "grey" => ((142, 142, 152), (172, 172, 182)),
        "blue" => ((122, 152, 200), (152, 182, 230)),
        _ => ((150, 140, 180), (176, 160, 214)), // lavender
    };
    if busy { bright } else { base }
}

fn rgb(c: char, body: (u8, u8, u8)) -> Option<(u8, u8, u8)> {
    match c {
        'B' => Some(body),
        'E' | 'N' => Some((40, 32, 56)),
        'M' => Some((232, 120, 150)),
        'T' => Some((250, 248, 240)),
        'P' => Some((244, 136, 168)),
        'D' => Some((140, 200, 255)),
        'W' => Some((70, 130, 200)),
        _ => None,
    }
}

/// Three rows of half blocks (truecolor, or monochrome with features as holes).
pub fn sprite(px: &[String; 6], body: (u8, u8, u8), color: bool) -> [String; 3] {
    let row = |top: &str, bot: &str| {
        let (top, bot): (Vec<char>, Vec<char>) = (top.chars().collect(), bot.chars().collect());
        let mut s = String::new();
        for x in 0..top.len() {
            if !color {
                let solid = |ch: char| matches!(ch, 'B' | 'T' | 'W');
                s.push(match (solid(top[x]), solid(bot[x])) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    _ => ' ',
                });
                continue;
            }
            match (rgb(top[x], body), rgb(bot[x], body)) {
                (Some(t), Some(b)) => s += &format!("\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀\x1b[0m", t.0, t.1, t.2, b.0, b.1, b.2),
                (Some(t), None) => s += &format!("\x1b[38;2;{};{};{}m▀\x1b[0m", t.0, t.1, t.2),
                (None, Some(b)) => s += &format!("\x1b[38;2;{};{};{}m▄\x1b[0m", b.0, b.1, b.2),
                (None, None) => s.push(' '),
            }
        }
        s
    };
    [row(&px[0], &px[1]), row(&px[2], &px[3]), row(&px[4], &px[5])]
}

/// What the buddy is doing right now.
pub fn mood(s: &Snapshot, now: DateTime<Utc>) -> Mood {
    let secs = now.timestamp();
    let tick = secs % 2 == 0;
    if let Some((kind, _, at)) = &s.last {
        if (now - *at).num_seconds() < ACTIVE_SECS {
            return match kind {
                Kind::Write => if tick { Mood::Chomp } else { Mood::Idle },
                Kind::Read => if tick { Mood::SniffLeft } else { Mood::SniffRight },
                Kind::Forget => if tick { Mood::Ears } else { Mood::Blink },
                Kind::Other => Mood::Idle,
            };
        }
    }
    if matches!(s.backup, Backup::Failed | Backup::Conflict) {
        return Mood::Worried;
    }
    if s.verified_at.is_some_and(|t| (now - t).num_seconds() < HAPPY_SECS) {
        return Mood::Happy;
    }
    let idle_for = s.last.as_ref().map(|(_, _, at)| (now - *at).num_seconds()).unwrap_or(i64::MAX);
    if idle_for > NAP_SECS {
        return Mood::Sleepy;
    }
    match (secs % 7, secs % 13) {
        (0, _) => Mood::Blink,
        (_, 0) => Mood::Ears,
        _ => Mood::Idle,
    }
}

/// `--pixel`: the buddy on the left, three info lines on the right.
pub fn buddy(s: &Snapshot, color: bool, now: DateTime<Utc>) -> String {
    let m = mood(s, now);
    let busy = s.last.as_ref().is_some_and(|(_, _, at)| (now - *at).num_seconds() < ACTIVE_SECS);
    let rows = sprite(&pixels(m, matches!(s.backup, Backup::Safe(_))), palette(&s.color, busy), color);
    let name = s.name.clone().unwrap_or_else(|| "hippo".into());
    let first = match (&s.last, m) {
        (_, Mood::Sleepy) => format!("{} {}", paint(color, "35", &name), paint(color, "2", "is napping · z z")),
        (Some((_, what, at)), _) => format!(
            "{} {}",
            paint(color, if busy { "1;35" } else { "35" }, &format!("{name} {what}")),
            paint(color, "2", &format!("· {}", crate::util::ago(*at)))
        ),
        (None, _) => paint(color, "35", &format!("{name} is ready")),
    };
    let third = paint(color, "2", &format!("{} memories{}", s.count, s.remote.as_ref().map(|r| format!(" · {r}")).unwrap_or_default()));
    let info = [first, backup_text(&s.backup, color), third];
    rows.iter().zip(info).map(|(r, i)| format!("{r}  {i}")).collect::<Vec<_>>().join("\n")
}

/// A sticker sheet of every mood, for `hippo buddy`.
pub fn sheet(color_name: &str, color: bool) -> String {
    let sprites: Vec<[String; 3]> = MOODS.iter().map(|m| sprite(&pixels(*m, false), palette(color_name, false), color)).collect();
    let labels = ["idle", "blink", "ears", "chomp", "sniff", "sniff", "happy", "worried", "sleepy"];
    let mut out = String::new();
    for r in 0..3 {
        out += &sprites.iter().map(|s| s[r].clone()).collect::<Vec<_>>().join("  ");
        out.push('\n');
    }
    out += &labels.iter().map(|l| format!("{l:<10}")).collect::<Vec<_>>().join("  ");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_well_formed_and_distinct() {
        for m in MOODS {
            for water in [false, true] {
                assert!(pixels(m, water).iter().all(|r| r.chars().count() == 10), "{m:?}");
            }
        }
        let mono = |m| sprite(&pixels(m, false), (0, 0, 0), false);
        for m in [Mood::Blink, Mood::Ears, Mood::Chomp, Mood::SniffLeft, Mood::Happy, Mood::Worried] {
            assert_ne!(mono(Mood::Idle), mono(m), "{m:?} looks like idle");
        }
    }

    #[test]
    fn moods_follow_events() {
        let now = Utc::now();
        let mut s = Snapshot {
            name: None,
            color: "lavender".into(),
            last: Some((Kind::Write, "remembered x".into(), now)),
            backup: Backup::Pending,
            verified_at: None,
            count: 1,
            review: 0,
            remote: None,
        };
        assert!(matches!(mood(&s, now), Mood::Chomp | Mood::Idle));
        s.last = Some((Kind::Write, "x".into(), now - chrono::Duration::hours(2)));
        assert_eq!(mood(&s, now), Mood::Sleepy);
        s.backup = Backup::Failed;
        assert_eq!(mood(&s, now), Mood::Worried);
        s.backup = Backup::Safe(3);
        s.verified_at = Some(now);
        assert_eq!(mood(&s, now), Mood::Happy);
    }

    #[test]
    fn describes_actions() {
        let e = serde_json::json!({"action": "commit", "message": "remember: memories/prefers-rust.md\n\nAgent: claude-code"});
        assert_eq!(describe(&e).unwrap().1, "remembered prefers-rust");
        assert_eq!(describe(&serde_json::json!({"action": "search", "query": "rust"})).unwrap().0, Kind::Read);
        assert!(describe(&serde_json::json!({"action": "backup"})).is_none());
    }
}
