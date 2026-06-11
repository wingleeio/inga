//! End-to-end native pipeline: `inga build` a program, run the binary, and
//! require its stdout to match the interpreter byte-for-byte.
//!
//! Skips (with a note) when clang is unavailable.

use std::path::Path;
use std::process::Command;

fn clang_available() -> bool {
    Command::new("clang")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ensure_rt_lib(exe: &Path) -> Option<std::path::PathBuf> {
    let dir = exe.parent()?;
    let rt = dir.join("libinga_rt.a");
    if !rt.exists() {
        // Build the runtime staticlib into the same profile dir.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-p", "inga-rt"])
            .status()
            .ok()?;
        if !status.success() || !rt.exists() {
            return None;
        }
    }
    Some(rt)
}

#[test]
fn native_output_matches_interpreter() {
    if !clang_available() {
        eprintln!("skipping: clang not available");
        return;
    }
    let inga = Path::new(env!("CARGO_BIN_EXE_inga"));
    let Some(_rt) = ensure_rt_lib(inga) else {
        eprintln!("skipping: could not build libinga_rt.a");
        return;
    };
    let tmp = std::env::temp_dir().join(format!("inga-native-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();

    for example in ["hello.inga", "retry.inga", "shapes.inga", "arena.inga"] {
        let src_path = format!("{}/../../examples/{example}", env!("CARGO_MANIFEST_DIR"));
        let bin_path = tmp.join(example.trim_end_matches(".inga"));

        let build = Command::new(inga)
            .args(["build", &src_path, "-o"])
            .arg(&bin_path)
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "inga build {example} failed:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );

        let native = Command::new(&bin_path).output().unwrap();
        assert!(native.status.success(), "{example}: native binary failed");

        let interp = Command::new(inga).args(["run", &src_path]).output().unwrap();
        assert!(interp.status.success(), "{example}: interpreter run failed");

        assert_eq!(
            String::from_utf8_lossy(&native.stdout),
            String::from_utf8_lossy(&interp.stdout),
            "{example}: native and interpreted output differ"
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
