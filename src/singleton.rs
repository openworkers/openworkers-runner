//! One runner per platform database: the runner holds a session-level
//! advisory lock on a connection of its own.

use sqlx::{Connection, PgConnection};
use std::time::Duration;

/// The two keys of the advisory lock: "open" in ASCII, and the runner.
pub const LOCK_KEYS: (i32, i32) = (0x6f70656e, 1);

/// How often the runner checks that the session of the lock is alive, and
/// how long a check can take.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// Takes the lock, or fails when another runner holds it.
pub async fn acquire(url: &str) -> Result<PgConnection, Box<dyn std::error::Error + Send + Sync>> {
    let mut connection = PgConnection::connect(url).await?;

    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
        .bind(LOCK_KEYS.0)
        .bind(LOCK_KEYS.1)
        .fetch_one(&mut connection)
        .await?;

    if !acquired {
        return Err("another runner holds the lock of this database; stop it first".into());
    }

    Ok(connection)
}

/// Returns when the session of the lock is gone. Then another runner can
/// take the lock, so this runner must stop.
pub async fn hold(mut connection: PgConnection) {
    loop {
        tokio::time::sleep(CHECK_INTERVAL).await;

        let check = tokio::time::timeout(
            CHECK_TIMEOUT,
            sqlx::query("SELECT 1").execute(&mut connection),
        )
        .await;

        if !matches!(check, Ok(Ok(_))) {
            return;
        }
    }
}
