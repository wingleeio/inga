//! IR-generation tests (no clang needed — end-to-end native tests live in
//! inga-cli/tests/native.rs).

use inga_core::check_source;

fn example(name: &str) -> String {
    let path = format!("{}/../../examples/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn compileable_examples_produce_ir() {
    for name in ["hello.inga", "retry.inga", "shapes.inga", "arena.inga"] {
        let src = example(name);
        let checked = check_source(&src);
        let ir = inga_codegen::compile(&checked.program, &checked.info)
            .unwrap_or_else(|e| panic!("{name} failed to compile: {e:?}"));
        assert!(ir.contains("define i32 @main()"), "{name}: missing entry point");
    }
}

#[test]
fn bench_program_produces_ir() {
    let path = format!("{}/../../bench/bench.inga", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(path).unwrap();
    let checked = check_source(&src);
    let ir = inga_codegen::compile(&checked.program, &checked.info).expect("bench compiles");
    // Evidence passing: the service-using function takes a hidden instance param.
    assert!(ir.contains("define i64 @ing.fn.fibService(i64 %ev.Adder, i64 %p.n)"), "evidence param missing");
    // Result ABI: the failing function returns {value, err}.
    assert!(ir.contains("define { i64, i64 } @ing.fn.boom"), "fallible ABI missing");
}

#[test]
fn balatro_game_produces_ir() {
    let path = format!("{}/../../games/balatro.inga", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(path).unwrap();
    let checked = check_source(&src);
    let errors: Vec<&str> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == inga_core::diag::Severity::Error)
        .map(|d| d.message.as_str())
        .collect();
    assert!(errors.is_empty(), "game has check errors: {errors:?}");
    let ir = inga_codegen::compile(&checked.program, &checked.info).expect("game compiles");
    assert!(ir.contains("@rt_gfx_run"), "game should hand the loop to the runtime");
    assert!(ir.contains("@rt_gfx_rect"), "game should draw");
}

#[test]
fn unsupported_constructs_are_clear_errors() {
    let src = example("user_service.inga");
    let checked = check_source(&src);
    let err = inga_codegen::compile(&checked.program, &checked.info)
        .expect_err("user_service uses decode/encode and must be rejected");
    assert!(
        err.iter().any(|d| d.message.contains("not supported by `inga build`")),
        "got: {err:?}"
    );
}

#[test]
fn infallible_functions_have_plain_abi() {
    let src = "f :: (Int n) -> Int {\n    n + 1\n}\n\nmain :: () {\n    println(f(1))\n}\n";
    let checked = check_source(src);
    let ir = inga_codegen::compile(&checked.program, &checked.info).unwrap();
    assert!(ir.contains("define i64 @ing.fn.f(i64 %p.n)"), "got:\n{ir}");
}
