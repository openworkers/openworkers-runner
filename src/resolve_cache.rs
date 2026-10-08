//! The worker a request resolves to, and that worker's bindings, kept in
//! memory so that a request does not query Postgres.
//!
//! A trigger on every table they come from sends `worker_update`
//! (openworkers-cli migration 29), and `listen` drops every entry when one
//! arrives. The cache serves nothing while the listener is not connected or
//! the triggers are missing, because then a change would go unseen. An
//! entry also expires after `TTL`, the bound on a notification lost in a
//! way the listener cannot see.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lru::LruCache;
use once_cell::sync::Lazy;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgListener;
use sqlx::{PgPool, Postgres};

use crate::store::{self, RequestResolution, WorkerIdentifier, WorkerMeta, WorkerWithBindings};

const TTL: Duration = Duration::from_secs(60);
const RESOLUTIONS: usize = 10_000;
const WORKERS: usize = 2_000;
const CHANNEL: &str = "worker_update";
/// The tables migration 29 puts a trigger on.
const TRIGGERS: i64 = 10;
const RECHECK: Duration = Duration::from_secs(30);

/// True while the listener is connected and the triggers exist.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Bumped by every invalidation. A lookup that began before a bump does not
/// store what it read, which can predate the change.
static GENERATION: AtomicU64 = AtomicU64::new(0);

static CACHE: Lazy<Mutex<Cache>> = Lazy::new(|| Mutex::new(Cache::new(RESOLUTIONS, WORKERS)));

/// What a request names: its host, its x-worker-id and x-worker-name
/// headers, and its path.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    host: Option<String>,
    worker_id: Option<String>,
    worker_name: Option<String>,
    /// None for a standalone worker, whose resolution does not read the path.
    path: Option<String>,
}

struct Entry<T> {
    value: T,
    at: Instant,
}

struct Cache {
    resolutions: LruCache<Key, Entry<Option<RequestResolution>>>,
    workers: LruCache<String, Entry<WorkerMeta>>,
}

impl Cache {
    fn new(resolutions: usize, workers: usize) -> Self {
        Self {
            resolutions: LruCache::new(NonZeroUsize::new(resolutions).unwrap()),
            workers: LruCache::new(NonZeroUsize::new(workers).unwrap()),
        }
    }

    fn resolution(&mut self, key: &Key, now: Instant) -> Option<Option<RequestResolution>> {
        let pathless = Key {
            path: None,
            ..key.clone()
        };

        [pathless, key.clone()].into_iter().find_map(|key| {
            let entry = self.resolutions.get(&key)?;

            (now.duration_since(entry.at) < TTL).then(|| entry.value.clone())
        })
    }

    /// Only a project reads the path: a standalone worker is stored for
    /// every path. Anything else, a miss included, keeps the path, because
    /// a project whose route matches nothing resolves to nothing as well.
    fn put_resolution(&mut self, mut key: Key, value: Option<RequestResolution>, now: Instant) {
        if value
            .as_ref()
            .is_some_and(|r| r.project_id.is_none() && r.worker_id.is_some())
        {
            key.path = None;
        }

        self.resolutions.put(key, Entry { value, at: now });
    }

    fn worker(&mut self, worker_id: &str, now: Instant) -> Option<WorkerMeta> {
        let entry = self.workers.get(worker_id)?;

        (now.duration_since(entry.at) < TTL).then(|| entry.value.clone())
    }

    fn put_worker(&mut self, meta: WorkerMeta, now: Instant) {
        self.workers.put(
            meta.id.clone(),
            Entry {
                value: meta,
                at: now,
            },
        );
    }

    fn clear(&mut self) {
        self.resolutions.clear();
        self.workers.clear();
    }
}

/// The generation a lookup starts at, or None when the cache is off.
fn begin() -> Option<u64> {
    ENABLED
        .load(Ordering::SeqCst)
        .then(|| GENERATION.load(Ordering::SeqCst))
}

/// Whether a lookup that began at `generation` may store what it read.
fn still(generation: Option<u64>) -> bool {
    generation
        .is_some_and(|g| ENABLED.load(Ordering::SeqCst) && GENERATION.load(Ordering::SeqCst) == g)
}

/// Stores what a lookup that began at `generation` read, unless the cache
/// was cleared or turned off since.
fn store_resolution(generation: Option<u64>, key: Key, value: Option<RequestResolution>) {
    let mut cache = CACHE.lock().unwrap();

    if still(generation) {
        cache.put_resolution(key, value, Instant::now());
    }
}

fn store_worker(generation: Option<u64>, meta: WorkerMeta) {
    let mut cache = CACHE.lock().unwrap();

    if still(generation) {
        cache.put_worker(meta, Instant::now());
    }
}

fn invalidate(reason: &str) {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    CACHE.lock().unwrap().clear();
    tracing::debug!("resolve cache cleared: {reason}");
}

/// A pool connection, taken on first use only: a request that the cache
/// answers takes none.
struct Connection<'a> {
    pool: &'a PgPool,
    conn: Option<PoolConnection<Postgres>>,
}

impl Connection<'_> {
    async fn get(&mut self) -> Result<&mut sqlx::PgConnection, sqlx::Error> {
        if self.conn.is_none() {
            self.conn = Some(self.pool.acquire().await?);
        }

        Ok(self.conn.as_mut().unwrap())
    }
}

/// `store::resolve_worker_from_request`, answered from memory when it can.
pub async fn resolve(
    pool: &PgPool,
    host: Option<&str>,
    worker_id: Option<&str>,
    worker_name: Option<&str>,
    path: &str,
) -> Result<Option<RequestResolution>, sqlx::Error> {
    let key = Key {
        host: host.map(str::to_string),
        worker_id: worker_id.map(str::to_string),
        worker_name: worker_name.map(str::to_string),
        path: Some(path.to_string()),
    };
    let generation = begin();

    if generation.is_some()
        && let Some(resolution) = CACHE.lock().unwrap().resolution(&key, Instant::now())
    {
        return Ok(resolution);
    }

    let mut conn = pool.acquire().await?;
    let resolution =
        store::resolve_worker_from_request(&mut conn, host, worker_id, worker_name, path).await?;

    store_resolution(generation, key, resolution.clone());

    Ok(resolution)
}

/// `store::get_worker_with_bindings` by id, with the worker and its bindings
/// from memory when it can. The code comes from the code cache, or else
/// from the database.
pub async fn worker(
    pool: &PgPool,
    worker_id: &str,
) -> Result<Option<WorkerWithBindings>, sqlx::Error> {
    let mut conn = Connection { pool, conn: None };
    let generation = begin();
    let cached = generation
        .is_some()
        .then(|| CACHE.lock().unwrap().worker(worker_id, Instant::now()))
        .flatten();

    let meta = match cached {
        Some(meta) => meta,
        None => {
            let read = store::get_worker_meta(
                conn.get().await?,
                WorkerIdentifier::Id(worker_id.to_string()),
            )
            .await;

            let Some(meta) = read else {
                return Ok(None);
            };

            store_worker(generation, meta.clone());

            meta
        }
    };

    if let Some(code) = meta.cached_code() {
        return Ok(Some(meta.into_worker(code)));
    }

    Ok(meta.with_code(conn.get().await?).await)
}

/// Keeps the cache in step with the database: LISTEN on `worker_update`,
/// on a connection of its own so that it takes none from the pools.
pub async fn listen(database_url: String) {
    loop {
        ENABLED.store(false, Ordering::SeqCst);

        if let Err(err) = listen_once(&database_url).await {
            tracing::warn!("resolve cache off, listener: {err}");
        }

        invalidate("listener stopped");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn listen_once(database_url: &str) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect(database_url).await?;
    listener.listen(CHANNEL).await?;

    let triggers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_trigger WHERE tgname = 'worker_update_notify'")
            .fetch_one(&mut listener)
            .await?;

    // Checked again after RECHECK, so a migration run later turns it on
    if triggers < TRIGGERS {
        tracing::error!(
            "resolve cache off: {triggers} of {TRIGGERS} worker_update triggers; run the openworkers-cli migrations"
        );
        tokio::time::sleep(RECHECK).await;
        return Ok(());
    }

    // Anything stored before this point may have missed a notification.
    invalidate("listener connected");
    ENABLED.store(true, Ordering::SeqCst);
    tracing::info!("resolve cache on");

    loop {
        match listener.try_recv().await? {
            Some(notification) => invalidate(notification.payload()),
            // The connection dropped: notifications in the gap are lost
            None => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::BackendType;

    fn key(path: &str) -> Key {
        Key {
            host: Some("example.com".to_string()),
            worker_id: None,
            worker_name: None,
            path: Some(path.to_string()),
        }
    }

    fn resolved(worker: Option<&str>, project: Option<&str>) -> Option<RequestResolution> {
        Some(RequestResolution {
            worker_id: worker.map(str::to_string),
            project_id: project.map(str::to_string),
            backend_type: Some(BackendType::Worker),
            assets_storage_id: None,
            asset_type: None,
        })
    }

    #[test]
    fn a_standalone_worker_answers_every_path() {
        let mut cache = Cache::new(8, 8);
        let now = Instant::now();

        cache.put_resolution(key("/a"), resolved(Some("w"), None), now);

        let hit = cache.resolution(&key("/b"), now).unwrap().unwrap();
        assert_eq!(hit.worker_id.as_deref(), Some("w"));
    }

    #[test]
    fn a_project_answers_its_own_path_only() {
        let mut cache = Cache::new(8, 8);
        let now = Instant::now();

        cache.put_resolution(key("/a"), resolved(Some("w"), Some("p")), now);

        assert!(cache.resolution(&key("/a"), now).is_some());
        assert!(cache.resolution(&key("/b"), now).is_none());
    }

    #[test]
    fn a_miss_answers_its_own_path_only() {
        let mut cache = Cache::new(8, 8);
        let now = Instant::now();

        cache.put_resolution(key("/a"), None, now);

        assert!(matches!(cache.resolution(&key("/a"), now), Some(None)));
        assert!(cache.resolution(&key("/b"), now).is_none());
    }

    #[test]
    fn an_entry_expires_after_the_ttl() {
        let mut cache = Cache::new(8, 8);
        let now = Instant::now();

        cache.put_resolution(key("/a"), resolved(Some("w"), None), now);

        assert!(
            cache
                .resolution(&key("/a"), now + TTL - Duration::from_millis(1))
                .is_some()
        );
        assert!(cache.resolution(&key("/a"), now + TTL).is_none());
    }

    fn meta(id: &str) -> WorkerMeta {
        WorkerMeta {
            id: id.to_string(),
            name: None,
            user_id: "owner".to_string(),
            code_type: crate::store::CodeType::Javascript,
            version: 1,
            env: Default::default(),
            bindings: vec![],
            env_updated_at: None,
        }
    }

    /// A lookup reads the database, a change clears the cache meanwhile,
    /// and the lookup then stores: what it read predates the change.
    #[test]
    #[serial_test::serial]
    fn a_clear_during_a_lookup_keeps_its_answer_out() {
        ENABLED.store(true, Ordering::SeqCst);
        invalidate("test start");

        let generation = begin();
        invalidate("a change while the lookup reads");
        store_resolution(generation, key("/race"), resolved(Some("w"), None));
        store_worker(generation, meta("race-worker"));

        let mut cache = CACHE.lock().unwrap();
        let now = Instant::now();
        assert!(cache.resolution(&key("/race"), now).is_none());
        assert!(cache.worker("race-worker", now).is_none());
        drop(cache);

        // The same lookup without a clear in between stores its answer
        let generation = begin();
        store_resolution(generation, key("/race"), resolved(Some("w"), None));
        assert!(
            CACHE
                .lock()
                .unwrap()
                .resolution(&key("/race"), now)
                .is_some()
        );

        ENABLED.store(false, Ordering::SeqCst);
        invalidate("test end");
    }

    #[test]
    #[serial_test::serial]
    fn a_lookup_older_than_an_invalidation_stores_nothing() {
        ENABLED.store(true, Ordering::SeqCst);
        let generation = begin();
        assert!(still(generation));

        invalidate("test");
        assert!(!still(generation));

        ENABLED.store(false, Ordering::SeqCst);
        assert!(begin().is_none());
        assert!(!still(begin()));
    }
}
