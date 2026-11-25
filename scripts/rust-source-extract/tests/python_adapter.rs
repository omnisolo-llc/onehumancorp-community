fn manifest_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        let candidate = std::path::PathBuf::from(dir);
        if candidate.join("Cargo.toml").is_file() {
            return candidate;
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors() {
            let candidate = ancestor.join("scripts/rust-source-extract");
            if candidate.join("Cargo.toml").is_file() {
                return candidate;
            }
            if ancestor.ends_with("scripts/rust-source-extract")
                && ancestor.join("Cargo.toml").is_file()
            {
                return ancestor.to_path_buf();
            }
        }
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn python_adapter_preserves_source_and_fails_closed() {
    let manifest = manifest_dir();
    let script = manifest.parent().unwrap().join("test_rust_source.py");
    let binary = std::env::var_os("CARGO_BIN_EXE_ohc-rust-source-extract")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_BIN_EXE_ohc-rust-source-extract")));
    let output = std::process::Command::new("python3")
        .arg("-B")
        .arg(&script)
        .env("OHC_RUST_EXTRACT_TEST_BINARY", binary)
        .output()
        .expect("native build requires Python 3");
    assert!(
        output.status.success(),
        "Python exact-source adapter failed ({}):\n{}\n{}",
        script.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
