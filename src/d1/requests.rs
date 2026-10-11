//! Management requests from `d1_requests`: `ow d1 execute` and migrations.
//!
//! A client inserts a request; a trigger notifies `d1_request`. The runner
//! runs it on the database and writes the result into the same row, which
//! notifies `d1_result`. The client deletes the row once it has the result.
//!
//! Request JSON: `{"type": "op", "op": <D1Op>}` or
//! `{"type": "migrate", "name": "...", "sql": "...", "checksum": "..."}`.
//! The result is a `D1Result`.

use std::time::Duration;

use openworkers_core::D1Error;
use openworkers_core::D1ErrorKind;
use openworkers_core::D1Op;
use openworkers_core::D1Result;
use sqlx::postgres::PgListener;

use super::Migration;
use super::Service;

const CHANNEL: &str = "d1_request";

/// Pending requests are also read on this tick, so a notification lost while
/// the listener reconnects only delays them.
const SWEEP: Duration = Duration::from_secs(60);

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Op { op: D1Op },
    Migrate(Migration),
}

/// Serves the requests while the process lives, on a connection of its own.
pub async fn listen(service: &'static Service, database_url: String) {
    loop {
        if let Err(err) = listen_once(service, &database_url).await {
            tracing::warn!("D1 requests: listener: {err}");
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn listen_once(service: &Service, database_url: &str) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect(database_url).await?;
    let mut sweep = tokio::time::interval(SWEEP);

    listener.listen(CHANNEL).await?;

    loop {
        let notified = tokio::select! {
            notification = listener.try_recv() => match notification? {
                Some(_) => true,
                None => return Ok(()),
            },
            _ = sweep.tick() => false,
        };

        serve_pending(service, &mut listener).await?;

        if !notified {
            clean_up(service, &mut listener).await?;
        }
    }
}

async fn serve_pending(service: &Service, listener: &mut PgListener) -> Result<(), sqlx::Error> {
    let pending: Vec<(String, String, serde_json::Value, bool)> = sqlx::query_as(
        r#"
        SELECT r.id::text, r.database_id::text, r.request, d.deleted_at IS NOT NULL
        FROM d1_requests r
        JOIN d1_databases d ON d.id = r.database_id
        WHERE r.finished_at IS NULL
        ORDER BY r.created_at
        "#,
    )
    .fetch_all(&mut *listener)
    .await?;

    for (id, database_id, request, deleted) in pending {
        let result = if deleted {
            failure(D1ErrorKind::Unavailable, "the database is deleted".into())
        } else {
            serve(service, &database_id, request).await
        };

        let result = serde_json::to_value(&result).expect("a D1Result serializes");

        sqlx::query(
            "UPDATE d1_requests SET result = $2, finished_at = now() \
             WHERE id::text = $1 AND finished_at IS NULL",
        )
        .bind(&id)
        .bind(result)
        .execute(&mut *listener)
        .await?;
    }

    Ok(())
}

async fn serve(service: &Service, database_id: &str, request: serde_json::Value) -> D1Result {
    match serde_json::from_value::<Request>(request) {
        Ok(Request::Op { op }) => service.execute(database_id, op).await,
        Ok(Request::Migrate(migration)) => service.migrate(database_id, migration).await,
        Err(err) => failure(D1ErrorKind::Type, format!("bad request: {err}")),
    }
}

/// Removes the files of deleted databases and requests nobody collected
async fn clean_up(service: &Service, listener: &mut PgListener) -> Result<(), sqlx::Error> {
    let deleted: Vec<String> =
        sqlx::query_scalar("SELECT id::text FROM d1_databases WHERE deleted_at IS NOT NULL")
            .fetch_all(&mut *listener)
            .await?;

    for id in deleted {
        service.remove_files(&id);
    }

    sqlx::query("DELETE FROM d1_requests WHERE created_at < now() - interval '1 hour'")
        .execute(&mut *listener)
        .await?;

    Ok(())
}

fn failure(kind: D1ErrorKind, message: String) -> D1Result {
    D1Result::Error(D1Error {
        kind,
        message,
        statement: None,
    })
}
