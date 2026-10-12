//! Snapshots of D1 databases in object storage, and their index.
//!
//! A snapshot is a `VACUUM INTO` copy of the database, compressed with zstd.
//! It is uploaded, its size is checked with a HEAD request, and only then a
//! row in `d1_archives` makes it visible to a restore. A failure at any step
//! leaves the previous snapshot as the latest one.

use std::path::Path;
use std::time::Duration;

use futures::future::BoxFuture;
use openworkers_core::StorageOp;
use openworkers_core::StorageResult;
use sha2::Digest;
use sha2::Sha256;

use crate::services::storage::execute_s3_operation;
use crate::store::StorageConfig;

#[derive(Debug, Clone)]
pub struct ArchiveConfig {
    pub storage: StorageConfig,
    /// Time between two snapshots of a changed database
    pub interval: Duration,
    /// Log an error when the oldest unsaved write is this old
    pub alert_after: Duration,
    /// Refuse writes when the oldest unsaved write is this old
    pub pause_after: Duration,
    /// Verified snapshots kept per database
    pub keep: usize,
}

impl ArchiveConfig {
    /// `None` when `D1_ARCHIVE_ENDPOINT` is not set: no snapshots.
    pub fn from_env() -> Option<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let secs = |name: &str, default: u64| {
            Duration::from_secs(var(name).and_then(|v| v.parse().ok()).unwrap_or(default))
        };

        Some(Self {
            storage: StorageConfig {
                id: "d1-archive".to_string(),
                endpoint: var("D1_ARCHIVE_ENDPOINT")?
                    .trim_end_matches('/')
                    .to_string(),
                bucket: var("D1_ARCHIVE_BUCKET")?,
                prefix: var("D1_ARCHIVE_PREFIX"),
                access_key_id: var("D1_ARCHIVE_ACCESS_KEY_ID")?,
                secret_access_key: var("D1_ARCHIVE_SECRET_ACCESS_KEY")?,
                region: var("D1_ARCHIVE_REGION"),
                public_url: None,
            },
            interval: secs("D1_SNAPSHOT_SECS", 300),
            alert_after: secs("D1_SNAPSHOT_ALERT_SECS", 15 * 60),
            pause_after: secs("D1_SNAPSHOT_PAUSE_SECS", 60 * 60),
            keep: 3,
        })
    }
}

/// One verified snapshot, as `d1_archives` records it
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub database_id: String,
    pub seq: i64,
    pub epoch: i64,
    pub key: String,
    /// SHA-256 of the stored (compressed) object, hex
    pub checksum: String,
    pub size: u64,
}

/// Object storage for snapshot files
pub trait Store: Send + Sync {
    fn put(&self, key: String, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), String>>;
    fn get(&self, key: String) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>>;
    /// The size of a stored object, or `None` when it does not exist
    fn size(&self, key: String) -> BoxFuture<'_, Result<Option<u64>, String>>;
    fn delete(&self, key: String) -> BoxFuture<'_, Result<(), String>>;
}

/// The index of verified snapshots
pub trait Index: Send + Sync {
    fn epoch(&self) -> i64;
    /// Records a snapshot; fails when a newer epoch exists
    fn record(&self, snapshot: Snapshot) -> BoxFuture<'_, Result<(), String>>;
    /// Every snapshot of a database, newest first
    fn list(&self, database_id: String) -> BoxFuture<'_, Result<Vec<Snapshot>, String>>;
    fn forget(&self, database_id: String, seq: i64) -> BoxFuture<'_, Result<(), String>>;
}

pub struct Archive {
    pub config: ArchiveConfig,
    pub store: Box<dyn Store>,
    pub index: Box<dyn Index>,
}

impl Archive {
    /// Uploads a snapshot, checks it, records it, then drops the oldest ones
    /// beyond `keep`: their rows first, so a restore never reads a deleted
    /// object.
    pub async fn save(&self, database_id: &str, bytes: Vec<u8>) -> Result<Snapshot, String> {
        let epoch = self.index.epoch();
        let seq = chrono::Utc::now().timestamp_millis();
        let key = match super::parse_object_key(database_id) {
            Some((namespace, object)) => {
                format!("do/{namespace}/{object}/{epoch}/{seq}.sqlite.zst")
            }
            None => format!("d1/{database_id}/{epoch}/{seq}.sqlite.zst"),
        };
        let checksum = hex::encode(Sha256::digest(&bytes));
        let size = bytes.len() as u64;

        self.store.put(key.clone(), bytes).await?;

        match self.store.size(key.clone()).await? {
            Some(stored) if stored == size => {}
            other => return Err(format!("{key}: stored size {other:?}, expected {size}")),
        }

        let snapshot = Snapshot {
            database_id: database_id.to_string(),
            seq,
            epoch,
            key,
            checksum,
            size,
        };

        self.index.record(snapshot.clone()).await?;

        let snapshots = self.index.list(database_id.to_string()).await?;

        for old in snapshots.into_iter().skip(self.config.keep) {
            self.index.forget(old.database_id, old.seq).await?;
            self.store.delete(old.key).await?;
        }

        Ok(snapshot)
    }

    /// The latest snapshot of a database, checked against its checksum
    pub async fn fetch_latest(&self, database_id: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(latest) = self
            .index
            .list(database_id.to_string())
            .await?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };

        let bytes = self
            .store
            .get(latest.key.clone())
            .await?
            .ok_or_else(|| format!("{}: missing from the archive", latest.key))?;

        let checksum = hex::encode(Sha256::digest(&bytes));

        if checksum != latest.checksum {
            return Err(format!("{}: checksum mismatch", latest.key));
        }

        Ok(Some(bytes))
    }

    /// Removes every snapshot of a deleted database
    pub async fn forget_all(&self, database_id: &str) -> Result<(), String> {
        for snapshot in self.index.list(database_id.to_string()).await? {
            self.index
                .forget(snapshot.database_id, snapshot.seq)
                .await?;
            self.store.delete(snapshot.key).await?;
        }

        Ok(())
    }
}

/// Copies the database at `path` into a compressed snapshot. Opens a
/// read-only connection, so it runs beside the writer: WAL gives it a
/// consistent view without stopping the queue.
pub fn take_snapshot(path: &Path, scratch: &Path) -> Result<Vec<u8>, String> {
    let _ = std::fs::remove_file(scratch);

    let result = (|| {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("open: {e}"))?;

        conn.execute("VACUUM INTO ?1", [scratch.to_string_lossy()])
            .map_err(|e| format!("VACUUM INTO: {e}"))?;

        let copy = std::fs::read(scratch).map_err(|e| format!("read: {e}"))?;

        zstd::encode_all(copy.as_slice(), 3).map_err(|e| format!("compress: {e}"))
    })();

    let _ = std::fs::remove_file(scratch);

    result
}

/// Writes a snapshot to `path` after checking it: it decompresses, and
/// SQLite reports no corruption. The file appears only when it is whole.
pub fn restore_snapshot(bytes: &[u8], path: &Path, scratch: &Path) -> Result<(), String> {
    let copy = zstd::decode_all(bytes).map_err(|e| format!("decompress: {e}"))?;

    std::fs::write(scratch, copy).map_err(|e| format!("write: {e}"))?;

    let check = (|| {
        let conn = rusqlite::Connection::open_with_flags(
            scratch,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| format!("open: {e}"))?;

        let result: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|e| format!("integrity_check: {e}"))?;

        if result == "ok" {
            Ok(())
        } else {
            Err(format!("integrity_check: {result}"))
        }
    })();

    if let Err(err) = check {
        let _ = std::fs::remove_file(scratch);
        return Err(err);
    }

    std::fs::rename(scratch, path).map_err(|e| format!("rename: {e}"))
}

/// Snapshot files in the S3 bucket of `D1_ARCHIVE_*`
pub struct S3Store {
    pub config: StorageConfig,
}

impl Store for S3Store {
    fn put(&self, key: String, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            match execute_s3_operation(&self.config, StorageOp::Put { key, body: bytes }).await {
                StorageResult::Error(err) => Err(err),
                _ => Ok(()),
            }
        })
    }

    fn get(&self, key: String) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            match execute_s3_operation(&self.config, StorageOp::Get { key }).await {
                StorageResult::Body(body) => Ok(body),
                StorageResult::Error(err) => Err(err),
                _ => Err("unexpected S3 result".to_string()),
            }
        })
    }

    fn size(&self, key: String) -> BoxFuture<'_, Result<Option<u64>, String>> {
        Box::pin(async move {
            match execute_s3_operation(&self.config, StorageOp::Head { key }).await {
                StorageResult::Head { size, .. } => Ok(Some(size)),
                StorageResult::Error(err) if err == "Object not found" => Ok(None),
                StorageResult::Error(err) => Err(err),
                _ => Err("unexpected S3 result".to_string()),
            }
        })
    }

    fn delete(&self, key: String) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            match execute_s3_operation(&self.config, StorageOp::Delete { key }).await {
                StorageResult::Error(err) => Err(err),
                _ => Ok(()),
            }
        })
    }
}

/// `d1_archives`, with the epoch this runner took at start
pub struct PgIndex {
    pool: sqlx::PgPool,
    epoch: i64,
}

impl PgIndex {
    /// Takes a new epoch, which fences every runner that started before
    pub async fn start(pool: sqlx::PgPool) -> Result<Self, sqlx::Error> {
        let epoch: i64 =
            sqlx::query_scalar("INSERT INTO runner_epochs DEFAULT VALUES RETURNING epoch")
                .fetch_one(&pool)
                .await?;

        Ok(Self { pool, epoch })
    }
}

impl Index for PgIndex {
    fn epoch(&self) -> i64 {
        self.epoch
    }

    fn record(&self, snapshot: Snapshot) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let query = match super::parse_object_key(&snapshot.database_id) {
                Some((namespace, object)) => sqlx::query(
                    r#"
                    INSERT INTO durable_object_archives
                        (namespace_id, object_id, seq, epoch, key, checksum, size)
                    SELECT $1::uuid, $2, $3, $4, $5, $6, $7
                    WHERE $4 = (SELECT max(epoch) FROM runner_epochs)
                    "#,
                )
                .bind(namespace.to_string())
                .bind(object.to_string()),
                None => sqlx::query(
                    r#"
                    INSERT INTO d1_archives (database_id, seq, epoch, key, checksum, size)
                    SELECT $1::uuid, $2, $3, $4, $5, $6
                    WHERE $3 = (SELECT max(epoch) FROM runner_epochs)
                    "#,
                )
                .bind(snapshot.database_id.clone()),
            };

            let inserted = query
                .bind(snapshot.seq)
                .bind(snapshot.epoch)
                .bind(snapshot.key.clone())
                .bind(snapshot.checksum.clone())
                .bind(snapshot.size as i64)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?;

            if inserted.rows_affected() == 0 {
                return Err(format!(
                    "epoch {} is no longer the latest: another runner started",
                    snapshot.epoch
                ));
            }

            Ok(())
        })
    }

    fn list(&self, database_id: String) -> BoxFuture<'_, Result<Vec<Snapshot>, String>> {
        Box::pin(async move {
            let query = match super::parse_object_key(&database_id) {
                Some((namespace, object)) => sqlx::query_as(
                    r#"
                    SELECT seq, epoch, key, checksum, size
                    FROM durable_object_archives
                    WHERE namespace_id::text = $1 AND object_id = $2
                    ORDER BY seq DESC
                    "#,
                )
                .bind(namespace.to_string())
                .bind(object.to_string()),
                None => sqlx::query_as(
                    r#"
                    SELECT seq, epoch, key, checksum, size
                    FROM d1_archives
                    WHERE database_id::text = $1
                    ORDER BY seq DESC
                    "#,
                )
                .bind(database_id.clone()),
            };

            let rows: Vec<(i64, i64, String, String, i64)> = query
                .fetch_all(&self.pool)
                .await
                .map_err(|e| e.to_string())?;

            Ok(rows
                .into_iter()
                .map(|(seq, epoch, key, checksum, size)| Snapshot {
                    database_id: database_id.clone(),
                    seq,
                    epoch,
                    key,
                    checksum,
                    size: size as u64,
                })
                .collect())
        })
    }

    fn forget(&self, database_id: String, seq: i64) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let query = match super::parse_object_key(&database_id) {
                Some((namespace, object)) => sqlx::query(
                    "DELETE FROM durable_object_archives \
                     WHERE namespace_id::text = $1 AND object_id = $2 AND seq = $3",
                )
                .bind(namespace.to_string())
                .bind(object.to_string()),
                None => {
                    sqlx::query("DELETE FROM d1_archives WHERE database_id::text = $1 AND seq = $2")
                        .bind(database_id.clone())
                }
            };

            query
                .bind(seq)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?;

            Ok(())
        })
    }
}
