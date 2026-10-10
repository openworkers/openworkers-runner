//! One runner per platform database. The lock is held on a dedicated session.
use sqlx::{Connection, PgConnection};

pub async fn acquire(url: &str) -> Result<PgConnection, Box<dyn std::error::Error + Send + Sync>> {
    let mut connection = PgConnection::connect(url).await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(1869636978, 1)")
        .fetch_one(&mut connection)
        .await?;
    if !acquired {
        return Err("another OpenWorkers runner already owns this database; stop it before starting this runner".into());
    }
    Ok(connection)
}

// A new connection would not hold the lock.
pub async fn monitor(mut connection: PgConnection) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            sqlx::query("SELECT 1").execute(&mut connection),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            tracing::error!("singleton lock session lost; stopping the runner");
            std::process::exit(1);
        }
    }
}
