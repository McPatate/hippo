//! 🧹 Cleanup time: hippo has no opinion on how memories should be organized. It only points at
//! likely duplicates; the agent and the user decide together, and every change goes through
//! `hippo review`.

use crate::memory::Memory;
use std::collections::BTreeSet;

fn words(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    let union = a.union(b).count();
    if union == 0 { 0.0 } else { a.intersection(b).count() as f32 / union as f32 }
}

/// Pairs of memories that probably say the same thing, most similar first.
pub fn likely_duplicates(memories: &[Memory]) -> Vec<(String, String, f32)> {
    let sigs: Vec<_> = memories
        .iter()
        .map(|m| {
            let name: BTreeSet<String> = m.name.split('-').map(str::to_string).collect();
            (name, words(&format!("{} {}", m.name, m.description)), words(&m.body))
        })
        .collect();
    let mut out = vec![];
    for i in 0..memories.len() {
        for j in i + 1..memories.len() {
            let ((na, da, ba), (nb, db, bb)) = (&sigs[i], &sigs[j]);
            // `project-hippo` inside `bucket-memory-project-hippo`: an older copy under another name.
            let nested = na.len().min(nb.len()) >= 2 && (na.is_subset(nb) || nb.is_subset(na));
            let score = jaccard(da, db).max(jaccard(ba, bb)).max(if nested { 0.9 } else { 0.0 });
            if score >= 0.5 {
                out.push((memories[i].name.clone(), memories[j].name.clone(), score));
            }
        }
    }
    out.sort_by(|a, b| b.2.total_cmp(&a.2));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, description: &str, body: &str) -> Memory {
        Memory { name: name.into(), description: description.into(), body: body.into(), ..Default::default() }
    }

    #[test]
    fn finds_renamed_copies_not_neighbours() {
        let ms = [
            m("project-hippo", "hippo: encrypted portable memory", "Rust, HF buckets, age."),
            m("bucket-memory-project-hippo", "old name of the memory project", "Was bucket-memory."),
            m("hippo-release-notes", "how to write release notes", "Template with ascii art."),
            m("prefers-rust", "likes Rust", "Uses Rust for side projects and work."),
            m("rust-preference", "likes Rust", "Uses Rust for side projects and at work."),
        ];
        let pairs: Vec<_> = likely_duplicates(&ms).into_iter().map(|(a, b, _)| (a, b)).collect();
        assert!(pairs.contains(&("project-hippo".into(), "bucket-memory-project-hippo".into())));
        assert!(pairs.contains(&("prefers-rust".into(), "rust-preference".into())));
        assert!(!pairs.iter().any(|(a, b)| a == "hippo-release-notes" || b == "hippo-release-notes"));
    }
}
