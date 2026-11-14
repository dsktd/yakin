//! Keeps the `deprecated = "allow"` in vendor/dioxus-primitives honest: it exists only for
//! `use core::f32;` in select/text_search.rs. When a re-vendor drops that import, this test
//! fails, and the allow (and this test) should be removed.

#[test]
fn vendor_lint_allow_still_needed() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../vendor/dioxus-primitives/primitives");
    let src = std::fs::read_to_string(format!("{root}/src/select/text_search.rs")).unwrap();
    let manifest = std::fs::read_to_string(format!("{root}/Cargo.toml")).unwrap();
    if manifest.contains("deprecated = \"allow\"") {
        assert!(
            src.contains("use core::f32;"),
            "vendor/dioxus-primitives no longer imports core::f32: remove its `deprecated = \"allow\"` lint and this test"
        );
    }
}
