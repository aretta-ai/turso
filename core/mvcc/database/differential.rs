//! Differential-testing accessors for `MvStore`.
//!
//! Only compiled under `--features differential-accessors`. Exposes
//! owned snapshots of private `MvStore` internals (`txs`,
//! `finalized_tx_states`, the version-id / tx-id counters, a
//! `commit_ts` lookup, and the recovered `sqlite_schema` row versions
//! in `rows`) for use by the Aretta Books MVCC conformance harness at
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
use crate::mvcc::database::{
    MvStore, RowID, RowKey, SQLITE_SCHEMA_MVCC_TABLE_ID, Transaction, TransactionState, TxID,
};
use crate::sync::atomic::Ordering;
use crossbeam_skiplist::SkipMap;

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

/// One recovered `sqlite_schema` row version, projected for the #6005
/// decodability coordinate (ACCESSORS.md row 12) — the harness's window
/// onto the schema row versions held in `MvStore::rows` AFTER
/// `maybe_recover_logical_log` (read PRE-checkpoint, before any GC).
/// `payload_empty` is the EXACT discriminator the #6005 fix keys on in
/// `sqlite_schema_btree_identity` (`if version.row.payload().is_empty()
/// { return None }`); `ended` records whether the version is a tombstone
/// (delete) at read time. Lets the DT judge #6005 via the decodability
/// model coordinate instead of an out-of-band checkpoint panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveredSchemaRecord {
    pub rowid: i64,
    pub payload_empty: bool,
    pub ended: bool,
}

/// Project a live `Transaction` into a `TxnSnapshot` — used by the
/// macro-generated `MvStore::inspect_txs` accessor (ACCESSORS.md row 1).
/// Reads the atomic state slot with `Acquire`, mirrors `TransactionState`
/// via `TxnStateSnapshot::from`, locks the write-set Mutex to copy
/// `RowID`s out, and sorts the resulting list. The per-element sort is
/// part of the projection contract — the Lean side performs the same
/// canonicalization. The outer (by-key) sort is the consumer's
/// responsibility post-migration; `aristo::instrument::Inspect` does
/// not pre-sort.
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
/// `MvStore::inspect_finalized` accessor (ACCESSORS.md row 2). Trivial
/// Copy-deref into the existing `From<TransactionState>` impl.
impl From<&TransactionState> for FinalStateSnapshot {
    fn from(s: &TransactionState) -> Self {
        TxnStateSnapshot::from(*s)
    }
}

/// Projector for the macro-generated `MvStore::inspect_txs` accessor.
/// Walks the live `txs` `SkipMap` and projects each live `Transaction`
/// into an owned `TxnSnapshot` via the `From<&Transaction>` impl above.
/// The outer (by-key) order follows `SkipMap`'s sorted iteration; the
/// consumer is responsible for any further canonicalization.
pub(super) fn project_txs(m: &SkipMap<TxID, Transaction>) -> Vec<(TxID, TxnSnapshot)> {
    m.iter()
        .map(|e| (*e.key(), TxnSnapshot::from(e.value())))
        .collect()
}

/// Projector for the macro-generated `MvStore::inspect_finalized`
/// accessor. Walks the `finalized_tx_states` `SkipMap` and projects each
/// `TransactionState` into an owned `FinalStateSnapshot` via the
/// `From<&TransactionState>` impl above.
pub(super) fn project_finalized(
    m: &SkipMap<TxID, TransactionState>,
) -> Vec<(TxID, FinalStateSnapshot)> {
    m.iter()
        .map(|e| (*e.key(), FinalStateSnapshot::from(e.value())))
        .collect()
}

impl<Clock: LogicalClock> MvStore<Clock> {
    /// Current value of `MvStore::tx_ids` (the next-tx-id allocator).
    /// Reads with `Acquire` to align with the allocator's `fetch_add`.
    pub fn inspect_tx_ids_value(&self) -> u64 {
        self.tx_ids.load(Ordering::Acquire)
    }

    /// Current value of `MvStore::version_id_counter` (the next-
    /// version-id allocator). Reads with `Acquire` for the same
    /// reason as `inspect_tx_ids_value`.
    pub fn inspect_version_id_counter_value(&self) -> u64 {
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
    pub fn inspect_peek_next_ts(&self) -> u64 {
        self.last_committed_tx_ts.load(Ordering::Acquire) + 1
    }

    /// Look up the commit timestamp for `tx_id`. Checks
    /// `finalized_tx_states` first (where committed txns live after
    /// the commit-state-machine drives to terminal), falling back to
    /// the live `txs` table (where a committed tx may still sit for a
    /// short window before being moved). Returns `None` if the tx is
    /// not found, was aborted, or has not yet committed.
    pub fn inspect_commit_ts(&self, tx_id: TxID) -> Option<u64> {
        if let Some(entry) = self.finalized_tx_states.get(&tx_id) {
            if let TransactionState::Committed(ts) = *entry.value() {
                return Some(ts);
            }
        }
        if let Some(entry) = self.txs.get(&tx_id) {
            let encoded = entry.value().state.state.load(Ordering::Acquire);
            if let TransactionState::Committed(ts) = TransactionState::decode(encoded) {
                return Some(ts);
            }
        }
        None
    }

    /// Snapshot the recovered `sqlite_schema` row versions held in
    /// `MvStore::rows` (ACCESSORS.md row 12). Walks every resident key,
    /// keeps only the `sqlite_schema` MVCC table
    /// (`table_id == SQLITE_SCHEMA_MVCC_TABLE_ID`, `-1`) with an integer
    /// `RowKey`, and emits one `RecoveredSchemaRecord` per `RowVersion`
    /// in the chain (per-version read lock taken transiently). Read
    /// PRE-checkpoint / pre-GC so an empty-payload tombstone synthesized
    /// by `maybe_recover_logical_log` is still observable — the #6005
    /// decodability coordinate (`records.all(|r| !r.payload_empty)`).
    pub fn inspect_recovered_schema_records(&self) -> Vec<RecoveredSchemaRecord> {
        let mut records = Vec::new();
        for entry in self.rows.iter() {
            let id = entry.key();
            if id.table_id != SQLITE_SCHEMA_MVCC_TABLE_ID {
                continue;
            }
            let rowid = match &id.row_id {
                RowKey::Int(rowid) => *rowid,
                RowKey::Record(_) => continue,
            };
            for version in entry.value().read().iter() {
                records.push(RecoveredSchemaRecord {
                    rowid,
                    payload_empty: version.row.payload().is_empty(),
                    ended: version.end.is_some(),
                });
            }
        }
        records
    }
}
