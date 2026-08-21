//! Worker construction on top of the backends the build carries.
//!
//! Which backend runs a worker follows from its code, not from the build: a
//! runner carrying both a JavaScript engine and the wasm one serves both kinds,
//! so `Worker` names whichever backend built it.

use crate::runtime::Backend;
#[cfg(feature = "_js")]
use crate::runtime::JsWorker;
#[cfg(feature = "wasm")]
use crate::runtime::WasmWorker;
use crate::store::{CodeType, WorkerWithBindings, bindings_to_infos};

use openworkers_core::{
    BindingInfo, Event, OperationsHandle, RuntimeLimits, Script, TerminationReason, WorkerCode,
};

/// A live worker, on the backend its code selected.
pub enum Worker {
    #[cfg(feature = "_js")]
    Js(JsWorker),
    #[cfg(feature = "wasm")]
    Wasm(WasmWorker),
}

impl Worker {
    pub async fn exec(&mut self, task: Event) -> Result<(), TerminationReason> {
        match self {
            #[cfg(feature = "_js")]
            Self::Js(worker) => worker.exec(task).await,
            #[cfg(feature = "wasm")]
            Self::Wasm(worker) => worker.exec(task).await,
        }
    }
}

pub async fn create_worker(
    script: Script,
    limits: RuntimeLimits,
    ops: OperationsHandle,
) -> Result<Worker, TerminationReason> {
    #[cfg(feature = "wasm")]
    if script.code.is_wasm() {
        let worker = WasmWorker::new_with_ops(script, Some(limits), ops).await?;

        return Ok(Worker::Wasm(worker));
    }

    #[cfg(feature = "_js")]
    {
        let worker = JsWorker::new_with_ops(script, Some(limits), ops).await?;

        Ok(Worker::Js(worker))
    }

    // A build without a JavaScript engine has the wasm one, which took the
    // script above unless its code is something wasmtime cannot run
    #[cfg(not(feature = "_js"))]
    Err(TerminationReason::InitializationError(
        "this build cannot run JavaScript workers".to_string(),
    ))
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

/// Prepare a worker, taking whatever its backend already compiled for this
/// version out of its cache.
///
/// V8 looks its bytecode up in `parse_code`, inside the script; wasm hands
/// back machine code alongside it, because only the constructor can take that.
pub fn prepare_worker(
    data: &WorkerWithBindings,
    limits: &RuntimeLimits,
) -> Result<PreparedWorker, TerminationReason> {
    #[cfg(feature = "wasm")]
    if data.code_type == CodeType::Wasm {
        return crate::wasm_cache::prepare(data, limits);
    }

    #[cfg(not(feature = "wasm"))]
    let _ = limits;

    Ok(PreparedWorker {
        script: prepare_script(data)?,
        #[cfg(feature = "wasm")]
        precompiled: None,
    })
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
    if prepared.script.code.is_wasm() {
        return crate::wasm_cache::create_worker(prepared, limits, ops, worker_id, version).await;
    }

    #[cfg(not(feature = "wasm"))]
    let _ = (worker_id, version);

    create_worker(prepared.script, limits, ops).await
}

/// Parse worker code based on code type, naming the backend that will run it.
///
/// For JS/TS workers on v8: checks the code cache (fast path) and returns cached
/// bytecode. On cache miss, transpiles the source and returns JS - a code cache
/// entry is created in the background after first successful execution.
fn parse_code(data: &WorkerWithBindings) -> Result<(Backend, WorkerCode), TerminationReason> {
    match data.code_type {
        CodeType::Javascript | CodeType::Typescript => {
            #[cfg(feature = "_js")]
            {
                #[cfg(feature = "v8")]
                if let Some(snapshot) = crate::code_cache::get(&data.id, data.version) {
                    tracing::debug!(
                        "code cache HIT: worker={}, version={}, size={}",
                        crate::utils::short_id(&data.id),
                        data.version,
                        snapshot.len()
                    );
                    return Ok((Backend::Js, WorkerCode::snapshot(snapshot)));
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

                Ok((Backend::Js, WorkerCode::js(transpiled)))
            }

            #[cfg(not(feature = "_js"))]
            Err(TerminationReason::InitializationError(
                "this build cannot run JavaScript workers".to_string(),
            ))
        }
        CodeType::Wasm => {
            #[cfg(feature = "wasm")]
            {
                Ok((Backend::Wasm, WorkerCode::wasm(data.code.clone())))
            }

            #[cfg(not(feature = "wasm"))]
            Err(TerminationReason::InitializationError(
                "this build cannot run WebAssembly workers".to_string(),
            ))
        }
        CodeType::Snapshot => {
            #[cfg(feature = "v8")]
            {
                Ok((Backend::Js, WorkerCode::snapshot(data.code.clone())))
            }

            #[cfg(not(feature = "v8"))]
            Err(TerminationReason::InitializationError(
                "this build cannot run snapshot workers".to_string(),
            ))
        }
    }
}

/// Refuse a worker declaring a binding its backend cannot serve, or the guest
/// reads `env.ASSETS` as undefined and serves a broken page instead of an error.
fn check_bindings(backend: Backend, bindings: &[BindingInfo]) -> Result<(), TerminationReason> {
    let unsupported: Vec<&BindingInfo> = bindings
        .iter()
        .filter(|b| !backend.supports_binding(b.binding_type))
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
        backend.name(),
        types.join("/"),
        names.join(", ")
    )))
}

/// Prepare a Script from WorkerWithBindings
pub fn prepare_script(data: &WorkerWithBindings) -> Result<Script, TerminationReason> {
    let (backend, code) = parse_code(data)?;

    script_with_code(data, backend, code)
}

/// A Script for a cold start that will load a precompiled component: it leaves
/// the guest bytes out, because `WasmWorker::new_precompiled` does not read
/// them and copying them per request is what the cache is there to avoid.
#[cfg(feature = "wasm")]
pub(crate) fn script_without_code(data: &WorkerWithBindings) -> Result<Script, TerminationReason> {
    script_with_code(data, Backend::Wasm, WorkerCode::wasm(Vec::new()))
}

fn script_with_code(
    data: &WorkerWithBindings,
    backend: Backend,
    code: WorkerCode,
) -> Result<Script, TerminationReason> {
    let binding_infos = bindings_to_infos(&data.bindings);

    check_bindings(backend, &binding_infos)?;

    if !backend.supports_env() && !data.env.is_empty() {
        tracing::warn!(
            "worker={} declares {} env variables, the {} backend exposes none",
            crate::utils::short_id(&data.id),
            data.env.len(),
            backend.name()
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
