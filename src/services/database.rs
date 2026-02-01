//! SQL database service.
//!
//! This module provides SQL query execution with:
//! - Query validation via postgate
//! - JSON result wrapping
//! - Schema isolation for multi-tenant deployments
//! - Direct connection string support
//!
//! ## Providers
//!
//! - `postgres`: Direct connection to a PostgreSQL database
//! - `platform`: Multi-tenant mode using schema isolation on shared pool

use openworkers_core::{SqlParam, SqlPrimitive};
use sqlx::PgPool;

/// Database connection pool type alias
pub type DbPool = PgPool;

/// Query execution mode determined by SQL parsing
#[derive(Clone, Copy, Debug)]
pub enum QueryMode {
    /// SELECT query - wrap as subquery
    Select,
    /// INSERT/UPDATE/DELETE with RETURNING - wrap as CTE
    ReturningMutation,
    /// INSERT/UPDATE/DELETE without RETURNING - return rows affected
    Mutation,
}

impl QueryMode {
    /// Determine query mode from postgate parsed result
    pub fn from_parsed(parsed: &postgate::ParsedQuery) -> Self {
        if !parsed.returns_rows {
            QueryMode::Mutation
        } else if parsed.operation == postgate::SqlOperation::Select {
            QueryMode::Select
        } else {
            QueryMode::ReturningMutation
        }
    }
}

/// Execute query with direct connection string.
pub async fn execute_with_connection_string(
    connection_string: &str,
    sql: &str,
    params: &[SqlParam],
    mode: QueryMode,
) -> Result<String, String> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(connection_string)
        .await
        .map_err(|e| format!("Database connection failed: {}", e))?;

    match mode {
        QueryMode::Mutation => execute_mutation(&pool, sql, params).await,
        _ => execute_json_query(&pool, sql, params, mode).await,
    }
}

/// Execute query with schema isolation (SET search_path).
pub async fn execute_with_schema(
    pool: &DbPool,
    schema_name: &str,
    sql: &str,
    params: &[SqlParam],
    mode: QueryMode,
) -> Result<String, String> {
    let start = std::time::Instant::now();

    // Use a transaction to set search_path, then execute the query
    let safe_schema = schema_name.replace('"', "\"\"");

    tracing::debug!(
        "[db] pool stats - size: {}, idle: {}, acquiring transaction...",
        pool.size(),
        pool.num_idle()
    );

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("Failed to start transaction: {}", e))?;

    tracing::debug!("[db] transaction acquired in {:?}", start.elapsed());

    // Set the search_path for this transaction
    sqlx::query(&format!("SET LOCAL search_path TO \"{}\"", safe_schema))
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to set search_path: {}", e))?;

    tracing::debug!("[db] search_path set in {:?}", start.elapsed());

    // Execute the user query
    let result = match mode {
        QueryMode::Mutation => execute_mutation_tx(&mut tx, sql, params).await?,
        _ => execute_json_query_tx(&mut tx, sql, params, mode).await?,
    };

    tracing::debug!("[db] query executed in {:?}", start.elapsed());

    // Commit the transaction
    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit transaction: {}", e))?;

    tracing::debug!("[db] transaction committed in {:?}", start.elapsed());

    Ok(result)
}

/// Wrap user query to return JSON directly from PostgreSQL.
pub fn wrap_query_as_json(sql: &str, mode: QueryMode) -> String {
    let trimmed = sql.trim().trim_end_matches(';');

    match mode {
        QueryMode::Select => {
            // SELECT can be used as a subquery
            format!(
                "SELECT COALESCE(jsonb_agg(row_to_json(t)), '[]'::jsonb) as result FROM ({}) t",
                trimmed
            )
        }
        QueryMode::ReturningMutation => {
            // INSERT/UPDATE/DELETE with RETURNING needs a CTE
            format!(
                "WITH t AS ({}) SELECT COALESCE(jsonb_agg(row_to_json(t)), '[]'::jsonb) as result FROM t",
                trimmed
            )
        }
        QueryMode::Mutation => {
            unreachable!("Mutation queries should not be wrapped")
        }
    }
}

/// Execute wrapped JSON query and extract result.
pub async fn execute_json_query(
    pool: &DbPool,
    sql: &str,
    params: &[SqlParam],
    mode: QueryMode,
) -> Result<String, String> {
    use sqlx::Row;

    let wrapped = wrap_query_as_json(sql, mode);
    let mut query = sqlx::query(&wrapped);

    for param in params {
        query = bind_sql_param(query, param);
    }

    let row = query
        .fetch_one(pool)
        .await
        .map_err(|e| format!("Query execution failed: {}", e))?;

    let result: serde_json::Value = row
        .try_get("result")
        .map_err(|e| format!("Failed to get result: {}", e))?;

    serde_json::to_string(&result).map_err(|e| format!("Failed to serialize result: {}", e))
}

/// Execute wrapped JSON query within a transaction.
pub async fn execute_json_query_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql: &str,
    params: &[SqlParam],
    mode: QueryMode,
) -> Result<String, String> {
    use sqlx::Row;

    let wrapped = wrap_query_as_json(sql, mode);
    let mut query = sqlx::query(&wrapped);

    for param in params {
        query = bind_sql_param(query, param);
    }

    let row = query
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| format!("Query execution failed: {}", e))?;

    let result: serde_json::Value = row
        .try_get("result")
        .map_err(|e| format!("Failed to get result: {}", e))?;

    serde_json::to_string(&result).map_err(|e| format!("Failed to serialize result: {}", e))
}

/// Execute mutation (INSERT/UPDATE/DELETE) and return rows affected.
pub async fn execute_mutation(
    pool: &DbPool,
    sql: &str,
    params: &[SqlParam],
) -> Result<String, String> {
    let mut query = sqlx::query(sql);

    for param in params {
        query = bind_sql_param(query, param);
    }

    let result = query
        .execute(pool)
        .await
        .map_err(|e| format!("Query execution failed: {}", e))?;

    Ok(format!("{{\"rowsAffected\":{}}}", result.rows_affected()))
}

/// Execute mutation within a transaction.
pub async fn execute_mutation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql: &str,
    params: &[SqlParam],
) -> Result<String, String> {
    let mut query = sqlx::query(sql);

    for param in params {
        query = bind_sql_param(query, param);
    }

    let result = query
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Query execution failed: {}", e))?;

    Ok(format!("{{\"rowsAffected\":{}}}", result.rows_affected()))
}

/// Bind a SqlParam to a sqlx query.
pub fn bind_sql_param<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    param: &'q SqlParam,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match param {
        SqlParam::Primitive(p) => bind_sql_primitive(query, p),
        SqlParam::Array(arr) => {
            // Determine array type from first non-null element
            let first_type = arr.iter().find_map(|p| match p {
                SqlPrimitive::Null => None,
                SqlPrimitive::Bool(_) => Some("bool"),
                SqlPrimitive::Int(_) => Some("int"),
                SqlPrimitive::Float(_) => Some("float"),
                SqlPrimitive::String(_) => Some("string"),
            });

            match first_type {
                Some("bool") => {
                    let values: Vec<Option<bool>> = arr
                        .iter()
                        .map(|p| match p {
                            SqlPrimitive::Bool(b) => Some(*b),
                            SqlPrimitive::Null => None,
                            _ => None,
                        })
                        .collect();
                    query.bind(values)
                }
                Some("int") => {
                    let values: Vec<Option<i64>> = arr
                        .iter()
                        .map(|p| match p {
                            SqlPrimitive::Int(i) => Some(*i),
                            SqlPrimitive::Null => None,
                            _ => None,
                        })
                        .collect();
                    query.bind(values)
                }
                Some("float") => {
                    let values: Vec<Option<f64>> = arr
                        .iter()
                        .map(|p| match p {
                            SqlPrimitive::Float(f) => Some(*f),
                            SqlPrimitive::Int(i) => Some(*i as f64),
                            SqlPrimitive::Null => None,
                            _ => None,
                        })
                        .collect();
                    query.bind(values)
                }
                Some("string") | None => {
                    let values: Vec<Option<String>> = arr
                        .iter()
                        .map(|p| match p {
                            SqlPrimitive::String(s) => Some(s.clone()),
                            SqlPrimitive::Null => None,
                            _ => Some(format!("{:?}", p)),
                        })
                        .collect();
                    query.bind(values)
                }
                _ => unreachable!(),
            }
        }
    }
}

/// Bind a SqlPrimitive to a sqlx query.
pub fn bind_sql_primitive<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    param: &'q SqlPrimitive,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match param {
        SqlPrimitive::Null => query.bind(None::<String>),
        SqlPrimitive::Bool(b) => query.bind(*b),
        SqlPrimitive::Int(i) => query.bind(*i),
        SqlPrimitive::Float(f) => query.bind(*f),
        SqlPrimitive::String(s) => query.bind(s.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_select_query() {
        let sql = "SELECT id, name FROM users";
        let wrapped = wrap_query_as_json(sql, QueryMode::Select);
        assert!(wrapped.contains("jsonb_agg"));
        assert!(wrapped.contains("row_to_json"));
        assert!(wrapped.contains("SELECT id, name FROM users"));
    }

    #[test]
    fn test_wrap_select_trims_semicolon() {
        let sql = "SELECT * FROM users;";
        let wrapped = wrap_query_as_json(sql, QueryMode::Select);
        assert!(!wrapped.contains(";;"));
    }

    #[test]
    fn test_wrap_returning_mutation() {
        let sql = "INSERT INTO users (name) VALUES ('test') RETURNING id";
        let wrapped = wrap_query_as_json(sql, QueryMode::ReturningMutation);
        assert!(wrapped.starts_with("WITH t AS"));
        assert!(wrapped.contains("RETURNING id"));
    }

    #[test]
    #[should_panic(expected = "should not be wrapped")]
    fn test_wrap_mutation_panics() {
        let sql = "DELETE FROM users WHERE id = 1";
        wrap_query_as_json(sql, QueryMode::Mutation);
    }
}
