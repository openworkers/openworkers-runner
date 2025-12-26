use sqlx::prelude::FromRow;
use std::collections::HashMap;

#[derive(Debug)]
pub enum WorkerIdentifier {
    Id(String),
    Name(String),
}

#[derive(Clone, Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "enum_workers_language", rename_all = "lowercase")]
pub enum WorkerLanguage {
    Javascript,
    Typescript,
}

#[derive(Clone, Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "enum_binding_type", rename_all = "lowercase")]
pub enum BindingType {
    Var,
    Secret,
    Assets,
    Storage,
    Kv,
}

/// Assets binding config (static file serving from S3/R2)
#[derive(Clone, Debug)]
pub struct AssetsConfig {
    pub id: String,
    pub bucket: String,
    pub prefix: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub endpoint: Option<String>,
    pub public_url: Option<String>,
}

/// Storage binding config (object storage S3/R2 with full read/write)
#[derive(Clone, Debug)]
pub struct StorageConfig {
    pub id: String,
    pub bucket: String,
    pub prefix: Option<String>,
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: Option<String>,
}

/// KV binding config (key-value store)
#[derive(Clone, Debug)]
pub struct KvConfig {
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
    /// Assets binding (static files)
    Assets { key: String, config: AssetsConfig },
    /// Storage binding (S3/R2)
    Storage { key: String, config: StorageConfig },
    /// KV binding
    Kv { key: String, config: KvConfig },
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
    pub env: Option<sqlx::types::Json<std::collections::HashMap<String, String>>>,
    pub script: String,
    pub checksum: i64,
    pub language: WorkerLanguage,
}

/// Extended worker data with binding configs
#[derive(Debug)]
pub struct WorkerWithBindings {
    pub id: String,
    pub name: String,
    pub script: String,
    pub checksum: i64,
    pub language: WorkerLanguage,
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
            script: data.script,
            checksum: data.checksum,
            language: data.language,
            env,
            bindings,
        }
    }
}

pub async fn get_worker(
    conn: &mut sqlx::PgConnection,
    identifier: WorkerIdentifier,
) -> Option<WorkerData> {
    log::debug!("get_worker: {:?}", identifier);

    let query = format!(
        r#"
        SELECT
            W.id::text,
            W.name,
            W.script,
            W.language,
            cast(extract(epoch from W.updated_at) + COALESCE(extract(epoch from max(V.updated_at)), 0) as BIGINT) as checksum,
            json_object_agg(V.key, V.value) FILTER (WHERE V IS NOT NULL) AS env
        FROM workers AS W
        LEFT OUTER JOIN environment_values AS V ON W.environment_id=V.environment_id AND W.user_id=V.user_id
        LEFT OUTER JOIN environments AS E ON W.environment_id=E.id AND W.user_id=E.user_id
        WHERE {}
        GROUP BY W.id, E.id
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
            log::debug!(
                "worker found: id: {}, checksum: {}, language: {:?}",
                worker.id,
                worker.checksum,
                worker.language
            );
            Some(worker)
        }
        Err(err) => {
            log::warn!("worker not found: {:?}", err);
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
    log::debug!("get_worker_with_bindings: {:?}", identifier);

    // First get the basic worker data
    let worker_query = format!(
        r#"
        SELECT
            W.id::text,
            W.name,
            W.script,
            W.language,
            cast(extract(epoch from W.updated_at) as BIGINT) as checksum
        FROM workers AS W
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
        script: String,
        language: WorkerLanguage,
        checksum: i64,
    }

    let basic = match sqlx::query_as::<_, BasicWorker>(&worker_query)
        .bind(&id_str)
        .fetch_one(&mut *conn)
        .await
    {
        Ok(w) => w,
        Err(err) => {
            log::warn!("worker not found: {:?}", err);
            return None;
        }
    };

    // Get all bindings for this worker
    let bindings_query = r#"
        SELECT
            V.key,
            V.value,
            V.type as binding_type
        FROM environment_values AS V
        JOIN workers AS W ON W.environment_id = V.environment_id AND W.user_id = V.user_id
        WHERE W.id::text = $1
    "#;

    let binding_rows: Vec<BindingRow> = match sqlx::query_as::<_, BindingRow>(bindings_query)
        .bind(&basic.id)
        .fetch_all(&mut *conn)
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            log::warn!("failed to fetch bindings: {:?}", err);
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
                if let Some(config_id) = row.value {
                    if let Some(config) = fetch_assets_config(&mut *conn, &config_id).await {
                        bindings.push(Binding::Assets {
                            key: row.key,
                            config,
                        });
                    }
                }
            }

            BindingType::Storage => {
                if let Some(config_id) = row.value {
                    if let Some(config) = fetch_storage_config(&mut *conn, &config_id).await {
                        bindings.push(Binding::Storage {
                            key: row.key,
                            config,
                        });
                    }
                }
            }

            BindingType::Kv => {
                if let Some(config_id) = row.value {
                    if let Some(config) = fetch_kv_config(&mut *conn, &config_id).await {
                        bindings.push(Binding::Kv {
                            key: row.key,
                            config,
                        });
                    }
                }
            }
        }
    }

    log::debug!(
        "worker found: id: {}, checksum: {}, bindings: {}",
        basic.id,
        basic.checksum,
        bindings.len()
    );

    Some(WorkerWithBindings {
        id: basic.id,
        name: basic.name,
        script: basic.script,
        checksum: basic.checksum,
        language: basic.language,
        env,
        bindings,
    })
}

/// Fetch assets config by ID
async fn fetch_assets_config(
    conn: &mut sqlx::PgConnection,
    config_id: &str,
) -> Option<AssetsConfig> {
    #[derive(Debug, FromRow)]
    struct Row {
        id: String,
        bucket: String,
        prefix: Option<String>,
        access_key_id: String,
        secret_access_key: String,
        endpoint: Option<String>,
        public_url: Option<String>,
    }

    let query = r#"
        SELECT id::text, bucket, prefix, access_key_id, secret_access_key, endpoint, public_url
        FROM storage_configs
        WHERE id::text = $1
    "#;

    match sqlx::query_as::<_, Row>(query)
        .bind(config_id)
        .fetch_one(conn)
        .await
    {
        Ok(row) => Some(AssetsConfig {
            id: row.id,
            bucket: row.bucket,
            prefix: row.prefix,
            access_key_id: row.access_key_id,
            secret_access_key: row.secret_access_key,
            endpoint: row.endpoint,
            public_url: row.public_url,
        }),
        Err(err) => {
            log::warn!(
                "failed to fetch storage_config for assets {}: {:?}",
                config_id,
                err
            );
            None
        }
    }
}

/// Fetch storage config by ID
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
    }

    let query = r#"
        SELECT id::text, bucket, prefix, endpoint, access_key_id, secret_access_key, region
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
        }),
        Err(err) => {
            log::warn!("failed to fetch storage_config {}: {:?}", config_id, err);
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
            log::warn!("failed to fetch kv_config {}: {:?}", config_id, err);
            None
        }
    }
}

pub async fn get_worker_id_from_domain(
    conn: &mut sqlx::PgConnection,
    domain: String,
) -> Option<String> {
    let query = sqlx::query_scalar!(
        "SELECT worker_id::text FROM domains WHERE name = $1 LIMIT 1",
        domain
    );

    match query.fetch_one(conn).await {
        Ok(worker_id) => worker_id,
        Err(err) => {
            log::warn!("failed to get worker id from domain: {:?}", err);
            None
        }
    }
}
