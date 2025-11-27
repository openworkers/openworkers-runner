//! Runtime selection based on feature flags
//!
//! This module re-exports the selected runtime based on the enabled feature.
//! Only one runtime feature should be enabled at a time.
//!
//! Usage:
//!   cargo build --features deno     (default)
//!   cargo build --features quickjs
//!   cargo build --features v8
//!   cargo build --features boa
//!   cargo build --features jsc

#[cfg(feature = "deno")]
pub use openworkers_runtime_deno::*;

#[cfg(feature = "quickjs")]
pub use openworkers_runtime_quickjs::*;

#[cfg(feature = "v8")]
pub use openworkers_runtime_v8::*;

#[cfg(feature = "boa")]
pub use openworkers_runtime_boa::*;

#[cfg(feature = "jsc")]
pub use openworkers_runtime_jsc::*;

// Compile-time check: ensure exactly one runtime is selected
#[cfg(not(any(
    feature = "deno",
    feature = "quickjs",
    feature = "v8",
    feature = "boa",
    feature = "jsc"
)))]
compile_error!("At least one runtime feature must be enabled: deno, quickjs, v8, boa, or jsc");
