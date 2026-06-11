//! End-to-end native pipeline: `inga build` a program, run the binary, and
//! require its stdout to match the expected golden output.
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
fn examples_build_and_run_with_expected_output() {
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

    let expected: &[(&str, &str)] = &[
        ("hello.inga", "hello, world!\ndoubled: [2, 4, 6, 8], total items: 4\nthe answer is 42\n"),
        ("retry.inga", "settled on 3 after retries\ngave up with -3\n"),
        ("shapes.inga", "area 12.56636\nrejected: a dot has no area\ntoo big at 600.0\n1\n"),
        ("arena.inga", "simulated 10000 particles in the region\n9000\n"),
        (
            "tasks.inga",
            "huge refused the job\nsmall: 500500\nmedium: 5000050000\nhuge: -1\ngrand total: 5000550499\n",
        ),
    ];
    for (example, want) in expected {
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
        assert_eq!(
            String::from_utf8_lossy(&native.stdout),
            *want,
            "{example}: unexpected output"
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn inga_test_runs_test_functions() {
    let inga = Path::new(env!("CARGO_BIN_EXE_inga"));
    let dir = std::env::temp_dir().join(format!("inga-test-cmd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("sample_test.inga");
    std::fs::write(
        &file,
        "double :: (Int n) -> Int {\n    n * 2\n}\n\ntestDouble :: () {\n    assertEq(double(4), 8)\n}\n\ntestBroken :: () {\n    assertEq(double(1), 3)\n}\n",
    )
    .unwrap();

    let out = Command::new(inga).arg("test").arg(&file).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "a failing test must fail the run");
    assert!(stdout.contains("testDouble"), "got: {stdout}");
    assert!(stdout.contains("1 passed, 1 failed"), "got: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}
