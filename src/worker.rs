//! Unified worker abstraction for JavaScript and WASM runtimes
//!
//! This module provides a single `Worker` type that adapts based on features:
//! - v8 only: direct type alias to JsWorker (zero overhead)
//! - wasm only: direct type alias to WasmWorker (zero overhead)
//! - both: enum with runtime dispatch based on CodeType

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use once_cell::sync::Lazy;

use crate::ops::RunnerOperations;
use crate::store::{CodeType, WorkerWithBindings, bindings_to_infos};

// =============================================================================
// Transpiled code cache
// =============================================================================

/// Cache key: (worker_id, version)
type CacheKey = (String, i32);

/// LRU cache for transpiled JavaScript code
/// Capacity: 1000 workers (transpiled code is relatively small)
static TRANSPILE_CACHE: Lazy<Mutex<LruCache<CacheKey, String>>> =
    Lazy::new(|| Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())));

#[cfg(all(feature = "v8", feature = "wasm"))]
use openworkers_core::Event;

use openworkers_core::{RuntimeLimits, Script, TerminationReason, WorkerCode};

// =============================================================================
// Worker type definition based on feature flags
// =============================================================================

/// Single runtime: V8 only - direct re-export (zero overhead)
#[cfg(all(feature = "v8", not(feature = "wasm")))]
pub use crate::runtime::Worker;

/// Single runtime: WASM only - direct re-export (zero overhead)
#[cfg(all(feature = "wasm", not(feature = "v8")))]
pub use openworkers_runtime_wasm::WasmWorker as Worker;

/// Dual runtime: V8 + WASM - enum with dispatch
#[cfg(all(feature = "v8", feature = "wasm"))]
pub enum Worker {
    Javascript(crate::runtime::Worker),
    Wasm(openworkers_runtime_wasm::WasmWorker),
}

#[cfg(all(feature = "v8", feature = "wasm"))]
impl Worker {
    /// Create a worker, selecting runtime based on code_type
    pub async fn new_with_ops(
        script: Script,
        limits: Option<RuntimeLimits>,
        ops: Arc<RunnerOperations>,
        code_type: &CodeType,
    ) -> Result<Self, TerminationReason> {
        match code_type {
            CodeType::Wasm => {
                openworkers_runtime_wasm::WasmWorker::new_with_ops(script, limits, ops)
                    .await
                    .map(Worker::Wasm)
            }
            _ => crate::runtime::Worker::new_with_ops(script, limits, ops)
                .await
                .map(Worker::Javascript),
        }
    }

    /// Execute a task
    pub async fn exec(&mut self, task: Event) -> Result<(), TerminationReason> {
        match self {
            Worker::Javascript(w) => w.exec(task).await,
            Worker::Wasm(w) => w.exec(task).await,
        }
    }
}

// =============================================================================
// Unified worker creation (handles signature differences)
// =============================================================================

/// Create a worker with the appropriate runtime based on code_type
pub async fn create_worker(
    script: Script,
    limits: RuntimeLimits,
    ops: Arc<RunnerOperations>,
    code_type: &CodeType,
) -> Result<Worker, TerminationReason> {
    #[cfg(all(feature = "v8", feature = "wasm"))]
    {
        Worker::new_with_ops(script, Some(limits), ops, code_type).await
    }

    #[cfg(all(feature = "v8", not(feature = "wasm")))]
    {
        let _ = code_type; // V8-only: code_type already validated in prepare_script
        Worker::new_with_ops(script, Some(limits), ops).await
    }

    #[cfg(all(feature = "wasm", not(feature = "v8")))]
    {
        let _ = code_type; // WASM-only: code_type already validated in prepare_script
        Worker::new_with_ops(script, Some(limits), ops).await
    }
}

// =============================================================================
// Script preparation (shared across all configurations)
// =============================================================================

/// Parse worker code based on code type (with caching for JS/TS)
fn parse_code(data: &WorkerWithBindings) -> Result<WorkerCode, TerminationReason> {
    match data.code_type {
        CodeType::Javascript | CodeType::Typescript => {
            #[cfg(feature = "v8")]
            {
                let cache_key = (data.id.clone(), data.version);

                // Try to get from cache first
                {
                    let mut cache = TRANSPILE_CACHE.lock().unwrap();

                    if let Some(cached_code) = cache.get(&cache_key) {
                        tracing::debug!(
                            "transpile cache HIT: worker={}, version={}",
                            crate::utils::short_id(&data.id),
                            data.version
                        );
                        return Ok(WorkerCode::js(cached_code.clone()));
                    }
                }

                tracing::debug!(
                    "transpile cache MISS: worker={}, version={}",
                    crate::utils::short_id(&data.id),
                    data.version
                );

                // Transpile and cache
                let transpiled = crate::transform::parse_worker_code(&data.code, &data.code_type)
                    .map_err(|e| {
                    TerminationReason::InitializationError(format!(
                        "Failed to parse worker code: {}",
                        e
                    ))
                })?;

                {
                    let mut cache = TRANSPILE_CACHE.lock().unwrap();
                    cache.put(cache_key, transpiled.clone());
                }

                Ok(WorkerCode::js(transpiled))
            }

            #[cfg(not(feature = "v8"))]
            Err(TerminationReason::InitializationError(
                "JavaScript runtime not available".to_string(),
            ))
        }
        CodeType::Wasm => {
            #[cfg(feature = "wasm")]
            {
                Ok(WorkerCode::wasm(data.code.clone()))
            }

            #[cfg(not(feature = "wasm"))]
            Err(TerminationReason::InitializationError(
                "WASM runtime not available".to_string(),
            ))
        }
        CodeType::Snapshot => {
            #[cfg(feature = "v8")]
            {
                Ok(WorkerCode::snapshot(data.code.clone()))
            }

            #[cfg(not(feature = "v8"))]
            Err(TerminationReason::InitializationError(
                "Snapshot runtime not available".to_string(),
            ))
        }
    }
}

/// Prepare a Script from WorkerWithBindings
pub fn prepare_script(data: &WorkerWithBindings) -> Result<Script, TerminationReason> {
    let code = parse_code(data)?;
    let binding_infos = bindings_to_infos(&data.bindings);

    Ok(Script {
        code,
        env: if data.env.is_empty() {
            None
        } else {
            Some(data.env.clone())
        },
        bindings: binding_infos,
    })
}
