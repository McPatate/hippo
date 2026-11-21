//! CLI parsing edge cases.

use std::process::Command;

#[test]
fn bool_env_flags_accept_1() {
    let tmp = tempfile::tempdir().unwrap();
    for (var, args) in [("HIPPO_PIXEL", vec!["statusline"]), ("HIPPO_AGENT_MODE", vec!["status"])] {
        let out = Command::new(env!("CARGO_BIN_EXE_hippo"))
            .args(&args)
            .env(var, "1")
            .env("HIPPO_HOME", tmp.path())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("invalid value"), "{var}=1 rejected: {err}");
    }
}

#[test]
fn every_subcommand_parses() {
    // clap panics at runtime on conflicting arg definitions; `--help` exercises every one.
    let out = Command::new(env!("CARGO_BIN_EXE_hippo")).arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&out.stdout).to_string();
    let cmds: Vec<&str> = help
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help")
        .collect();
    assert!(cmds.len() > 10, "{help}");
    for c in cmds {
        let o = Command::new(env!("CARGO_BIN_EXE_hippo")).args([c, "--help"]).output().unwrap();
        assert!(o.status.success(), "`hippo {c} --help` failed: {}", String::from_utf8_lossy(&o.stderr));
    }
}

#[test]
fn add_sets_visibility_only_for_known_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path().join("v");
    let key = hippo::crypto::Key::generate(&tmp.path().join("key.txt")).unwrap();
    let v = hippo::vault::Vault::init(&vault, &key).unwrap();
    v.update_registry("projects: link", |r| r.learn("st", &hippo::project::Ident::of(tmp.path()))).unwrap();
    let add = |vis: &str| {
        Command::new(env!("CARGO_BIN_EXE_hippo"))
            .current_dir(tmp.path())
            .env("HIPPO_HOME", tmp.path())
            .args(["--human", "--vault"])
            .arg(&vault)
            .args(["add", "signoff", "-m", "Commit with -s.", "--visibility", vis])
            .output()
            .unwrap()
    };
    assert!(add("project:st").status.success());
    assert_eq!(v.read_memory("signoff").unwrap().extra.get("visibility").map(String::as_str), Some("project:st"));
    let out = add("project:nope");
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("no project `nope`"));
    assert!(add("all").status.success());
    assert!(!v.read_memory("signoff").unwrap().extra.contains_key("visibility"));
}

#[test]
fn session_start_index_ends_with_a_review_nudge() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let key = hippo::crypto::Key::generate(&tmp.path().join("key.txt")).unwrap();
    let v = hippo::vault::Vault::init(&tmp.path().join("v"), &key).unwrap();
    let mem = |name: &str| hippo::memory::Memory { name: name.into(), kind: "feedback".into(), body: format!("About {name}."), ..Default::default() };
    v.remember(&mem("pr-style")).unwrap();
    let context = || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hippo"))
            .env("HIPPO_HOME", tmp.path())
            .env("CLAUDE_CONFIG_DIR", tmp.path().join("claude"))
            .arg("--vault")
            .arg(&v.root)
            .arg("context")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        write!(child.stdin.take().unwrap(), "{}", serde_json::json!({"cwd": tmp.path()})).unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    };
    assert!(!context().contains("/hippo review"));
    v.remember_by(&mem("atomic-prs"), Some("claude-code")).unwrap();
    v.remember_by(&mem("tabs"), Some("claude-code")).unwrap();
    let out = context();
    assert!(out.starts_with("🦛 hippo: 3 memories"), "{out}");
    assert!(out.lines().last().unwrap().starts_with("🦛 2 change(s) to the memory wait for the user's decision"), "{out}");
    let log = std::fs::read_to_string(v.root.join(".hippo/log.jsonl")).unwrap();
    assert!(log.lines().last().unwrap().contains("\"pending\":2"), "{log}");
}

#[test]
fn repo_roots_are_cached_and_checked_against_the_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let key = hippo::crypto::Key::generate(&tmp.path().join("key.txt")).unwrap();
    let v = hippo::vault::Vault::init(&tmp.path().join("v"), &key).unwrap();
    let repo = tmp.path().canonicalize().unwrap().join("repo");
    let git = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(&repo).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let clone = |msg: &str| {
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", msg]);
        git(&["rev-parse", "HEAD"])
    };
    let cache = tmp.path().join("roots.json");
    let project = || {
        let out = Command::new(env!("CARGO_BIN_EXE_hippo")).current_dir(&repo).env("HIPPO_HOME", tmp.path()).arg("--vault").arg(&v.root).arg("project").output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let roots: std::collections::BTreeMap<String, Vec<String>> = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        (roots, std::fs::metadata(&cache).unwrap().modified().unwrap())
    };
    let first = clone("first");
    let (roots, written) = project();
    assert_eq!(roots.values().collect::<Vec<_>>(), [&vec![first.clone()]]);
    assert_eq!(project(), (roots, written), "a hit writes nothing");
    let other = clone("another repo at the same path");
    assert_ne!(other, first);
    assert_eq!(project().0.values().collect::<Vec<_>>(), [&vec![other]]);
}
