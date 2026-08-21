//! Worker construction on top of the selected backend.
//!
//! Backends are mutually exclusive, so `Worker` is a plain alias and every call
//! site is free of runtime dispatch.

use crate::store::{CodeType, WorkerWithBindings, bindings_to_infos};

use openworkers_core::{
    BindingInfo, OperationsHandle, RuntimeLimits, Script, TerminationReason, WorkerCode,
};

pub use crate::runtime::Worker;

pub async fn create_worker(
    script: Script,
    limits: RuntimeLimits,
    ops: OperationsHandle,
) -> Result<Worker, TerminationReason> {
    Worker::new_with_ops(script, Some(limits), ops).await
}

/// A worker ready to be built.
pub struct PreparedWorker {
    pub script: Script,
    /// The component the wasm backend compiled for an earlier cold start of
    /// this version. `script` then carries no guest bytes, because the
    /// precompiled constructor does not read them.
    #[cfg(feature = "wasm")]
    pub precompiled: Option<Vec<u8>>,
}

/// Prepare a worker, taking whatever the backend already compiled for this
/// version out of its cache.
///
/// V8 looks its bytecode up in `parse_code`, inside the script; wasm hands
/// back machine code alongside it, because only the constructor can take that.
pub fn prepare_worker(
    data: &WorkerWithBindings,
    limits: &RuntimeLimits,
) -> Result<PreparedWorker, TerminationReason> {
    #[cfg(feature = "wasm")]
    return crate::wasm_cache::prepare(data, limits);

    #[cfg(not(feature = "wasm"))]
    {
        let _ = limits;

        Ok(PreparedWorker {
            script: prepare_script(data)?,
        })
    }
}

/// Build a worker from what `prepare_worker` found.
pub async fn create_cached_worker(
    prepared: PreparedWorker,
    limits: RuntimeLimits,
    ops: OperationsHandle,
    worker_id: &str,
    version: i32,
) -> Result<Worker, TerminationReason> {
    #[cfg(feature = "wasm")]
    return crate::wasm_cache::create_worker(prepared, limits, ops, worker_id, version).await;

    #[cfg(not(feature = "wasm"))]
    {
        let _ = (worker_id, version);

        create_worker(prepared.script, limits, ops).await
    }
}

/// Parse worker code based on code type.
///
/// For JS/TS workers on v8: checks the code cache (fast path) and returns cached
/// bytecode. On cache miss, transpiles the source and returns JS - a code cache
/// entry is created in the background after first successful execution.
fn parse_code(data: &WorkerWithBindings) -> Result<WorkerCode, TerminationReason> {
    match data.code_type {
        CodeType::Javascript | CodeType::Typescript => {
            #[cfg(feature = "_js")]
            {
                #[cfg(feature = "v8")]
                if let Some(snapshot) = crate::snapshot_cache::get(&data.id, data.version) {
                    tracing::debug!(
                        "code cache HIT: worker={}, version={}, size={}",
                        crate::utils::short_id(&data.id),
                        data.version,
                        snapshot.len()
                    );
                    return Ok(WorkerCode::snapshot(snapshot));
                }

                #[cfg(feature = "v8")]
                tracing::debug!(
                    "code cache MISS: worker={}, version={}",
                    crate::utils::short_id(&data.id),
                    data.version
                );

                let language = match data.code_type {
                    CodeType::Javascript => openworkers_transform::CodeLanguage::JavaScript,
                    CodeType::Typescript => openworkers_transform::CodeLanguage::TypeScript,
                    _ => unreachable!(),
                };

                let transpiled = openworkers_transform::parse_worker_code(&data.code, language)
                    .map_err(|e| {
                        TerminationReason::InitializationError(format!(
                            "Failed to parse worker code: {}",
                            e
                        ))
                    })?;

                Ok(WorkerCode::js(transpiled))
            }

            #[cfg(not(feature = "_js"))]
            Err(unsupported("JavaScript"))
        }
        CodeType::Wasm => {
            #[cfg(feature = "wasm")]
            {
                Ok(WorkerCode::wasm(data.code.clone()))
            }

            #[cfg(not(feature = "wasm"))]
            Err(unsupported("WebAssembly"))
        }
        CodeType::Snapshot => {
            #[cfg(feature = "v8")]
            {
                Ok(WorkerCode::snapshot(data.code.clone()))
            }

            #[cfg(not(feature = "v8"))]
            Err(unsupported("snapshot"))
        }
    }
}

fn unsupported(code_type: &str) -> TerminationReason {
    TerminationReason::InitializationError(format!(
        "the {} backend cannot run {} workers",
        crate::runtime::NAME,
        code_type
    ))
}

/// Refuse a worker declaring a binding the backend cannot serve, or the guest
/// reads `env.ASSETS` as undefined and serves a broken page instead of an error.
fn check_bindings(bindings: &[BindingInfo]) -> Result<(), TerminationReason> {
    let unsupported: Vec<&BindingInfo> = bindings
        .iter()
        .filter(|b| !crate::runtime::supports_binding(b.binding_type))
        .collect();

    if unsupported.is_empty() {
        return Ok(());
    }

    // One entry per type, or three KV bindings would name kv three times
    let mut types: Vec<&str> = Vec::new();

    for binding in &unsupported {
        let name = crate::runtime::binding_type_name(binding.binding_type);

        if !types.contains(&name) {
            types.push(name);
        }
    }

    let names: Vec<&str> = unsupported.iter().map(|b| b.name.as_str()).collect();

    Err(TerminationReason::InitializationError(format!(
        "the {} backend does not implement {} bindings, declared as {}",
        crate::runtime::NAME,
        types.join("/"),
        names.join(", ")
    )))
}

/// Prepare a Script from WorkerWithBindings
pub fn prepare_script(data: &WorkerWithBindings) -> Result<Script, TerminationReason> {
    script_with_code(data, parse_code(data)?)
}

/// A Script for a cold start that will load a precompiled component: it leaves
/// the guest bytes out, because `WasmWorker::new_precompiled` does not read
/// them and copying them per request is what the cache is there to avoid.
#[cfg(feature = "wasm")]
pub(crate) fn script_without_code(data: &WorkerWithBindings) -> Result<Script, TerminationReason> {
    script_with_code(data, WorkerCode::wasm(Vec::new()))
}

fn script_with_code(
    data: &WorkerWithBindings,
    code: WorkerCode,
) -> Result<Script, TerminationReason> {
    let binding_infos = bindings_to_infos(&data.bindings);

    check_bindings(&binding_infos)?;

    if !crate::runtime::SUPPORTS_ENV && !data.env.is_empty() {
        tracing::warn!(
            "worker={} declares {} env variables, the {} backend exposes none",
            crate::utils::short_id(&data.id),
            data.env.len(),
            crate::runtime::NAME
        );
    }

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
