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
