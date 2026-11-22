//! Commands that need the agent directory say so instead of panicking when
//! no home resolves. (`prime-agent factory` no longer starts a Python: its
//! list, import and export run in process.)
#![cfg(unix)]

use std::process::Command;

#[test]
fn missing_home_reports_agent_directory_requirement_without_a_panic() {
    let output = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .arg("config")
        .env_remove("HOME")
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HOME or PRIME_AGENT_CODING_AGENT_DIR is required"),
        "unexpected stderr: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "unexpected panic: {stderr}");

    let help = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .arg("--help")
        .env_remove("HOME")
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .output()
        .unwrap();
    assert!(help.status.success(), "help should not require a home");

    for args in [["help", "package"], ["package", "--help"]] {
        let help = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .args(args)
            .env_remove("HOME")
            .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
            .output()
            .unwrap();
        assert!(
            help.status.success(),
            "{args:?} should show help without a home: {}",
            String::from_utf8_lossy(&help.stderr)
        );
    }

    let package = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .args(["package", "list"])
        .env_remove("HOME")
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .output()
        .unwrap();
    assert_eq!(package.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&package.stderr)
        .contains("HOME or PRIME_AGENT_CODING_AGENT_DIR is required"));
}
