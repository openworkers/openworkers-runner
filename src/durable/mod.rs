//! Durable Objects: the namespaces of the bindings, their ids, and the
//! registry of the objects this runner serves.
//!
//! Each object runs on one thread of the worker pool, which a hash of its key
//! selects, with a mailbox in the registry. An object that is idle with an
//! empty mailbox removes its entry, so a later request starts it again.

pub mod id;

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

#[cfg(feature = "v8")]
pub use registry::{IDLE, ObjectSource, load, send, stop_namespace};

/// Without a JavaScript engine that serves objects, none runs.
#[cfg(not(feature = "v8"))]
pub fn stop_namespace(_namespace_id: &str) -> bool {
    true
}

#[cfg(feature = "v8")]
mod registry {
    use std::collections::{HashMap, HashSet};
    use std::future::Future;
    use std::hash::{Hash, Hasher};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use once_cell::sync::Lazy;
    use openworkers_core::{
        FetchInit, HttpRequest, HttpResponse, OperationsHandle, RuntimeLimits, Script,
    };
    use openworkers_runtime_v8::{DurableObject, ObjectSpec};
    use tokio::sync::{mpsc, oneshot};

    use crate::worker_pool::WORKER_POOL;

    /// An object with no event in flight for this long gives its isolate back.
    pub const IDLE: Duration = Duration::from_secs(60);

    /// What an object is built from: the worker that exports its class.
    pub struct ObjectSource {
        pub script: Script,
        pub ops: OperationsHandle,
        pub limits: RuntimeLimits,
        pub idle: Duration,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct ObjectKey {
        namespace: String,
        id: String,
    }

    struct Entry {
        mailbox: mpsc::UnboundedSender<FetchInit>,
        /// Tells the task that serves the entry from a later one
        generation: u64,
    }

    #[derive(Default)]
    struct Registry {
        objects: HashMap<ObjectKey, Entry>,
        /// Tasks that serve an object, per namespace
        running: HashMap<String, usize>,
        /// Deleted namespaces; their IDs are never reused
        stopped: HashSet<String>,
    }

    static REGISTRY: Lazy<Mutex<Registry>> = Lazy::new(Default::default);
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    /// Counts the task of an object until it ends, however it ends.
    struct Running(String);

    impl Drop for Running {
        fn drop(&mut self) {
            let mut registry = REGISTRY.lock().unwrap();

            if let Some(count) = registry.running.get_mut(&self.0) {
                *count -= 1;

                if *count == 0 {
                    registry.running.remove(&self.0);
                }
            }
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

    /// Sends `request` to the object `id` of `namespace`, and starts the
    /// object if it is not running. `load` gives what the object is built
    /// from; it runs once per start.
    pub async fn send<L, Fut>(
        namespace: &super::NamespaceConfig,
        id: String,
        request: HttpRequest,
        load: L,
    ) -> Result<HttpResponse, String>
    where
        L: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<ObjectSource, String>> + Send + 'static,
    {
        let key = ObjectKey {
            namespace: namespace.id.clone(),
            id,
        };
        let class_name = namespace.class_name.clone();
        let (res_tx, res_rx) = oneshot::channel();
        let init = FetchInit::new(request, res_tx);

        {
            let mut registry = REGISTRY.lock().unwrap();

            if registry.stopped.contains(&key.namespace) {
                return Err("the Durable Object namespace was deleted".to_string());
            }

            // A task that is gone left a closed mailbox, and the send hands
            // the request back
            let init = match registry.objects.get(&key) {
                Some(entry) => match entry.mailbox.send(init) {
                    Ok(()) => None,
                    Err(mpsc::error::SendError(init)) => Some(init),
                },
                None => Some(init),
            };

            if let Some(init) = init {
                let (mailbox, rx) = mpsc::unbounded_channel();
                let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
                mailbox.send(init).expect("the receiver is here");
                registry.objects.insert(
                    key.clone(),
                    Entry {
                        mailbox,
                        generation,
                    },
                );
                *registry.running.entry(key.namespace.clone()).or_default() += 1;
                let running = Running(key.namespace.clone());

                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                key.hash(&mut hasher);

                WORKER_POOL.spawn_on(hasher.finish(), move || async move {
                    serve(key, generation, class_name, rx, load).await;
                    drop(running);
                });
            }
        }

        res_rx
            .await
            .map_err(|_| "the Durable Object stopped".to_string())
    }

    /// Serves one object until it is idle with an empty mailbox.
    async fn serve<L, Fut>(
        key: ObjectKey,
        generation: u64,
        class_name: String,
        mut mailbox: mpsc::UnboundedReceiver<FetchInit>,
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

        let spec = ObjectSpec {
            script: source.script,
            class_name,
            id: key.id.clone(),
            limits: source.limits,
            ops: source.ops,
            idle: source.idle,
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

            let object = match DurableObject::new(&spec) {
                Ok(object) => object,
                Err(reason) => {
                    tracing::warn!("durable object {}: not built: {reason:?}", key.id);
                    continue;
                }
            };

            if let Err(reason) = object.serve(first, &mut mailbox, spec.idle).await {
                tracing::warn!("durable object {}: stopped: {reason:?}", key.id);
            }
        }
    }

    /// Removes the entry of this task, unless a request came in meanwhile.
    /// Requests are sent under the same lock, so none is lost. Answers
    /// whether the task can end.
    fn leave(
        key: &ObjectKey,
        generation: u64,
        mailbox: &mut mpsc::UnboundedReceiver<FetchInit>,
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

        if data.code_type == CodeType::Wasm {
            return Err("a WebAssembly worker cannot export a Durable Object class".to_string());
        }

        let script = crate::worker::prepare_script(&data).map_err(|e| format!("{e:?}"))?;

        let mut ops = crate::ops::RunnerOperations::new()
            .with_worker_id(data.id.clone())
            .with_user_id(data.user_id.clone())
            .with_bindings(data.bindings)
            .with_db_pool(db_pool)
            .with_object_storage(namespace.id, object_id);

        if let Some(log_tx) = log_tx {
            ops = ops.with_log_tx(log_tx.for_worker(data.id));
        }

        Ok(ObjectSource {
            script,
            ops: std::sync::Arc::new(ops),
            limits: crate::task_executor::TaskExecutionConfig::default_limits(),
            idle: IDLE,
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
