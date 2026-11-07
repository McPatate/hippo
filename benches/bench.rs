//! hippo benchmarks: `cargo bench` (N memories via HIPPO_BENCH_N, default 300).
//!
//! Prints a table and appends one JSON line per run to `bench-results.jsonl` so efficiency can
//! be tracked over time. Uses a directory remote (network excluded) — set HIPPO_BENCH_BUCKET to
//! also time a real HF bucket backup.

use hippo::backup::{self, RestoreOptions};
use hippo::config::RemoteConfig;
use hippo::crypto::Key;
use hippo::index::Index;
use hippo::memory::Memory;
use hippo::remote::Remote;
use hippo::vault::Vault;
use std::time::{Duration, Instant};

struct Stats(Vec<Duration>);
impl Stats {
    fn pct(&self, p: f64) -> f64 {
        let mut v: Vec<f64> = self.0.iter().map(|d| d.as_secs_f64() * 1e3).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[((v.len() - 1) as f64 * p).round() as usize]
    }
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed())
}

fn lorem(i: usize) -> String {
    format!(
        "Memory number {i}. The user prefers concise answers, writes Rust, works at Hugging Face, \
         likes hippos and TUIs. Topic {} keyword-{i}.\n\n{}",
        ["privacy", "storage", "agents", "music", "travel"][i % 5],
        "Some longer markdown body text to make the file realistic. ".repeat(8)
    )
}

#[tokio::main]
async fn main() {
    let n: usize = std::env::var("HIPPO_BENCH_N").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let key = Key::generate(&root.join("key.txt")).unwrap();
    let rc = RemoteConfig::Dir { path: root.join("bucket") };
    let remote = Remote::open(&rc).unwrap();
    let mut rows: Vec<(String, String)> = vec![];
    let mut json = serde_json::Map::new();
    macro_rules! rec {
        ($name:expr, $human:expr, $val:expr) => {{
            rows.push(($name.to_string(), $human));
            json.insert($name.to_string(), $val);
        }};
    }

    // init
    let (v, d) = time(|| Vault::init(&root.join("vault"), &key).unwrap());
    rec!("init_ms", format!("{:.1} ms", d.as_secs_f64() * 1e3), (d.as_secs_f64() * 1e3).into());

    // remember (write + git commit), N times
    let mut w = vec![];
    for i in 0..n {
        let m = Memory { name: format!("mem-{i}"), kind: "note".into(), description: format!("memory {i}"), body: lorem(i), ..Default::default() };
        w.push(time(|| v.remember(&m).unwrap()).1);
    }
    let s = Stats(w);
    rec!("remember_p50_ms", format!("{:.2} ms (p95 {:.2})", s.pct(0.5), s.pct(0.95)), s.pct(0.5).into());
    json.insert("remember_p95_ms".into(), s.pct(0.95).into());

    // read
    let r = Stats((0..200).map(|i| time(|| v.read_memory(&format!("mem-{}", i % n)).unwrap()).1).collect());
    rec!("read_p50_ms", format!("{:.3} ms", r.pct(0.5)), r.pct(0.5).into());

    // list
    let (_, d) = time(|| v.list().unwrap());
    rec!("list_ms", format!("{:.1} ms ({n} memories)", d.as_secs_f64() * 1e3), (d.as_secs_f64() * 1e3).into());

    // search: cold (build index) and warm
    let (mut idx, _) = time(|| Index::open(&v.root).unwrap());
    let (_, d) = time(|| idx.refresh(&v.memories_dir()).unwrap());
    rec!("index_build_ms", format!("{:.1} ms", d.as_secs_f64() * 1e3), (d.as_secs_f64() * 1e3).into());
    let q = Stats((0..100).map(|i| time(|| { idx.refresh(&v.memories_dir()).unwrap(); idx.search(&format!("keyword-{} rust", i % n), 10).unwrap() }).1).collect());
    rec!("search_warm_p50_ms", format!("{:.2} ms (incl. refresh check)", q.pct(0.5)), q.pct(0.5).into());

    // backup: full then incremental
    let t = Instant::now();
    let full = backup::backup(&v, &key, &remote).await.unwrap();
    let d = t.elapsed();
    rec!("backup_full_ms", format!("{:.1} ms ({} KB)", d.as_secs_f64() * 1e3, full.bytes / 1024), (d.as_secs_f64() * 1e3).into());
    json.insert("backup_full_bytes".into(), full.bytes.into());

    v.remember(&Memory { name: "mem-0".into(), kind: "note".into(), body: "changed".into(), ..Default::default() }).unwrap();
    let t = Instant::now();
    let inc = backup::backup(&v, &key, &remote).await.unwrap();
    let d = t.elapsed();
    rec!("backup_incr_ms", format!("{:.1} ms ({} B)", d.as_secs_f64() * 1e3, inc.bytes), (d.as_secs_f64() * 1e3).into());
    json.insert("backup_incr_bytes".into(), inc.bytes.into());

    let t = Instant::now();
    backup::backup(&v, &key, &remote).await.unwrap();
    let d = t.elapsed();
    rec!("backup_noop_ms", format!("{:.1} ms", d.as_secs_f64() * 1e3), (d.as_secs_f64() * 1e3).into());

    // restore
    let t = Instant::now();
    backup::restore(&remote, &rc, &key, &root.join("restored"), RestoreOptions { vault_id: None, device: None }).await.unwrap();
    let d = t.elapsed();
    rec!("restore_ms", format!("{:.1} ms", d.as_secs_f64() * 1e3), (d.as_secs_f64() * 1e3).into());

    // CLI cold start (status)
    let bin = env!("CARGO_BIN_EXE_hippo");
    let c = Stats((0..10).map(|_| time(|| {
        std::process::Command::new(bin).args(["--human", "--vault"]).arg(&v.root).arg("status").output().unwrap()
    }).1).collect());
    rec!("cli_status_p50_ms", format!("{:.1} ms", c.pct(0.5)), c.pct(0.5).into());

    // Optional: real HF bucket
    if let Ok(bucket) = std::env::var("HIPPO_BENCH_BUCKET") {
        let hrc = RemoteConfig::Hf { bucket };
        let hr = Remote::open(&hrc).unwrap();
        let mut hv = Vault::open(&v.root).unwrap();
        hv.config.vault_id = format!("bench{}", hippo::util::random_hex(4));
        hv.config.device_id = hippo::util::random_hex(6);
        hv.config.save(&hv.root).unwrap();
        let _ = std::fs::remove_file(hv.root.join(".hippo/state.json"));
        hv.git.run(["update-ref", "-d", backup::VERIFIED_REF]).unwrap();
        let t = Instant::now();
        let r = backup::backup(&hv, &key, &hr).await.unwrap();
        let d = t.elapsed();
        rec!("hf_backup_full_ms", format!("{:.0} ms ({} KB)", d.as_secs_f64() * 1e3, r.bytes / 1024), (d.as_secs_f64() * 1e3).into());
        hv.remember(&Memory { name: "mem-1".into(), kind: "note".into(), body: "changed".into(), ..Default::default() }).unwrap();
        let t = Instant::now();
        backup::backup(&hv, &key, &hr).await.unwrap();
        let d = t.elapsed();
        rec!("hf_backup_incr_ms", format!("{:.0} ms", d.as_secs_f64() * 1e3), (d.as_secs_f64() * 1e3).into());
    }

    println!("\n🦛 hippo bench — {n} memories\n");
    for (k, v) in &rows {
        println!("  {k:<22} {v}");
    }
    json.insert("n".into(), n.into());
    json.insert("at".into(), chrono::Utc::now().to_rfc3339().into());
    json.insert("version".into(), env!("CARGO_PKG_VERSION").into());
    use std::io::Write;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bench-results.jsonl");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
    writeln!(f, "{}", serde_json::Value::Object(json)).unwrap();
    println!("\n  → appended to {}", path.display());
}
