//! Durable Object alarms. The alarm of an object is a row of its SQLite
//! file; `durable_object_alarms` in PostgreSQL is the index of due alarms.
//!
//! The index is never later than the file, except while a failed alarm
//! waits for its next run: it goes down before the file changes, and
//! follows the file after. A crash between the two makes an alarm early,
//! never late or lost, and the runner reads the file when the time comes.
//! The index writes of one object hold its lock, so a setAlarm and the end
//! of a delivery do not interleave.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use openworkers_core::{ObjectStorageOp, ObjectStorageResult};
use sqlx::PgPool;

/// Failed runs of an alarm before it is dropped, as on Cloudflare
pub const MAX_RETRIES: u32 = 6;

/// A new alarm, which can be earlier than the one the loop waits for
static CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();

type ObjectRef = (String, String);

static LOCKS: Lazy<Mutex<HashMap<ObjectRef, Arc<tokio::sync::Mutex<()>>>>> =
    Lazy::new(Default::default);

/// Runs `work` with the alarm lock of the object.
async fn locked<T>(namespace_id: &str, object_id: &str, work: impl Future<Output = T>) -> T {
    let key = (namespace_id.to_string(), object_id.to_string());
    let lock = Arc::clone(LOCKS.lock().unwrap().entry(key.clone()).or_default());
    let guard = lock.lock().await;
    let result = work.await;
    drop(guard);

    let mut locks = LOCKS.lock().unwrap();

    // The map and this task hold the only references: nobody waits
    if Arc::strong_count(&lock) == 2 {
        locks.remove(&key);
    }

    result
}

/// Moves the index to `at` or earlier, before the file changes to `at`.
async fn lower(db: &PgPool, namespace_id: &str, object_id: &str, at: i64) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO durable_object_alarms (namespace_id, object_id, scheduled_at) \
         VALUES ($1::uuid, $2, to_timestamp($3::float8 / 1000)) \
         ON CONFLICT (namespace_id, object_id) DO UPDATE \
         SET scheduled_at = LEAST(durable_object_alarms.scheduled_at, excluded.scheduled_at)",
    )
    .bind(namespace_id)
    .bind(object_id)
    .bind(at)
    .execute(db)
    .await?;

    Ok(())
}

/// Puts the index at the alarm of the file, with its count of failed runs,
/// and wakes the loop for it. A delivery under way stays marked: it sets the
/// index when it ends.
async fn follow(
    db: &PgPool,
    namespace_id: &str,
    object_id: &str,
    alarm: Option<(i64, u32)>,
) -> sqlx::Result<()> {
    match alarm {
        Some((at, retry_count)) => sqlx::query(
            "INSERT INTO durable_object_alarms (namespace_id, object_id, scheduled_at, retry_count) \
             VALUES ($1::uuid, $2, to_timestamp($3::float8 / 1000), $4) \
             ON CONFLICT (namespace_id, object_id) DO UPDATE \
             SET scheduled_at = excluded.scheduled_at, retry_count = excluded.retry_count",
        )
        .bind(namespace_id)
        .bind(object_id)
        .bind(at)
        .bind(retry_count as i32),
        None => sqlx::query(
            "DELETE FROM durable_object_alarms WHERE namespace_id = $1::uuid AND object_id = $2",
        )
        .bind(namespace_id)
        .bind(object_id),
    }
    .execute(db)
    .await?;

    CHANGED.notify_one();

    Ok(())
}

/// Sets the alarm of an object to `at`, or deletes it, in its file and in
/// the index.
pub async fn set(
    service: &crate::d1::Service,
    db: &PgPool,
    namespace_id: &str,
    object_id: &str,
    at: Option<i64>,
) -> ObjectStorageResult {
    locked(namespace_id, object_id, async {
        if let Some(at) = at
            && let Err(err) = lower(db, namespace_id, object_id, at).await
        {
            tracing::warn!("durable object alarm: cannot index {object_id}: {err}");

            return ObjectStorageResult::Error("the alarm cannot be set now".to_string());
        }

        let result = service
            .object_storage(namespace_id, object_id, ObjectStorageOp::SetAlarm(at))
            .await;

        if !matches!(result, ObjectStorageResult::Alarm(_)) {
            return result;
        }

        // The index is already at the alarm or earlier; a failure here only
        // makes the delivery read the file early
        let alarm = at.map(|at| (at, 0));

        if let Err(err) = follow(db, namespace_id, object_id, alarm).await {
            tracing::warn!("durable object alarm: cannot index {object_id}: {err}");
        }

        result
    })
    .await
}

/// An alarm the loop took from the index.
#[derive(Debug, PartialEq, sqlx::FromRow)]
pub struct Due {
    pub namespace_id: String,
    pub object_id: String,
    pub retry_count: i32,
}

/// Marks the due alarms as under way, and answers them.
pub async fn claim(db: &PgPool, limit: i64) -> sqlx::Result<Vec<Due>> {
    sqlx::query_as(
        "UPDATE durable_object_alarms SET delivering = true \
         WHERE (namespace_id, object_id) IN ( \
             SELECT namespace_id, object_id FROM durable_object_alarms \
             WHERE NOT delivering AND scheduled_at <= now() \
             ORDER BY scheduled_at LIMIT $1) \
         RETURNING namespace_id::text, object_id::text, retry_count",
    )
    .bind(limit)
    .fetch_all(db)
    .await
}

/// When a failed run of an alarm runs again, after `retry_count` failures:
/// 2, 4, 8... seconds later.
pub fn retry_at(now_ms: i64, retry_count: u32) -> i64 {
    now_ms + 1000 * (1 << retry_count.min(MAX_RETRIES)) as i64
}

/// What happens to the alarm of an object after a run.
pub enum Next {
    /// Delete the alarm `at` from the file if it is still there
    Clear,
    /// Keep the alarm of the file, with this count of failed runs
    Keep { at: i64, retry_count: u32 },
}

/// What follows the run of the alarm `at`, from the alarm of the file after
/// the run (`current`) and the failed runs so far, this one included. A
/// handler that set another alarm keeps it; a failed alarm runs again later,
/// and after MAX_RETRIES failed runs again it is dropped.
pub fn next(at: i64, current: Option<i64>, failed_runs: Option<u32>, now_ms: i64) -> Next {
    match (failed_runs, current) {
        (_, Some(current)) if current != at => Next::Keep {
            at: current,
            retry_count: 0,
        },
        (Some(failed), Some(_)) if failed <= MAX_RETRIES => Next::Keep {
            at: retry_at(now_ms, failed),
            retry_count: failed,
        },
        _ => Next::Clear,
    }
}

#[cfg(feature = "v8")]
pub use delivery::run;

#[cfg(feature = "v8")]
mod delivery {
    use super::*;

    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use crate::log::{LogSender, LogSink};

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as i64
    }

    /// Ends a delivery: the index follows the file, with `retry_count` for an
    /// alarm that failed, and is no longer under way.
    async fn settle(
        db: &PgPool,
        namespace_id: &str,
        object_id: &str,
        alarm: Option<(i64, u32)>,
    ) -> sqlx::Result<()> {
        match alarm {
            Some((at, retry_count)) => sqlx::query(
                "INSERT INTO durable_object_alarms (namespace_id, object_id, scheduled_at, retry_count) \
                 VALUES ($1::uuid, $2, to_timestamp($3::float8 / 1000), $4) \
                 ON CONFLICT (namespace_id, object_id) DO UPDATE \
                 SET scheduled_at = excluded.scheduled_at, retry_count = excluded.retry_count, \
                     delivering = false",
            )
            .bind(namespace_id)
            .bind(object_id)
            .bind(at)
            .bind(retry_count as i32),
            None => sqlx::query(
                "DELETE FROM durable_object_alarms WHERE namespace_id = $1::uuid AND object_id = $2",
            )
            .bind(namespace_id)
            .bind(object_id),
        }
        .execute(db)
        .await?;

        CHANGED.notify_one();

        Ok(())
    }

    /// The longest an alarm handler runs before the run counts as failed
    const ALARM_TIMEOUT: Duration = Duration::from_secs(15 * 60);

    /// Alarms taken per round
    const BATCH: i64 = 100;

    /// The loop also wakes this often, for an alarm that another process
    /// put in the index
    const MAX_WAIT: Duration = Duration::from_secs(60);

    /// Runs the alarms as they fall due, until `stop`.
    pub async fn run(db: PgPool, worker_db: PgPool, log_sink: LogSink, stop: CancellationToken) {
        // A runner that stopped during a run left it marked; it runs again
        if let Err(err) =
            sqlx::query("UPDATE durable_object_alarms SET delivering = false WHERE delivering")
                .execute(&db)
                .await
        {
            tracing::warn!("durable object alarms are off: {err}");
            return;
        }

        loop {
            if crate::worker_pool::is_draining() {
                return;
            }

            match claim(&db, BATCH).await {
                Ok(due) => {
                    for due in due {
                        let db = db.clone();
                        let worker_db = worker_db.clone();
                        let log_sink = log_sink.clone();

                        tokio::spawn(deliver(db, worker_db, log_sink, due));
                    }
                }
                Err(err) => tracing::warn!("durable object alarms: cannot claim: {err}"),
            }

            let next: Option<Option<chrono::DateTime<chrono::Utc>>> = sqlx::query_scalar(
                "SELECT min(scheduled_at) FROM durable_object_alarms WHERE NOT delivering",
            )
            .fetch_optional(&db)
            .await
            .unwrap_or_default();

            let wait = next
                .flatten()
                .map(|next| (next - chrono::Utc::now()).to_std().unwrap_or_default())
                .unwrap_or(MAX_WAIT)
                .min(MAX_WAIT);

            tokio::select! {
                _ = stop.cancelled() => return,
                _ = CHANGED.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    async fn deliver(db: PgPool, worker_db: PgPool, log_sink: LogSink, due: Due) {
        let Due {
            namespace_id,
            object_id,
            retry_count,
        } = due;
        let retry_count = retry_count.max(0) as u32;

        let namespace = match db.acquire().await {
            Ok(mut conn) => {
                crate::store::fetch_durable_object_namespace(&mut conn, &namespace_id, None).await
            }
            Err(err) => Err(err),
        };

        let (Ok(namespace), Some(service)) = (namespace, crate::d1::service()) else {
            settle_later(&db, &namespace_id, &object_id, retry_count).await;
            return;
        };

        // A deleted namespace, or one whose worker is gone, runs no alarm
        let Some(namespace) = namespace else {
            locked(&namespace_id, &object_id, async {
                settle(&db, &namespace_id, &object_id, None).await.ok();
            })
            .await;
            return;
        };

        let at = match service
            .object_storage(&namespace_id, &object_id, ObjectStorageOp::GetAlarm)
            .await
        {
            ObjectStorageResult::Alarm(at) => at,
            other => {
                tracing::warn!("durable object alarm: cannot read {object_id}: {other:?}");
                settle_later(&db, &namespace_id, &object_id, retry_count).await;
                return;
            }
        };

        // The index can be early, never late: the file has the alarm
        let at = match at {
            Some(at) if at <= now_ms() => at,
            later => {
                let alarm = later.map(|at| (at, retry_count));

                locked(&namespace_id, &object_id, async {
                    settle(&db, &namespace_id, &object_id, alarm).await.ok();
                })
                .await;
                return;
            }
        };

        let load = {
            let namespace = namespace.clone();
            let object_id = object_id.clone();
            let log_tx = LogSender::new(namespace.worker_id.clone(), log_sink.clone());

            move |depths| {
                crate::durable::load(namespace, object_id, depths, Some(worker_db), Some(log_tx))
            }
        };

        let outcome = match tokio::time::timeout(
            ALARM_TIMEOUT,
            crate::durable::alarm(&namespace, object_id.clone(), retry_count, load),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => Err("the alarm handler did not end in time".to_string()),
        };

        if let Err(error) = &outcome {
            let message = format!(
                "Durable Object {} {object_id}: alarm failed (run {} of {}): {error}",
                namespace.class_name,
                retry_count + 1,
                MAX_RETRIES + 1
            );

            LogSender::new(namespace.worker_id.clone(), log_sink).send(
                openworkers_core::LogEvent {
                    level: openworkers_core::LogLevel::Error,
                    message,
                },
            );
        }

        locked(&namespace_id, &object_id, async {
            let current = match service
                .object_storage(&namespace_id, &object_id, ObjectStorageOp::GetAlarm)
                .await
            {
                ObjectStorageResult::Alarm(current) => current,
                _ => Some(at),
            };
            let failed_runs = outcome.is_err().then_some(retry_count + 1);

            let alarm = match next(at, current, failed_runs, now_ms()) {
                Next::Keep { at, retry_count } => Some((at, retry_count)),
                Next::Clear => match service.clear_alarm(&namespace_id, &object_id, at).await {
                    ObjectStorageResult::Alarm(remaining) => remaining.map(|at| (at, 0)),
                    // The file keeps the alarm, which runs again later
                    _ => Some((retry_at(now_ms(), retry_count), retry_count)),
                },
            };

            if let Err(err) = settle(&db, &namespace_id, &object_id, alarm).await {
                tracing::warn!("durable object alarm: cannot index {object_id}: {err}");
            }
        })
        .await;
    }

    /// A delivery that could not start: it runs again a little later, with
    /// the same count of failed runs.
    async fn settle_later(db: &PgPool, namespace_id: &str, object_id: &str, retry_count: u32) {
        let alarm = Some((retry_at(now_ms(), 0), retry_count));

        locked(namespace_id, object_id, async {
            settle(db, namespace_id, object_id, alarm).await.ok();
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_alarm_runs_again_later_then_is_dropped() {
        let now = 1_000_000;

        for failed in 1..=MAX_RETRIES {
            match next(500, Some(500), Some(failed), now) {
                Next::Keep { at, retry_count } => {
                    assert_eq!(at, now + 1000 * (1 << failed));
                    assert_eq!(retry_count, failed);
                }
                Next::Clear => panic!("run {failed} is retried"),
            }
        }

        assert!(matches!(
            next(500, Some(500), Some(MAX_RETRIES + 1), now),
            Next::Clear
        ));
    }

    #[test]
    fn a_handler_that_set_another_alarm_keeps_it() {
        for failed_runs in [None, Some(1), Some(MAX_RETRIES + 1)] {
            assert!(matches!(
                next(500, Some(900), failed_runs, 0),
                Next::Keep {
                    at: 900,
                    retry_count: 0
                }
            ));
        }

        // A run that ended well, or a handler that deleted the alarm
        assert!(matches!(next(500, Some(500), None, 0), Next::Clear));
        assert!(matches!(next(500, None, Some(1), 0), Next::Clear));
    }
}
