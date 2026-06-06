//! Differential-testing accessors for `MvStore`.
//!
//! Only compiled under `--features differential-accessors`. Exposes
//! owned snapshots of private `MvStore` internals (`txs`,
//! `finalized_tx_states`, the version-id / tx-id counters, and a
//! `commit_ts` lookup) for use by the Aretta Books MVCC conformance
//! harness at
//! `verification/db/flavors/turso/mvcc-conformance/` in the
//! companion `aretta-books` repo.
//!
//! Wrapper types (`TxnSnapshot`, `FinalStateSnapshot`,
//! `TxnStateSnapshot`) shadow the private `Transaction` /
//! `TransactionState` types so the surface stays
//! source-compatible across upstream refactors without leaking
//! private structure. Live as a child module of `database` so
//! Rust visibility rules give us access to the private fields.
//!
//! NEVER use these accessors in production. They take locks, allocate
//! per call, and are intentionally lossy w.r.t. the Hekaton commit-
//! dep machinery — the conformance harness only cares about the
//! Lean-projected fields.

use crate::mvcc::clock::LogicalClock;
use crate::mvcc::database::{MvStore, RowID, TransactionState, TxID};
use crate::sync::atomic::Ordering;

/// Owned snapshot of a `TransactionState`. Mirrors the private enum.
/// `Preparing` and `Terminated` are included for completeness — the
/// MVP scenario never produces them, but multi-conn / multi-op work
/// will.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnStateSnapshot {
    Active,
    Preparing(u64),
    Aborted,
    Terminated,
    Committed(u64),
}

impl From<TransactionState> for TxnStateSnapshot {
    fn from(s: TransactionState) -> Self {
        match s {
            TransactionState::Active => Self::Active,
            TransactionState::Preparing(ts) => Self::Preparing(ts),
            TransactionState::Aborted => Self::Aborted,
            TransactionState::Terminated => Self::Terminated,
            TransactionState::Committed(ts) => Self::Committed(ts),
        }
    }
}

/// Owned snapshot of a live `Transaction` (only the projection-
/// relevant subset: state, begin_ts, write_set). The Hekaton commit-
/// dependency state (`commit_dep_counter`, `commit_dep_set`,
/// `abort_now`) is intentionally elided — the Lean projection does
/// not model it.
#[derive(Clone, Debug)]
pub struct TxnSnapshot {
    pub state: TxnStateSnapshot,
    pub begin_ts: u64,
    /// Sorted ascending by `RowID` for stable cross-side comparison;
    /// see `Projection.lean::projectTxn` / the Rust `projection`
    /// module for the matching canonicalization on the Lean side.
    pub write_set: Vec<RowID>,
}

/// Owned snapshot of a finalized state. Same shape as
/// `TxnStateSnapshot`; kept as a type alias so the public surface
/// documents which `MvStore` field each accessor reads.
pub type FinalStateSnapshot = TxnStateSnapshot;

impl<Clock: LogicalClock> MvStore<Clock> {
    /// Owned snapshot of `MvStore::txs` (live transactions), sorted
    /// ascending by `TxID`. `write_set` per txn is also sorted; see
    /// `TxnSnapshot::write_set` for the rationale.
    pub fn diff_txs(&self) -> Vec<(TxID, TxnSnapshot)> {
        let mut out: Vec<(TxID, TxnSnapshot)> = self
            .txs
            .iter()
            .map(|entry| {
                let tx_id = *entry.key();
                let tx = entry.value();
                // `AtomicTransactionState::state` is `pub(crate)`;
                // child-module visibility lets us read it directly
                // without consuming the atomic via `From`.
                let encoded = tx.state.state.load(Ordering::Acquire);
                let state = TxnStateSnapshot::from(
                    TransactionState::decode(encoded),
                );
                let begin_ts = tx.begin_ts;
                let mut write_set: Vec<RowID> = tx
                    .write_set
                    .lock()
                    .entries
                    .iter()
                    .map(|(id, _)| id.clone())
                    .collect();
                write_set.sort();
                (tx_id, TxnSnapshot { state, begin_ts, write_set })
            })
            .collect();
        out.sort_by_key(|p| p.0);
        out
    }

    /// Owned snapshot of `MvStore::finalized_tx_states` (removed-but-
    /// still-referenced txns), sorted ascending by `TxID`.
    pub fn diff_finalized(&self) -> Vec<(TxID, FinalStateSnapshot)> {
        let mut out: Vec<(TxID, FinalStateSnapshot)> = self
            .finalized_tx_states
            .iter()
            .map(|entry| {
                let tx_id = *entry.key();
                let state = TxnStateSnapshot::from(*entry.value());
                (tx_id, state)
            })
            .collect();
        out.sort_by_key(|p| p.0);
        out
    }

    /// Current value of `MvStore::tx_ids` (the next-tx-id allocator).
    /// Reads with `Acquire` to align with the allocator's `fetch_add`.
    pub fn diff_tx_ids_value(&self) -> u64 {
        self.tx_ids.load(Ordering::Acquire)
    }

    /// Current value of `MvStore::version_id_counter` (the next-
    /// version-id allocator). Reads with `Acquire` for the same
    /// reason as `diff_tx_ids_value`.
    pub fn diff_version_id_counter_value(&self) -> u64 {
        self.version_id_counter.load(Ordering::Acquire)
    }

    /// Look up the commit timestamp for `tx_id`. Checks
    /// `finalized_tx_states` first (where committed txns live after
    /// the commit-state-machine drives to terminal), falling back to
    /// the live `txs` table (where a committed tx may still sit for a
    /// short window before being moved). Returns `None` if the tx is
    /// not found, was aborted, or has not yet committed.
    pub fn diff_commit_ts(&self, tx_id: TxID) -> Option<u64> {
        if let Some(entry) = self.finalized_tx_states.get(&tx_id) {
            if let TransactionState::Committed(ts) = *entry.value() {
                return Some(ts);
            }
        }
        if let Some(entry) = self.txs.get(&tx_id) {
            let encoded = entry.value().state.state.load(Ordering::Acquire);
            if let TransactionState::Committed(ts) =
                TransactionState::decode(encoded)
            {
                return Some(ts);
            }
        }
        None
    }
}
