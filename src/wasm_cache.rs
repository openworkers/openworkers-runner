//! Precompiled component cache for the wasm backend.
//!
//! The wasm counterpart of the V8 code cache: compiling a component with
//! Cranelift dominates a cold start, so the first start of a worker version
//! stores the machine code in `code_cache` and later ones load that.

use crate::runtime::WasmWorker;
use crate::store::WorkerWithBindings;
use crate::worker::PreparedWorker;
use crate::worker::Worker;
use openworkers_core::OperationsHandle;
use openworkers_core::RuntimeLimits;
use openworkers_core::TerminationReason;
use openworkers_runtime_wasm::PrecompiledComponent;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static HITS: AtomicU64 = AtomicU64::new(0);

/// Cold starts that loaded a cached component instead of compiling one
pub fn hits() -> u64 {
    HITS.load(Ordering::Relaxed)
}

/// Look the worker's component up before its script is built, so a hit never
/// copies the guest bytes the precompiled constructor would ignore.
pub fn prepare(
    data: &WorkerWithBindings,
    limits: &RuntimeLimits,
) -> Result<PreparedWorker, TerminationReason> {
    let engine_key = openworkers_runtime_wasm::compatibility_key(Some(limits.clone()))?;

    let Some(artifact) = crate::code_cache::get_wasm(&data.id, data.version, &engine_key) else {
        tracing::debug!(
            "component cache MISS: worker={}, version={}",
            crate::utils::short_id(&data.id),
            data.version
        );

        return Ok(PreparedWorker {
            script: crate::worker::prepare_script(data)?,
            precompiled: None,
        });
    };

    tracing::debug!(
        "component cache HIT: worker={}, version={}, size={}",
        crate::utils::short_id(&data.id),
        data.version,
        artifact.len()
    );

    Ok(PreparedWorker {
        script: crate::worker::script_without_code(data)?,
        precompiled: Some(artifact),
    })
}

/// Build the worker `prepare` found.
///
/// A miss compiles as usual, then keeps the machine code the worker is already
/// holding: serializing it costs about 250 us, against the 13 ms compiling the
/// guest a second time would.
pub async fn create_worker(
    prepared: PreparedWorker,
    limits: RuntimeLimits,
    ops: OperationsHandle,
    worker_id: &str,
    version: i32,
) -> Result<Worker, TerminationReason> {
    let PreparedWorker {
        script,
        precompiled,
    } = prepared;

    if let Some(artifact) = precompiled {
        HITS.fetch_add(1, Ordering::Relaxed);

        // SAFETY: the artifact was serialized below from a component this
        // process compiled; nothing else writes to the cache.
        let component = unsafe { PrecompiledComponent::from_trusted_bytes(artifact) };

        let worker =
            WasmWorker::new_precompiled(component, script, Some(limits), Some(ops)).await?;

        return Ok(Worker::Wasm(worker));
    }

    let engine_key = openworkers_runtime_wasm::compatibility_key(Some(limits.clone()))?;

    let worker = WasmWorker::new_with_ops(script, Some(limits), ops).await?;

    match worker.serialize_component() {
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

    Ok(Worker::Wasm(worker))
}
