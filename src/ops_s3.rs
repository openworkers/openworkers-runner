//! S3 storage operations.
//!
//! This module re-exports from `services::storage` for backwards compatibility.
//! New code should import from `crate::services::storage` directly.

pub use crate::services::storage::*;
