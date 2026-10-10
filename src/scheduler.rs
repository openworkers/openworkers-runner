//! Starts the scheduled event of each cron when its next run comes.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::event_scheduled::ScheduledData;
use crate::log::LogSink;

/// Crons can have a seconds field, so the scheduler looks every second.
const TICK: Duration = Duration::from_secs(1);

/// Due crons claimed in one tick; the others wait for the next tick.
const CRONS_PER_TICK: i64 = 100;

#[derive(FromRow)]
struct DueCron {
    id: Uuid,
    worker_id: Uuid,
    value: String,
    next_run: DateTime<Utc>,
}

/// The pattern syntax of the API and of openworkers-scheduler. `sloppy_ranges`
/// keeps `/7` and `0/7`, which crons in the database use.
fn parser() -> croner::parser::CronParser {
    croner::parser::CronParser::builder()
        .seconds(croner::parser::Seconds::Optional)
        .dom_and_dow(true)
        .sloppy_ranges(true)
        .build()
}

/// The first run of `pattern` after `now`, in UTC, or None when the pattern
/// is not valid or never runs.
pub fn next_run(pattern: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    parser()
        .parse(pattern)
        .and_then(|schedule| schedule.find_next_occurrence(&now, false))
        .ok()
}

pub async fn run(db: PgPool, worker_db: PgPool, log_sink: LogSink, stop: CancellationToken) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }

        if crate::worker_pool::is_draining() {
            return;
        }

        let due = match claim_due(&db, Utc::now()).await {
            Ok(due) => due,
            Err(error) => {
                tracing::error!(%error, "failed to claim the due crons");
                continue;
            }
        };

        for data in due {
            crate::event_scheduled::dispatch(data, db.clone(), worker_db.clone(), log_sink.clone())
                .await;
        }
    }
}

/// Claims the crons due at `now`. For each cron, one transaction moves
/// `next_run` to the first run after `now` and records the scheduled event.
/// A cron that is late runs once. A cron that another claim moved first is
/// skipped. A pattern that is not valid gets a NULL `next_run`, so it stops
/// until the API writes the cron again.
///
/// The event is recorded before it runs: a stop between the two skips that
/// run.
pub async fn claim_due(db: &PgPool, now: DateTime<Utc>) -> Result<Vec<ScheduledData>, sqlx::Error> {
    let crons: Vec<DueCron> = sqlx::query_as(
        "SELECT id, worker_id, value, next_run FROM crons \
         WHERE next_run <= $1 AND deleted_at IS NULL \
         ORDER BY next_run LIMIT $2",
    )
    .bind(now)
    .bind(CRONS_PER_TICK)
    .fetch_all(db)
    .await?;

    let mut due = Vec::with_capacity(crons.len());

    for cron in crons {
        let Some(next) = next_run(&cron.value, now) else {
            tracing::error!(cron = %cron.id, pattern = %cron.value, "cron pattern is not valid; the cron stops");

            sqlx::query("UPDATE crons SET next_run = NULL WHERE id = $1 AND next_run = $2")
                .bind(cron.id)
                .bind(cron.next_run)
                .execute(db)
                .await?;

            continue;
        };

        let mut tx = db.begin().await?;

        let claimed = sqlx::query(
            "UPDATE crons SET next_run = $1, last_run = $2 \
             WHERE id = $3 AND next_run = $2 AND deleted_at IS NULL",
        )
        .bind(next)
        .bind(cron.next_run)
        .bind(cron.id)
        .execute(&mut *tx)
        .await?;

        if claimed.rows_affected() == 0 {
            continue;
        }

        let event_id = Uuid::new_v4();

        sqlx::query(
            "INSERT INTO scheduled_events (id, cron_id, worker_id, executed_at, scheduled_at) \
             VALUES ($1, $2, $3, NOW(), $4)",
        )
        .bind(event_id)
        .bind(cron.id)
        .bind(cron.worker_id)
        .bind(cron.next_run)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        due.push(ScheduledData {
            id: event_id.to_string(),
            cron: cron.value,
            scheduled_time: cron.next_run.timestamp_millis().max(0) as u64,
            worker_id: cron.worker_id.to_string(),
        });
    }

    Ok(due)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 10, hour, minute, second)
            .unwrap()
    }

    #[test]
    fn patterns_in_the_database_still_parse() {
        for pattern in [
            "0 * * * *",
            "0 0 * * * *",
            "* * * * * *",
            "/7 * * * * *",
            "*/7 * * * * *",
            "0/7 * * * * *",
            "1/7 * * * * *",
            "0/7 * * * *",
            "*/5 * * * *",
        ] {
            assert!(next_run(pattern, at(12, 0, 0)).is_some(), "{pattern}");
        }
    }

    #[test]
    fn the_next_run_comes_strictly_after_now() {
        assert_eq!(next_run("* * * * *", at(12, 0, 0)), Some(at(12, 1, 0)));
        assert_eq!(next_run("* * * * * *", at(12, 0, 0)), Some(at(12, 0, 1)));
        assert_eq!(
            next_run("30 12 * * *", at(12, 30, 0)),
            Some(at(12, 30, 0) + chrono::Duration::days(1))
        );
    }

    #[test]
    fn a_pattern_that_is_not_valid_has_no_next_run() {
        for pattern in ["", "nonsense", "61 * * * *", "* * * * * * * *"] {
            assert_eq!(next_run(pattern, at(12, 0, 0)), None, "{pattern:?}");
        }
    }
}
