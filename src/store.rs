use sqlx::prelude::FromRow;
use std::collections::HashMap;

use crate::utils::short_id;

#[derive(Debug)]
pub enum WorkerIdentifier {
    Id(String),
    Name(String),
}

#[derive(Clone, Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "enum_code_type", rename_all = "lowercase")]
pub enum CodeType {
    Javascript,
    Typescript,
    Wasm,
    Snapshot,
}

impl std::fmt::Display for CodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CodeType::Javascript => "javascript",
            CodeType::Typescript => "typescript",
            CodeType::Wasm => "wasm",
            CodeType::Snapshot => "snapshot",
        };
        write!(f, "{}", s)
    }
}

#[derive(Clone, Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "enum_binding_type", rename_all = "lowercase")]
pub enum BindingType {
    Var,
    Secret,
    Assets,
    Storage,
    Kv,
    Database,
    Worker,
}

#[derive(Clone, Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "enum_database_provider", rename_all = "lowercase")]
pub enum DatabaseProvider {
    Platform,
    Postgres,
}

/// S3/R2-compatible storage config (used for both Assets and Storage bindings)
#[derive(Clone, Debug)]
pub struct StorageConfig {
    pub id: String,
    pub bucket: String,
    pub prefix: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub endpoint: String,
    pub region: Option<String>,
    pub public_url: Option<String>,
}

/// KV binding config (key-value store)
#[derive(Clone, Debug)]
pub struct KvConfig {
    pub id: String,
    pub name: String,
}

/// Database binding config (multi-provider)
#[derive(Clone, Debug)]
pub struct DatabaseConfig {
    pub id: String,
    pub name: String,
    /// Provider: platform (shared multi-tenant) or postgres (direct connection)
    pub provider: DatabaseProvider,
    /// Connection string (for postgres provider)
    pub connection_string: Option<String>,
    /// Schema name (for platform provider - multi-tenant on shared pool)
    pub schema_name: Option<String>,
    /// Maximum rows returned per query
    pub max_rows: i32,
    /// Query timeout in seconds
    pub timeout_seconds: i32,
}

/// Worker binding config (worker-to-worker calls)
#[derive(Clone, Debug)]
pub struct WorkerBindingConfig {
    pub id: String,
    pub name: String,
}

/// A worker binding (environment variable or resource binding)
#[derive(Clone, Debug)]
pub enum Binding {
    /// Plain environment variable
    Var { key: String, value: String },
    /// Secret environment variable (hidden in logs)
    Secret { key: String, value: String },
    /// Assets binding (static files from S3/R2)
    Assets { key: String, config: StorageConfig },
    /// Storage binding (read/write S3/R2)
    Storage { key: String, config: StorageConfig },
    /// KV binding
    Kv { key: String, config: KvConfig },
    /// Database binding (PostgreSQL)
    Database { key: String, config: DatabaseConfig },
    /// Worker binding (worker-to-worker calls)
    Worker {
        key: String,
        config: WorkerBindingConfig,
    },
}

impl Binding {
    /// Convert to BindingInfo (name + type only, no credentials)
    /// Returns None for Var/Secret (they're not resource bindings)
    pub fn to_binding_info(&self) -> Option<openworkers_core::BindingInfo> {
        match self {
            Binding::Var { .. } | Binding::Secret { .. } => None,
            Binding::Assets { key, .. } => Some(openworkers_core::BindingInfo::assets(key.clone())),
            Binding::Storage { key, .. } => {
                Some(openworkers_core::BindingInfo::storage(key.clone()))
            }
            Binding::Kv { key, .. } => Some(openworkers_core::BindingInfo::kv(key.clone())),
            Binding::Database { key, .. } => {
                Some(openworkers_core::BindingInfo::database(key.clone()))
            }
            Binding::Worker { key, .. } => Some(openworkers_core::BindingInfo::worker(key.clone())),
        }
    }
}

/// Convert a list of Bindings to BindingInfos (for Script)
pub fn bindings_to_infos(bindings: &[Binding]) -> Vec<openworkers_core::BindingInfo> {
    bindings
        .iter()
        .filter_map(|b| b.to_binding_info())
        .collect()
}

#[derive(Debug, FromRow)]
pub struct WorkerData {
    pub id: String,
    pub name: String,
    pub user_id: String,
    pub env: Option<sqlx::types::Json<std::collections::HashMap<String, String>>>,
    pub code: Vec<u8>,
    pub code_type: CodeType,
    pub version: i32,
}

/// Extended worker data with binding configs
#[derive(Debug)]
pub struct WorkerWithBindings {
    pub id: String,
    pub name: String,
    /// Owner/tenant ID for isolate pool isolation
    pub user_id: String,
    pub code: Vec<u8>,
    pub code_type: CodeType,
    pub version: i32,
    /// Simple env vars (for backwards compatibility)
    pub env: HashMap<String, String>,
    /// All bindings (vars, secrets, and resource bindings)
    pub bindings: Vec<Binding>,
}

impl From<WorkerData> for WorkerWithBindings {
    fn from(data: WorkerData) -> Self {
        let env = data.env.map(|j| j.0).unwrap_or_default();

        // Convert env to var bindings for backwards compatibility
        let bindings = env
            .iter()
            .map(|(k, v)| Binding::Var {
                key: k.clone(),
                value: v.clone(),
            })
            .collect();

        Self {
            id: data.id,
            name: data.name,
            user_id: data.user_id,
            code: data.code,
            code_type: data.code_type,
            version: data.version,
            env,
            bindings,
        }
    }
}

pub async fn get_worker(
    conn: &mut sqlx::PgConnection,
    identifier: WorkerIdentifier,
) -> Option<WorkerData> {
    tracing::debug!("get_worker: {:?}", identifier);

    let query = format!(
        r#"
        SELECT
            W.id::text,
            W.name,
            W.user_id::text,
            D.code,
            D.code_type,
            W.current_version as version,
            json_object_agg(V.key, V.value) FILTER (WHERE V IS NOT NULL) AS env
        FROM workers AS W
        JOIN worker_deployments AS D ON D.worker_id = W.id AND D.version = W.current_version
        LEFT OUTER JOIN environment_values AS V ON W.environment_id=V.environment_id AND W.user_id=V.user_id
        LEFT OUTER JOIN environments AS E ON W.environment_id=E.id AND W.user_id=E.user_id
        WHERE {}
        GROUP BY W.id, E.id, D.code, D.code_type
        "#,
        match identifier {
            WorkerIdentifier::Id(_) => "W.id::text = $1",
            WorkerIdentifier::Name(_) => "W.name = $1",
        }
    );

    let identifier = match identifier {
        WorkerIdentifier::Id(id) => id,
        WorkerIdentifier::Name(name) => name,
    };

    match sqlx::query_as::<_, WorkerData>(query.as_str())
        .bind(identifier)
        .fetch_one(conn)
        .await
    {
        Ok(worker) => {
            tracing::debug!(
                "worker found: id: {}, version: {}, code_type: {:?}",
                worker.id,
                worker.version,
                worker.code_type
            );
            Some(worker)
        }
        Err(err) => {
            tracing::warn!("worker not found: {:?}", err);
            None
        }
    }
}

/// Raw binding row from the database
#[derive(Debug, FromRow)]
struct BindingRow {
    key: String,
    value: Option<String>,
    binding_type: BindingType,
}

/// Get worker with full binding configs
pub async fn get_worker_with_bindings(
    conn: &mut sqlx::PgConnection,
    identifier: WorkerIdentifier,
) -> Option<WorkerWithBindings> {
    tracing::debug!("get_worker_with_bindings: {:?}", identifier);

    // First get the basic worker data with code from deployments
    let worker_query = format!(
        r#"
        SELECT
            W.id::text,
            W.name,
            W.user_id::text,
            D.code,
            D.code_type,
            W.current_version as version
        FROM workers AS W
        JOIN worker_deployments AS D ON D.worker_id = W.id AND D.version = W.current_version
        WHERE {}
        "#,
        match &identifier {
            WorkerIdentifier::Id(_) => "W.id::text = $1",
            WorkerIdentifier::Name(_) => "W.name = $1",
        }
    );

    let id_str = match &identifier {
        WorkerIdentifier::Id(id) => id.clone(),
        WorkerIdentifier::Name(name) => name.clone(),
    };

    #[derive(Debug, FromRow)]
    struct BasicWorker {
        id: String,
        name: String,
        user_id: String,
        code: Vec<u8>,
        code_type: CodeType,
        version: i32,
    }

    let basic = match sqlx::query_as::<_, BasicWorker>(&worker_query)
        .bind(&id_str)
        .fetch_one(&mut *conn)
        .await
    {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!("worker not found: {:?}", err);
            return None;
        }
    };

    // Get all bindings for this worker
    // If worker is in a project, use project's environment_id, otherwise use worker's environment_id
    let bindings_query = r#"
        SELECT
            V.key,
            V.value,
            V.type as binding_type
        FROM workers AS W
        LEFT JOIN projects AS P ON W.project_id = P.id
        JOIN environment_values AS V ON V.environment_id = COALESCE(P.environment_id, W.environment_id) AND V.user_id = W.user_id
        WHERE W.id::text = $1
    "#;

    let binding_rows: Vec<BindingRow> = match sqlx::query_as::<_, BindingRow>(bindings_query)
        .bind(&basic.id)
        .fetch_all(&mut *conn)
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!("failed to fetch bindings: {:?}", err);
            Vec::new()
        }
    };

    // Convert rows to Binding enum, fetching configs as needed
    let mut bindings = Vec::new();
    let mut env = HashMap::new();

    for row in binding_rows {
        match row.binding_type {
            BindingType::Var => {
                if let Some(value) = row.value {
                    env.insert(row.key.clone(), value.clone());
                    bindings.push(Binding::Var {
                        key: row.key,
                        value,
                    });
                }
            }

            BindingType::Secret => {
                if let Some(value) = row.value {
                    env.insert(row.key.clone(), value.clone());
                    bindings.push(Binding::Secret {
                        key: row.key,
                        value,
                    });
                }
            }

            BindingType::Assets => {
                if let Some(config_id) = row.value
                    && let Some(config) = fetch_storage_config(&mut *conn, &config_id).await
                {
                    bindings.push(Binding::Assets {
                        key: row.key,
                        config,
                    });
                }
            }

            BindingType::Storage => {
                if let Some(config_id) = row.value
                    && let Some(config) = fetch_storage_config(&mut *conn, &config_id).await
                {
                    bindings.push(Binding::Storage {
                        key: row.key,
                        config,
                    });
                }
            }

            BindingType::Kv => {
                if let Some(config_id) = row.value
                    && let Some(config) = fetch_kv_config(&mut *conn, &config_id).await
                {
                    bindings.push(Binding::Kv {
                        key: row.key,
                        config,
                    });
                }
            }

            BindingType::Database => {
                if let Some(config_id) = row.value
                    && let Some(config) = fetch_database_config(&mut *conn, &config_id).await
                {
                    bindings.push(Binding::Database {
                        key: row.key,
                        config,
                    });
                }
            }

            BindingType::Worker => {
                if let Some(worker_id) = row.value
                    && let Some(config) = fetch_worker_binding_config(&mut *conn, &worker_id).await
                {
                    bindings.push(Binding::Worker {
                        key: row.key,
                        config,
                    });
                }
            }
        }
    }

    tracing::debug!(
        "worker found: id: {}, version: {}, bindings: {}, type: {}",
        short_id(&basic.id),
        basic.version,
        bindings.len(),
        basic.code_type
    );

    Some(WorkerWithBindings {
        id: basic.id,
        name: basic.name,
        user_id: basic.user_id,
        code: basic.code,
        code_type: basic.code_type,
        version: basic.version,
        env,
        bindings,
    })
}

/// Fetch storage config by ID (used for both Assets and Storage bindings)
async fn fetch_storage_config(
    conn: &mut sqlx::PgConnection,
    config_id: &str,
) -> Option<StorageConfig> {
    #[derive(Debug, FromRow)]
    struct Row {
        id: String,
        bucket: String,
        prefix: Option<String>,
        endpoint: String,
        access_key_id: String,
        secret_access_key: String,
        region: Option<String>,
        public_url: Option<String>,
    }

    let query = r#"
        SELECT id::text, bucket, prefix, endpoint, access_key_id, secret_access_key, region, public_url
        FROM storage_configs
        WHERE id::text = $1
    "#;

    match sqlx::query_as::<_, Row>(query)
        .bind(config_id)
        .fetch_one(conn)
        .await
    {
        Ok(row) => Some(StorageConfig {
            id: row.id,
            bucket: row.bucket,
            prefix: row.prefix,
            endpoint: row.endpoint,
            access_key_id: row.access_key_id,
            secret_access_key: row.secret_access_key,
            region: row.region,
            public_url: row.public_url,
        }),
        Err(err) => {
            tracing::warn!("failed to fetch storage_config {}: {:?}", config_id, err);
            None
        }
    }
}

/// Fetch KV config by ID
async fn fetch_kv_config(conn: &mut sqlx::PgConnection, config_id: &str) -> Option<KvConfig> {
    #[derive(Debug, FromRow)]
    struct Row {
        id: String,
        name: String,
    }

    let query = r#"
        SELECT id::text, name
        FROM kv_configs
        WHERE id::text = $1
    "#;

    match sqlx::query_as::<_, Row>(query)
        .bind(config_id)
        .fetch_one(conn)
        .await
    {
        Ok(row) => Some(KvConfig {
            id: row.id,
            name: row.name,
        }),
        Err(err) => {
            tracing::warn!("failed to fetch kv_config {}: {:?}", config_id, err);
            None
        }
    }
}

/// Fetch database config by ID
async fn fetch_database_config(
    conn: &mut sqlx::PgConnection,
    config_id: &str,
) -> Option<DatabaseConfig> {
    #[derive(Debug, FromRow)]
    struct Row {
        id: String,
        name: String,
        provider: DatabaseProvider,
        connection_string: Option<String>,
        schema_name: Option<String>,
        max_rows: i32,
        timeout_seconds: i32,
    }

    let query = r#"
        SELECT id::text, name, provider, connection_string, schema_name, max_rows, timeout_seconds
        FROM database_configs
        WHERE id::text = $1
    "#;

    match sqlx::query_as::<_, Row>(query)
        .bind(config_id)
        .fetch_one(conn)
        .await
    {
        Ok(row) => Some(DatabaseConfig {
            id: row.id,
            name: row.name,
            provider: row.provider,
            connection_string: row.connection_string,
            schema_name: row.schema_name,
            max_rows: row.max_rows,
            timeout_seconds: row.timeout_seconds,
        }),
        Err(err) => {
            tracing::warn!("failed to fetch database_config {}: {:?}", config_id, err);
            None
        }
    }
}

/// Fetch worker binding config by worker ID
async fn fetch_worker_binding_config(
    conn: &mut sqlx::PgConnection,
    worker_id: &str,
) -> Option<WorkerBindingConfig> {
    #[derive(Debug, FromRow)]
    struct Row {
        id: String,
        name: String,
    }

    let query = r#"
        SELECT id::text, name
        FROM workers
        WHERE id::text = $1
    "#;

    match sqlx::query_as::<_, Row>(query)
        .bind(worker_id)
        .fetch_one(conn)
        .await
    {
        Ok(row) => Some(WorkerBindingConfig {
            id: row.id,
            name: row.name,
        }),
        Err(err) => {
            tracing::warn!(
                "failed to fetch worker_binding {}: {:?}",
                short_id(worker_id),
                err
            );
            None
        }
    }
}

/// Endpoint type from DB enum
#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "enum_endpoint_type", rename_all = "lowercase")]
pub enum EndpointType {
    Worker,
    Project,
}

/// Endpoint resolution result
#[derive(Debug, Clone)]
pub enum Endpoint {
    Worker { worker_id: String },
    Project { project_id: String },
}

/// Get endpoint by name (from subdomain like mon-app.workers.rocks)
pub async fn get_endpoint_by_name(conn: &mut sqlx::PgConnection, name: &str) -> Option<Endpoint> {
    #[derive(sqlx::FromRow)]
    struct EndpointRow {
        #[sqlx(rename = "type")]
        endpoint_type: EndpointType,
        worker_id: Option<String>,
        project_id: Option<String>,
    }

    tracing::debug!("Looking up endpoint by name: {}", name);

    let result = sqlx::query_as::<_, EndpointRow>(
        "SELECT type, worker_id::text, project_id::text FROM endpoints WHERE name = $1",
    )
    .bind(name)
    .fetch_one(conn)
    .await;

    match result {
        Ok(row) => {
            tracing::debug!(
                "Found endpoint: type={:?}, worker_id={:?}, project_id={:?}",
                row.endpoint_type,
                row.worker_id,
                row.project_id
            );
            match row.endpoint_type {
                EndpointType::Worker => row.worker_id.map(|id| Endpoint::Worker { worker_id: id }),
                EndpointType::Project => row
                    .project_id
                    .map(|id| Endpoint::Project { project_id: id }),
            }
        }
        Err(err) => {
            tracing::warn!("Failed to get endpoint by name '{}': {:?}", name, err);
            None
        }
    }
}

/// Get endpoint from custom domain
pub async fn get_endpoint_from_domain(
    conn: &mut sqlx::PgConnection,
    domain: &str,
) -> Option<Endpoint> {
    #[derive(sqlx::FromRow)]
    struct DomainRow {
        worker_id: Option<String>,
        project_id: Option<String>,
    }

    let result = sqlx::query_as::<_, DomainRow>(
        "SELECT worker_id::text, project_id::text FROM domains WHERE name = $1",
    )
    .bind(domain)
    .fetch_one(conn)
    .await;

    match result {
        Ok(row) => {
            if let Some(worker_id) = row.worker_id {
                Some(Endpoint::Worker { worker_id })
            } else {
                row.project_id
                    .map(|project_id| Endpoint::Project { project_id })
            }
        }
        Err(err) => {
            tracing::warn!("failed to get endpoint from domain {}: {:?}", domain, err);
            None
        }
    }
}

/// Backend type for project routes
#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "enum_backend_type", rename_all = "lowercase")]
pub enum BackendType {
    Worker,
    Storage,
}

/// Project route
#[derive(Debug, Clone)]
pub struct Route {
    pub pattern: String,
    pub priority: i32,
    pub backend_type: BackendType,
    pub worker_id: Option<String>,
    pub storage_config_id: Option<String>,
    pub cache_ttl: Option<i32>,
}

/// Request resolution result
#[derive(Debug, Clone)]
pub struct RequestResolution {
    pub worker_id: Option<String>,
    pub project_id: Option<String>,
    pub backend_type: BackendType,
    pub assets_storage_id: Option<String>,
}

/// Resolve worker from request in a single DB call
/// This calls the PL/SQL function resolve_worker_from_request() which handles:
/// - Endpoint resolution (from domain, worker_id, or worker_name)
/// - Route matching (for projects)
/// - Backend resolution (worker or storage)
pub async fn resolve_worker_from_request(
    conn: &mut sqlx::PgConnection,
    domain: Option<&str>,
    worker_id: Option<&str>,
    worker_name: Option<&str>,
    path: &str,
) -> Option<RequestResolution> {
    #[derive(sqlx::FromRow)]
    struct ResolutionRow {
        worker_id: Option<String>,
        project_id: Option<String>,
        backend_type: BackendType,
        assets_storage_id: Option<String>,
    }

    let result = sqlx::query_as::<_, ResolutionRow>(
        "SELECT worker_id::text, project_id::text, backend_type, assets_storage_id::text
         FROM resolve_worker_from_request($1, $2::uuid, $3, $4)",
    )
    .bind(domain)
    .bind(worker_id)
    .bind(worker_name)
    .bind(path)
    .fetch_one(conn)
    .await;

    match result {
        Ok(row) => Some(RequestResolution {
            worker_id: row.worker_id,
            project_id: row.project_id,
            backend_type: row.backend_type,
            assets_storage_id: row.assets_storage_id,
        }),
        Err(err) => {
            tracing::debug!("Failed to resolve request: {:?}", err);
            None
        }
    }
}

/// Get storage config by ID
pub async fn get_storage_config(
    conn: &mut sqlx::PgConnection,
    storage_config_id: &str,
) -> Option<StorageConfig> {
    #[derive(sqlx::FromRow)]
    struct StorageConfigRow {
        id: String,
        bucket: String,
        prefix: Option<String>,
        access_key_id: String,
        secret_access_key: String,
        endpoint: String,
        region: Option<String>,
        public_url: Option<String>,
    }

    let result = sqlx::query_as::<_, StorageConfigRow>(
        "SELECT id::text, bucket, prefix, access_key_id, secret_access_key, endpoint, region, public_url
         FROM storage_configs
         WHERE id = $1::uuid",
    )
    .bind(storage_config_id)
    .fetch_one(conn)
    .await;

    match result {
        Ok(row) => Some(StorageConfig {
            id: row.id,
            bucket: row.bucket,
            prefix: row.prefix,
            access_key_id: row.access_key_id,
            secret_access_key: row.secret_access_key,
            endpoint: row.endpoint,
            region: row.region,
            public_url: row.public_url,
        }),
        Err(err) => {
            tracing::warn!(
                "Failed to get storage config {}: {:?}",
                storage_config_id,
                err
            );
            None
        }
    }
}

/// Legacy function - kept for backwards compatibility
pub async fn get_worker_id_from_domain(
    conn: &mut sqlx::PgConnection,
    domain: String,
) -> Option<String> {
    // Use new endpoint resolution, return worker_id if it's a worker endpoint
    match get_endpoint_from_domain(conn, &domain).await {
        Some(Endpoint::Worker { worker_id }) => Some(worker_id),
        _ => None,
    }
}
