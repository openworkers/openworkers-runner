//! S3-compatible storage service.
//!
//! This module re-exports the storage operations from `ops_s3.rs`.
//! In the future, this will be the canonical location for storage code.
//!
//! ## Operations
//!
//! - `get` - Retrieve an object
//! - `put` - Store an object
//! - `head` - Get object metadata
//! - `list` - List objects with optional prefix
//! - `delete` - Remove an object
//! - `fetch` - HTTP-style fetch (returns streaming response)
//!
//! ## Security
//!
//! All paths are sanitized to prevent directory traversal attacks.
//! The sanitization handles multiple levels of URL encoding.

// Re-export everything from ops_s3 for now
// TODO: Move ops_s3.rs content here and deprecate ops_s3.rs
pub use crate::ops_s3::*;
