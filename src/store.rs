use sqlx::prelude::FromRow;

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

#[derive(Debug, FromRow)]
pub struct WorkerData {
    pub id: String,
    pub name: String,
    pub env: Option<sqlx::types::Json<std::collections::HashMap<String, String>>>,
    pub script: String,
    pub checksum: i64,
    pub language: WorkerLanguage,
}

pub async fn get_worker(conn: &mut sqlx::PgConnection, identifier: WorkerIdentifier) -> Option<WorkerData> {
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
            log::debug!("worker found: id: {}, checksum: {}, language: {:?}", worker.id, worker.checksum, worker.language);
            Some(worker)
        }
        Err(err) => {
            log::warn!("worker not found: {:?}", err);
            None
        }
    }
}

pub async fn get_worker_id_from_domain(conn: &mut sqlx::PgConnection, domain: String) -> Option<String> {
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
