//! S3 storage operations (deprecated - use services::storage instead)
//!
//! This module re-exports from `services::storage` for backwards compatibility.
//! New code should import from `crate::services::storage` directly.

#[deprecated(
    since = "0.13.0",
    note = "Use crate::services::storage instead of crate::ops_s3"
)]
pub use crate::services::storage::*;
