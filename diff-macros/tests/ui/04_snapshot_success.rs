//! Phase 2 success path: `snapshot = T` on a `SkipMap<K, V>` field with
//! a `From<&V> for T` impl, no method-name override.

use crossbeam_skiplist::SkipMap;
use diff_macros::DifferentialSubject;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Tx {
    begin_ts: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TxSnapshot {
    begin_ts: u64,
}

impl From<&Tx> for TxSnapshot {
    fn from(t: &Tx) -> Self {
        TxSnapshot { begin_ts: t.begin_ts }
    }
}

#[derive(DifferentialSubject)]
struct Store {
    #[diff(private, snapshot = TxSnapshot)]
    txs: SkipMap<u64, Tx>,
}

fn main() {
    let s = Store { txs: SkipMap::new() };
    s.txs.insert(7, Tx { begin_ts: 100 });
    s.txs.insert(3, Tx { begin_ts: 50 });
    s.txs.insert(5, Tx { begin_ts: 75 });

    // Default method name is `diff_<field>` = `diff_txs`. Result is sorted by K.
    let got = s.diff_txs();
    assert_eq!(
        got,
        vec![
            (3u64, TxSnapshot { begin_ts: 50 }),
            (5, TxSnapshot { begin_ts: 75 }),
            (7, TxSnapshot { begin_ts: 100 }),
        ]
    );
}
