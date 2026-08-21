//! The runtime backends selected at build time.
//!
//! A build takes at most one JavaScript engine; the wasm backend is orthogonal
//! and adds to it. This module re-exports each backend's worker type and states
//! what it supports, so the rest of the crate never repeats the cfg chain.
//!
//!   cargo build --features v8,wasm  (recommended)
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
        any(feature = "jsc", feature = "quickjs", feature = "boa")
    ),
    all(feature = "jsc", any(feature = "quickjs", feature = "boa")),
    all(feature = "quickjs", feature = "boa"),
))]
compile_error!(
    "JavaScript engines are mutually exclusive: select at most one of v8|jsc|quickjs|boa"
);

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

/// Whether this build carries a JavaScript engine.
pub const RUNS_JAVASCRIPT: bool = cfg!(feature = "_js");

/// Whether this build carries the wasm engine.
pub const RUNS_WASM: bool = cfg!(feature = "wasm");

/// Name of the selected JavaScript engine, for logs and error messages.
#[cfg(feature = "v8")]
const JS_NAME: &str = "v8";
#[cfg(feature = "jsc")]
const JS_NAME: &str = "jsc";
#[cfg(feature = "quickjs")]
const JS_NAME: &str = "quickjs";
#[cfg(feature = "boa")]
const JS_NAME: &str = "boa";
/// A build with no JavaScript engine refuses a JavaScript worker before any
/// backend is named, so this one only stands in for the format string.
#[cfg(not(feature = "_js"))]
const JS_NAME: &str = "javascript";

/// Binding types v8 serves: it builds an `env` object per binding, while the
/// other JavaScript engines ignore `Script::bindings`.
#[cfg(feature = "v8")]
const JS_BINDINGS: &[BindingType] = &[
    BindingType::Assets,
    BindingType::Storage,
    BindingType::Kv,
    BindingType::Database,
    BindingType::Worker,
];
#[cfg(not(feature = "v8"))]
const JS_BINDINGS: &[BindingType] = &[];

/// A backend a build can carry. Which one runs a worker follows from the code
/// the worker was deployed with, not from the build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// The selected JavaScript engine
    Js,
    /// Wasmtime, running `wasi:http/proxy` components
    Wasm,
}

impl Backend {
    /// Backend name, as it is written in logs and errors
    pub const fn name(self) -> &'static str {
        match self {
            Self::Js => JS_NAME,
            Self::Wasm => "wasm",
        }
    }

    /// Binding types the backend serves. Wasm links the `openworkers:bindings`
    /// WIT package, which has no assets or worker interface.
    pub const fn supported_bindings(self) -> &'static [BindingType] {
        match self {
            Self::Js => JS_BINDINGS,
            Self::Wasm => &[BindingType::Kv, BindingType::Database, BindingType::Storage],
        }
    }

    pub fn supports_binding(self, binding_type: BindingType) -> bool {
        self.supported_bindings().contains(&binding_type)
    }

    /// Whether the backend reads `Script::env` and exposes the variables to the
    /// guest.
    pub const fn supports_env(self) -> bool {
        match self {
            Self::Js => cfg!(any(feature = "v8", feature = "jsc")),
            Self::Wasm => true,
        }
    }

    /// What a worker on this backend can rely on.
    fn capabilities(self) -> String {
        let bindings: Vec<&str> = self
            .supported_bindings()
            .iter()
            .copied()
            .map(binding_type_name)
            .collect();

        format!(
            "{} (env={}, bindings={})",
            self.name(),
            self.supports_env(),
            if bindings.is_empty() {
                "none".to_string()
            } else {
                bindings.join("/")
            },
        )
    }
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

/// One line naming the backends in this build and what each of them serves.
pub fn capabilities() -> String {
    let mut backends: Vec<String> = Vec::new();

    if RUNS_JAVASCRIPT {
        backends.push(Backend::Js.capabilities());
    }

    if RUNS_WASM {
        backends.push(Backend::Wasm.capabilities());
    }

    format!("runtime backends: {}", backends.join(", "))
}
