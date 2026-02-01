//! Key-Value store service.
//!
//! This module provides KV operations backed by PostgreSQL.
//!
//! ## Operations
//!
//! - `get` - Retrieve a value by key
//! - `put` - Store a value with optional TTL
//! - `delete` - Remove a key
//! - `list` - List keys with optional prefix filter
//!
//! ## Storage
//!
//! KV data is stored in the `kv_data` table with namespace isolation.
//! Each namespace corresponds to a KV binding configuration.

use openworkers_core::KvResult;
use sqlx::PgPool;

/// Maximum value size in bytes (100KB)
pub const MAX_VALUE_SIZE: usize = 100 * 1024;

/// Get a value from the KV store.
pub async fn get(pool: &PgPool, namespace_id: &str, key: &str) -> KvResult {
    let result = sqlx::query_scalar::<_, serde_json::Value>(
        r#"
        SELECT value FROM kv_data
        WHERE namespace_id = $1::uuid AND key = $2
        AND (expires_at IS NULL OR expires_at > NOW())
        "#,
    )
    .bind(namespace_id)
    .bind(key)
    .fetch_optional(pool)
    .await;

    match result {
        Ok(value) => KvResult::Value(value),
        Err(e) => {
            tracing::error!("[kv] get error: {}", e);
            KvResult::Error(format!("KV get failed: {}", e))
        }
    }
}

/// Put a value into the KV store with optional TTL.
pub async fn put(
    pool: &PgPool,
    namespace_id: &str,
    key: &str,
    value: &serde_json::Value,
    expires_in: Option<u64>,
) -> KvResult {
    // Check value size
    let value_str = match serde_json::to_string(value) {
        Ok(s) => s,
        Err(e) => {
            return KvResult::Error(format!("Invalid JSON value: {}", e));
        }
    };

    if value_str.len() > MAX_VALUE_SIZE {
        return KvResult::Error(format!(
            "Value too large: {} bytes (max {} bytes)",
            value_str.len(),
            MAX_VALUE_SIZE
        ));
    }

    // Execute upsert with or without TTL
    let result = if let Some(ttl) = expires_in {
        sqlx::query(
            r#"
            INSERT INTO kv_data (namespace_id, key, value, expires_at, updated_at)
            VALUES ($1::uuid, $2, $3, NOW() + $4 * INTERVAL '1 second', NOW())
            ON CONFLICT (namespace_id, key)
            DO UPDATE SET value = $3, expires_at = NOW() + $4 * INTERVAL '1 second', updated_at = NOW()
            "#,
        )
        .bind(namespace_id)
        .bind(key)
        .bind(value)
        .bind(ttl as i64)
        .execute(pool)
        .await
    } else {
        sqlx::query(
            r#"
            INSERT INTO kv_data (namespace_id, key, value, expires_at, updated_at)
            VALUES ($1::uuid, $2, $3, NULL, NOW())
            ON CONFLICT (namespace_id, key)
            DO UPDATE SET value = $3, expires_at = NULL, updated_at = NOW()
            "#,
        )
        .bind(namespace_id)
        .bind(key)
        .bind(value)
        .execute(pool)
        .await
    };

    match result {
        Ok(_) => KvResult::Ok,
        Err(e) => {
            tracing::error!("[kv] put error: {}", e);
            KvResult::Error(format!("KV put failed: {}", e))
        }
    }
}

/// Delete a key from the KV store.
pub async fn delete(pool: &PgPool, namespace_id: &str, key: &str) -> KvResult {
    let result = sqlx::query(
        r#"
        DELETE FROM kv_data
        WHERE namespace_id = $1::uuid AND key = $2
        "#,
    )
    .bind(namespace_id)
    .bind(key)
    .execute(pool)
    .await;

    match result {
        Ok(_) => KvResult::Ok,
        Err(e) => {
            tracing::error!("[kv] delete error: {}", e);
            KvResult::Error(format!("KV delete failed: {}", e))
        }
    }
}

/// List keys in the KV store with optional prefix filter.
pub async fn list(
    pool: &PgPool,
    namespace_id: &str,
    prefix: Option<&str>,
    limit: Option<u32>,
) -> KvResult {
    let limit_val = limit.unwrap_or(1000) as i64;

    let result = if let Some(prefix) = prefix {
        let pattern = format!("{}%", prefix);
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT key FROM kv_data
            WHERE namespace_id = $1::uuid
            AND key LIKE $2
            AND (expires_at IS NULL OR expires_at > NOW())
            ORDER BY key
            LIMIT $3
            "#,
        )
        .bind(namespace_id)
        .bind(&pattern)
        .bind(limit_val)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT key FROM kv_data
            WHERE namespace_id = $1::uuid
            AND (expires_at IS NULL OR expires_at > NOW())
            ORDER BY key
            LIMIT $2
            "#,
        )
        .bind(namespace_id)
        .bind(limit_val)
        .fetch_all(pool)
        .await
    };

    match result {
        Ok(keys) => KvResult::Keys(keys),
        Err(e) => {
            tracing::error!("[kv] list error: {}", e);
            KvResult::Error(format!("KV list failed: {}", e))
        }
    }
}
