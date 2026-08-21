//! The runtime backend selected at build time.
//!
//! Exactly one backend feature is enabled per build. This module re-exports the
//! backend's worker type and states what it supports, so the rest of the crate
//! never repeats the cfg chain.
//!
//!   cargo build --features v8       (recommended)
//!   cargo build --features jsc
//!   cargo build --features quickjs
//!   cargo build --features boa
//!   cargo build --features wasm

#[cfg(not(any(
    feature = "v8",
    feature = "jsc",
    feature = "quickjs",
    feature = "boa",
    feature = "wasm"
)))]
compile_error!("no runtime backend selected: build with --features v8|jsc|quickjs|boa|wasm");

#[cfg(any(
    all(
        feature = "v8",
        any(
            feature = "jsc",
            feature = "quickjs",
            feature = "boa",
            feature = "wasm"
        )
    ),
    all(
        feature = "jsc",
        any(feature = "quickjs", feature = "boa", feature = "wasm")
    ),
    all(feature = "quickjs", any(feature = "boa", feature = "wasm")),
    all(feature = "boa", feature = "wasm"),
))]
compile_error!("runtime backends are mutually exclusive: select exactly one");

use openworkers_core::BindingType;

#[cfg(feature = "boa")]
pub use openworkers_runtime_boa::Worker as JsWorker;
#[cfg(feature = "jsc")]
pub use openworkers_runtime_jsc::Worker as JsWorker;
#[cfg(feature = "quickjs")]
pub use openworkers_runtime_quickjs::Worker as JsWorker;
#[cfg(feature = "v8")]
pub use openworkers_runtime_v8::Worker as JsWorker;
#[cfg(feature = "wasm")]
pub use openworkers_runtime_wasm::WasmWorker;

#[cfg(feature = "v8")]
pub use openworkers_runtime_v8::snapshot;

/// Selected backend, for logs and error messages.
#[cfg(feature = "v8")]
pub const NAME: &str = "v8";
#[cfg(feature = "jsc")]
pub const NAME: &str = "jsc";
#[cfg(feature = "quickjs")]
pub const NAME: &str = "quickjs";
#[cfg(feature = "boa")]
pub const NAME: &str = "boa";
#[cfg(feature = "wasm")]
pub const NAME: &str = "wasm";

/// Binding types the backend serves: v8 builds an `env` object per binding,
/// wasm links the `openworkers:bindings` WIT package, which has no assets or
/// worker interface, and the rest ignore `Script::bindings`.
#[cfg(feature = "v8")]
pub const SUPPORTED_BINDINGS: &[BindingType] = &[
    BindingType::Assets,
    BindingType::Storage,
    BindingType::Kv,
    BindingType::Database,
    BindingType::Worker,
];
#[cfg(feature = "wasm")]
pub const SUPPORTED_BINDINGS: &[BindingType] =
    &[BindingType::Kv, BindingType::Database, BindingType::Storage];
#[cfg(any(feature = "jsc", feature = "quickjs", feature = "boa"))]
pub const SUPPORTED_BINDINGS: &[BindingType] = &[];

pub fn supports_binding(binding_type: BindingType) -> bool {
    SUPPORTED_BINDINGS.contains(&binding_type)
}

/// Binding type as it is named in logs and errors
pub fn binding_type_name(binding_type: BindingType) -> &'static str {
    match binding_type {
        BindingType::Assets => "assets",
        BindingType::Storage => "storage",
        BindingType::Kv => "kv",
        BindingType::Database => "database",
        BindingType::Worker => "worker",
        BindingType::Images => "images",
    }
}

/// Whether the backend reads `Script::env` and exposes the variables to the guest.
pub const SUPPORTS_ENV: bool = cfg!(any(feature = "v8", feature = "jsc", feature = "wasm"));

/// Whether the guest is JavaScript; the wasm backend runs components instead.
pub const RUNS_JAVASCRIPT: bool = cfg!(feature = "_js");

/// One line naming the backend and what a worker can rely on.
pub fn capabilities() -> String {
    let bindings: Vec<&str> = SUPPORTED_BINDINGS
        .iter()
        .copied()
        .map(binding_type_name)
        .collect();

    format!(
        "runtime backend: {NAME} (guest={}, env={}, bindings={})",
        if RUNS_JAVASCRIPT {
            "javascript"
        } else {
            "wasm component"
        },
        SUPPORTS_ENV,
        if bindings.is_empty() {
            "none".to_string()
        } else {
            bindings.join("/")
        },
    )
}
