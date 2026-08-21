//! Prepared component cache for the wasm backend.
//!
//! The wasm counterpart of the V8 code cache, in two layers. `code_cache`
//! holds the machine code an earlier cold start serialized, so a version is
//! compiled once per process at most. On top of it, this module keeps the
//! live `PreparedComponent` - the component loaded into the engine with its
//! pre-instantiations - so a request assembles a worker for a few reference
//! bumps instead of re-linking the artifact every time.

use crate::runtime::WasmWorker;
use crate::store::WorkerWithBindings;
use crate::worker::PreparedWorker;
use crate::worker::Worker;
use lru::LruCache;
use openworkers_core::OperationsHandle;
use openworkers_core::RuntimeLimits;
use openworkers_core::TerminationReason;
use openworkers_runtime_wasm::PrecompiledComponent;
use openworkers_runtime_wasm::PreparedComponent;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Live components kept ready, one per worker version and engine key. Each
/// holds compiled code about the size of its `code_cache` artifact, so the
/// cap mirrors the byte budget there rather than adding a second knob.
const PREPARED_CAP: usize = 256;

type PreparedKey = (String, i32, String);

static PREPARED: OnceLock<Mutex<LruCache<PreparedKey, Arc<PreparedComponent>>>> = OnceLock::new();

static HITS: AtomicU64 = AtomicU64::new(0);

/// Cold starts that assembled a cached component instead of compiling one
pub fn hits() -> u64 {
    HITS.load(Ordering::Relaxed)
}

fn prepared_cache() -> &'static Mutex<LruCache<PreparedKey, Arc<PreparedComponent>>> {
    PREPARED.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(PREPARED_CAP).expect("cap is not zero"),
        ))
    })
}

fn get_prepared(key: &PreparedKey) -> Option<Arc<PreparedComponent>> {
    prepared_cache()
        .lock()
        .expect("prepared cache")
        .get(key)
        .cloned()
}

fn put_prepared(key: PreparedKey, component: Arc<PreparedComponent>) {
    prepared_cache()
        .lock()
        .expect("prepared cache")
        .put(key, component);
}

/// Look the worker's component up before its script is built, so a hit never
/// copies the guest bytes the assembly would ignore.
pub fn prepare(
    data: &WorkerWithBindings,
    limits: &RuntimeLimits,
) -> Result<PreparedWorker, TerminationReason> {
    let engine_key = openworkers_runtime_wasm::compatibility_key(Some(limits.clone()))?;
    let key = (data.id.clone(), data.version, engine_key);

    if let Some(component) = get_prepared(&key) {
        tracing::debug!(
            "component cache HIT: worker={}, version={}",
            crate::utils::short_id(&data.id),
            data.version
        );

        return Ok(PreparedWorker {
            script: crate::worker::script_without_code(data)?,
            prepared: Some(component),
        });
    }

    if let Some(artifact) = crate::code_cache::get_wasm(&data.id, data.version, &key.2) {
        tracing::debug!(
            "component artifact HIT: worker={}, version={}, size={}",
            crate::utils::short_id(&data.id),
            data.version,
            artifact.len()
        );

        // SAFETY: the artifact was serialized by this process from a component
        // it compiled; nothing else writes to the cache.
        let artifact = unsafe { PrecompiledComponent::from_trusted_bytes(artifact) };

        let component = Arc::new(WasmWorker::prepare_precompiled(
            &artifact,
            Some(limits.clone()),
        )?);

        put_prepared(key, component.clone());

        return Ok(PreparedWorker {
            script: crate::worker::script_without_code(data)?,
            prepared: Some(component),
        });
    }

    tracing::debug!(
        "component cache MISS: worker={}, version={}",
        crate::utils::short_id(&data.id),
        data.version
    );

    Ok(PreparedWorker {
        script: crate::worker::prepare_script(data)?,
        prepared: None,
    })
}

/// Build the worker `prepare` found.
///
/// A miss compiles once into a `PreparedComponent`, serializes it for
/// `code_cache` (about 250 us, against the 13 ms a second compile would
/// cost), and keeps the live component for the next request.
pub async fn create_worker(
    prepared: PreparedWorker,
    limits: RuntimeLimits,
    ops: OperationsHandle,
    worker_id: &str,
    version: i32,
) -> Result<Worker, TerminationReason> {
    let PreparedWorker { script, prepared } = prepared;

    if let Some(component) = prepared {
        HITS.fetch_add(1, Ordering::Relaxed);

        let worker = WasmWorker::from_prepared(&component, script, Some(limits), Some(ops)).await?;

        return Ok(Worker::Wasm(worker));
    }

    let engine_key = openworkers_runtime_wasm::compatibility_key(Some(limits.clone()))?;

    let wasm = match &script.code {
        openworkers_core::WorkerCode::WebAssembly(bytes) => bytes.clone(),
        other => {
            return Err(TerminationReason::InitializationError(format!(
                "wasm worker carries non-wasm code: {:?}",
                std::mem::discriminant(other)
            )));
        }
    };

    let component = Arc::new(WasmWorker::prepare(&wasm, Some(limits.clone()))?);

    match component.serialize() {
        Ok(artifact) => {
            tracing::debug!(
                "Cached component: worker={}, version={}, size={}",
                crate::utils::short_id(worker_id),
                version,
                artifact.len()
            );

            crate::code_cache::put_wasm(worker_id, version, &engine_key, &artifact);
        }
        Err(e) => {
            tracing::warn!(
                "Failed to serialize component for worker={}: {}",
                crate::utils::short_id(worker_id),
                e
            );
        }
    }

    put_prepared(
        (worker_id.to_string(), version, engine_key),
        component.clone(),
    );

    let worker = WasmWorker::from_prepared(&component, script, Some(limits), Some(ops)).await?;

    Ok(Worker::Wasm(worker))
}
