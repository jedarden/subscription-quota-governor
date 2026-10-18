//! Keep the site-local command adapter safety contracts in the normal CI gate.
#[test]
fn python_adapter_contracts() {
    let isolated_home = tempfile::tempdir().expect("isolated adapter test home");
    let status = std::process::Command::new("python3")
        .args([
            "-B",
            "-m",
            "unittest",
            "discover",
            "-s",
            "tests/python",
            "-p",
            "test_*.py",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", isolated_home.path())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .status()
        .expect("python3 is required for adapter contract tests");
    assert!(status.success(), "Python adapter contracts failed");
}
