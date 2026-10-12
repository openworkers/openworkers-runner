//! Management requests from `d1_requests`: `ow d1 execute` and migrations.
//!
//! A client inserts a request; a trigger notifies `d1_request`. The runner
//! runs it on the database and writes the result into the same row, which
//! notifies `d1_result`. The client deletes the row once it has the result.
//!
//! Request JSON: `{"type": "op", "op": <D1Op>}`,
//! `{"type": "migrate", "name": "...", "sql": "...", "checksum": "..."}` or
//! `{"type": "restore"}`. The result is a `D1Result`.

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
    Op {
        op: D1Op,
    },
    Migrate(Migration),
    /// Replace the local file with the latest snapshot
    Restore,
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
        Ok(Request::Restore) => match service.reset_to_snapshot(database_id) {
            Ok(()) => D1Result::Exec {
                count: 0,
                duration: 0.0,
            },
            Err(err) => failure(D1ErrorKind::Unavailable, err),
        },
        Err(err) => failure(D1ErrorKind::Type, format!("bad request: {err}")),
    }
}

/// Removes the files and snapshots of deleted databases and Durable Object
/// namespaces, and requests nobody collected
async fn clean_up(service: &Service, listener: &mut PgListener) -> Result<(), sqlx::Error> {
    // Rows that triggers queued when a database or a snapshot row went away,
    // including through the cascade of a deleted user
    let purge: Vec<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, database_id::text, key FROM d1_purge ORDER BY id LIMIT 1000")
            .fetch_all(&mut *listener)
            .await?;

    for (row, database_id, key) in purge {
        let done = match &key {
            None => service.remove_files(&database_id),
            Some(key) => match service.remove_snapshot_object(key).await {
                Ok(()) => true,
                Err(err) => {
                    tracing::warn!("D1: cannot remove snapshot {key}: {err}");
                    false
                }
            },
        };

        if done {
            sqlx::query("DELETE FROM d1_purge WHERE id = $1")
                .bind(row)
                .execute(&mut *listener)
                .await?;
        }
    }

    let deleted: Vec<String> =
        sqlx::query_scalar("SELECT id::text FROM d1_databases WHERE deleted_at IS NOT NULL")
            .fetch_all(&mut *listener)
            .await?;

    for id in deleted {
        service.remove_files(&id);

        if let Err(err) = service.forget_snapshots(&id).await {
            tracing::warn!("D1: cannot remove the snapshots of deleted database {id}: {err}");
        }
    }

    sqlx::query("DELETE FROM d1_requests WHERE created_at < now() - interval '1 hour'")
        .execute(&mut *listener)
        .await?;

    // A runner can run before the migrations of Durable Objects
    if let Err(err) = clean_up_namespaces(service, listener).await {
        tracing::warn!("D1: cannot clean up Durable Object namespaces: {err}");
    }

    Ok(())
}

/// The same for Durable Object namespaces. Their objects stop first, so none
/// writes a file again after its removal.
async fn clean_up_namespaces(
    service: &Service,
    listener: &mut PgListener,
) -> Result<(), sqlx::Error> {
    let purge: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, namespace_id::text, key FROM durable_object_purge ORDER BY id LIMIT 1000",
    )
    .fetch_all(&mut *listener)
    .await?;

    for (row, namespace_id, key) in purge {
        let done = match &key {
            None => {
                crate::durable::stop_namespace(&namespace_id)
                    && service.remove_namespace_files(&namespace_id)
            }
            Some(key) => match service.remove_snapshot_object(key).await {
                Ok(()) => true,
                Err(err) => {
                    tracing::warn!("D1: cannot remove snapshot {key}: {err}");
                    false
                }
            },
        };

        if done {
            sqlx::query("DELETE FROM durable_object_purge WHERE id = $1")
                .bind(row)
                .execute(&mut *listener)
                .await?;
        }
    }

    let deleted: Vec<String> = sqlx::query_scalar(
        "SELECT id::text FROM durable_object_namespaces WHERE deleted_at IS NOT NULL",
    )
    .fetch_all(&mut *listener)
    .await?;

    for namespace_id in deleted {
        if crate::durable::stop_namespace(&namespace_id) {
            service.remove_namespace_files(&namespace_id);
        }
    }

    // The trigger queues the snapshot objects in durable_object_purge
    sqlx::query(
        "DELETE FROM durable_object_archives WHERE namespace_id IN \
         (SELECT id FROM durable_object_namespaces WHERE deleted_at IS NOT NULL)",
    )
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
