//! Durable Objects: the namespaces of the bindings, their ids, and the
//! registry of the objects this runner serves.
//!
//! Each object runs on one thread of the worker pool, which a hash of its key
//! selects, with a mailbox in the registry. An object that is idle with an
//! empty mailbox removes its entry, so a later request starts it again.

pub mod alarms;
pub mod id;

use std::sync::{Arc, Mutex};

use id::IdKey;

/// A Durable Object namespace: the class a worker exports, and the key of its
/// ids.
#[derive(Clone, Debug)]
pub struct NamespaceConfig {
    /// Stable id of the namespace
    pub id: String,
    /// The account of the namespace, which must own the worker too
    pub user_id: String,
    /// The worker that exports the class
    pub worker_id: String,
    pub class_name: String,
    pub key: IdKey,
}

/// The call depths of the events in flight of an object. Nothing tells
/// which event makes a call, so each call of the object goes one deeper
/// than its deepest event.
#[derive(Default)]
pub struct CallDepths(Mutex<Vec<u32>>);

impl CallDepths {
    pub fn deepest(&self) -> u32 {
        self.0.lock().unwrap().iter().copied().max().unwrap_or(0)
    }

    /// Counts an event of `depth` until the guard drops.
    pub fn enter(self: &Arc<Self>, depth: u32) -> DepthGuard {
        self.0.lock().unwrap().push(depth);

        DepthGuard {
            depths: Arc::clone(self),
            depth,
        }
    }
}

pub struct DepthGuard {
    depths: Arc<CallDepths>,
    depth: u32,
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        let mut depths = self.depths.0.lock().unwrap();
        let index = depths
            .iter()
            .position(|depth| *depth == self.depth)
            .expect("the guard counted its depth");

        depths.swap_remove(index);
    }
}

#[cfg(feature = "v8")]
pub use registry::{
    AlarmError, DRAIN, IDLE, MAX_RESIDENT_PER_ACCOUNT, ObjectSource, alarm, load, retire,
    running_versions, send, stop_namespace,
};

/// Without a JavaScript engine that serves objects, none runs.
#[cfg(not(feature = "v8"))]
pub fn stop_namespace(_namespace_id: &str) -> bool {
    true
}

static WORKERS_CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Tells the version check that a worker may have a new deployment.
pub fn workers_changed() {
    WORKERS_CHANGED.notify_one();
}

/// The `(worker, version)` pairs of `running` that are not the current
/// deployment of their worker, or whose worker is gone.
pub async fn outdated(
    db: &sqlx::PgPool,
    running: &[(String, i32)],
) -> Result<Vec<(String, i32)>, sqlx::Error> {
    let workers: Vec<&str> = running.iter().map(|(worker, _)| worker.as_str()).collect();
    let current: std::collections::HashMap<String, Option<i32>> =
        sqlx::query_as("SELECT id::text, current_version FROM workers WHERE id::text = ANY($1)")
            .bind(&workers)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();

    Ok(running
        .iter()
        .filter(|(worker, version)| current.get(worker).copied().flatten() != Some(*version))
        .cloned()
        .collect())
}

/// Stops the objects that run an old deployment of their worker, so the next
/// request starts them with the new one.
#[cfg(feature = "v8")]
pub async fn watch_versions(db: sqlx::PgPool) {
    // A check without a notification, in case one is lost
    const VERSION_CHECK: std::time::Duration = std::time::Duration::from_secs(60);

    loop {
        tokio::select! {
            _ = WORKERS_CHANGED.notified() => {}
            _ = tokio::time::sleep(VERSION_CHECK) => {}
        }

        let running = running_versions();

        if running.is_empty() {
            continue;
        }

        match outdated(&db, &running).await {
            Ok(outdated) => {
                for (worker, version) in outdated {
                    retire(&worker, version);
                }
            }
            Err(err) => tracing::warn!("durable objects: cannot check versions: {err}"),
        }
    }
}

#[cfg(feature = "v8")]
mod registry {
    use std::collections::{HashMap, HashSet};
    use std::future::Future;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use once_cell::sync::Lazy;
    use openworkers_core::{
        FetchInit, HttpRequest, HttpResponse, LogEvent, LogLevel, OperationsHandle, RuntimeLimits,
        Script, TerminationReason,
    };
    use openworkers_runtime_v8::{AlarmInit, DurableObject, ObjectEvent, ObjectSpec};
    use tokio::sync::{mpsc, oneshot};

    use super::CallDepths;
    use crate::worker_pool::WORKER_POOL;

    /// An object with no event in flight for this long gives its isolate back.
    pub const IDLE: Duration = Duration::from_secs(60);

    /// An object that stops has this long to end the events it took.
    pub const DRAIN: Duration = Duration::from_secs(30);

    /// Each object has an isolate of its own, so an account that starts
    /// objects without end would take the memory of the runner.
    pub const MAX_RESIDENT_PER_ACCOUNT: usize = 100;

    /// What an object is built from: the worker that exports its class.
    pub struct ObjectSource {
        pub script: Script,
        /// The deployment of the worker the script comes from
        pub version: i32,
        pub ops: OperationsHandle,
        pub limits: RuntimeLimits,
        pub idle: Duration,
        pub drain: Duration,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct ObjectKey {
        namespace: String,
        id: String,
    }

    struct Entry {
        mailbox: mpsc::UnboundedSender<ObjectEvent>,
        /// Tells the task that serves the entry from a later one
        generation: u64,
        depths: Arc<CallDepths>,
        /// The worker that exports the class
        worker_id: String,
        /// The deployment it runs, once loaded
        version: Option<i32>,
        /// Closes when the task ends
        done: oneshot::Receiver<()>,
    }

    #[derive(Default)]
    struct Registry {
        objects: HashMap<ObjectKey, Entry>,
        /// Tasks that serve an object, per namespace
        running: HashMap<String, usize>,
        /// Tasks that serve an object, per account
        accounts: HashMap<String, usize>,
        /// Deleted namespaces; their IDs are never reused
        stopped: HashSet<String>,
        /// Objects that stop for a new deployment, with the generation of
        /// their task: the next task of the same object waits for its end
        retiring: HashMap<ObjectKey, (u64, oneshot::Receiver<()>)>,
    }

    static REGISTRY: Lazy<Mutex<Registry>> = Lazy::new(Default::default);
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    /// Counts the task of an object until it ends, however it ends.
    struct Running {
        key: ObjectKey,
        generation: u64,
        account: String,
        _done: oneshot::Sender<()>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            let mut registry = REGISTRY.lock().unwrap();

            release(&mut registry.running, &self.key.namespace);
            release(&mut registry.accounts, &self.account);

            if registry
                .retiring
                .get(&self.key)
                .is_some_and(|(generation, _)| *generation == self.generation)
            {
                registry.retiring.remove(&self.key);
            }
        }
    }

    fn release(counts: &mut HashMap<String, usize>, key: &str) {
        let count = counts.get_mut(key).expect("a running task is counted");
        *count -= 1;

        if *count == 0 {
            counts.remove(key);
        }
    }

    /// Stops the objects of a deleted namespace: no new request reaches them,
    /// and each one ends after the requests it has taken. Answers whether
    /// none runs any more, so its files can go.
    pub fn stop_namespace(namespace_id: &str) -> bool {
        let mut registry = REGISTRY.lock().unwrap();

        registry.stopped.insert(namespace_id.to_string());
        registry
            .objects
            .retain(|key, _| key.namespace != namespace_id);

        !registry.running.contains_key(namespace_id)
    }

    /// The workers that export the classes of running objects, with the
    /// deployments the objects run.
    pub fn running_versions() -> Vec<(String, i32)> {
        let registry = REGISTRY.lock().unwrap();
        let versions: HashSet<(String, i32)> = registry
            .objects
            .values()
            .filter_map(|entry| Some((entry.worker_id.clone(), entry.version?)))
            .collect();

        versions.into_iter().collect()
    }

    /// Stops the objects that run `version` of `worker_id`: they take no new
    /// request, and end once their events do, or after DRAIN. The next
    /// request starts the object again, after the old one ends.
    pub fn retire(worker_id: &str, version: i32) {
        let mut registry = REGISTRY.lock().unwrap();
        let old: Vec<ObjectKey> = registry
            .objects
            .iter()
            .filter(|(_, entry)| entry.worker_id == worker_id && entry.version == Some(version))
            .map(|(key, _)| key.clone())
            .collect();

        for key in old {
            let entry = registry.objects.remove(&key).expect("the key is listed");
            registry
                .retiring
                .insert(key, (entry.generation, entry.done));
        }
    }

    /// Sends `request` to the object `id` of `namespace`, as a call of
    /// `depth`, and starts the object if it is not running. `load` gives what
    /// the object is built from, with the depths of its events; it runs once
    /// per start.
    pub async fn send<L, Fut>(
        namespace: &super::NamespaceConfig,
        id: String,
        depth: u32,
        request: HttpRequest,
        load: L,
    ) -> Result<HttpResponse, String>
    where
        L: FnOnce(Arc<CallDepths>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<ObjectSource, String>> + Send + 'static,
    {
        let (res_tx, res_rx) = oneshot::channel();
        let event = ObjectEvent::Fetch(FetchInit::new(request, res_tx));

        // The event counts at its depth until its response comes
        let entered = admit(namespace, id, depth, event, load)?;
        let response = res_rx.await;
        drop(entered);

        response.map_err(|_| "the Durable Object stopped".to_string())
    }

    /// Why an alarm did not end well.
    #[derive(Debug, PartialEq)]
    pub enum AlarmError {
        /// The runner did not take the alarm: its handler did not run
        Refused(String),
        /// The handler failed, or the object stopped before it ended
        Failed(String),
    }

    /// Runs the alarm handler of the object `id` of `namespace`, as `send`
    /// runs a request. `retry_count` is how many times the alarm failed.
    pub async fn alarm<L, Fut>(
        namespace: &super::NamespaceConfig,
        id: String,
        retry_count: u32,
        load: L,
    ) -> Result<(), AlarmError>
    where
        L: FnOnce(Arc<CallDepths>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<ObjectSource, String>> + Send + 'static,
    {
        let (done, outcome) = oneshot::channel();
        let event = ObjectEvent::Alarm(AlarmInit { retry_count, done });

        let _entered = admit(namespace, id, 0, event, load).map_err(AlarmError::Refused)?;

        match outcome.await {
            Ok(outcome) => outcome.map_err(AlarmError::Failed),
            Err(_) => Err(AlarmError::Failed("the Durable Object stopped".to_string())),
        }
    }

    /// Puts `event` in the mailbox of the object, and starts the object if
    /// it is not running.
    fn admit<L, Fut>(
        namespace: &super::NamespaceConfig,
        id: String,
        depth: u32,
        event: ObjectEvent,
        load: L,
    ) -> Result<super::DepthGuard, String>
    where
        L: FnOnce(Arc<CallDepths>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<ObjectSource, String>> + Send + 'static,
    {
        let key = ObjectKey {
            namespace: namespace.id.clone(),
            id,
        };
        let class_name = namespace.class_name.clone();
        let mut registry = REGISTRY.lock().unwrap();

        if registry.stopped.contains(&key.namespace) {
            return Err("the Durable Object namespace was deleted".to_string());
        }

        // A task that is gone left a closed mailbox, and the send hands the
        // event back
        let event = match registry.objects.get(&key) {
            Some(entry) => match entry.mailbox.send(event) {
                Ok(()) => return Ok(entry.depths.enter(depth)),
                Err(mpsc::error::SendError(event)) => event,
            },
            None => event,
        };

        let account = namespace.user_id.clone();

        if registry.accounts.get(&account).copied().unwrap_or(0) >= MAX_RESIDENT_PER_ACCOUNT {
            return Err("too many Durable Objects run for this account".to_string());
        }

        let (mailbox, rx) = mpsc::unbounded_channel();
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        let depths = Arc::new(CallDepths::default());
        let entered = depths.enter(depth);
        let (done_tx, done) = oneshot::channel();
        let previous = registry.retiring.remove(&key).map(|(_, done)| done);
        mailbox.send(event).expect("the receiver is here");
        registry.objects.insert(
            key.clone(),
            Entry {
                mailbox,
                generation,
                depths: Arc::clone(&depths),
                worker_id: namespace.worker_id.clone(),
                version: None,
                done,
            },
        );
        *registry.running.entry(key.namespace.clone()).or_default() += 1;
        *registry.accounts.entry(account.clone()).or_default() += 1;
        let running = Running {
            key: key.clone(),
            generation,
            account,
            _done: done_tx,
        };

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);

        WORKER_POOL.spawn_on(hasher.finish(), move || async move {
            // Old and new code never run for the same object at once
            if let Some(previous) = previous {
                previous.await.ok();
            }

            serve(key, generation, class_name, rx, move || load(depths)).await;
            drop(running);
        });

        Ok(entered)
    }

    /// Serves one object until it is idle with an empty mailbox.
    async fn serve<L, Fut>(
        key: ObjectKey,
        generation: u64,
        class_name: String,
        mut mailbox: mpsc::UnboundedReceiver<ObjectEvent>,
        load: L,
    ) where
        L: FnOnce() -> Fut,
        Fut: Future<Output = Result<ObjectSource, String>>,
    {
        let source = match load().await {
            Ok(source) => source,
            // The requests in the mailbox fail with it, and the next one
            // finds it closed and loads again
            Err(error) => {
                tracing::warn!("durable object {}: not loaded: {error}", key.id);
                return;
            }
        };

        if let Some(entry) = REGISTRY.lock().unwrap().objects.get_mut(&key)
            && entry.generation == generation
        {
            entry.version = Some(source.version);
        }

        let spec = ObjectSpec {
            script: source.script,
            class_name,
            id: key.id.clone(),
            limits: source.limits,
            ops: source.ops,
            idle: source.idle,
            drain: source.drain,
        };

        loop {
            let first = match mailbox.try_recv() {
                Ok(first) => first,
                Err(_) => {
                    if leave(&key, generation, &mut mailbox) {
                        return;
                    }

                    continue;
                }
            };

            // The logs of the worker are where its developer looks
            let log = |what: &str, reason: TerminationReason| {
                let message = format!(
                    "Durable Object {} {}: {what}: {reason}",
                    spec.class_name, key.id
                );
                spec.ops.handle_log(LogLevel::Error, message);
            };

            let object = match DurableObject::new(&spec) {
                Ok(object) => object,
                Err(reason) => {
                    log("not built", reason);
                    continue;
                }
            };

            if let Err(reason) = object
                .serve(first, &mut mailbox, spec.idle, spec.drain)
                .await
            {
                log("stopped", reason);
            }
        }
    }

    /// Removes the entry of this task, unless a request came in meanwhile.
    /// Requests are sent under the same lock, so none is lost. Answers
    /// whether the task can end.
    fn leave(
        key: &ObjectKey,
        generation: u64,
        mailbox: &mut mpsc::UnboundedReceiver<ObjectEvent>,
    ) -> bool {
        let mut registry = REGISTRY.lock().unwrap();

        if !mailbox.is_empty() {
            return false;
        }

        if registry
            .objects
            .get(key)
            .is_some_and(|entry| entry.generation == generation)
        {
            registry.objects.remove(key);
        }

        mailbox.close();
        true
    }

    /// What the object `object_id` of `namespace` is built from: the worker
    /// that exports the class, its bindings and its logs, as for a request
    /// to it.
    pub async fn load(
        namespace: super::NamespaceConfig,
        object_id: String,
        depths: Arc<CallDepths>,
        db_pool: Option<crate::ops::DbPool>,
        log_tx: Option<crate::ops::LogTx>,
    ) -> Result<ObjectSource, String> {
        use crate::store::{CodeType, WorkerIdentifier};

        let db_pool = db_pool.ok_or("Durable Objects need the database")?;
        let mut conn = db_pool.acquire().await.map_err(|e| e.to_string())?;
        let worker = WorkerIdentifier::Id(namespace.worker_id.clone());
        let data = crate::store::get_worker_with_bindings(&mut conn, worker)
            .await
            .ok_or("the worker of the Durable Object class is gone")?;
        drop(conn);

        // The binding checks the account of the namespace; the worker must be
        // of that account too, or a namespace could run the code of another
        // account with its bindings
        if data.user_id != namespace.user_id {
            return Err("the worker of the Durable Object class is gone".to_string());
        }

        // From here the worker is of the account, and its logs are where its
        // developer looks
        let failed = |error: String| {
            if let Some(log_tx) = &log_tx {
                log_tx.for_worker(data.id.clone()).send(LogEvent {
                    level: LogLevel::Error,
                    message: format!(
                        "Durable Object {} {object_id}: not loaded: {error}",
                        namespace.class_name
                    ),
                });
            }

            error
        };

        if data.code_type == CodeType::Wasm {
            return Err(failed(
                "a WebAssembly worker cannot export a Durable Object class".to_string(),
            ));
        }

        let script = crate::worker::prepare_script(&data).map_err(|e| failed(format!("{e:?}")))?;

        let mut ops = crate::ops::RunnerOperations::new()
            .with_worker_id(data.id.clone())
            .with_user_id(data.user_id.clone())
            .with_bindings(data.bindings)
            .with_db_pool(db_pool)
            .for_object(namespace.id, object_id, depths);

        if let Some(log_tx) = log_tx {
            ops = ops.with_log_tx(log_tx.for_worker(data.id));
        }

        Ok(ObjectSource {
            script,
            version: data.version,
            ops: std::sync::Arc::new(ops),
            limits: crate::task_executor::TaskExecutionConfig::default_limits(),
            idle: IDLE,
            drain: DRAIN,
        })
    }

    #[cfg(test)]
    pub(super) fn is_running(namespace: &str, id: &str) -> bool {
        let key = ObjectKey {
            namespace: namespace.to_string(),
            id: id.to_string(),
        };

        REGISTRY.lock().unwrap().objects.contains_key(&key)
    }
}

#[cfg(all(test, feature = "v8"))]
mod tests;
