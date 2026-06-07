//! Phase 2 rejection: `snapshot = T` on a non-`SkipMap` field must
//! fail to compile with a clear diagnostic. Phase 3+ may lift this
//! to support BTreeMap / HashMap / Vec.

use diff_macros::DifferentialSubject;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct State;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StateSnapshot;

impl From<&State> for StateSnapshot {
    fn from(_: &State) -> Self {
        StateSnapshot
    }
}

#[derive(DifferentialSubject)]
struct Store {
    #[diff(private, snapshot = StateSnapshot)]
    states: std::collections::BTreeMap<u64, State>,
}

fn main() {}
