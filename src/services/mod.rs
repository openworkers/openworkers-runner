//! Service layer for binding operations.
//!
//! This module provides clean abstractions for external operations:
//! - `fetch` - HTTP client with internal worker routing
//! - `storage` - S3-compatible object storage
//! - `kv` - Key-value store operations
//! - `database` - SQL database queries (requires `database` feature)
//!
//! Each service is self-contained and can be tested independently.

#[cfg(feature = "database")]
pub mod database;
pub mod fetch;
pub mod kv;
pub mod storage;
