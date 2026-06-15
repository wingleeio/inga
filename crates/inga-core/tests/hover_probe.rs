//! Hover coverage: every name a cursor can land on answers with something
//! useful. Each assertion pins one hover-producing site in the checker.

use inga_core::check_source;

/// The hover text whose span covers `needle`'s position in `src`
/// (innermost wins, like the LSP).
fn hover_at(src: &str, needle: &str) -> String {
    let checked = check_source(src);
    assert!(
        checked.diagnostics.is_empty(),
        "probe program must be clean: {:?}",
        checked.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let offset = src.find(needle).unwrap_or_else(|| panic!("needle `{needle}` not in src")) as u32;
    checked
        .info
        .hovers
        .iter()
        .filter(|(span, _)| span.start <= offset && offset < span.end)
        .min_by_key(|(span, _)| span.end - span.start)
        .map(|(_, text)| text.clone())
        .unwrap_or_else(|| panic!("no hover at `{needle}`"))
}

const SRC: &str = r#"use std/http
use std/json
use std/time

struct Stats { Int visits, String label }

service Counter {
    bump :: () -> Int
}

provider MemCounter :: () {
    MutMap<String, Int> hits = MutMap()

    Counter {
        bump: () -> {
            n = hits.get("k") |> getOrElse(0)
            hits.set("k", n + 1)
            n + 1
        }
    }
}

handle :: (HttpRequest req) -> HttpResponse uses Counter {
    Counter counter
    s = Stats { visits: counter.bump(), label: req.path }
    t = s.label
    d = time.utc(time.now())
    y = d.year
    xs = MutList()
    xs.push(1)
    pair = (1, "two")
    p0 = pair.0
    dur = 100.millis
    arena = 256.kb
    println(t, y, p0, dur, arena, xs.size())
    match req.path {
        "/visit" -> HttpResponse(200, json.encode(s))
        _ -> HttpResponse(404, req.body)
    }
}

main :: () {
    provide Http, MemCounter
    println(handle(HttpRequest("GET", "/visit", "", "")).status)
}
"#;

#[test]
fn struct_field_access_hovers() {
    assert_eq!(hover_at(SRC, "path }"), "path : String");
    assert_eq!(hover_at(SRC, "label\n"), "label : String");
    assert_eq!(hover_at(SRC, "year"), "year : Int");
    assert_eq!(hover_at(SRC, "body)"), "body : String");
}

#[test]
fn record_literal_hovers() {
    // The struct name in a literal shows the typed signature...
    assert_eq!(hover_at(SRC, "Stats {"), "struct Stats { Int visits, String label }");
    // ...and each field key shows its declared type.
    assert_eq!(hover_at(SRC, "visits:"), "visits : Int");
    assert_eq!(hover_at(SRC, "label:"), "label : String");
}

#[test]
fn constructor_hovers_show_the_signature() {
    assert_eq!(hover_at(SRC, "HttpResponse(200"), "struct HttpResponse { Int status, String body }");
    assert_eq!(hover_at(SRC, "HttpRequest(\"GET\""), "struct HttpRequest { String method, String path, String query, String body }");
}

#[test]
fn container_method_hovers_are_typed() {
    assert_eq!(hover_at(SRC, "get(\"k\")"), "get(String key) -> Int?");
    assert_eq!(hover_at(SRC, "set(\"k\""), "set(String key, Int value) -> Unit");
    assert_eq!(hover_at(SRC, "push(1)"), "push(Int value) -> Unit");
}

#[test]
fn binding_hovers_reflect_later_constraints() {
    // `xs = MutList()` is refined by the `xs.push(1)` below it.
    assert_eq!(hover_at(SRC, "xs = MutList"), "xs : MutList<Int>");
}

#[test]
fn string_template_captures_hover_typed() {
    let src = "route :: (String p) -> String {\n    match p {\n        \"/users/${Int id}/${slug}\" -> \"${id} ${slug}\"\n        _ -> \"x\"\n    }\n}\n\nmain :: () {\n    println(route(\"/users/1/a\"))\n}\n";
    assert_eq!(hover_at(src, "id}/"), "id : Int");
    assert_eq!(hover_at(src, "slug}\" ->"), "slug : String");
}

#[test]
fn tuple_index_and_suffix_hovers() {
    assert_eq!(hover_at(SRC, "0\n    dur"), ".0 : Int");
    assert_eq!(hover_at(SRC, "millis"), ".millis — Int to Duration");
    assert_eq!(hover_at(SRC, "kb"), ".kb — Int bytes (×1024)");
}
