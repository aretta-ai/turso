//! Trybuild smoke suite for the `DifferentialSubject` derive.
//!
//! Three cases per orchestrator decision: success path, tagged-but-no-expose
//! (legal — metadata-only), and unsupported field type (compile error).

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/01_success.rs");
    t.pass("tests/ui/02_missing_expose.rs");
    t.compile_fail("tests/ui/03_unsupported_field_type.rs");
}
