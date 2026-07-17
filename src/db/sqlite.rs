use super::{DepositQueueCounts, Erc20Deposit, WebhookDeliveryRecord, WriteQueueError};
use anyhow::{anyhow, Result};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use std::any::Any;
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Result payload flowing back from the writer thread. Type-erased because a
/// single command channel carries closures with heterogeneous return types.
type WriteResult = Result<Box<dyn Any + Send>>;

/// A write closure must be `Fn` (not `FnOnce`): when a batched transaction
/// fails, the writer rolls the batch back and re-executes each command
/// individually, so a command may run more than once.
type WriteFn = Box<dyn Fn(&Connection) -> WriteResult + Send>;

struct WriteCommand {
    run: WriteFn,
    reply: SyncSender<WriteResult>,
    enqueued_at: Instant,
}

/// Tuning for the single-writer actor. All knobs are env-overridable with
/// safe defaults, so no deployment config change is required.
#[derive(Clone, Debug)]
pub struct WriterConfig {
    /// Max time an interactive caller waits for its write result
    /// (`EVM_WRITE_TIMEOUT_MS`, default 5000).
    pub write_timeout: Duration,
    /// Interactive lane capacity (`EVM_INTERACTIVE_QUEUE_CAPACITY`, default 64).
    pub interactive_capacity: usize,
    /// Background lane capacity (`EVM_BACKGROUND_QUEUE_CAPACITY`, default 2048).
    pub background_capacity: usize,
    /// Max background commands grouped into one transaction
    /// (`EVM_BACKGROUND_BATCH_SIZE`, default 50).
    pub batch_size: usize,
    /// Minimum time between opportunistic runtime `wal_checkpoint(PASSIVE)`
    /// attempts (`EVM_CHECKPOINT_INTERVAL_SECS`, default 30). Keeps the WAL
    /// from growing unbounded under sustained load without adding a
    /// checkpoint after every single background batch.
    pub checkpoint_interval: Duration,
    /// Abort the process if the writer thread panics (always true in
    /// production; disabled only by writer-death unit tests).
    pub abort_on_panic: bool,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            write_timeout: Duration::from_millis(5000),
            interactive_capacity: 64,
            background_capacity: 2048,
            batch_size: 50,
            checkpoint_interval: Duration::from_secs(30),
            abort_on_panic: true,
        }
    }
}

impl WriterConfig {
    pub fn from_env() -> Self {
        fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        let d = Self::default();
        Self {
            write_timeout: Duration::from_millis(env_parse(
                "EVM_WRITE_TIMEOUT_MS",
                d.write_timeout.as_millis() as u64,
            )),
            interactive_capacity: env_parse(
                "EVM_INTERACTIVE_QUEUE_CAPACITY",
                d.interactive_capacity,
            ),
            background_capacity: env_parse("EVM_BACKGROUND_QUEUE_CAPACITY", d.background_capacity),
            batch_size: env_parse("EVM_BACKGROUND_BATCH_SIZE", d.batch_size).max(1),
            checkpoint_interval: Duration::from_secs(env_parse(
                "EVM_CHECKPOINT_INTERVAL_SECS",
                d.checkpoint_interval.as_secs(),
            )),
            abort_on_panic: true,
        }
    }
}

/// Warn when an interactive write sat in the queue longer than this before
/// the writer picked it up.
const INTERACTIVE_WAIT_WARN: Duration = Duration::from_millis(500);

enum Lane {
    Interactive,
    Background,
}

struct LaneQueues {
    interactive: VecDeque<WriteCommand>,
    background: VecDeque<WriteCommand>,
    /// False once the writer thread is gone (or is being asked to exit).
    writer_alive: bool,
    /// True only for the graceful path where the last `Db` clone was dropped;
    /// distinguishes teardown from an unexpected writer death.
    shutting_down: bool,
}

/// The two-lane work queue feeding the single writer thread.
///
/// Hand-rolled on `Mutex`+`Condvar` rather than channels because the writer
/// must (a) wait on both lanes at once with strict interactive priority, and
/// (b) peek the interactive lane cheaply between batched background commands.
/// Neither std nor tokio mpsc channels can express that without polling.
struct WriteQueue {
    lanes: Mutex<LaneQueues>,
    /// Signaled when work arrives (or on shutdown); waited on by the writer.
    work_available: Condvar,
    /// Signaled when background capacity frees up (or on shutdown); waited on
    /// by blocked background producers.
    space_available: Condvar,
    interactive_capacity: usize,
    background_capacity: usize,
}

impl WriteQueue {
    fn new(interactive_capacity: usize, background_capacity: usize) -> Self {
        Self {
            lanes: Mutex::new(LaneQueues {
                interactive: VecDeque::new(),
                background: VecDeque::new(),
                writer_alive: true,
                shutting_down: false,
            }),
            work_available: Condvar::new(),
            space_available: Condvar::new(),
            interactive_capacity,
            background_capacity,
        }
    }

    fn lock(&self) -> MutexGuard<'_, LaneQueues> {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn try_push_interactive(&self, cmd: WriteCommand) -> std::result::Result<(), WriteQueueError> {
        let mut lanes = self.lock();
        if !lanes.writer_alive {
            return Err(WriteQueueError::WriterGone);
        }
        if lanes.interactive.len() >= self.interactive_capacity {
            return Err(WriteQueueError::QueueFull);
        }
        lanes.interactive.push_back(cmd);
        self.work_available.notify_one();
        Ok(())
    }

    /// Blocking push: waits for capacity instead of dropping. Correctness
    /// requirement for the monitor — if a `record_deposit` were dropped but
    /// the chunk's `set_last_processed_block` later succeeded, the cursor
    /// would advance past an unrecorded deposit and lose it permanently.
    fn push_background(&self, cmd: WriteCommand) -> std::result::Result<(), WriteQueueError> {
        let mut lanes = self.lock();
        while lanes.writer_alive && lanes.background.len() >= self.background_capacity {
            lanes = self
                .space_available
                .wait(lanes)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if !lanes.writer_alive {
            return Err(WriteQueueError::WriterGone);
        }
        lanes.background.push_back(cmd);
        self.work_available.notify_one();
        Ok(())
    }

    /// Writer side: blocks until work arrives. Returns `None` when the queue
    /// has been shut down / marked dead, which is the writer's exit signal.
    fn pop_blocking(&self) -> Option<(WriteCommand, Lane)> {
        let mut lanes = self.lock();
        loop {
            if !lanes.writer_alive {
                return None;
            }
            if let Some(cmd) = lanes.interactive.pop_front() {
                return Some((cmd, Lane::Interactive));
            }
            if let Some(cmd) = lanes.background.pop_front() {
                self.space_available.notify_one();
                return Some((cmd, Lane::Background));
            }
            lanes = self
                .work_available
                .wait(lanes)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn try_pop_background(&self) -> Option<WriteCommand> {
        let mut lanes = self.lock();
        let cmd = lanes.background.pop_front();
        if cmd.is_some() {
            self.space_available.notify_one();
        }
        cmd
    }

    fn has_interactive(&self) -> bool {
        !self.lock().interactive.is_empty()
    }

    fn depths(&self) -> (usize, usize) {
        let lanes = self.lock();
        (lanes.interactive.len(), lanes.background.len())
    }

    /// Graceful teardown (last `Db` clone dropped): the writer exits its loop
    /// and the death handler treats it as expected (no abort).
    fn shutdown(&self) {
        let mut lanes = self.lock();
        lanes.writer_alive = false;
        lanes.shutting_down = true;
        self.work_available.notify_all();
        self.space_available.notify_all();
    }

    /// Unexpected-death signal (also used by tests to simulate writer death
    /// without a panic). Producers are woken with `WriterGone`.
    fn mark_writer_dead(&self) {
        let mut lanes = self.lock();
        lanes.writer_alive = false;
        self.work_available.notify_all();
        self.space_available.notify_all();
    }

    fn is_shutting_down(&self) -> bool {
        self.lock().shutting_down
    }
}

/// Marks the queue shut down when the last `Db` clone is dropped, so writer
/// threads (and their open connections) don't leak — mainly relevant for
/// tests; the production `Db` lives for the whole process.
struct WriterShutdown {
    queue: Arc<WriteQueue>,
}

impl Drop for WriterShutdown {
    fn drop(&mut self) {
        self.queue.shutdown();
    }
}

/// Cloneable handle to the dedicated writer thread.
///
/// Design note: this is a hand-rolled single-writer actor. `tokio-rusqlite`
/// and `rusqlite-isle` implement the base "one thread owns the Connection"
/// pattern, but neither provides the priority lanes or transaction batching
/// that are the substance of this change, so we own the implementation
/// instead of wrapping a crate and re-implementing the interesting parts.
#[derive(Clone)]
struct WriterHandle {
    queue: Arc<WriteQueue>,
    /// Flipped false when the writer thread dies; surfaced through
    /// `Db::writer_healthy` and the service health endpoint.
    healthy: Arc<AtomicBool>,
    write_timeout: Duration,
    _shutdown: Arc<WriterShutdown>,
}

#[derive(Clone)]
pub struct Db {
    writer: WriterHandle,
    read: Pool<SqliteConnectionManager>,
}

/// Strip `sqlite:` scheme; rusqlite expects a filesystem path.
pub fn normalize_db_path(database_url: &str) -> &str {
    database_url.strip_prefix("sqlite:").unwrap_or(database_url)
}

pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("../../migrations/V1__initial.sql")),
        M::up(include_str!("../../migrations/V2__webhook_deliveries.sql")),
        M::up(include_str!("../../migrations/V3__next_index_counter.sql")),
    ])
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    // synchronous=NORMAL: in WAL mode this skips the per-commit fsync (only
    // checkpoints sync). On EFS every fsync is a network round-trip, so this
    // is the single biggest write-latency lever. Tradeoff: on power loss /
    // hard crash the last few commits may roll back, but the DB stays
    // consistent and all affected data is recoverable (registers are retried
    // by callers, deposits re-detected from the last block cursor).
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA synchronous=NORMAL;",
    )?;
    Ok(())
}

pub fn apply_pragmas_for_import(conn: &Connection) -> Result<()> {
    apply_pragmas(conn).map_err(Into::into)
}

/// Result columns of `PRAGMA wal_checkpoint(..)`: `busy` is non-zero if the
/// checkpoint could not fully complete because of a concurrent reader/writer,
/// `log_frames` is the WAL size in frames at the time of the call, and
/// `checkpointed_frames` is how many of those were moved into the main
/// database file (for `TRUNCATE`, a fully successful checkpoint truncates the
/// WAL to zero afterward; for `PASSIVE`, only what could be moved without
/// blocking is).
struct WalCheckpointResult {
    busy: i64,
    log_frames: i64,
    checkpointed_frames: i64,
}

fn run_wal_checkpoint(conn: &Connection, mode: &str) -> rusqlite::Result<WalCheckpointResult> {
    conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
        Ok(WalCheckpointResult {
            busy: row.get(0)?,
            log_frames: row.get(1)?,
            checkpointed_frames: row.get(2)?,
        })
    })
}

/// Size in bytes of the `-wal` sidecar file, if present. `None` once the WAL
/// has been fully checkpointed away (SQLite may delete or zero it) or if the
/// path can't be stat'd for any other reason — purely informational logging,
/// never treated as an error.
fn wal_file_size_bytes(db_path: &str) -> Option<u64> {
    std::fs::metadata(format!("{db_path}-wal"))
        .ok()
        .map(|m| m.len())
}

/// Forces a full checkpoint before the writer starts serving traffic, so
/// every restart begins from a small WAL regardless of how large it grew in
/// the previous run. Without this, restarting alone does not help: SQLite
/// simply reopens the same oversized `-wal` file and resumes fighting
/// auto-checkpoint attempts against it on every commit (the root cause of the
/// 2026-07 write-queue-saturation incident — see docs/fix-p0-* history).
fn checkpoint_startup(conn: &Connection, db_path: &str) -> Result<()> {
    let before_bytes = wal_file_size_bytes(db_path);
    let result = run_wal_checkpoint(conn, "TRUNCATE")?;
    let after_bytes = wal_file_size_bytes(db_path);
    tracing::info!(
        wal_bytes_before = ?before_bytes,
        wal_bytes_after = ?after_bytes,
        busy = result.busy,
        log_frames = result.log_frames,
        checkpointed_frames = result.checkpointed_frames,
        "startup WAL checkpoint complete"
    );
    if result.busy != 0 {
        tracing::warn!(
            log_frames = result.log_frames,
            checkpointed_frames = result.checkpointed_frames,
            "startup WAL checkpoint did not fully complete (busy); WAL may still be large"
        );
    }
    Ok(())
}

/// Opportunistic, non-blocking runtime checkpoint: called from the writer
/// thread after a background batch commits. Only runs when nothing
/// interactive is waiting and at least `interval` has passed since the last
/// attempt, so it never competes with request latency and never runs on
/// every single batch. `PASSIVE` mode never blocks concurrent readers or
/// writers, so it's safe to call from the single writer thread with no extra
/// locking.
///
/// Returns the checkpoint outcome when an attempt was actually made (mainly
/// so tests can assert on it deterministically); production callers ignore
/// it. Note `PASSIVE` never truncates the physical `-wal` file — unlike
/// `TRUNCATE`, its "before/after WAL bytes" log fields are expected to be
/// equal even on a fully successful checkpoint; `log_frames`/
/// `checkpointed_frames` are the meaningful signal here instead.
fn maybe_checkpoint(
    conn: &Connection,
    queue: &WriteQueue,
    db_path: &str,
    interval: Duration,
    last_checkpoint: &mut Instant,
) -> Option<WalCheckpointResult> {
    if queue.has_interactive() || last_checkpoint.elapsed() < interval {
        return None;
    }
    *last_checkpoint = Instant::now();

    let before_bytes = wal_file_size_bytes(db_path);
    match run_wal_checkpoint(conn, "PASSIVE") {
        Ok(result) => {
            let after_bytes = wal_file_size_bytes(db_path);
            tracing::info!(
                wal_bytes_before = ?before_bytes,
                wal_bytes_after = ?after_bytes,
                busy = result.busy,
                log_frames = result.log_frames,
                checkpointed_frames = result.checkpointed_frames,
                "opportunistic runtime WAL checkpoint"
            );
            Some(result)
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "opportunistic runtime WAL checkpoint failed"
            );
            None
        }
    }
}

/// Parse `"0xtx:42"` -> (`0xtx`, 42). Bare `"0xtx"` -> (`0xtx`, 0).
fn parse_local_key(local_key: &str) -> Result<(String, i64)> {
    if let Some((tx, idx)) = local_key.rsplit_once(':') {
        if !idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit()) {
            return Ok((tx.to_string(), idx.parse()?));
        }
    }
    Ok((local_key.to_string(), 0))
}

fn last_block_key(chain: &str) -> String {
    format!("last_block:{chain}")
}

/// r2d2's own default when `.max_size(..)` is not set on the pool builder.
/// Used by `Db::new` so existing callers (in particular the ~50 test call
/// sites that construct a `Db` directly) keep their current behavior.
const DEFAULT_READ_POOL_MAX_SIZE: u32 = 10;

/// Spawns the dedicated writer thread that exclusively owns the write
/// `Connection`. Returns the cloneable handle used by `Db`.
fn spawn_writer(conn: Connection, db_path: String, cfg: WriterConfig) -> WriterHandle {
    let queue = Arc::new(WriteQueue::new(
        cfg.interactive_capacity,
        cfg.background_capacity,
    ));
    let healthy = Arc::new(AtomicBool::new(true));

    let thread_queue = Arc::clone(&queue);
    let thread_healthy = Arc::clone(&healthy);
    let batch_size = cfg.batch_size;
    let checkpoint_interval = cfg.checkpoint_interval;
    let abort_on_panic = cfg.abort_on_panic;

    std::thread::Builder::new()
        .name("evmhot-sqlite-writer".to_string())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                writer_loop(
                    &conn,
                    &thread_queue,
                    batch_size,
                    &db_path,
                    checkpoint_interval,
                );
            }));
            let graceful = thread_queue.is_shutting_down() && outcome.is_ok();
            thread_healthy.store(false, Ordering::SeqCst);
            thread_queue.mark_writer_dead();
            if !graceful {
                // A half-alive process that serves reads but can never write
                // again is worse than a restart (silent-zombie failure mode).
                // Aborting lets ECS restart the single task cleanly.
                tracing::error!(
                    panicked = outcome.is_err(),
                    "SQLite writer thread died unexpectedly; database writes are impossible"
                );
                if abort_on_panic {
                    std::process::abort();
                }
            }
        })
        .expect("failed to spawn SQLite writer thread");

    WriterHandle {
        queue: Arc::clone(&queue),
        healthy,
        write_timeout: cfg.write_timeout,
        _shutdown: Arc::new(WriterShutdown { queue }),
    }
}

/// Core loop of the writer thread. Interactive commands always jump ahead;
/// background commands are grouped into batched transactions (fewer commits
/// means fewer WAL appends, which matters on EFS where each fsync is a
/// network round-trip).
fn writer_loop(
    conn: &Connection,
    queue: &WriteQueue,
    batch_size: usize,
    db_path: &str,
    checkpoint_interval: Duration,
) {
    // Owned by this thread alone (the writer), so no locking is needed even
    // though it's mutated on every background batch.
    let mut last_checkpoint = Instant::now();
    while let Some((cmd, lane)) = queue.pop_blocking() {
        match lane {
            Lane::Interactive => execute_interactive(conn, queue, cmd),
            Lane::Background => run_background_batch(
                conn,
                queue,
                cmd,
                batch_size,
                db_path,
                checkpoint_interval,
                &mut last_checkpoint,
            ),
        }
    }
}

fn execute_interactive(conn: &Connection, queue: &WriteQueue, cmd: WriteCommand) {
    let waited = cmd.enqueued_at.elapsed();
    if waited >= INTERACTIVE_WAIT_WARN {
        let (interactive_depth, background_depth) = queue.depths();
        tracing::warn!(
            waited_ms = waited.as_millis() as u64,
            interactive_depth,
            background_depth,
            "interactive write waited unusually long in the queue"
        );
    }
    let result = (cmd.run)(conn);
    // The receiver may be gone if the caller timed out; the write still
    // executed (at-least-once semantics — interactive writes are idempotent).
    let _ = cmd.reply.send(result);
}

/// Executes up to `batch_size` background commands inside one
/// `BEGIN IMMEDIATE; ...; COMMIT;`. Replies are buffered and only delivered
/// after a successful commit, so a caller never sees Ok for a write that was
/// later rolled back. If anything in the batch fails, the whole batch rolls
/// back and every command is re-executed individually so only the truly
/// failing command returns an error — preserving per-command semantics.
/// Between commands the interactive lane is peeked; if something is waiting,
/// the batch commits early so interactive latency stays bounded at roughly
/// one command plus one commit.
fn run_background_batch(
    conn: &Connection,
    queue: &WriteQueue,
    first: WriteCommand,
    batch_size: usize,
    db_path: &str,
    checkpoint_interval: Duration,
    last_checkpoint: &mut Instant,
) {
    if conn.execute_batch("BEGIN IMMEDIATE").is_err() {
        // busy_timeout exhausted or similar: fall back to executing this one
        // command outside an explicit transaction.
        let result = (first.run)(conn);
        let _ = first.reply.send(result);
        return;
    }

    let mut executed: Vec<(WriteCommand, WriteResult)> = Vec::new();
    let mut failed = false;
    let mut next = Some(first);

    loop {
        let Some(cmd) = next.take() else { break };
        let result = (cmd.run)(conn);
        let is_err = result.is_err();
        executed.push((cmd, result));
        if is_err {
            failed = true;
            break;
        }
        if executed.len() >= batch_size || queue.has_interactive() {
            break;
        }
        next = queue.try_pop_background();
    }

    if failed {
        let _ = conn.execute_batch("ROLLBACK");
        reexecute_individually(conn, executed);
        return;
    }

    match conn.execute_batch("COMMIT") {
        Ok(()) => {
            for (cmd, result) in executed {
                let _ = cmd.reply.send(result);
            }
            maybe_checkpoint(conn, queue, db_path, checkpoint_interval, last_checkpoint);
        }
        Err(_) => {
            let _ = conn.execute_batch("ROLLBACK");
            reexecute_individually(conn, executed);
        }
    }
}

/// Poison-batch fallback: after a rollback, run each command on its own
/// (implicit per-statement transactions) so only the genuinely failing
/// command reports an error. Commands are `Fn`, not `FnOnce`, precisely so
/// this re-execution is possible.
fn reexecute_individually(conn: &Connection, batch: Vec<(WriteCommand, WriteResult)>) {
    for (cmd, _) in batch {
        let result = (cmd.run)(conn);
        let _ = cmd.reply.send(result);
    }
}

fn downcast_result<T: 'static>(boxed: Box<dyn Any + Send>) -> Result<T> {
    boxed
        .downcast::<T>()
        .map(|b| *b)
        .map_err(|_| anyhow!("writer returned an unexpected result type"))
}

impl Db {
    /// Only reached directly by this module's own tests now — the crate's
    /// `db::Db` facade calls `with_pool_size`/`with_options` directly since
    /// the Postgres arm needs a different construction path. Kept `pub` and
    /// unremoved so the pre-migration SQLite test suite below (moved here
    /// verbatim) needs zero edits.
    #[allow(dead_code)]
    pub fn new(database_url: &str) -> Result<Self> {
        Self::with_pool_size(database_url, DEFAULT_READ_POOL_MAX_SIZE)
    }

    /// Same as `Db::new`, but with an explicit read-pool size instead of
    /// r2d2's default of 10. Production wiring (`HotWalletService::new`)
    /// uses this so the pool size is configurable via `Config::db_read_pool_size`
    /// (env `DB_READ_POOL_SIZE`) instead of being a silent hardcoded default.
    ///
    /// That default matters because every chain's monitor, sweeper, and
    /// webhook-retry loop, plus inbound `/evm/register` calls, all share this
    /// one pool. With multiple chains configured, those background loops
    /// alone can exceed a small fixed pool under a catch-up backlog or a
    /// flaky RPC provider, producing sustained `"timed out waiting for
    /// connection"` errors even with the 5s fail-fast timeout below.
    pub fn with_pool_size(database_url: &str, max_size: u32) -> Result<Self> {
        Self::with_options(database_url, max_size, WriterConfig::from_env())
    }

    /// Full-control constructor; production goes through `with_pool_size`
    /// (env-derived `WriterConfig`), tests use this to shrink queues/timeouts.
    pub fn with_options(
        database_url: &str,
        max_size: u32,
        writer_config: WriterConfig,
    ) -> Result<Self> {
        let path = normalize_db_path(database_url);
        let mut write_conn = Connection::open(path)?;
        apply_pragmas(&write_conn)?;
        migrations().to_latest(&mut write_conn)?;
        checkpoint_startup(&write_conn, path)?;

        let manager = SqliteConnectionManager::file(path).with_init(|c| apply_pragmas(&*c));
        // Explicit, short connection_timeout: r2d2's default is 30s, which
        // means a read-pool contention spike (e.g. every connection busy
        // during a catch-up backlog) would block whichever thread called
        // `read.get()` for up to 30s. Callers on the async paths route
        // through `Db::blocking`, so that block lands on the blocking pool
        // rather than a Tokio worker thread, but failing fast is still
        // preferable to a long silent stall either way.
        let read_pool = Pool::builder()
            .max_size(max_size)
            .connection_timeout(Duration::from_secs(5))
            .build(manager)?;

        Ok(Self {
            writer: spawn_writer(write_conn, path.to_string(), writer_config),
            read: read_pool,
        })
    }

    /// False once the dedicated writer thread has died. Reads still work,
    /// but every write will fail with `WriteQueueError::WriterGone`; in
    /// production the process aborts shortly after this flips.
    pub fn writer_healthy(&self) -> bool {
        self.writer.healthy.load(Ordering::SeqCst)
    }

    /// Runs a `Db` operation on Tokio's blocking thread pool.
    ///
    /// Every `Db` method is a synchronous call: the write side blocks on the
    /// writer actor's queue (bounded by the interactive timeout or background
    /// backpressure), and the read side blocks on `r2d2::Pool::get()`.
    /// Calling them directly from an async fn risks stalling whichever Tokio
    /// worker thread happens to run the call, which can starve everything
    /// else on that runtime (see the monitor/sweeper/webhook callers). `Db`
    /// is a cheap `Clone`, so this just moves a clone onto `spawn_blocking`.
    /// Superseded at the crate boundary by `db::Db::blocking` (identical
    /// logic, generic over either backend); kept here for parity in case
    /// this module's own tests ever need it directly.
    #[allow(dead_code)]
    pub async fn blocking<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Db) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| anyhow!("Db blocking task panicked or was cancelled: {e}"))?
    }

    /// Background-lane write: blocks (backpressure) when the lane is full
    /// rather than dropping — dropping a monitor write could advance the
    /// block cursor past an unrecorded deposit and lose it permanently.
    /// Waits without a timeout for the result; the writer always executes
    /// every dequeued command.
    fn with_write<F, T>(&self, f: F) -> Result<T>
    where
        F: Fn(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = sync_channel::<WriteResult>(1);
        self.writer
            .queue
            .push_background(WriteCommand {
                run: Box::new(move |conn| f(conn).map(|v| Box::new(v) as Box<dyn Any + Send>)),
                reply: reply_tx,
                enqueued_at: Instant::now(),
            })
            .map_err(anyhow::Error::from)?;
        match reply_rx.recv() {
            Ok(result) => downcast_result(result?),
            Err(_) => Err(WriteQueueError::WriterGone.into()),
        }
    }

    /// Interactive-lane write: fails fast with `WriteQueueError::QueueFull`
    /// when the lane is at capacity (the HTTP layer maps this to a 503), and
    /// gives up waiting after the configured write timeout. The command may
    /// still execute after a timeout (at-least-once); every interactive write
    /// is idempotent, so the caller's retry is safe.
    fn with_write_priority<F, T>(&self, f: F) -> Result<T>
    where
        F: Fn(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = sync_channel::<WriteResult>(1);
        self.writer
            .queue
            .try_push_interactive(WriteCommand {
                run: Box::new(move |conn| f(conn).map(|v| Box::new(v) as Box<dyn Any + Send>)),
                reply: reply_tx,
                enqueued_at: Instant::now(),
            })
            .map_err(|e| {
                if matches!(e, WriteQueueError::QueueFull) {
                    let (interactive_depth, background_depth) = self.writer.queue.depths();
                    tracing::warn!(
                        interactive_depth,
                        background_depth,
                        "interactive write queue full; failing fast"
                    );
                }
                anyhow::Error::from(e)
            })?;
        match reply_rx.recv_timeout(self.writer.write_timeout) {
            Ok(result) => downcast_result(result?),
            Err(RecvTimeoutError::Timeout) => {
                let (interactive_depth, background_depth) = self.writer.queue.depths();
                tracing::warn!(
                    timeout_ms = self.writer.write_timeout.as_millis() as u64,
                    interactive_depth,
                    background_depth,
                    "interactive write timed out waiting for the writer (command may still execute)"
                );
                Err(WriteQueueError::Timeout(self.writer.write_timeout).into())
            }
            Err(RecvTimeoutError::Disconnected) => Err(WriteQueueError::WriterGone.into()),
        }
    }

    #[allow(dead_code)]
    pub fn get_next_derivation_index(&self) -> Result<u32> {
        let conn = self.read.get()?;
        let idx: u32 = conn.query_row(
            "SELECT COALESCE(MAX(derivation_index) + 1, 0) FROM accounts",
            [],
            |row| row.get(0),
        )?;
        Ok(idx)
    }

    /// Interactive lane: triggered directly by `POST /register`.
    pub fn register_account(
        &self,
        id: &str,
        index: u32,
        address: &str,
        webhook_url: &str,
    ) -> Result<()> {
        let id = id.to_string();
        let address = address.to_string();
        let webhook_url = webhook_url.to_string();
        self.with_write_priority(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO accounts (id, derivation_index, address, webhook_url)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, index, address, webhook_url],
            )?;
            Ok(())
        })
    }

    /// Registers an account with a sequentially allocated derivation index
    /// (P0 collision fix — see docs/fix-p0-register-address-collision.md,
    /// Option 1). Runs as one writer command wrapping one transaction:
    /// existing-id check, `next_index` allocation, address derivation, and
    /// the account insert are atomic, so a re-register race can never burn an
    /// index or produce two addresses for one id.
    ///
    /// Returns `(derivation_index, address, created)` where `created` is
    /// false when the id already existed (its stored address is returned).
    pub fn register_account_auto(
        &self,
        id: &str,
        webhook_url: &str,
        derive_address: impl Fn(u32) -> Result<String> + Send + 'static,
    ) -> Result<(u32, String, bool)> {
        let id = id.to_string();
        let webhook_url = webhook_url.to_string();
        self.with_write_priority(move |conn| {
            // Interactive commands run outside any writer-managed batch
            // transaction, so this command owns its own transaction.
            let tx = conn.unchecked_transaction()?;

            let existing: Option<(u32, String)> = tx
                .query_row(
                    "SELECT derivation_index, address FROM accounts WHERE id = ?1",
                    [id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((index, address)) = existing {
                tx.commit()?;
                return Ok((index, address, false));
            }

            let next_index: u32 = tx
                .query_row(
                    "SELECT value FROM state WHERE key = 'next_index'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .map(|v| v.parse::<u32>())
                .transpose()?
                .unwrap_or(0);

            let address = derive_address(next_index)?;

            tx.execute(
                "INSERT INTO accounts (id, derivation_index, address, webhook_url)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, next_index, address, webhook_url],
            )?;
            tx.execute(
                "INSERT INTO state (key, value) VALUES ('next_index', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![(next_index + 1).to_string()],
            )?;
            tx.commit()?;

            Ok((next_index, address, true))
        })
    }

    pub fn get_registration_id_by_address(&self, address: &str) -> Result<Option<String>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT id FROM accounts WHERE address = ?1",
            [address],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Superseded at the crate boundary by `db::Db::get_account_by_address`,
    /// which calls the dispatch-level `get_registration_id_by_address`
    /// directly rather than round-tripping through this alias.
    #[allow(dead_code)]
    pub fn get_account_by_address(&self, address: &str) -> Result<Option<String>> {
        self.get_registration_id_by_address(address)
    }

    pub fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String, String)>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT derivation_index, address, webhook_url FROM accounts WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_webhook_url(&self, account_id: &str) -> Result<Option<String>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT webhook_url FROM accounts WHERE id = ?1",
            [account_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn record_deposit(
        &self,
        chain: &str,
        tx_hash: &str,
        account_id: &str,
        amount: &str,
    ) -> Result<bool> {
        let chain = chain.to_string();
        let tx_hash = tx_hash.to_string();
        let account_id = account_id.to_string();
        let amount = amount.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO deposits (chain, tx_hash, account_id, amount, status)
                 VALUES (?1, ?2, ?3, ?4, 'detected')",
                params![chain, tx_hash, account_id, amount],
            )?;
            Ok(conn.changes() == 1)
        })
    }

    pub fn mark_deposit_swept(&self, chain: &str, tx_hash: &str) -> Result<()> {
        let chain = chain.to_string();
        let tx_hash = tx_hash.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "UPDATE deposits SET status = 'swept' WHERE chain = ?1 AND tx_hash = ?2",
                params![chain, tx_hash],
            )?;
            Ok(())
        })
    }

    pub fn mark_deposit_failed(&self, chain: &str, tx_hash: &str) -> Result<()> {
        let chain = chain.to_string();
        let tx_hash = tx_hash.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "UPDATE deposits SET status = 'failed' WHERE chain = ?1 AND tx_hash = ?2",
                params![chain, tx_hash],
            )?;
            Ok(())
        })
    }

    pub fn get_detected_deposits(&self, chain: &str) -> Result<Vec<(String, String, String)>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT tx_hash, account_id, amount FROM deposits
             WHERE chain = ?1 AND status = 'detected'",
        )?;
        let rows = stmt.query_map([chain], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_last_processed_block(&self, chain: &str) -> Result<u64> {
        let conn = self.read.get()?;
        let key = last_block_key(chain);
        let val: Option<String> = conn
            .query_row("SELECT value FROM state WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(val.map(|v| v.parse().unwrap_or(0)).unwrap_or(0))
    }

    pub fn set_last_processed_block(&self, chain: &str, block: u64) -> Result<()> {
        self.with_write(Self::set_last_processed_block_fn(chain, block))
    }

    /// Same write as `set_last_processed_block`, but on the interactive lane.
    /// Used by the `POST /block_number` HTTP path so an admin cursor reset is
    /// not stuck behind a monitor catch-up backlog (the monitor itself keeps
    /// using the background-lane variant).
    pub fn set_last_processed_block_priority(&self, chain: &str, block: u64) -> Result<()> {
        self.with_write_priority(Self::set_last_processed_block_fn(chain, block))
    }

    fn set_last_processed_block_fn(
        chain: &str,
        block: u64,
    ) -> impl Fn(&Connection) -> Result<()> + Send + 'static {
        let key = last_block_key(chain);
        let block_str = block.to_string();
        move |conn| {
            conn.execute(
                "INSERT INTO state (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, block_str],
            )?;
            Ok(())
        }
    }

    pub fn store_token_metadata(
        &self,
        chain: &str,
        address: &str,
        symbol: &str,
        decimals: u8,
        name: &str,
    ) -> Result<()> {
        let chain = chain.to_string();
        let address = address.to_string();
        let symbol = symbol.to_string();
        let name = name.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO token_metadata
                 (chain, token_address, symbol, decimals, name)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![chain, address, symbol, decimals, name],
            )?;
            Ok(())
        })
    }

    pub fn get_token_metadata(
        &self,
        chain: &str,
        address: &str,
    ) -> Result<Option<(String, u8, String)>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT symbol, decimals, name FROM token_metadata
             WHERE chain = ?1 AND token_address = ?2",
            params![chain, address],
            |row| Ok((row.get(0)?, row.get::<_, u8>(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_erc20_deposit(
        &self,
        chain: &str,
        tx_hash: &str,
        log_index: u64,
        account_id: &str,
        amount: &str,
        token_address: &str,
        token_symbol: &str,
    ) -> Result<bool> {
        let chain = chain.to_string();
        let tx_hash = tx_hash.to_string();
        let account_id = account_id.to_string();
        let amount = amount.to_string();
        let token_address = token_address.to_string();
        let token_symbol = token_symbol.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO erc20_deposits
                 (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'detected')",
                params![
                    chain,
                    tx_hash,
                    log_index as i64,
                    account_id,
                    amount,
                    token_address,
                    token_symbol
                ],
            )?;
            Ok(conn.changes() == 1)
        })
    }

    pub fn get_detected_erc20_deposits(&self, chain: &str) -> Result<Vec<Erc20Deposit>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT tx_hash, log_index, account_id, amount, token_address, token_symbol
             FROM erc20_deposits WHERE chain = ?1 AND status = 'detected'",
        )?;
        let rows = stmt.query_map([chain], |row| {
            let tx_hash: String = row.get(0)?;
            let log_index: i64 = row.get(1)?;
            Ok(Erc20Deposit {
                key: format!("{tx_hash}:{log_index}"),
                account_id: row.get(2)?,
                amount: row.get(3)?,
                token_address: row.get(4)?,
                token_symbol: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn mark_erc20_deposit_swept(&self, chain: &str, local_key: &str) -> Result<()> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let chain = chain.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "UPDATE erc20_deposits SET status = 'swept'
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
            )?;
            Ok(())
        })
    }

    pub fn mark_erc20_deposits_swept_for_account_token(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<(String, String)>> {
        let chain = chain.to_string();
        let account_id = account_id.to_string();
        let token_address = token_address.to_string();
        self.with_write(move |conn| {
            let mut stmt = conn.prepare(
                "UPDATE erc20_deposits SET status = 'swept'
                 WHERE chain = ?1 AND account_id = ?2 AND token_address = ?3 AND status = 'detected'
                 RETURNING tx_hash, log_index, amount",
            )?;
            let rows = stmt.query_map(params![chain, account_id, token_address], |row| {
                let tx_hash: String = row.get(0)?;
                let log_index: i64 = row.get(1)?;
                let amount: String = row.get(2)?;
                Ok((format!("{tx_hash}:{log_index}"), amount))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
    }

    pub fn increment_zero_balance_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let chain = chain.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
                 VALUES (?1, ?2, ?3, '', 1)
                 ON CONFLICT(chain, tx_hash, log_index) DO UPDATE SET
                   zero_balance_retry_count = zero_balance_retry_count + 1",
                params![chain, tx_hash, log_index],
            )?;
            let count: i64 = conn.query_row(
                "SELECT zero_balance_retry_count FROM sweep_meta
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
    }

    #[allow(dead_code)]
    pub fn set_sweep_tx_hash(&self, chain: &str, local_key: &str, tx_hash: &str) -> Result<()> {
        let (deposit_tx, log_index) = parse_local_key(local_key)?;
        let chain = chain.to_string();
        let tx_hash = tx_hash.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
                 VALUES (?1, ?2, ?3, ?4, 0)
                 ON CONFLICT(chain, tx_hash, log_index) DO UPDATE SET sweep_tx_hash = excluded.sweep_tx_hash",
                params![chain, deposit_tx, log_index, tx_hash],
            )?;
            Ok(())
        })
    }

    pub fn set_sweep_tx_hash_for_keys(
        &self,
        chain: &str,
        local_keys: &[String],
        tx_hash: &str,
    ) -> Result<()> {
        let chain = chain.to_string();
        let local_keys = local_keys.to_vec();
        let tx_hash = tx_hash.to_string();
        self.with_write(move |conn| {
            for local_key in &local_keys {
                let (deposit_tx, log_index) = parse_local_key(local_key)?;
                let existing: i64 = conn
                    .query_row(
                        "SELECT zero_balance_retry_count FROM sweep_meta
                         WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                        params![chain, deposit_tx, log_index],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                conn.execute(
                    "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(chain, tx_hash, log_index) DO UPDATE SET sweep_tx_hash = excluded.sweep_tx_hash",
                    params![chain, deposit_tx, log_index, tx_hash, existing],
                )?;
            }
            Ok(())
        })
    }

    #[allow(dead_code)]
    pub fn get_sweep_meta(&self, chain: &str, local_key: &str) -> Result<Option<(String, u64)>> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT sweep_tx_hash, zero_balance_retry_count FROM sweep_meta
             WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
            params![chain, tx_hash, log_index],
            |row| {
                let count: i64 = row.get(1)?;
                Ok((row.get(0)?, count as u64))
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn increment_sweep_failure_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let chain = chain.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "INSERT INTO sweep_failures (chain, tx_hash, log_index, consecutive_failure_count)
                 VALUES (?1, ?2, ?3, 1)
                 ON CONFLICT(chain, tx_hash, log_index) DO UPDATE SET
                   consecutive_failure_count = consecutive_failure_count + 1",
                params![chain, tx_hash, log_index],
            )?;
            let count: i64 = conn.query_row(
                "SELECT consecutive_failure_count FROM sweep_failures
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
    }

    pub fn mark_erc20_deposit_failed(&self, chain: &str, local_key: &str) -> Result<()> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let chain = chain.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "UPDATE erc20_deposits SET status = 'failed'
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
            )?;
            Ok(())
        })
    }

    pub fn mark_erc20_deposits_failed_for_account_token(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>> {
        let chain = chain.to_string();
        let account_id = account_id.to_string();
        let token_address = token_address.to_string();
        self.with_write(move |conn| {
            let mut stmt = conn.prepare(
                "UPDATE erc20_deposits SET status = 'failed'
                 WHERE chain = ?1 AND account_id = ?2 AND token_address = ?3 AND status = 'detected'
                 RETURNING tx_hash, log_index",
            )?;
            let rows = stmt.query_map(params![chain, account_id, token_address], |row| {
                let tx_hash: String = row.get(0)?;
                let log_index: i64 = row.get(1)?;
                Ok(format!("{tx_hash}:{log_index}"))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
    }

    pub fn deposit_queue_counts(&self, chain: &str) -> Result<DepositQueueCounts> {
        let conn = self.read.get()?;
        let native_detected: i64 = conn.query_row(
            "SELECT COUNT(*) FROM deposits WHERE chain = ?1 AND status = 'detected'",
            [chain],
            |row| row.get(0),
        )?;
        let native_failed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM deposits WHERE chain = ?1 AND status = 'failed'",
            [chain],
            |row| row.get(0),
        )?;
        let erc20_detected: i64 = conn.query_row(
            "SELECT COUNT(*) FROM erc20_deposits WHERE chain = ?1 AND status = 'detected'",
            [chain],
            |row| row.get(0),
        )?;
        let erc20_failed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM erc20_deposits WHERE chain = ?1 AND status = 'failed'",
            [chain],
            |row| row.get(0),
        )?;
        Ok(DepositQueueCounts {
            native_detected: native_detected as u64,
            native_failed: native_failed as u64,
            erc20_detected: erc20_detected as u64,
            erc20_failed: erc20_failed as u64,
        })
    }

    /// Interactive lane: triggered directly by the admin retry-sweep HTTP endpoint.
    pub fn retry_native_deposit(&self, chain: &str, tx_hash: &str) -> Result<bool> {
        let chain = chain.to_string();
        let tx_hash = tx_hash.to_string();
        self.with_write_priority(move |conn| {
            conn.execute(
                "UPDATE deposits SET status = 'detected'
                 WHERE chain = ?1 AND tx_hash = ?2 AND status = 'failed'",
                params![chain, tx_hash],
            )?;
            Ok(conn.changes() == 1)
        })
    }

    /// Interactive lane: triggered directly by the admin retry-sweep HTTP endpoint.
    pub fn retry_erc20_deposit(&self, chain: &str, tx_hash: &str, log_index: u64) -> Result<bool> {
        let chain = chain.to_string();
        let tx_hash = tx_hash.to_string();
        self.with_write_priority(move |conn| {
            conn.execute(
                "UPDATE erc20_deposits SET status = 'detected'
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3 AND status = 'failed'",
                params![chain, tx_hash, log_index as i64],
            )?;
            let updated = conn.changes() == 1;
            if updated {
                conn.execute(
                    "DELETE FROM sweep_failures
                     WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                    params![chain, tx_hash, log_index as i64],
                )?;
            }
            Ok(updated)
        })
    }

    pub fn get_sweep_failure_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let conn = self.read.get()?;
        let count: Option<i64> = conn
            .query_row(
                "SELECT consecutive_failure_count FROM sweep_failures
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
                |row| row.get(0),
            )
            .optional()?;
        Ok(count.unwrap_or(0) as u64)
    }

    /// Insert or refresh a pending delivery. Returns true when the worker should be notified.
    pub fn upsert_webhook_delivery(
        &self,
        id: &str,
        event: &str,
        registration_id: &str,
        webhook_url: &str,
        payload: &str,
    ) -> Result<bool> {
        let id = id.to_string();
        let event = event.to_string();
        let registration_id = registration_id.to_string();
        let webhook_url = webhook_url.to_string();
        let payload = payload.to_string();
        self.with_write(move |conn| {
            let now = now_unix_secs();
            let existing: Option<String> = conn
                .query_row(
                    "SELECT status FROM webhook_deliveries WHERE id = ?1 AND event = ?2",
                    params![id, event],
                    |row| row.get(0),
                )
                .optional()?;

            if existing.as_deref() == Some("delivered") {
                return Ok(false);
            }

            if existing.is_none() {
                conn.execute(
                    "INSERT INTO webhook_deliveries
                     (id, event, registration_id, webhook_url, payload, status, attempt_count,
                      last_http_status, last_error, leased_until, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, NULL, NULL, NULL, ?6)",
                    params![id, event, registration_id, webhook_url, payload, now],
                )?;
                return Ok(true);
            }

            if existing.as_deref() == Some("failed") {
                return Ok(false);
            }

            conn.execute(
                "UPDATE webhook_deliveries
                 SET webhook_url = ?3, payload = ?4, updated_at = ?5
                 WHERE id = ?1 AND event = ?2 AND status = 'pending'",
                params![id, event, webhook_url, payload, now],
            )?;
            Ok(true)
        })
    }

    pub fn claim_webhook_delivery(
        &self,
        id: &str,
        event: &str,
        lease_until: i64,
        max_retries: u32,
    ) -> Result<bool> {
        let now = now_unix_secs();
        let id = id.to_string();
        let event = event.to_string();
        self.with_write(move |conn| {
            conn.execute(
                "UPDATE webhook_deliveries
                 SET leased_until = ?3, updated_at = ?4
                 WHERE id = ?1 AND event = ?2
                   AND status = 'pending'
                   AND attempt_count < ?5
                   AND (leased_until IS NULL OR leased_until < ?4)",
                params![id, event, lease_until, now, max_retries as i64],
            )?;
            Ok(conn.changes() == 1)
        })
    }

    pub fn record_webhook_attempt(
        &self,
        id: &str,
        event: &str,
        http_status: Option<u16>,
        error: Option<&str>,
        status: &str,
    ) -> Result<u64> {
        let id = id.to_string();
        let event = event.to_string();
        let error = error.map(str::to_string);
        let status = status.to_string();
        self.with_write(move |conn| {
            let now = now_unix_secs();
            conn.execute(
                "UPDATE webhook_deliveries
                 SET attempt_count = attempt_count + 1,
                     last_http_status = ?3,
                     last_error = ?4,
                     status = ?5,
                     leased_until = NULL,
                     updated_at = ?6
                 WHERE id = ?1 AND event = ?2",
                params![id, event, http_status.map(i64::from), error, status, now],
            )?;
            let count: i64 = conn.query_row(
                "SELECT attempt_count FROM webhook_deliveries WHERE id = ?1 AND event = ?2",
                params![id, event],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
    }

    pub fn get_pending_webhook_delivery_keys(
        &self,
        max_retries: u32,
        batch_size: u32,
    ) -> Result<Vec<(String, String)>> {
        let now = now_unix_secs();
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, event FROM webhook_deliveries
             WHERE status = 'pending'
               AND attempt_count < ?1
               AND (leased_until IS NULL OR leased_until < ?2)
             ORDER BY updated_at ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![max_retries as i64, now, batch_size as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_webhook_delivery(
        &self,
        id: &str,
        event: &str,
    ) -> Result<Option<WebhookDeliveryRecord>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT id, event, registration_id, webhook_url, payload, status, attempt_count,
                    last_http_status, last_error
             FROM webhook_deliveries WHERE id = ?1 AND event = ?2",
            params![id, event],
            |row| {
                let http_status: Option<i64> = row.get(7)?;
                Ok(WebhookDeliveryRecord {
                    id: row.get(0)?,
                    event: row.get(1)?,
                    registration_id: row.get(2)?,
                    webhook_url: row.get(3)?,
                    payload: row.get(4)?,
                    status: row.get(5)?,
                    attempt_count: row.get::<_, i64>(6)? as u64,
                    last_http_status: http_status.map(|s| s as u16),
                    last_error: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Interactive lane: triggered directly by the admin retry-webhook HTTP endpoint.
    pub fn retry_webhook_delivery(&self, id: &str, event: &str) -> Result<bool> {
        let now = now_unix_secs();
        let id = id.to_string();
        let event = event.to_string();
        self.with_write_priority(move |conn| {
            conn.execute(
                "UPDATE webhook_deliveries
                 SET status = 'pending', attempt_count = 0, leased_until = NULL,
                     last_http_status = NULL, last_error = NULL, updated_at = ?3
                 WHERE id = ?1 AND event = ?2 AND status = 'failed'",
                params![id, event, now],
            )?;
            Ok(conn.changes() == 1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_chain_isolated_deposits() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.record_deposit("base", "0xabc", "user1", "100").unwrap();
        db.record_deposit("polygon", "0xabc", "user2", "200")
            .unwrap();

        let base = db.get_detected_deposits("base").unwrap();
        let polygon = db.get_detected_deposits("polygon").unwrap();

        assert_eq!(base.len(), 1);
        assert_eq!(base[0].0, "0xabc");
        assert_eq!(base[0].2, "100");
        assert_eq!(polygon.len(), 1);
        assert_eq!(polygon[0].2, "200");
    }

    #[test]
    fn test_per_chain_last_block() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.set_last_processed_block("base", 100).unwrap();
        db.set_last_processed_block("polygon", 200).unwrap();

        assert_eq!(db.get_last_processed_block("base").unwrap(), 100);
        assert_eq!(db.get_last_processed_block("polygon").unwrap(), 200);
    }

    #[test]
    fn test_record_deposit_duplicate_returns_false() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        assert!(db.record_deposit("base", "0xabc", "user1", "100").unwrap());
        assert!(!db.record_deposit("base", "0xabc", "user1", "100").unwrap());
    }

    #[test]
    fn test_record_erc20_deposit_duplicate_returns_false() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        assert!(db
            .record_erc20_deposit("polygon", "0xabc", 1, "user1", "100", "0xtoken", "USDC")
            .unwrap());
        assert!(!db
            .record_erc20_deposit("polygon", "0xabc", 1, "user1", "100", "0xtoken", "USDC")
            .unwrap());
    }

    #[test]
    fn test_increment_zero_balance_count_monotonic() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        assert_eq!(
            db.increment_zero_balance_count("polygon", "0xabc:1")
                .unwrap(),
            1
        );
        assert_eq!(
            db.increment_zero_balance_count("polygon", "0xabc:1")
                .unwrap(),
            2
        );
        assert_eq!(
            db.increment_sweep_failure_count("polygon", "0xabc:1")
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_db_new_idempotent_on_same_path() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();

        let db1 = Db::new(path).unwrap();
        db1.register_account("u1", 0, "0x1", "https://example.com")
            .unwrap();

        let db2 = Db::new(path).unwrap();
        let acct = db2.get_account_by_id("u1").unwrap().unwrap();
        assert_eq!(acct.1, "0x1");
    }

    #[test]
    fn test_retry_erc20_deposit_resets_failed_status_and_clears_failures() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.record_erc20_deposit("base", "0xabc", 120, "user1", "100", "0xtoken", "USDC")
            .unwrap();
        db.mark_erc20_deposit_failed("base", "0xabc:120").unwrap();
        db.increment_sweep_failure_count("base", "0xabc:120")
            .unwrap();

        assert_eq!(db.get_detected_erc20_deposits("base").unwrap().len(), 0);
        assert_eq!(db.get_sweep_failure_count("base", "0xabc:120").unwrap(), 1);

        assert!(db.retry_erc20_deposit("base", "0xabc", 120).unwrap());
        assert_eq!(db.get_detected_erc20_deposits("base").unwrap().len(), 1);
        assert_eq!(db.get_sweep_failure_count("base", "0xabc:120").unwrap(), 0);
        assert!(!db.retry_erc20_deposit("base", "0xabc", 120).unwrap());
    }

    #[test]
    fn test_retry_native_deposit() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.record_deposit("polygon", "0xabc", "user1", "100")
            .unwrap();
        db.mark_deposit_failed("polygon", "0xabc").unwrap();

        assert!(db.retry_native_deposit("polygon", "0xabc").unwrap());
        assert_eq!(db.get_detected_deposits("polygon").unwrap().len(), 1);
    }

    #[test]
    fn test_deposit_queue_counts() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.record_deposit("base", "0x1", "u1", "100").unwrap();
        db.record_erc20_deposit("base", "0x2", 1, "u1", "200", "0xt", "USDC")
            .unwrap();
        db.mark_erc20_deposit_failed("base", "0x2:1").unwrap();

        let counts = db.deposit_queue_counts("base").unwrap();
        assert_eq!(
            counts,
            DepositQueueCounts {
                native_detected: 1,
                native_failed: 0,
                erc20_detected: 0,
                erc20_failed: 1,
            }
        );
    }

    #[test]
    fn test_normalize_db_path_strips_sqlite_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("wallet.db");
        let bare_str = bare.to_str().unwrap();

        let db_bare = Db::new(bare_str).unwrap();
        db_bare
            .register_account("u1", 0, "0x1", "https://example.com")
            .unwrap();

        let prefixed = format!("sqlite:{bare_str}");
        let db_prefixed = Db::new(&prefixed).unwrap();
        assert!(db_prefixed.get_account_by_id("u1").unwrap().is_some());
    }

    #[test]
    fn test_upsert_webhook_delivery_skips_delivered() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        assert!(db
            .upsert_webhook_delivery(
                "polygon:0xabc",
                "deposit_detected",
                "user1",
                "https://example.com/hook",
                r#"{"id":"polygon:0xabc","event":"deposit_detected"}"#,
            )
            .unwrap());

        db.record_webhook_attempt(
            "polygon:0xabc",
            "deposit_detected",
            Some(200),
            None,
            "delivered",
        )
        .unwrap();

        assert!(!db
            .upsert_webhook_delivery(
                "polygon:0xabc",
                "deposit_detected",
                "user1",
                "https://example.com/hook",
                r#"{"id":"polygon:0xabc","event":"deposit_detected"}"#,
            )
            .unwrap());

        let row = db
            .get_webhook_delivery("polygon:0xabc", "deposit_detected")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "delivered");
    }

    #[test]
    fn test_claim_webhook_delivery_respects_lease() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.upsert_webhook_delivery(
            "base:0x1",
            "deposit_swept",
            "user1",
            "https://example.com/hook",
            r#"{"id":"base:0x1","event":"deposit_swept"}"#,
        )
        .unwrap();

        let now = now_unix_secs();
        assert!(db
            .claim_webhook_delivery("base:0x1", "deposit_swept", now + 60, 5)
            .unwrap());
        assert!(!db
            .claim_webhook_delivery("base:0x1", "deposit_swept", now + 120, 5)
            .unwrap());
    }

    #[test]
    fn test_retry_webhook_delivery_resets_failed_row() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.upsert_webhook_delivery(
            "polygon:0xdead",
            "deposit_detected",
            "user1",
            "https://example.com/hook",
            r#"{"id":"polygon:0xdead","event":"deposit_detected"}"#,
        )
        .unwrap();
        db.record_webhook_attempt(
            "polygon:0xdead",
            "deposit_detected",
            Some(503),
            Some("HTTP status 503"),
            "failed",
        )
        .unwrap();

        assert!(db
            .retry_webhook_delivery("polygon:0xdead", "deposit_detected")
            .unwrap());

        let row = db
            .get_webhook_delivery("polygon:0xdead", "deposit_detected")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempt_count, 0);
        assert!(!db
            .retry_webhook_delivery("polygon:0xdead", "deposit_detected")
            .unwrap());
    }

    #[test]
    fn test_new_uses_default_pool_max_size() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(db.read.max_size(), DEFAULT_READ_POOL_MAX_SIZE);
    }

    #[test]
    fn test_with_pool_size_configures_read_pool_capacity() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::with_pool_size(tmp.path().to_str().unwrap(), 3).unwrap();
        assert_eq!(db.read.max_size(), 3);
    }

    /// Regression test for the pool-exhaustion incident: a hardcoded pool size
    /// (previously r2d2's implicit default of 10, with no way to raise it) is
    /// shared by every chain's monitor/sweeper/webhook loops plus inbound
    /// registrations. This proves `with_pool_size` actually bounds concurrent
    /// checkouts to the configured value, rather than silently falling back to
    /// r2d2's default.
    #[test]
    fn test_read_pool_respects_configured_max_size() {
        use std::sync::Barrier;
        use std::thread;
        use std::time::Duration;

        let tmp = NamedTempFile::new().unwrap();
        let pool_size = 2u32;
        let db = Db::with_pool_size(tmp.path().to_str().unwrap(), pool_size).unwrap();

        // Barrier for "every thread below has a connection checked out",
        // signaling the main thread that the pool is genuinely exhausted.
        // Parties = pool_size worker threads + the main thread itself.
        let barrier = Arc::new(Barrier::new(pool_size as usize + 1));
        let handles: Vec<_> = (0..pool_size)
            .map(|_| {
                let db = db.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let _conn = db
                        .read
                        .get()
                        .expect("pool should have capacity for this thread");
                    barrier.wait();
                    // Hold the connection past the 5s connection_timeout so the
                    // main thread's extra `get()` below contends for a pool
                    // that's genuinely exhausted, not just briefly busy.
                    thread::sleep(Duration::from_secs(6));
                })
            })
            .collect();

        barrier.wait();
        let extra = db.read.get();
        assert!(
            extra.is_err(),
            "expected read.get() to fail once all {pool_size} pooled connections are checked out"
        );

        for h in handles {
            h.join().unwrap();
        }
    }

    // ========== Single-writer actor tests ==========

    use std::sync::atomic::AtomicUsize;
    use std::thread;

    fn test_writer_config() -> WriterConfig {
        WriterConfig {
            abort_on_panic: false,
            ..WriterConfig::default()
        }
    }

    fn db_with(config: WriterConfig) -> (NamedTempFile, Db) {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::with_options(tmp.path().to_str().unwrap(), 5, config).unwrap();
        (tmp, db)
    }

    /// Occupies the writer thread for `hold` by parking a background command
    /// on it. Returns after the command has definitely started executing.
    /// Re-execution-safe (write closures are `Fn`): only the first run sleeps.
    fn occupy_writer(db: &Db, hold: Duration) -> thread::JoinHandle<()> {
        let started = Arc::new(AtomicBool::new(false));
        let started_inner = Arc::clone(&started);
        let first_run = Arc::new(AtomicBool::new(true));
        let db = db.clone();
        let handle = thread::spawn(move || {
            db.with_write(move |_conn| {
                started_inner.store(true, Ordering::SeqCst);
                if first_run.swap(false, Ordering::SeqCst) {
                    thread::sleep(hold);
                }
                Ok(())
            })
            .unwrap();
        });
        while !started.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        handle
    }

    #[test]
    fn test_pragma_synchronous_reads_back_normal() {
        let (_tmp, db) = db_with(test_writer_config());
        let conn = db.read.get().unwrap();
        let mode: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, 1, "expected synchronous=NORMAL (1), got {mode}");
    }

    // ========== WAL checkpoint tests ==========

    /// Writes rows without ever checkpointing, then returns the WAL size in
    /// bytes so callers can assert it's grown past zero.
    fn write_rows_without_checkpoint(conn: &Connection, count: usize) {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS t (v TEXT)")
            .unwrap();
        for i in 0..count {
            conn.execute("INSERT INTO t (v) VALUES (?1)", params![format!("row-{i}")])
                .unwrap();
        }
    }

    /// Regression for the 2026-07 write-queue-saturation incident: a
    /// restart alone did nothing because SQLite just reopened the same
    /// oversized WAL. `checkpoint_startup` must actually shrink it.
    #[test]
    fn test_checkpoint_startup_truncates_existing_wal() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        let conn = Connection::open(path).unwrap();
        apply_pragmas(&conn).unwrap();
        write_rows_without_checkpoint(&conn, 500);

        let wal_before = wal_file_size_bytes(path).unwrap_or(0);
        assert!(
            wal_before > 0,
            "expected uncheckpointed writes to leave a non-empty WAL, got {wal_before}"
        );

        checkpoint_startup(&conn, path).unwrap();

        let wal_after = wal_file_size_bytes(path).unwrap_or(0);
        assert!(
            wal_after < wal_before,
            "expected startup checkpoint to shrink the WAL: before={wal_before} after={wal_after}"
        );
    }

    /// `Db::with_options` must run the startup checkpoint itself (not just
    /// the standalone helper) so every real construction path is covered,
    /// including the one production actually uses.
    #[test]
    fn test_db_with_options_checkpoints_preexisting_wal_on_open() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();

        let conn = Connection::open(path).unwrap();
        apply_pragmas(&conn).unwrap();
        write_rows_without_checkpoint(&conn, 500);
        // Deliberately leak rather than drop: closing the last connection to
        // a WAL database triggers SQLite's own checkpoint-on-close, which
        // would clean up the WAL before `Db::with_options` ever gets a
        // chance to and defeat the point of this test. A real incident looks
        // like this too — the previous process's connection never got a
        // clean close (killed, or the close-time checkpoint itself stalled
        // on EFS), leaving an oversized WAL for the next process to inherit.
        std::mem::forget(conn);

        let wal_before = wal_file_size_bytes(path).unwrap_or(0);
        assert!(
            wal_before > 0,
            "expected uncheckpointed writes to leave a non-empty WAL, got {wal_before}"
        );

        let db = Db::with_options(path, 5, test_writer_config()).unwrap();
        let wal_after = wal_file_size_bytes(path).unwrap_or(0);
        assert!(
            wal_after < wal_before,
            "expected Db::with_options to checkpoint the pre-existing WAL on open: \
             before={wal_before} after={wal_after}"
        );
        drop(db);
    }

    #[test]
    fn test_run_wal_checkpoint_reports_frame_counts() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        let conn = Connection::open(path).unwrap();
        apply_pragmas(&conn).unwrap();
        write_rows_without_checkpoint(&conn, 200);

        // PASSIVE never truncates the physical file, so its returned counts
        // are the reliable signal for "how much was actually pending."
        let result = run_wal_checkpoint(&conn, "PASSIVE").unwrap();
        assert_eq!(
            result.busy, 0,
            "expected an uncontended checkpoint to succeed"
        );
        assert!(
            result.log_frames > 0,
            "expected a non-zero WAL frame count before checkpointing"
        );
        assert_eq!(
            result.checkpointed_frames, result.log_frames,
            "a fully successful checkpoint with no concurrent readers should \
             checkpoint every WAL frame"
        );

        // Nothing new to do: with no writes in between, a repeat PASSIVE
        // checkpoint reports the same (already fully backfilled) counts
        // rather than erroring or double-counting.
        let second = run_wal_checkpoint(&conn, "PASSIVE").unwrap();
        assert_eq!(second.busy, 0);
        assert_eq!(second.checkpointed_frames, second.log_frames);
    }

    /// `TRUNCATE` mode additionally shrinks the physical `-wal` file to zero
    /// bytes on full success — this is the property `checkpoint_startup`
    /// relies on to fix "restart reopens the same oversized WAL."
    #[test]
    fn test_run_wal_checkpoint_truncate_shrinks_file_on_full_success() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        let conn = Connection::open(path).unwrap();
        apply_pragmas(&conn).unwrap();
        write_rows_without_checkpoint(&conn, 200);

        let wal_before = wal_file_size_bytes(path).unwrap_or(0);
        assert!(wal_before > 0);

        let result = run_wal_checkpoint(&conn, "TRUNCATE").unwrap();
        assert_eq!(
            result.busy, 0,
            "expected an uncontended checkpoint to succeed"
        );

        let wal_after = wal_file_size_bytes(path).unwrap_or(0);
        assert_eq!(
            wal_after, 0,
            "expected a fully successful TRUNCATE checkpoint to shrink the WAL to 0 bytes"
        );
    }

    fn make_test_queue() -> WriteQueue {
        WriteQueue::new(64, 2048)
    }

    /// `maybe_checkpoint` must skip entirely (no attempt, timer untouched)
    /// while an interactive command is waiting, so the opportunistic
    /// checkpoint never adds latency to a real request.
    #[test]
    fn test_maybe_checkpoint_skips_when_interactive_waiting() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        let conn = Connection::open(path).unwrap();
        apply_pragmas(&conn).unwrap();
        write_rows_without_checkpoint(&conn, 500);
        let wal_before = wal_file_size_bytes(path).unwrap_or(0);
        assert!(wal_before > 0);

        let queue = make_test_queue();
        let (reply_tx, _reply_rx) = sync_channel::<WriteResult>(1);
        queue
            .try_push_interactive(WriteCommand {
                run: Box::new(|_conn| Ok(Box::new(()) as Box<dyn Any + Send>)),
                reply: reply_tx,
                enqueued_at: Instant::now(),
            })
            .unwrap();

        let mut last_checkpoint = Instant::now() - Duration::from_secs(3600);
        let outcome = maybe_checkpoint(
            &conn,
            &queue,
            path,
            Duration::from_secs(30),
            &mut last_checkpoint,
        );

        assert!(
            outcome.is_none(),
            "expected no checkpoint attempt while an interactive command is queued"
        );
        let wal_after = wal_file_size_bytes(path).unwrap_or(0);
        assert_eq!(wal_after, wal_before);
    }

    /// `maybe_checkpoint` must skip when the interval hasn't elapsed yet,
    /// even with an empty interactive lane, so it never runs on every single
    /// background batch.
    #[test]
    fn test_maybe_checkpoint_skips_before_interval_elapses() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        let conn = Connection::open(path).unwrap();
        apply_pragmas(&conn).unwrap();
        write_rows_without_checkpoint(&conn, 500);
        let wal_before = wal_file_size_bytes(path).unwrap_or(0);
        assert!(wal_before > 0);

        let queue = make_test_queue();
        let mut last_checkpoint = Instant::now();
        let outcome = maybe_checkpoint(
            &conn,
            &queue,
            path,
            Duration::from_secs(3600),
            &mut last_checkpoint,
        );

        assert!(
            outcome.is_none(),
            "expected no checkpoint attempt before the interval elapses"
        );
        let wal_after = wal_file_size_bytes(path).unwrap_or(0);
        assert_eq!(wal_after, wal_before);
    }

    /// Once both gates are open (no interactive work, interval elapsed) the
    /// checkpoint actually runs and shrinks the WAL, and the timer resets so
    /// the next call doesn't immediately re-run.
    #[test]
    fn test_maybe_checkpoint_runs_and_resets_timer_once_due() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        let conn = Connection::open(path).unwrap();
        apply_pragmas(&conn).unwrap();
        write_rows_without_checkpoint(&conn, 500);

        let queue = make_test_queue();
        let mut last_checkpoint = Instant::now() - Duration::from_secs(3600);
        let outcome = maybe_checkpoint(
            &conn,
            &queue,
            path,
            Duration::from_secs(30),
            &mut last_checkpoint,
        );

        // PASSIVE never shrinks the physical file (see module docs on
        // `maybe_checkpoint`), so the frame counts it returns — not WAL
        // byte size — are the correct signal that it actually ran.
        let result = outcome.expect("expected a due checkpoint to actually run");
        assert_eq!(result.busy, 0);
        assert!(
            result.log_frames > 0 && result.checkpointed_frames == result.log_frames,
            "expected the due checkpoint to fully backfill the pending frames, got {:?}/{:?}",
            result.log_frames,
            result.checkpointed_frames
        );
        assert!(
            last_checkpoint.elapsed() < Duration::from_secs(5),
            "expected the timer to reset to roughly now after running"
        );

        // Immediately calling again should be a no-op (interval not
        // elapsed), proving the reset timer actually gates the next call.
        write_rows_without_checkpoint(&conn, 500);
        let second_outcome = maybe_checkpoint(
            &conn,
            &queue,
            path,
            Duration::from_secs(30),
            &mut last_checkpoint,
        );
        assert!(
            second_outcome.is_none(),
            "expected the just-reset timer to skip an immediate second checkpoint"
        );
    }

    /// Regression (mandatory): one failing command inside a batch rolls the
    /// batch back, all other commands are re-executed individually and land,
    /// and only the truly failing command reports an error.
    #[test]
    fn test_batch_poison_command_falls_back_to_individual_execution() {
        let (_tmp, db) = db_with(test_writer_config());

        // Park the writer so the commands below queue up and get batched
        // into a single transaction together.
        let hold = occupy_writer(&db, Duration::from_millis(200));

        let mut handles = Vec::new();
        for i in 0..5 {
            let db = db.clone();
            handles.push(thread::spawn(move || {
                let tx_hash = format!("0xgood{i}");
                db.record_deposit("base", &tx_hash, "user", "100")
            }));
        }
        // Poison command: syntactically invalid SQL fails at execute time.
        let poison_db = db.clone();
        let poison = thread::spawn(move || {
            poison_db.with_write(|conn| {
                conn.execute("THIS IS NOT SQL", [])?;
                Ok(())
            })
        });

        for h in handles {
            assert!(
                h.join().unwrap().is_ok(),
                "good commands must land despite the poison command in the same batch"
            );
        }
        assert!(
            poison.join().unwrap().is_err(),
            "the poison command must be the only one that errors"
        );
        hold.join().unwrap();

        let deposits = db.get_detected_deposits("base").unwrap();
        assert_eq!(deposits.len(), 5, "all 5 good deposits must be committed");
    }

    /// Regression (mandatory): a caller whose interactive timeout fires still
    /// gets its write executed (at-least-once), and the retry hits the
    /// existing-account fast path with no duplicate index.
    #[test]
    fn test_timeout_then_late_execution_register_retry_is_safe() {
        let (_tmp, db) = db_with(WriterConfig {
            write_timeout: Duration::from_millis(50),
            ..test_writer_config()
        });

        // Writer stuck well past the interactive timeout.
        let hold = occupy_writer(&db, Duration::from_millis(400));

        let err = db
            .register_account_auto("late_user", "https://example.com", |i| {
                Ok(format!("0xaddr{i}"))
            })
            .unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<WriteQueueError>(),
                Some(WriteQueueError::Timeout(_))
            ),
            "expected Timeout, got: {err}"
        );

        hold.join().unwrap();
        // Give the writer a moment to drain the late command.
        thread::sleep(Duration::from_millis(200));

        let stored = db.get_account_by_id("late_user").unwrap();
        assert!(
            stored.is_some(),
            "the timed-out register must still execute (at-least-once)"
        );
        let (index, address, _) = stored.unwrap();

        // Retry returns the existing account: same index, same address.
        let (retry_index, retry_address, created) = db
            .register_account_auto("late_user", "https://example.com", |i| {
                Ok(format!("0xaddr{i}"))
            })
            .unwrap();
        assert!(!created);
        assert_eq!(retry_index, index);
        assert_eq!(retry_address, address);
    }

    /// Contention: a saturated background lane must not delay an interactive
    /// register beyond (roughly) one in-flight command, far under the timeout.
    #[test]
    fn test_background_saturation_does_not_delay_interactive_register() {
        let (_tmp, db) = db_with(test_writer_config());

        // Queue a pile of slow background commands (~25 x 20ms = 500ms of
        // writer work), then one interactive register. If priority did not
        // work, the register would wait for the whole pile.
        let hold = occupy_writer(&db, Duration::from_millis(100));
        let mut producers = Vec::new();
        for i in 0..25 {
            let db = db.clone();
            producers.push(thread::spawn(move || {
                db.with_write(move |conn| {
                    thread::sleep(Duration::from_millis(20));
                    conn.execute(
                        "INSERT OR IGNORE INTO deposits (chain, tx_hash, account_id, amount, status)
                         VALUES ('base', ?1, 'u', '1', 'detected')",
                        [format!("0xslow{i}")],
                    )?;
                    Ok(())
                })
                .unwrap();
            }));
        }
        thread::sleep(Duration::from_millis(50)); // let producers enqueue

        let started = Instant::now();
        db.register_account("prio_user", 0, "0xprio", "https://example.com")
            .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(1000),
            "interactive register took {elapsed:?}; priority lane is not jumping ahead"
        );

        hold.join().unwrap();
        for p in producers {
            p.join().unwrap();
        }
    }

    #[test]
    fn test_interactive_queue_full_returns_typed_error_immediately() {
        let (_tmp, db) = db_with(WriterConfig {
            interactive_capacity: 1,
            ..test_writer_config()
        });

        // Writer stuck; one interactive command occupies the only lane slot.
        let hold = occupy_writer(&db, Duration::from_millis(300));
        let occupant_db = db.clone();
        let occupant = thread::spawn(move || {
            occupant_db
                .register_account("occupant", 0, "0xocc", "https://example.com")
                .unwrap();
        });
        thread::sleep(Duration::from_millis(50)); // let the occupant enqueue

        let started = Instant::now();
        let err = db
            .register_account("rejected", 1, "0xrej", "https://example.com")
            .unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<WriteQueueError>(),
                Some(WriteQueueError::QueueFull)
            ),
            "expected QueueFull, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "queue-full must fail fast, not wait"
        );

        hold.join().unwrap();
        occupant.join().unwrap();
    }

    /// Backpressure: a full background lane blocks the producer (no write is
    /// ever dropped), and FIFO order within the lane guarantees a chunk's
    /// deposits commit before its cursor advance.
    #[test]
    fn test_background_lane_full_blocks_producer_and_preserves_order() {
        let (_tmp, db) = db_with(WriterConfig {
            background_capacity: 2,
            ..test_writer_config()
        });

        let hold = occupy_writer(&db, Duration::from_millis(300));

        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let mut producers = Vec::new();
        // One producer issues deposit-then-cursor in program order, like the
        // monitor does; extra producers overfill the capacity-2 lane so at
        // least one send() must block instead of dropping.
        {
            let db = db.clone();
            let order = Arc::clone(&order);
            producers.push(thread::spawn(move || {
                let o1 = Arc::clone(&order);
                db.with_write(move |conn| {
                    o1.lock().unwrap().push("deposit");
                    conn.execute(
                        "INSERT OR IGNORE INTO deposits (chain, tx_hash, account_id, amount, status)
                         VALUES ('base', '0xdep', 'u', '1', 'detected')",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
                let o2 = Arc::clone(&order);
                db.with_write(move |conn| {
                    o2.lock().unwrap().push("cursor");
                    conn.execute(
                        "INSERT INTO state (key, value) VALUES ('last_block:base', '42')
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
            }));
        }
        for i in 0..4 {
            let db = db.clone();
            producers.push(thread::spawn(move || {
                db.record_deposit("base", &format!("0xfill{i}"), "u", "1")
                    .unwrap();
            }));
        }

        hold.join().unwrap();
        for p in producers {
            p.join().unwrap(); // every blocked producer completes; nothing dropped
        }

        let recorded = order.lock().unwrap().clone();
        let dep_pos = recorded.iter().position(|s| *s == "deposit").unwrap();
        let cur_pos = recorded.iter().position(|s| *s == "cursor").unwrap();
        assert!(
            dep_pos < cur_pos,
            "a chunk's record_deposit must execute before its set_last_processed_block"
        );
        assert_eq!(db.get_detected_deposits("base").unwrap().len(), 5);
        assert_eq!(db.get_last_processed_block("base").unwrap(), 42);
    }

    #[test]
    fn test_fifo_ordering_preserved_within_background_lane() {
        let (_tmp, db) = db_with(test_writer_config());
        let hold = occupy_writer(&db, Duration::from_millis(400));

        let order = Arc::new(Mutex::new(Vec::<usize>::new()));
        let db2 = db.clone();
        let order2 = Arc::clone(&order);
        let producer = thread::spawn(move || {
            let mut waiters = Vec::new();
            for i in 0..10 {
                let db3 = db2.clone();
                let o = Arc::clone(&order2);
                // Sequential blocking sends from one thread would serialize on
                // the replies; enqueue via short-lived threads spawned in
                // order with a small delay so queue order is deterministic.
                waiters.push(thread::spawn(move || {
                    db3.with_write(move |_conn| {
                        o.lock().unwrap().push(i);
                        Ok(())
                    })
                    .unwrap();
                }));
                thread::sleep(Duration::from_millis(10));
            }
            for w in waiters {
                w.join().unwrap();
            }
        });

        producer.join().unwrap();
        hold.join().unwrap();

        let recorded = order.lock().unwrap().clone();
        assert_eq!(recorded, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn test_writer_death_flags_unhealthy_and_fails_subsequent_writes() {
        let (_tmp, db) = db_with(test_writer_config());
        assert!(db.writer_healthy());

        // A panicking command kills the writer loop (abort disabled in tests).
        let result = db.with_write(|_conn| -> Result<()> { panic!("boom") });
        assert!(result.is_err());

        // The death handler runs on the writer thread; poll briefly.
        let deadline = Instant::now() + Duration::from_secs(2);
        while db.writer_healthy() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!db.writer_healthy(), "writer_healthy must flip false");

        let err = db.record_deposit("base", "0xdead", "u", "1").unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<WriteQueueError>(),
                Some(WriteQueueError::WriterGone)
            ),
            "expected WriterGone, got: {err}"
        );
        let err = db
            .register_account("dead", 0, "0xd", "https://example.com")
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<WriteQueueError>(),
            Some(WriteQueueError::WriterGone)
        ));

        // Reads still work.
        assert!(db.get_detected_deposits("base").unwrap().is_empty());
    }

    /// The three rerouted HTTP write paths (retry sweeps, retry webhook,
    /// cursor set) plus register must complete even when the background lane
    /// is full and blocked — proof they ride the interactive lane.
    #[test]
    fn test_priority_paths_bypass_full_background_lane() {
        let (_tmp, db) = db_with(WriterConfig {
            background_capacity: 1,
            ..test_writer_config()
        });

        // Seed rows the priority calls will touch (writer still healthy).
        db.record_deposit("base", "0xn", "u", "1").unwrap();
        db.mark_deposit_failed("base", "0xn").unwrap();
        db.record_erc20_deposit("base", "0xe", 1, "u", "1", "0xt", "USDC")
            .unwrap();
        db.mark_erc20_deposit_failed("base", "0xe:1").unwrap();
        db.upsert_webhook_delivery("wid", "ev", "u", "https://example.com", "{}")
            .unwrap();
        db.record_webhook_attempt("wid", "ev", Some(500), Some("err"), "failed")
            .unwrap();

        // Stall the writer and overfill the capacity-1 background lane so
        // background senders are blocked in push_background.
        let hold = occupy_writer(&db, Duration::from_millis(500));
        let mut background = Vec::new();
        for i in 0..3 {
            let db = db.clone();
            background.push(thread::spawn(move || {
                db.record_deposit("base", &format!("0xbg{i}"), "u", "1")
                    .unwrap();
            }));
        }
        thread::sleep(Duration::from_millis(50));

        let started = Instant::now();
        assert!(db.retry_native_deposit("base", "0xn").unwrap());
        assert!(db.retry_erc20_deposit("base", "0xe", 1).unwrap());
        assert!(db.retry_webhook_delivery("wid", "ev").unwrap());
        db.set_last_processed_block_priority("base", 7).unwrap();
        db.register_account("prio2", 3, "0xp2", "https://example.com")
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "priority paths must not wait behind the blocked background lane"
        );

        hold.join().unwrap();
        for b in background {
            b.join().unwrap();
        }
    }

    // ========== P0 collision fix: sequential next_index counter ==========

    #[test]
    fn test_register_account_auto_allocates_distinct_sequential_indices() {
        let (_tmp, db) = db_with(test_writer_config());

        let mut seen_indices = std::collections::HashSet::new();
        let mut seen_addresses = std::collections::HashSet::new();
        for i in 0..10 {
            let (index, address, created) = db
                .register_account_auto(&format!("user{i}"), "https://example.com", |idx| {
                    Ok(format!("0xaddr{idx}"))
                })
                .unwrap();
            assert!(created);
            assert_eq!(index, i, "indices must be sequential");
            assert!(seen_indices.insert(index));
            assert!(seen_addresses.insert(address));
        }
    }

    #[test]
    fn test_register_account_auto_reregister_returns_existing() {
        let (_tmp, db) = db_with(test_writer_config());

        let (index, address, created) = db
            .register_account_auto("alice", "https://example.com", |i| Ok(format!("0xaddr{i}")))
            .unwrap();
        assert!(created);

        let (index2, address2, created2) = db
            .register_account_auto("alice", "https://example.com", |i| Ok(format!("0xaddr{i}")))
            .unwrap();
        assert!(!created2, "re-register must not create a new account");
        assert_eq!(index2, index);
        assert_eq!(address2, address);

        // No index was burned by the re-register.
        let (bob_index, _, _) = db
            .register_account_auto("bob", "https://example.com", |i| Ok(format!("0xaddr{i}")))
            .unwrap();
        assert_eq!(bob_index, index + 1);
    }

    #[test]
    fn test_next_index_counter_survives_db_reopen() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        {
            let db = Db::with_options(&path, 5, test_writer_config()).unwrap();
            for i in 0..3 {
                db.register_account_auto(&format!("u{i}"), "https://example.com", |idx| {
                    Ok(format!("0xaddr{idx}"))
                })
                .unwrap();
            }
        }

        let db = Db::with_options(&path, 5, test_writer_config()).unwrap();
        let (index, _, created) = db
            .register_account_auto("u_new", "https://example.com", |idx| {
                Ok(format!("0xaddr{idx}"))
            })
            .unwrap();
        assert!(created);
        assert_eq!(index, 3, "counter must persist across reopen (no reuse)");
    }

    #[test]
    fn test_migration_seeds_counter_above_legacy_max_index() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        // Simulate a pre-V3 database with legacy hash-derived indices.
        {
            let mut conn = Connection::open(&path).unwrap();
            apply_pragmas(&conn).unwrap();
            Migrations::new(vec![
                M::up(include_str!("../../migrations/V1__initial.sql")),
                M::up(include_str!("../../migrations/V2__webhook_deliveries.sql")),
            ])
            .to_latest(&mut conn)
            .unwrap();
            conn.execute(
                "INSERT INTO accounts (id, derivation_index, address, webhook_url)
                 VALUES ('legacy', 12345, '0xlegacy', 'https://example.com')",
                [],
            )
            .unwrap();
        }

        // Opening through Db runs V3, which must seed the counter past 12345.
        let db = Db::with_options(&path, 5, test_writer_config()).unwrap();
        let (index, _, created) = db
            .register_account_auto("fresh", "https://example.com", |idx| {
                Ok(format!("0xaddr{idx}"))
            })
            .unwrap();
        assert!(created);
        assert_eq!(
            index, 12346,
            "new allocations must start above the legacy max index"
        );
    }

    /// E2E saturation: sustained synthetic catch-up traffic on the background
    /// lane must not push register latency past the interactive timeout.
    #[test]
    fn test_register_latency_stays_bounded_under_background_saturation() {
        let (_tmp, db) = db_with(test_writer_config()); // 5s timeout, batch 50

        let stop = Arc::new(AtomicBool::new(false));
        let writes_done = Arc::new(AtomicUsize::new(0));
        let mut hammers = Vec::new();
        for t in 0..4 {
            let db = db.clone();
            let stop = Arc::clone(&stop);
            let writes_done = Arc::clone(&writes_done);
            hammers.push(thread::spawn(move || {
                let mut i = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    db.record_deposit("base", &format!("0xh{t}x{i}"), "u", "1")
                        .unwrap();
                    db.set_last_processed_block("base", i as u64).unwrap();
                    writes_done.fetch_add(2, Ordering::Relaxed);
                    i += 1;
                }
            }));
        }

        // Let the hammers build a steady stream, then measure registers.
        thread::sleep(Duration::from_millis(100));
        let mut worst = Duration::ZERO;
        for i in 0..20 {
            let started = Instant::now();
            db.register_account_auto(&format!("sat_user{i}"), "https://example.com", |idx| {
                Ok(format!("0xaddr{idx}"))
            })
            .unwrap();
            worst = worst.max(started.elapsed());
        }

        stop.store(true, Ordering::Relaxed);
        for h in hammers {
            h.join().unwrap();
        }

        assert!(
            worst < WriterConfig::default().write_timeout,
            "worst register latency {worst:?} exceeded the interactive timeout \
             under background saturation ({} background writes)",
            writes_done.load(Ordering::Relaxed)
        );
    }
}
