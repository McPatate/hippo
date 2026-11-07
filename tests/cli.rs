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
