//! Phase 1's `expose = [...]` and Phase 2's `snapshot = ...` target
//! different field shapes (Option<T> vs SkipMap<K, V>). Combining
//! them on the same field is meaningless and must error.

use crossbeam_skiplist::SkipMap;
use diff_macros::DifferentialSubject;

struct Tx;
struct TxSnapshot;

impl From<&Tx> for TxSnapshot {
    fn from(_: &Tx) -> Self {
        TxSnapshot
    }
}

#[derive(DifferentialSubject)]
struct Store {
    #[diff(private, expose = [version: u8], snapshot = TxSnapshot)]
    txs: SkipMap<u64, Tx>,
}

fn main() {}
