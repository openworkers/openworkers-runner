//! D1 databases: one SQLite file per database in `D1_DATA_DIR`.
//!
//! Each database has one connection and one queue: calls run one at a time,
//! in order, on the blocking thread pool. A result is returned after its
//! SQLite commit, which `synchronous=FULL` makes durable on the local volume.
//!
//! With `D1_ARCHIVE_*` set, a changed database is copied to object storage
//! every few minutes, and a missing file is restored from its latest copy
//! before the database serves a call (see `archive`).

pub mod archive;
pub mod requests;
mod sqlite;

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use openworkers_core::D1Error;
use openworkers_core::D1ErrorKind;
use openworkers_core::D1Op;
use openworkers_core::D1Result;
use rusqlite::InterruptHandle;

pub use archive::Archive;
use archive::Index;
pub use sqlite::Limits;
pub use sqlite::Migration;

use sqlite::Database;
use sqlite::Job;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub limits: Limits,
    /// Longest time a call waits in its queue and runs
    pub timeout: Duration,
    /// Calls waiting for one database before new calls are refused
    pub max_queue: usize,
    pub max_open: usize,
    /// A database unused this long is closed
    pub idle: Duration,
    /// Writes are refused below this free space on the data volume
    pub min_free_bytes: u64,
}

impl Config {
    /// `None` when `D1_DATA_DIR` is not set: D1 is off.
    pub fn from_env() -> Option<Self> {
        let data_dir = std::env::var_os("D1_DATA_DIR")?;
        let number = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };

        let defaults = Limits::default();

        Some(Self {
            data_dir: PathBuf::from(data_dir),
            limits: Limits {
                database_bytes: number("D1_MAX_DATABASE_BYTES", defaults.database_bytes),
                result_bytes: number("D1_MAX_RESULT_BYTES", defaults.result_bytes as u64) as usize,
                ..defaults
            },
            timeout: Duration::from_millis(number("D1_TIMEOUT_MS", 30_000)),
            max_queue: number("D1_MAX_QUEUE", 64) as usize,
            max_open: number("D1_MAX_OPEN", 256) as usize,
            idle: Duration::from_secs(number("D1_IDLE_SECS", 300)),
            min_free_bytes: number("D1_MIN_FREE_BYTES", 2 * 1024 * 1024 * 1024),
        })
    }
}

/// One database: its connection, opened on first use, its queue length, and
/// how far its latest snapshot reaches
struct Handle {
    database: Arc<tokio::sync::Mutex<Option<Database>>>,
    waiting: AtomicUsize,
    last_used: Mutex<Instant>,
    /// Transactions committed since the database was opened
    commits: Arc<AtomicU64>,
    /// The value of `commits` that the latest snapshot holds
    saved: AtomicU64,
    /// When the oldest commit that no snapshot holds happened
    unsaved_since: Mutex<Option<Instant>>,
}

impl Handle {
    fn unsaved(&self) -> bool {
        self.commits.load(Ordering::SeqCst) != self.saved.load(Ordering::SeqCst)
    }
}

pub struct Service {
    config: Config,
    archive: Option<Archive>,
    /// Set when the data volume has less than `min_free_bytes` free
    disk_low: AtomicBool,
    /// One snapshot pass at a time
    snapshots: tokio::sync::Mutex<()>,
    databases: Mutex<HashMap<String, Arc<Handle>>>,
    /// Held for the life of the process, so a second runner cannot open the
    /// same files, even while this one is paused
    _lock: File,
}

static SERVICE: OnceLock<Service> = OnceLock::new();

/// Starts D1 when `D1_DATA_DIR` is set, with snapshots when `D1_ARCHIVE_*`
/// is set too. Fails when another process holds the data directory.
pub async fn init(pool: Option<sqlx::PgPool>) -> Result<(), String> {
    let Some(config) = Config::from_env() else {
        return Ok(());
    };

    let mut service = Service::new(config)?;

    if let Some(archive_config) = archive::ArchiveConfig::from_env() {
        let pool = pool.ok_or("D1: snapshots need the platform database")?;
        let index = archive::PgIndex::start(pool)
            .await
            .map_err(|e| format!("D1: cannot take a runner epoch: {e}"))?;

        tracing::info!(
            "D1 snapshots every {} s, runner epoch {}",
            archive_config.interval.as_secs(),
            index.epoch()
        );

        service = service.with_archive(Archive {
            store: Box::new(archive::S3Store {
                config: archive_config.storage.clone(),
            }),
            index: Box::new(index),
            config: archive_config,
        });
    } else {
        tracing::warn!("D1 snapshots are off: D1_ARCHIVE_* is not set");
    }

    tracing::info!("D1 data directory: {}", service.config.data_dir.display());

    if SERVICE.set(service).is_err() {
        return Err("D1 is already initialized".to_string());
    }

    tokio::spawn(async {
        let Some(service) = SERVICE.get() else {
            return;
        };

        let mut tick = tokio::time::interval(Duration::from_secs(30));
        let mut last_pass = Instant::now();

        loop {
            tick.tick().await;
            service.close_idle();
            service.check_disk();

            let due = service
                .archive
                .as_ref()
                .is_some_and(|archive| last_pass.elapsed() >= archive.config.interval);

            if due {
                last_pass = Instant::now();
                service.snapshot_all().await;
            }
        }
    });

    Ok(())
}

/// Takes a last snapshot of every changed database, on a clean shutdown
pub async fn shutdown() {
    let Some(service) = SERVICE.get() else {
        return;
    };

    if service.archive.is_none() {
        return;
    }

    if tokio::time::timeout(Duration::from_secs(30), service.snapshot_all())
        .await
        .is_err()
    {
        tracing::error!("D1: the shutdown snapshots did not finish in 30 s");
    }
}

pub fn service() -> Option<&'static Service> {
    SERVICE.get()
}

/// Installs a service built by hand, for tests and embedders; returns false
/// when one is installed already
pub fn install(service: Service) -> bool {
    SERVICE.set(service).is_ok()
}

/// The error for a binding when D1 is off
pub fn unavailable(binding: &str) -> D1Result {
    D1Result::Error(error(
        D1ErrorKind::Unavailable,
        format!("D1 binding '{binding}' has no database: D1 is not enabled on this runner"),
    ))
}

impl Service {
    pub fn new(config: Config) -> Result<Self, String> {
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|err| format!("D1: cannot create the data directory: {err}"))?;

        let lock = File::create(config.data_dir.join(".lock"))
            .map_err(|err| format!("D1: cannot create the lock file: {err}"))?;

        lock.try_lock()
            .map_err(|_| "D1: another process holds the data directory".to_string())?;

        // A snapshot or restore cut short leaves its scratch file behind
        if let Ok(entries) = std::fs::read_dir(&config.data_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().ends_with(".scratch") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        Ok(Self {
            config,
            archive: None,
            disk_low: AtomicBool::new(false),
            snapshots: tokio::sync::Mutex::new(()),
            databases: Mutex::new(HashMap::new()),
            _lock: lock,
        })
    }

    pub fn with_archive(mut self, archive: Archive) -> Self {
        self.archive = Some(archive);
        self
    }

    pub async fn execute(&self, database_id: &str, op: D1Op) -> D1Result {
        self.submit(database_id, Job::Op(op)).await
    }

    pub async fn migrate(&self, database_id: &str, migration: Migration) -> D1Result {
        self.submit(database_id, Job::Migrate(migration)).await
    }

    async fn submit(&self, database_id: &str, job: Job) -> D1Result {
        let Ok(id) = uuid::Uuid::parse_str(database_id) else {
            return D1Result::Error(error(
                D1ErrorKind::Unavailable,
                "the binding does not name a database".to_string(),
            ));
        };

        let handle = self.handle(&id.to_string());

        if handle.waiting.fetch_add(1, Ordering::SeqCst) >= self.config.max_queue {
            handle.waiting.fetch_sub(1, Ordering::SeqCst);

            return D1Result::Error(error(
                D1ErrorKind::Overloaded,
                "too many queries are waiting for this database".to_string(),
            ));
        }

        let result = self.run(&id.to_string(), &handle, job).await;

        handle.waiting.fetch_sub(1, Ordering::SeqCst);
        *handle.last_used.lock().unwrap() = Instant::now();

        if handle.unsaved() {
            handle
                .unsaved_since
                .lock()
                .unwrap()
                .get_or_insert_with(Instant::now);
        }

        result
    }

    /// Why writes are refused now, if they are
    fn paused(&self, handle: &Handle) -> Option<String> {
        if self.disk_low.load(Ordering::SeqCst) {
            return Some("the data volume is almost full".to_string());
        }

        let archive = self.archive.as_ref()?;
        let since = (*handle.unsaved_since.lock().unwrap())?;

        (since.elapsed() >= archive.config.pause_after).then(|| {
            format!(
                "no snapshot for {} minutes; retry later",
                since.elapsed().as_secs() / 60
            )
        })
    }

    async fn run(&self, id: &str, handle: &Handle, job: Job) -> D1Result {
        let queued = Instant::now();
        let deadline = tokio::time::Instant::now() + self.config.timeout;

        let Ok(mut guard) =
            tokio::time::timeout_at(deadline, handle.database.clone().lock_owned()).await
        else {
            crate::metrics::record_d1_call(queued.elapsed().as_secs_f64(), 0.0, "overloaded");

            return D1Result::Error(error(
                D1ErrorKind::Overloaded,
                "timed out waiting for the database".to_string(),
            ));
        };

        let queue_time = queued.elapsed().as_secs_f64();

        if guard.is_none() {
            let path = self.config.data_dir.join(format!("{id}.sqlite"));
            let limits = self.config.limits.clone();
            let commits = handle.commits.clone();

            if let Err(err) = self.restore_if_missing(id).await {
                tracing::error!("D1: cannot restore database {id}: {err}");

                return D1Result::Error(error(
                    D1ErrorKind::Unavailable,
                    "the database cannot be restored from its last snapshot".to_string(),
                ));
            }

            match tokio::task::spawn_blocking(move || Database::open(&path, &limits, commits)).await
            {
                Ok(Ok(database)) => *guard = Some(database),
                Ok(Err(err)) => {
                    tracing::error!("D1: cannot open database {id}: {err}");

                    return D1Result::Error(error(
                        D1ErrorKind::Unavailable,
                        "the database cannot be opened".to_string(),
                    ));
                }
                Err(_) => return internal_error(),
            }
        }

        let interrupt = InterruptOnDrop(Some(guard.as_ref().unwrap().interrupt_handle()));
        let paused = self.paused(handle);

        let mut job = tokio::task::spawn_blocking(move || {
            let result = guard.as_mut().unwrap().run(job, paused);

            drop(guard);
            result
        });

        let result = tokio::select! {
            result = &mut job => result,
            _ = tokio::time::sleep_until(deadline) => {
                // The statement can commit before the interrupt reaches it;
                // the job reports what happened.
                interrupt.interrupt();
                job.await
            }
        };

        interrupt.disarm();

        let result = result.unwrap_or_else(|_| internal_error());
        let execution_time = queued.elapsed().as_secs_f64() - queue_time;

        crate::metrics::record_d1_call(queue_time, execution_time, outcome(&result));

        result
    }

    /// How long the oldest commit that no snapshot holds has waited
    pub fn oldest_unsaved(&self) -> Option<Duration> {
        self.archive.as_ref()?;

        self.databases
            .lock()
            .unwrap()
            .values()
            .filter_map(|handle| *handle.unsaved_since.lock().unwrap())
            .map(|since| since.elapsed())
            .max()
    }

    fn handle(&self, id: &str) -> Arc<Handle> {
        let mut databases = self.databases.lock().unwrap();

        if let Some(handle) = databases.get(id) {
            return handle.clone();
        }

        if databases.len() >= self.config.max_open {
            let oldest = databases
                .iter()
                .filter(|(_, handle)| Arc::strong_count(handle) == 1 && !handle.unsaved())
                .min_by_key(|(_, handle)| *handle.last_used.lock().unwrap())
                .map(|(id, _)| id.clone());

            if let Some(oldest) = oldest {
                databases.remove(&oldest);
            }
        }

        let handle = Arc::new(Handle {
            database: Arc::new(tokio::sync::Mutex::new(None)),
            waiting: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
            commits: Arc::new(AtomicU64::new(0)),
            saved: AtomicU64::new(0),
            unsaved_since: Mutex::new(None),
        });

        databases.insert(id.to_string(), handle.clone());
        handle
    }

    /// Deletes the files of a deleted database. Returns false when a call
    /// still uses it; the next sweep tries again.
    pub fn remove_files(&self, id: &str) -> bool {
        let Ok(id) = uuid::Uuid::parse_str(id) else {
            return true;
        };

        let id = id.to_string();
        let mut databases = self.databases.lock().unwrap();

        if databases
            .get(&id)
            .is_some_and(|handle| Arc::strong_count(handle) > 1)
        {
            return false;
        }

        databases.remove(&id);
        drop(databases);

        for suffix in ["sqlite", "sqlite-wal", "sqlite-shm"] {
            let path = self.config.data_dir.join(format!("{id}.{suffix}"));

            match std::fs::remove_file(&path) {
                Ok(()) if suffix == "sqlite" => tracing::info!("D1: removed deleted database {id}"),
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!("D1: cannot remove {}: {err}", path.display());
                    return false;
                }
            }
        }

        true
    }

    /// Deletes one snapshot object; fails when snapshots are off, so the
    /// request to delete it stays queued
    pub async fn remove_snapshot_object(&self, key: &str) -> Result<(), String> {
        match &self.archive {
            Some(archive) => archive.store.delete(key.to_string()).await,
            None => Err("snapshots are off on this runner".to_string()),
        }
    }

    /// Drops the connections nobody used for `idle`; SQLite checkpoints the
    /// WAL when the last connection closes. A database with commits that no
    /// snapshot holds stays open until a snapshot holds them.
    fn close_idle(&self) {
        let now = Instant::now();

        self.databases.lock().unwrap().retain(|_, handle| {
            Arc::strong_count(handle) > 1
                || handle.unsaved()
                || now.duration_since(*handle.last_used.lock().unwrap()) < self.config.idle
        });
    }

    fn check_disk(&self) {
        let Some(free) = free_bytes(&self.config.data_dir) else {
            return;
        };

        let low = free < self.config.min_free_bytes;

        if self.disk_low.swap(low, Ordering::SeqCst) != low {
            if low {
                tracing::error!("D1: {free} bytes free on the data volume; writes are paused");
            } else {
                tracing::info!("D1: {free} bytes free on the data volume; writes resume");
            }
        }
    }

    /// Takes a snapshot of every open database with unsaved commits, and
    /// logs the ones that wait for too long
    pub async fn snapshot_all(&self) {
        let Some(archive) = &self.archive else {
            return;
        };

        let _pass = self.snapshots.lock().await;

        let unsaved: Vec<(String, Arc<Handle>)> = self
            .databases
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, handle)| handle.unsaved())
            .map(|(id, handle)| (id.clone(), handle.clone()))
            .collect();

        for (id, handle) in unsaved {
            let started = Instant::now();
            let result = self.snapshot(archive, &id, &handle).await;

            crate::metrics::record_d1_snapshot(started.elapsed().as_secs_f64(), result.is_ok());

            if let Err(err) = result {
                let since = *handle.unsaved_since.lock().unwrap();
                let minutes = since.map_or(0, |since| since.elapsed().as_secs() / 60);

                if since.is_some_and(|since| since.elapsed() >= archive.config.alert_after) {
                    tracing::error!(
                        "D1: database {id} has commits without a snapshot for {minutes} min: {err}"
                    );
                } else {
                    tracing::warn!("D1: snapshot of database {id} failed: {err}");
                }
            }
        }
    }

    async fn snapshot(&self, archive: &Archive, id: &str, handle: &Handle) -> Result<(), String> {
        let started = Instant::now();
        let covered = handle.commits.load(Ordering::SeqCst);
        let path = self.config.data_dir.join(format!("{id}.sqlite"));
        let scratch = self.config.data_dir.join(format!("{id}.snapshot.scratch"));

        let bytes = tokio::task::spawn_blocking(move || archive::take_snapshot(&path, &scratch))
            .await
            .map_err(|e| e.to_string())??;

        let snapshot = archive.save(id, bytes).await?;

        handle.saved.store(covered, Ordering::SeqCst);

        *handle.unsaved_since.lock().unwrap() =
            (handle.commits.load(Ordering::SeqCst) != covered).then_some(started);

        tracing::debug!(
            "D1: snapshot of {id}: {} bytes in {} ms",
            snapshot.size,
            started.elapsed().as_millis()
        );

        Ok(())
    }

    /// Writes the latest snapshot of a database whose file is missing. A
    /// database without a snapshot starts empty.
    async fn restore_if_missing(&self, id: &str) -> Result<(), String> {
        let path = self.config.data_dir.join(format!("{id}.sqlite"));

        let Some(archive) = &self.archive else {
            return Ok(());
        };

        if path.exists() {
            return Ok(());
        }

        let Some(bytes) = archive.fetch_latest(id).await? else {
            return Ok(());
        };

        let scratch = self.config.data_dir.join(format!("{id}.restore.scratch"));
        let size = bytes.len();
        let started = Instant::now();

        tokio::task::spawn_blocking(move || archive::restore_snapshot(&bytes, &path, &scratch))
            .await
            .map_err(|e| e.to_string())??;

        crate::metrics::record_d1_restore(started.elapsed().as_secs_f64());
        tracing::info!("D1: restored database {id} from its last snapshot ({size} bytes)");

        Ok(())
    }

    /// Moves the files of a database aside, so its next call restores the
    /// latest snapshot. Fails while a call uses the database.
    pub fn reset_to_snapshot(&self, id: &str) -> Result<(), String> {
        if self.archive.is_none() {
            return Err("snapshots are off on this runner".to_string());
        }

        let id = uuid::Uuid::parse_str(id)
            .map_err(|_| "not a database ID".to_string())?
            .to_string();

        let mut databases = self.databases.lock().unwrap();

        if databases
            .get(&id)
            .is_some_and(|handle| Arc::strong_count(handle) > 1)
        {
            return Err("the database is in use; retry".to_string());
        }

        databases.remove(&id);
        drop(databases);

        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");

        for suffix in ["sqlite", "sqlite-wal", "sqlite-shm"] {
            let path = self.config.data_dir.join(format!("{id}.{suffix}"));

            if path.exists() {
                let aside = self
                    .config
                    .data_dir
                    .join(format!("{id}.{suffix}.replaced-{stamp}"));

                std::fs::rename(&path, &aside).map_err(|e| format!("move aside: {e}"))?;
            }
        }

        tracing::info!("D1: database {id} reset; its next call restores the last snapshot");

        Ok(())
    }

    /// Removes the snapshots of a deleted database
    pub async fn forget_snapshots(&self, id: &str) -> Result<(), String> {
        match &self.archive {
            Some(archive) => archive.forget_all(id).await,
            None => Ok(()),
        }
    }
}

/// Interrupts the running statement when the caller stops waiting for it,
/// for example when the worker request is cancelled.
struct InterruptOnDrop(Option<InterruptHandle>);

impl InterruptOnDrop {
    fn interrupt(&self) {
        if let Some(handle) = &self.0 {
            handle.interrupt();
        }
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for InterruptOnDrop {
    fn drop(&mut self) {
        self.interrupt();
    }
}

/// A metric label for the outcome of a call
fn outcome(result: &D1Result) -> &'static str {
    match result {
        D1Result::Error(err) => match err.kind {
            D1ErrorKind::Sql => "sql",
            D1ErrorKind::Type => "type",
            D1ErrorKind::Range => "range",
            D1ErrorKind::Interrupted => "interrupted",
            D1ErrorKind::Overloaded => "overloaded",
            D1ErrorKind::Unavailable => "unavailable",
        },
        _ => "ok",
    }
}

/// Free bytes for an unprivileged user on the volume of `path`
fn free_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

    // SAFETY: a valid C string and a zeroed struct that statvfs fills
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }

    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

fn error(kind: D1ErrorKind, message: String) -> D1Error {
    D1Error {
        kind,
        message,
        statement: None,
    }
}

fn internal_error() -> D1Result {
    D1Result::Error(error(
        D1ErrorKind::Unavailable,
        "internal error".to_string(),
    ))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod archive_tests;
