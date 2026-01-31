//! Service layer for binding operations.
//!
//! This module provides clean abstractions for external operations:
//! - `fetch` - HTTP client with internal worker routing
//! - `storage` - S3-compatible object storage
//! - `kv` - Key-value store operations
//! - `database` - SQL database queries
//!
//! Each service is self-contained and can be tested independently.

pub mod fetch;
pub mod storage;

// TODO: Extract from ops.rs
// pub mod kv;
// pub mod database;
