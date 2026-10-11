//! D1 databases: one SQLite file per database in `D1_DATA_DIR`.
//!
//! Each database has one connection and one queue: calls run one at a time,
//! in order, on the blocking thread pool. A result is returned after its
//! SQLite commit, which `synchronous=FULL` makes durable on the local volume.

pub mod requests;
mod sqlite;

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use openworkers_core::D1Error;
use openworkers_core::D1ErrorKind;
use openworkers_core::D1Op;
use openworkers_core::D1Result;
use rusqlite::InterruptHandle;

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
        })
    }
}

/// One database: its connection, opened on first use, and its queue length
struct Handle {
    database: Arc<tokio::sync::Mutex<Option<Database>>>,
    waiting: AtomicUsize,
    last_used: Mutex<Instant>,
}

pub struct Service {
    config: Config,
    databases: Mutex<HashMap<String, Arc<Handle>>>,
    /// Held for the life of the process, so a second runner cannot open the
    /// same files, even while this one is paused
    _lock: File,
}

static SERVICE: OnceLock<Service> = OnceLock::new();

/// Starts D1 when `D1_DATA_DIR` is set. Fails when another process holds
/// the data directory.
pub fn init() -> Result<(), String> {
    let Some(config) = Config::from_env() else {
        return Ok(());
    };

    let service = Service::new(config)?;

    tracing::info!("D1 data directory: {}", service.config.data_dir.display());

    if SERVICE.set(service).is_err() {
        return Err("D1 is already initialized".to_string());
    }

    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(60));

        loop {
            tick.tick().await;

            if let Some(service) = SERVICE.get() {
                service.close_idle();
            }
        }
    });

    Ok(())
}

pub fn service() -> Option<&'static Service> {
    SERVICE.get()
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

        Ok(Self {
            config,
            databases: Mutex::new(HashMap::new()),
            _lock: lock,
        })
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

        result
    }

    async fn run(&self, id: &str, handle: &Handle, job: Job) -> D1Result {
        let deadline = tokio::time::Instant::now() + self.config.timeout;

        let Ok(mut guard) =
            tokio::time::timeout_at(deadline, handle.database.clone().lock_owned()).await
        else {
            return D1Result::Error(error(
                D1ErrorKind::Overloaded,
                "timed out waiting for the database".to_string(),
            ));
        };

        if guard.is_none() {
            let path = self.config.data_dir.join(format!("{id}.sqlite"));
            let limits = self.config.limits.clone();

            match tokio::task::spawn_blocking(move || Database::open(&path, &limits)).await {
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

        let mut job = tokio::task::spawn_blocking(move || {
            let result = guard.as_mut().unwrap().run(job);

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

        result.unwrap_or_else(|_| internal_error())
    }

    fn handle(&self, id: &str) -> Arc<Handle> {
        let mut databases = self.databases.lock().unwrap();

        if let Some(handle) = databases.get(id) {
            return handle.clone();
        }

        if databases.len() >= self.config.max_open {
            let oldest = databases
                .iter()
                .filter(|(_, handle)| Arc::strong_count(handle) == 1)
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
        });

        databases.insert(id.to_string(), handle.clone());
        handle
    }

    /// Deletes the files of a deleted database, unless a call still uses it;
    /// the next sweep tries again.
    pub fn remove_files(&self, id: &str) {
        let Ok(id) = uuid::Uuid::parse_str(id) else {
            return;
        };

        let id = id.to_string();
        let mut databases = self.databases.lock().unwrap();

        if databases
            .get(&id)
            .is_some_and(|handle| Arc::strong_count(handle) > 1)
        {
            return;
        }

        databases.remove(&id);
        drop(databases);

        for suffix in ["sqlite", "sqlite-wal", "sqlite-shm"] {
            let path = self.config.data_dir.join(format!("{id}.{suffix}"));

            match std::fs::remove_file(&path) {
                Ok(()) if suffix == "sqlite" => tracing::info!("D1: removed deleted database {id}"),
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => tracing::warn!("D1: cannot remove {}: {err}", path.display()),
            }
        }
    }

    /// Drops the connections nobody used for `idle`; SQLite checkpoints the
    /// WAL when the last connection closes.
    fn close_idle(&self) {
        let now = Instant::now();

        self.databases.lock().unwrap().retain(|_, handle| {
            Arc::strong_count(handle) > 1
                || now.duration_since(*handle.last_used.lock().unwrap()) < self.config.idle
        });
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
