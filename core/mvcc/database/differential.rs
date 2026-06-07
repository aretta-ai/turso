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
use crate::mvcc::database::{MvStore, RowID, Transaction, TransactionState, TxID};
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

/// Project a live `Transaction` into a `TxnSnapshot` — used by the
/// macro-generated `MvStore::diff_txs` accessor (ACCESSORS.md row 1).
/// Reads the atomic state slot with `Acquire`, mirrors `TransactionState`
/// via `TxnStateSnapshot::from`, locks the write-set Mutex to copy
/// `RowID`s out, and sorts the resulting list. The sort is part of the
/// catalog contract — the Lean side performs the same canonicalization.
impl From<&Transaction> for TxnSnapshot {
    fn from(tx: &Transaction) -> Self {
        // `AtomicTransactionState::state` is `pub(crate)`; child-module
        // visibility lets us read it directly without consuming the
        // atomic via `From`.
        let encoded = tx.state.state.load(Ordering::Acquire);
        let state = TxnStateSnapshot::from(TransactionState::decode(encoded));
        let begin_ts = tx.begin_ts;
        let mut write_set: Vec<RowID> = tx
            .write_set
            .lock()
            .entries
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        write_set.sort();
        TxnSnapshot {
            state,
            begin_ts,
            write_set,
        }
    }
}

/// Project a borrowed `TransactionState` into a `FinalStateSnapshot`
/// (= `TxnStateSnapshot`) — used by the macro-generated
/// `MvStore::diff_finalized` accessor (ACCESSORS.md row 2). Trivial
/// Copy-deref into the existing `From<TransactionState>` impl.
impl From<&TransactionState> for FinalStateSnapshot {
    fn from(s: &TransactionState) -> Self {
        TxnStateSnapshot::from(*s)
    }
}

impl<Clock: LogicalClock> MvStore<Clock> {
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

    /// Peek the next commit timestamp the clock would publish, without
    /// consuming it. Mirrors the semantic of the Lean model's
    /// `MvccState.nextTs` — "the smallest timestamp the next commit
    /// could plausibly be assigned, given everything that has already
    /// committed".
    ///
    /// Implementation: derived from `last_committed_tx_ts + 1`. We do
    /// NOT peek `MvccClock`'s internal `Mutex<u64>` because the clock
    /// burns timestamps on `begin_tx` as well as `commit_tx`, while
    /// Lean's `nextTs` advances only on commit. The
    /// `last_committed_tx_ts` atomic is published by `commit_tx`
    /// alongside the durable commit so it tracks the same monotone
    /// landmark Lean does.
    ///
    /// Note: in the empty state both this accessor and Lean's
    /// `nextTs` are 0 only if no commit has happened. The accessor
    /// returns `last_committed_tx_ts + 1` (so after a commit at ts=1
    /// it returns 2, matching Lean's `nextTs := max(nextTs, ts+1)`).
    /// To make the after-begin boundary line up, the Lean model's
    /// `Op.begin` handler maxes `nextTs` against `extBeginTs + 1` —
    /// see `Step.lean::Op.begin`.
    pub fn diff_peek_next_ts(&self) -> u64 {
        self.last_committed_tx_ts.load(Ordering::Acquire) + 1
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
