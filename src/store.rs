use sqlx::prelude::FromRow;

pub type Database = sqlx::Pool<sqlx::Postgres>;

#[derive(Debug)]
pub enum WorkerIdentifier {
    Id(String),
    Name(String),
}

#[derive(Debug, FromRow)]
pub struct WorkerData {
    pub id: String,
    pub env: Option<String>,
    // env: Option<sqlx::types::Json<std::collections::HashMap<String, String>>>,
    pub script: String,
    pub checksum: i64,
}

pub async fn get_worker(db: &Database, identifier: WorkerIdentifier) -> Option<WorkerData> {
    log::debug!("get_worker: {:?}", identifier);

    let query = format!(
        r#"
        SELECT
            W.id::text,
            W.name,
            W.script,
            cast(extract(epoch from W.updated_at) + COALESCE(extract(epoch from max(V.updated_at)), 0) as BIGINT) as checksum,
            json_object_agg(V.key, V.value) FILTER (WHERE V IS NOT NULL)::text AS env
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
        .fetch_one(db)
        .await
    {
        Ok(worker) => {
            log::debug!("worker found: {:?}", worker.id);
            Some(worker)
        }
        Err(err) => {
            log::warn!("worker not found: {:?}", err);
            None
        }
    }
}

pub async fn get_worker_id_from_domain(db: &Database, domain: String) -> Option<String> {
    let query = sqlx::query_scalar!(
        "SELECT worker_id::text FROM domains WHERE name = $1 LIMIT 1",
        domain
    );

    match query.fetch_one(db).await {
        Ok(worker_id) => worker_id,
        Err(err) => {
            log::warn!("failed to get worker id from domain: {:?}", err);
            None
        }
    }
}
