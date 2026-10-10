use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(FromRow)]
struct Cron {
    id: Uuid,
    worker_id: Uuid,
    value: String,
    next_run: DateTime<Utc>,
}

fn parser() -> croner::parser::CronParser {
    croner::parser::CronParser::builder()
        .seconds(croner::parser::Seconds::Optional)
        .dom_and_dow(true)
        .sloppy_ranges(true)
        .build()
}

pub async fn run(
    db: PgPool,
    worker_db: PgPool,
    logs: std::sync::mpsc::Sender<crate::log::LogMessage>,
    stop: CancellationToken,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        if crate::worker_pool::is_draining() {
            return;
        }
        if let Err(error) = tick_once(&db, &worker_db, &logs).await {
            tracing::error!(%error, "cron scheduling failed; retrying next tick");
        }
    }
}

async fn tick_once(
    db: &PgPool,
    worker_db: &PgPool,
    logs: &std::sync::mpsc::Sender<crate::log::LogMessage>,
) -> Result<(), sqlx::Error> {
    // Bound each pass; a bad cron must not prevent other crons from running.
    let crons = sqlx::query_as::<_, Cron>(
        "SELECT id, worker_id, value, next_run FROM crons WHERE next_run <= NOW() \
         AND deleted_at IS NULL ORDER BY next_run LIMIT 100",
    )
    .fetch_all(db)
    .await?;
    for cron in crons {
        if crate::worker_pool::is_draining() {
            break;
        }
        let next = parser()
            .parse(&cron.value)
            .and_then(|schedule| schedule.find_next_occurrence(&Utc::now(), false));
        let next = match next {
            Ok(next) => next,
            Err(error) => {
                tracing::error!(cron = %cron.id, %error, "invalid cron; disabling until edited");
                sqlx::query("UPDATE crons SET next_run = NULL WHERE id = $1 AND next_run = $2")
                    .bind(cron.id)
                    .bind(cron.next_run)
                    .execute(db)
                    .await?;
                continue;
            }
        };
        let mut tx = db.begin().await?;
        let changed = sqlx::query(
            "UPDATE crons SET next_run = $1, last_run = $2 WHERE id = $3 \
             AND next_run = $2 AND deleted_at IS NULL",
        )
        .bind(next)
        .bind(cron.next_run)
        .bind(cron.id)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() == 0 {
            continue;
        }
        let event_id = Uuid::new_v4();
        sqlx::query("INSERT INTO scheduled_events (id, cron_id, worker_id, executed_at, scheduled_at) VALUES ($1,$2,$3,NOW(),$4)")
            .bind(event_id).bind(cron.id).bind(cron.worker_id).bind(cron.next_run)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        // A crash after commit can skip this occurrence.
        crate::event_scheduled::dispatch(
            crate::event_scheduled::ScheduledData {
                id: event_id.to_string(),
                cron: cron.value,
                scheduled_time: cron.next_run.timestamp_millis().max(0) as u64,
                worker_id: cron.worker_id.to_string(),
            },
            db.clone(),
            worker_db.clone(),
            logs.clone(),
        )
        .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    #[test]
    fn legacy_step_patterns_and_next_tick_are_preserved() {
        for pattern in ["/7 * * * * *", "0/7 * * * *", "*/5 * * * * *"] {
            assert!(parser().parse(pattern).is_ok());
        }
        let now = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        assert_eq!(
            parser()
                .parse("* * * * *")
                .unwrap()
                .find_next_occurrence(&now, false)
                .unwrap(),
            now + chrono::Duration::minutes(1)
        );
    }
}
