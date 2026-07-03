//! Repro: wal_commit_requires_fsync-563dc58c
//!
//! Conductor session/evidence: https://code.aretta.ai/conductor/dashboard (job repro-8643ada870087d23)
//! SUT commit: 7b6cbaec04e86c0d9ac47819c77444af5054c50a
//! Generated: 2026-07-03T07:22:01.612383691Z
//!
//! Property (wal_commit_requires_fsync @ core/storage/wal.rs): a commit frame
//! must reach stable storage via fsync before the transaction is reported as
//! durable. `write_frame_raw` issues the commit-frame bytes to the WAL with a
//! plain pwrite and returns without an fsync, so turso acknowledges the commit
//! while the WAL bytes are still only in volatile (un-synced) storage. A power
//! loss after the write but before that cache is flushed loses a transaction
//! that was already reported committed.
//!
//! This test models a power loss with a self-contained, crash-consistent IO
//! (built over the SUT's public `turso_core::IO` / `turso_core::File`
//! abstractions): a write lands only in a file's *volatile* image; an fsync is
//! what promotes the volatile image into the *durable* image. Simulating a
//! power loss keeps only the durable (fsync'd) bytes. We commit a row, cut
//! power, reopen from the durable image, and read back the surviving row count
//! from the SUT. Correct behavior (and SQLite's) is that a committed row
//! survives the crash; today it is lost because the commit was never fsync'd.

// The test name mirrors the violation id (hex suffix), which is not snake_case.
#![allow(non_snake_case)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use turso_core::io::{FileId, FileSyncType};
use turso_core::{
    Buffer, Clock, Completion, Database, DatabaseOpts, File, MemoryIO, MonotonicInstant, OpenFlags,
    WallClockInstant, IO,
};

/// Per-file crash-consistency state.
///
/// `volatile` is what the running database can read back immediately after a
/// write (mirrors the OS page cache). `durable` is what survives a power loss;
/// it only advances to match `volatile` when the file is fsync'd.
#[derive(Default)]
struct FileState {
    volatile: Vec<u8>,
    durable: Vec<u8>,
}

struct CrashFile {
    state: Mutex<FileState>,
}

impl File for CrashFile {
    fn lock_file(&self, _exclusive: bool) -> turso_core::Result<()> {
        Ok(())
    }

    fn unlock_file(&self) -> turso_core::Result<()> {
        Ok(())
    }

    fn pread(&self, pos: u64, c: Completion) -> turso_core::Result<Completion> {
        let read = c.as_read();
        let buf = read.buf();
        let buf_len = buf.len();
        if buf_len == 0 {
            c.complete(0);
            return Ok(c);
        }
        let state = self.state.lock().unwrap();
        let pos = pos as usize;
        if pos >= state.volatile.len() {
            c.complete(0);
            return Ok(c);
        }
        let read_len = buf_len.min(state.volatile.len() - pos);
        buf.as_mut_slice()[..read_len].copy_from_slice(&state.volatile[pos..pos + read_len]);
        c.complete(read_len as i32);
        Ok(c)
    }

    fn pwrite(
        &self,
        pos: u64,
        buffer: Arc<Buffer>,
        c: Completion,
    ) -> turso_core::Result<Completion> {
        let data = buffer.as_slice();
        write_volatile(&self.state, pos as usize, data);
        c.complete(data.len() as i32);
        Ok(c)
    }

    fn pwritev(
        &self,
        pos: u64,
        buffers: Vec<Arc<Buffer>>,
        c: Completion,
    ) -> turso_core::Result<Completion> {
        let mut offset = pos as usize;
        let mut total = 0usize;
        for buffer in buffers {
            let data = buffer.as_slice();
            write_volatile(&self.state, offset, data);
            offset += data.len();
            total += data.len();
        }
        c.complete(total as i32);
        Ok(c)
    }

    fn sync(&self, c: Completion, _sync_type: FileSyncType) -> turso_core::Result<Completion> {
        // fsync: promote the volatile image to the durable image. Only after
        // this point are the written bytes guaranteed to survive a power loss.
        let mut state = self.state.lock().unwrap();
        state.durable = state.volatile.clone();
        c.complete(0);
        Ok(c)
    }

    fn truncate(&self, len: u64, c: Completion) -> turso_core::Result<Completion> {
        let mut state = self.state.lock().unwrap();
        state.volatile.resize(len as usize, 0);
        c.complete(0);
        Ok(c)
    }

    fn size(&self) -> turso_core::Result<u64> {
        Ok(self.state.lock().unwrap().volatile.len() as u64)
    }
}

fn write_volatile(state: &Mutex<FileState>, pos: usize, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let mut state = state.lock().unwrap();
    let end = pos + data.len();
    if state.volatile.len() < end {
        state.volatile.resize(end, 0);
    }
    state.volatile[pos..end].copy_from_slice(data);
}

/// A crash-consistent in-memory IO. Reads/writes see the volatile image;
/// `sync` promotes volatile -> durable per file; `power_loss` discards every
/// un-synced write by rebuilding a fresh IO seeded only from durable bytes.
struct CrashIo {
    files: Mutex<HashMap<String, Arc<CrashFile>>>,
    // Used solely for clock / RNG plumbing required by the IO trait; never
    // touches the real filesystem.
    clock: Arc<MemoryIO>,
}

impl CrashIo {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            files: Mutex::new(HashMap::new()),
            clock: Arc::new(MemoryIO::new()),
        })
    }

    /// Model a power loss: build a brand-new IO whose files contain only the
    /// durable (fsync'd) bytes captured so far. Anything written but not
    /// fsync'd is gone.
    fn power_loss(&self) -> Arc<Self> {
        let files = self.files.lock().unwrap();
        let mut recovered = HashMap::new();
        for (path, file) in files.iter() {
            let durable = file.state.lock().unwrap().durable.clone();
            recovered.insert(
                path.clone(),
                Arc::new(CrashFile {
                    state: Mutex::new(FileState {
                        volatile: durable.clone(),
                        durable,
                    }),
                }),
            );
        }
        Arc::new(Self {
            files: Mutex::new(recovered),
            clock: Arc::new(MemoryIO::new()),
        })
    }
}

impl Clock for CrashIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.clock.current_time_monotonic()
    }

    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.clock.current_time_wall_clock()
    }
}

impl IO for CrashIo {
    fn open_file(
        &self,
        path: &str,
        flags: OpenFlags,
        _direct: bool,
    ) -> turso_core::Result<Arc<dyn File>> {
        let mut files = self.files.lock().unwrap();
        if !files.contains_key(path) {
            if !flags.contains(OpenFlags::Create) {
                return Err(turso_core::LimboError::InternalError(format!(
                    "file not found: {path}"
                )));
            }
            files.insert(
                path.to_string(),
                Arc::new(CrashFile {
                    state: Mutex::new(FileState::default()),
                }),
            );
        }
        Ok(files.get(path).unwrap().clone() as Arc<dyn File>)
    }

    fn remove_file(&self, path: &str) -> turso_core::Result<()> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }

    fn file_id(&self, path: &str) -> turso_core::Result<FileId> {
        Ok(FileId::from_path_hash(path))
    }

    fn get_memory_io(&self) -> Arc<MemoryIO> {
        self.clock.clone()
    }
}

/// Read back `SELECT count(*) FROM t`, treating a lost schema (missing table)
/// as `0` surviving rows — both outcomes are the durability violation.
fn surviving_rows(conn: &Arc<turso_core::Connection>) -> i64 {
    let mut stmt = match conn.prepare("SELECT count(*) FROM t") {
        Ok(stmt) => stmt,
        Err(_) => return 0,
    };
    let mut count = 0i64;
    let ran = stmt.run_with_row_callback(|row| {
        count = row.get::<i64>(0).unwrap_or(0);
        Ok(())
    });
    if ran.is_err() {
        return 0;
    }
    count
}

#[test]
fn wal_commit_requires_fsync__563dc58c() {
    let db_path = "wal_commit_requires_fsync.db";
    let io = CrashIo::new();

    // Phase 1: single connection, sequential workload. Create a table and
    // commit one row. Each statement autocommits.
    let db = Database::open_file_with_flags(
        io.clone() as Arc<dyn IO>,
        db_path,
        OpenFlags::default(),
        DatabaseOpts::new(),
        None,
    )
    .expect("open db");
    let conn = db.connect().expect("connect");
    conn.execute("PRAGMA journal_mode=WAL").expect("wal mode");
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .expect("create table");
    conn.execute("INSERT INTO t (v) VALUES (1)")
        .expect("insert");

    // The SUT has acknowledged the commit: the row is visible pre-crash.
    assert_eq!(
        surviving_rows(&conn),
        1,
        "pre-crash sanity: the committed row must be visible before power loss"
    );

    // Phase 2: power loss BEFORE dropping the pre-crash handles, so no
    // shutdown-time flush can retroactively make the commit durable. Keep only
    // fsync'd bytes.
    let recovered_io = io.power_loss();
    drop(conn);
    drop(db);

    // Phase 3: reopen from the post-power-loss durable image and read back the
    // surviving row count directly from the SUT (causal-downstream value).
    let db2 = Database::open_file_with_flags(
        recovered_io.clone() as Arc<dyn IO>,
        db_path,
        OpenFlags::default(),
        DatabaseOpts::new(),
        None,
    )
    .expect("reopen db");
    let conn2 = db2.connect().expect("reconnect");
    let recovered = surviving_rows(&conn2);

    assert_eq!(
        recovered, 1,
        "committed row lost across power loss + reopen: the INSERT was acknowledged as \
         committed but its WAL commit frame was never fsync'd, so it did not reach stable \
         storage (WAL durability violation); expected 1 surviving row, got {recovered} \
         [repro:wal_commit_requires_fsync-563dc58c] [divergence:value|expected=true|actual=false]"
    );
}
