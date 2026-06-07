//! Phase 2 method-name override: `name = "<suffix>"` produces
//! `diff_<suffix>` instead of `diff_<field>`. Use case: long field
//! name (`finalized_tx_states`) shortened to a tidy accessor
//! (`diff_finalized`).

use crossbeam_skiplist::SkipMap;
use diff_macros::DifferentialSubject;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Active,
    Committed(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StateSnapshot(State);

impl From<&State> for StateSnapshot {
    fn from(s: &State) -> Self {
        StateSnapshot(*s)
    }
}

#[derive(DifferentialSubject)]
struct Store {
    #[diff(private, snapshot = StateSnapshot, name = "finalized")]
    finalized_tx_states: SkipMap<u64, State>,
}

fn main() {
    let s = Store { finalized_tx_states: SkipMap::new() };
    s.finalized_tx_states.insert(1, State::Active);
    s.finalized_tx_states.insert(2, State::Committed(99));

    // Method name is `diff_finalized`, NOT `diff_finalized_tx_states`.
    let got = s.diff_finalized();
    assert_eq!(
        got,
        vec![
            (1u64, StateSnapshot(State::Active)),
            (2, StateSnapshot(State::Committed(99))),
        ]
    );
}
