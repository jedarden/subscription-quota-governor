//! Keep the site-local command adapter safety contracts in the normal CI gate.
#[test]
fn python_adapter_contracts() {
    let isolated_home = tempfile::tempdir().expect("isolated adapter test home");
    let mut command = std::process::Command::new("python3");
    command
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
        .env("HOME", isolated_home.path())
        .env("PYTHONDONTWRITEBYTECODE", "1");
    // The host's `bead` may be a router that locates the native binary through
    // host.json. Preserve that lookup while isolating HOME for adapter tests.
    if let Some(home) = std::env::var_os("HOME") {
        let host_config = std::path::PathBuf::from(home).join(".config/bead-route/host.json");
        if host_config.is_file() && std::env::var_os("BEAD_ROUTE_HOST_CONFIG").is_none() {
            command.env("BEAD_ROUTE_HOST_CONFIG", host_config);
        }
    }
    let status = command
        .status()
        .expect("python3 is required for adapter contract tests");
    assert!(status.success(), "Python adapter contracts failed");
}
