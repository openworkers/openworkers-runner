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
    let code = parse_code(data)?;
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
