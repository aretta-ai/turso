//! Trybuild smoke suite for the `DifferentialSubject` derive.
//!
//! Three cases per orchestrator decision: success path, tagged-but-no-expose
//! (legal — metadata-only), and unsupported field type (compile error).

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    // Phase 1
    t.pass("tests/ui/01_success.rs");
    t.pass("tests/ui/02_missing_expose.rs");
    t.compile_fail("tests/ui/03_unsupported_field_type.rs");
    // Phase 2
    t.pass("tests/ui/04_snapshot_success.rs");
    t.pass("tests/ui/05_snapshot_with_name.rs");
    t.compile_fail("tests/ui/06_snapshot_on_non_skipmap.rs");
    t.compile_fail("tests/ui/07_expose_and_snapshot_conflict.rs");
}
