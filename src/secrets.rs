//! Secret detection. Memories are sent to model providers by agents, so secrets must not
//! end up in them even though backups are encrypted.
//!
//! - `heuristic`: offline regexes + entropy, microseconds; runs on every save.
//! - `trufflehog`: if installed, offline (`--no-verification --no-update`); ~0.8 s startup, so it
//!   runs during backups (concurrently with the upload) and in `hippo scan`, never inline.
//!
//! Findings never contain the secret itself, only a redacted preview.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub kind: String,
    pub file: Option<String>,
    pub line: Option<usize>,
    /// First characters only, e.g. `hf_A…(37 chars)`.
    pub preview: String,
    pub source: String,
}

static PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("Hugging Face token", r"\bhf_[A-Za-z0-9]{30,}\b"),
        ("Anthropic API key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
        ("OpenAI API key", r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,}"),
        ("GitHub token", r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36}\b|\bgithub_pat_[A-Za-z0-9_]{22,}"),
        ("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}"),
        ("AWS access key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        ("Slack token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
        ("Stripe key", r"\b[sr]k_(?:live|test)_[A-Za-z0-9]{16,}"),
        ("Google API key", r"\bAIza[0-9A-Za-z_-]{35}"),
        ("age secret key", r"AGE-SECRET-KEY-1[0-9A-Z]{50,}"),
        ("private key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        ("JWT", r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
        ("URL with password", r"[a-z][a-z0-9+.-]*://[^\s:/@]+:([^\s/@]{6,})@"),
    ]
    .into_iter()
    .map(|(k, p)| (k, Regex::new(p).unwrap()))
    .collect()
});

static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:password|passwd|pwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|private[_-]?key)\b\s*[:=]\s*["'`]?([^\s"'`]{8,})"#).unwrap()
});

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{32,}").unwrap());

fn redact(s: &str) -> String {
    let head: String = s.chars().take(4).collect();
    format!("{head}…({} chars)", s.chars().count())
}

fn entropy(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| {
        let p = c as f64 / n;
        -p * p.log2()
    }).sum()
}

fn looks_random(s: &str) -> bool {
    // Paths, repo ids and slugs (`org/repo/Model-7B-Instruct`) are judged by their longest part.
    if s.split(['/', '-', '_', '.']).map(str::len).max().unwrap_or(0) < 24 {
        return false;
    }
    let (up, low, dig) = (
        s.chars().any(|c| c.is_ascii_uppercase()),
        s.chars().any(|c| c.is_ascii_lowercase()),
        s.chars().any(|c| c.is_ascii_digit()),
    );
    let hex = s.chars().all(|c| c.is_ascii_hexdigit());
    up && low && dig && !hex && entropy(s) >= 4.2
}

fn is_placeholder(v: &str) -> bool {
    let l = v.to_lowercase();
    ["<", ">", "...", "…", "xxx", "${", "{{", "redacted", "example", "changeme", "your_", "your-", "******"]
        .iter()
        .any(|p| l.contains(p))
}

/// Fast offline scan of one text.
pub fn heuristic(text: &str) -> Vec<Finding> {
    let mut out = vec![];
    for (i, line) in text.lines().enumerate() {
        let mut hit = |kind: &str, m: &str| {
            out.push(Finding {
                kind: kind.into(),
                file: None,
                line: Some(i + 1),
                preview: redact(m),
                source: "heuristic".into(),
            })
        };
        let mut matched = false;
        for (kind, re) in PATTERNS.iter() {
            if let Some(m) = re.find(line) {
                if !is_placeholder(m.as_str()) {
                    hit(kind, m.as_str());
                    matched = true;
                }
            }
        }
        if matched {
            continue;
        }
        if let Some(c) = ASSIGNMENT.captures(line) {
            let v = c.get(1).unwrap().as_str();
            if !is_placeholder(v) && v.chars().any(|c| c.is_ascii_digit() || !c.is_alphanumeric()) {
                hit("credential assignment", v);
                continue;
            }
        }
        for m in TOKEN.find_iter(line) {
            if looks_random(m.as_str()) && !is_placeholder(m.as_str()) {
                hit("high-entropy string", m.as_str());
                break;
            }
        }
    }
    out
}

pub fn trufflehog_available() -> bool {
    Command::new("trufflehog").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[derive(Deserialize)]
struct TfFinding {
    #[serde(rename = "DetectorName")]
    detector: String,
    #[serde(rename = "Raw", default)]
    raw: String,
    #[serde(rename = "SourceMetadata", default)]
    meta: serde_json::Value,
}

fn run_trufflehog(args: &[&str]) -> Result<Option<Vec<Finding>>> {
    if !trufflehog_available() {
        return Ok(None);
    }
    let out = Command::new("trufflehog")
        .args(args)
        .args(["--no-verification", "--no-update", "--json", "--log-level=-1"])
        .output()?;
    let mut findings = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(f) = serde_json::from_str::<TfFinding>(line) else { continue };
        let data = &f.meta["Data"];
        let (file, line) = if let Some(g) = data.get("Git") {
            (g["file"].as_str().map(str::to_string), g["line"].as_u64().map(|l| l as usize))
        } else {
            let fs = &data["Filesystem"];
            (fs["file"].as_str().map(str::to_string), fs["line"].as_u64().map(|l| l as usize))
        };
        findings.push(Finding { kind: f.detector, file, line, preview: redact(&f.raw), source: "trufflehog".into() });
    }
    Ok(Some(findings))
}

/// Scan the vault's git history (optionally only commits after `since`). None = not installed.
pub fn trufflehog_git(vault: &Path, since: Option<&str>) -> Result<Option<Vec<Finding>>> {
    let uri = format!("file://{}", vault.display());
    let mut args = vec!["git", uri.as_str(), "--branch", "main"];
    if let Some(s) = since {
        args.extend(["--since-commit", s]);
    }
    let mut r = run_trufflehog(&args)?;
    // Vault files are relative paths; keep them that way for display.
    if let Some(f) = r.as_mut() {
        f.dedup_by(|a, b| a.kind == b.kind && a.file == b.file && a.preview == b.preview);
    }
    Ok(r)
}

/// Human-readable one-liner for errors and warnings.
pub fn describe(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| {
            let loc = match (&f.file, f.line) {
                (Some(file), Some(l)) => format!(" ({file}:{l})"),
                (Some(file), None) => format!(" ({file})"),
                (None, Some(l)) => format!(" (line {l})"),
                _ => String::new(),
            };
            format!("{} {}{loc}", f.kind, f.preview)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_secrets() {
        let cases = [
            concat!("token: hf", "_JkLmNoPqRsTuVwXyZaBcDeFgHiJkLmNoPq"),
            concat!("key sk-", "ant-api03-AbCdEfGhIjKlMnOpQrStUv"),
            concat!("gh", "p_wWPw5k4aXcaT4fNP0UcnZwJUVFk6LO0pINUx"),
            concat!("AKIA", "QWERTYUIOPASDFGH"),
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            concat!("db: postgres://admin:", "s3cr3tPassw0rd@db.example/prod"),
            "password = Tr0ub4dor&3",
            "blob 8fJ2kLq9ZxPw3RtYv7NmB4cD1eF6gH0iJ5kL2mN9",
        ];
        for c in cases {
            assert!(!heuristic(c).is_empty(), "missed: {c}");
        }
    }

    #[test]
    fn ignores_normal_text() {
        let cases = [
            "Prefers Rust for side projects and concise answers.",
            "commit 35783eb6274c41cffe83514b918ee16007bf7702 fixed it",
            "uuid 123e4567-e89b-12d3-a456-426614174000",
            "password: use the one in 1Password",
            "api_key = <your-key-here>",
            "see https://huggingface.co/docs/hub/storage-buckets-s3",
            "path /Users/alice/Documents/side-projects/hippo/src/backup.rs",
            "token: ${HF_TOKEN}",
            "model ai/org/Qwen3-Coder-30B-A3B-Instruct-2507 on b200",
        ];
        for c in cases {
            assert!(heuristic(c).is_empty(), "false positive: {c} → {:?}", heuristic(c));
        }
    }

    #[test]
    fn previews_are_redacted() {
        let f = &heuristic(concat!("hf", "_JkLmNoPqRsTuVwXyZaBcDeFgHiJkLmNoPq"))[0];
        assert!(!f.preview.contains("NoPq"), "{}", f.preview);
    }
}

fn findings_path(vault: &Path) -> std::path::PathBuf {
    vault.join(".hippo").join("secrets.json")
}

/// Findings from background scans, `.hippo/secrets.json` (redacted previews only).
pub fn load_findings(vault: &Path) -> Vec<Finding> {
    std::fs::read_to_string(findings_path(vault)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn record_findings(vault: &Path, found: Vec<Finding>) -> Result<()> {
    if found.is_empty() {
        return Ok(());
    }
    let mut all = load_findings(vault);
    for f in found {
        if !all.contains(&f) {
            all.push(f);
        }
    }
    crate::util::atomic_write(&findings_path(vault), serde_json::to_string_pretty(&all)?.as_bytes())
}

pub fn clear_findings(vault: &Path) -> Result<()> {
    let p = findings_path(vault);
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}
