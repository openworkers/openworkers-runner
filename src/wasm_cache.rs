//! Precompiled component cache for the wasm backend.
//!
//! The wasm counterpart of the V8 code cache: compiling a component with
//! Cranelift dominates a cold start, so the first start of a worker version
//! stores the machine code in `snapshot_cache` and later ones load that.

use crate::runtime::Worker;
use openworkers_core::OperationsHandle;
use openworkers_core::RuntimeLimits;
use openworkers_core::Script;
use openworkers_core::TerminationReason;
use openworkers_core::WorkerCode;
use openworkers_runtime_wasm::PrecompiledComponent;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static HITS: AtomicU64 = AtomicU64::new(0);

/// Cold starts that loaded a cached component instead of compiling one
pub fn hits() -> u64 {
    HITS.load(Ordering::Relaxed)
}

/// Build a worker, reusing the component compiled for an earlier cold start of
/// the same version.
///
/// On a miss the component is compiled as usual and precompiled again in the
/// background: that spends one core for the length of a compile, but keeps it
/// off the request that paid for the cold start already.
pub async fn create_worker(
    script: Script,
    limits: RuntimeLimits,
    ops: OperationsHandle,
    worker_id: &str,
    version: i32,
) -> Result<Worker, TerminationReason> {
    let engine_key = openworkers_runtime_wasm::compatibility_key(Some(limits.clone()))?;

    if let Some(artifact) = crate::snapshot_cache::get_wasm(worker_id, version, &engine_key) {
        tracing::debug!(
            "component cache HIT: worker={}, version={}, size={}",
            crate::utils::short_id(worker_id),
            version,
            artifact.len()
        );

        HITS.fetch_add(1, Ordering::Relaxed);

        // SAFETY: the artifact was put there by `precompile` below, in this
        // very process; nothing else writes to the cache.
        let component = unsafe { PrecompiledComponent::from_trusted_bytes(artifact) };

        return Worker::new_precompiled(component, script, Some(limits), Some(ops)).await;
    }

    tracing::debug!(
        "component cache MISS: worker={}, version={}",
        crate::utils::short_id(worker_id),
        version
    );

    // A worker whose code is not wasm fails in the runtime, with its message
    let wasm_bytes = match &script.code {
        WorkerCode::WebAssembly(bytes) => Some(bytes.clone()),
        _ => None,
    };

    let worker = Worker::new_with_ops(script, Some(limits.clone()), ops).await?;

    if let Some(wasm_bytes) = wasm_bytes {
        let worker_id = worker_id.to_string();

        tokio::task::spawn_blocking(move || {
            match openworkers_runtime_wasm::precompile(&wasm_bytes, Some(limits)) {
                Ok(artifact) => {
                    tracing::debug!(
                        "Precompiled component: worker={}, version={}, size={}",
                        crate::utils::short_id(&worker_id),
                        version,
                        artifact.len()
                    );

                    crate::snapshot_cache::put_wasm(&worker_id, version, &engine_key, &artifact);
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to precompile component for worker={}: {}",
                        crate::utils::short_id(&worker_id),
                        e
                    );
                }
            }
        });
    }

    Ok(worker)
}
