//! `hippo connect claude` / `disconnect claude` against a sandboxed Claude config and a fake
//! `claude` CLI. One test per binary: it mutates process env.

use std::path::Path;

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn connect_and_disconnect_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path().join("claude-config");
    std::fs::create_dir_all(&claude).unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = tmp.path().join("calls.log");
    // Fake `claude`: logs calls; `mcp get` succeeds (user scope) only after an `mcp add`.
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {calls}\ncase \"$1 $2\" in\n  \"mcp add\") echo \"Command: $7\" > {added};;\n  \"mcp remove\") rm -f {added};;\n  \"mcp get\") [ -f {added} ] && echo 'Scope: User config' && cat {added} && exit 0; exit 1;;\nesac\nexit 0\n",
        calls = calls.display(),
        added = tmp.path().join("added").display()
    );
    std::fs::write(bin.join("claude"), script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let settings = r#"{
  "model": "opus",
  "permissions": {
    "deny": [
      "Bash(rm -rf:*)"
    ]
  },
  "statusLine": {
    "type": "command",
    "command": "echo 'my line'",
    "padding": 1
  },
  "theme": "dark"
}
"#;
    std::fs::write(claude.join("settings.json"), settings).unwrap();
    std::fs::write(claude.join("CLAUDE.md"), "# Mine\n\nKeep this.\n").unwrap();
    // SAFETY: single-threaded test binary.
    unsafe {
        std::env::set_var("CLAUDE_CONFIG_DIR", &claude);
        std::env::set_var("HIPPO_HOME", tmp.path().join("hippo-home"));
        std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()));
    }

    let r = hippo::connect::connect_claude(true, false).unwrap();
    assert!(r.mcp.starts_with("added"), "{}", r.mcp);
    assert!(read(&calls).contains("mcp add -s user hippo --"));
    let md = read(&claude.join("CLAUDE.md"));
    assert!(md.starts_with("# Mine\n\nKeep this.\n") && md.contains("hippo:start") && md.contains("hippo:end"));
    let s: serde_json::Value = serde_json::from_str(&read(&claude.join("settings.json"))).unwrap();
    let cmd = s["statusLine"]["command"].as_str().unwrap();
    assert!(cmd.contains("statusline --wrap 'echo '\\''my line'\\'''"), "{cmd}");
    assert_eq!(s["statusLine"]["padding"], 1);
    let keys: Vec<&String> = s.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["model", "permissions", "statusLine", "theme", "sandbox"], "key order preserved");
    let dr = s["sandbox"]["filesystem"]["denyRead"][0].as_str().unwrap();
    assert!(dr.ends_with("hippo-home") && !dr.starts_with("//"), "{dr}");
    let deny: Vec<&str> = s["permissions"]["deny"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(deny[0], "Bash(rm -rf:*)", "user rule kept first");
    assert!(deny.iter().any(|r| r.starts_with("Read(//") && r.ends_with("hippo-home/**)")), "{deny:?}");
    assert!(deny.iter().any(|r| r.starts_with("Edit(//") && r.ends_with("hippo-home)")), "{deny:?}");
    assert_eq!(read(&claude.join("settings.json.hippo-backup")), settings, "one-time backup of the original");

    // Idempotent: no double block, no double wrap.
    let r = hippo::connect::connect_claude(true, true).unwrap();
    assert!(r.mcp.starts_with("already"));
    assert_eq!(read(&claude.join("CLAUDE.md")).matches("hippo:start").count(), 1);
    let s: serde_json::Value = serde_json::from_str(&read(&claude.join("settings.json"))).unwrap();
    let cmd = s["statusLine"]["command"].as_str().unwrap();
    assert_eq!(cmd.matches("--wrap").count(), 1, "{cmd}");
    assert!(cmd.starts_with("HIPPO_PIXEL=1 "));

    // Disconnect restores everything exactly.
    let r = hippo::connect::disconnect_claude().unwrap();
    assert!(r.mcp.starts_with("removed"), "{}", r.mcp);
    assert_eq!(read(&claude.join("CLAUDE.md")), "# Mine\n\nKeep this.\n");
    let s: serde_json::Value = serde_json::from_str(&read(&claude.join("settings.json"))).unwrap();
    let orig: serde_json::Value = serde_json::from_str(settings).unwrap();
    assert_eq!(s, orig);

    // Legacy unmarked block from an early version gets migrated, then removed cleanly.
    std::fs::write(claude.join("CLAUDE.md"), "# Memory\n\nUse the **hippo** MCP server as long-term memory (old)\n- x\n").unwrap();
    hippo::connect::connect_claude(false, false).unwrap();
    let md = read(&claude.join("CLAUDE.md"));
    assert!(md.starts_with("<!-- hippo:start") && !md.contains("(old)"), "{md}");
    hippo::connect::disconnect_claude().unwrap();
    assert!(!claude.join("CLAUDE.md").exists(), "file only held hippo's block");
}
