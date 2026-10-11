//! Snapshots and restores against an in-memory archive whose steps can fail.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::future::BoxFuture;
use openworkers_core::D1ErrorKind;
use openworkers_core::D1Op;
use openworkers_core::D1Result;
use openworkers_core::D1Rows;
use openworkers_core::D1Statement;

use super::Archive;
use super::Config;
use super::Limits;
use super::Service;
use super::archive::ArchiveConfig;
use super::archive::Index;
use super::archive::Snapshot;
use super::archive::Store;
use crate::store::StorageConfig;

const DB: &str = "44444444-4444-4444-8444-444444444444";

/// Failures to inject, shared between the test and the archive
#[derive(Default)]
struct Faults {
    put: AtomicBool,
    record: AtomicBool,
    /// Store a different object than the one uploaded
    corrupt: AtomicBool,
}

#[derive(Clone, Default)]
struct Memory {
    objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    rows: Arc<Mutex<Vec<Snapshot>>>,
    faults: Arc<Faults>,
    /// Epoch of the newest runner; a record from an older one is refused
    latest_epoch: Arc<Mutex<i64>>,
}

struct MemStore(Memory);
struct MemIndex(Memory, i64);

impl Store for MemStore {
    fn put(&self, key: String, mut bytes: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if self.0.faults.put.load(Ordering::SeqCst) {
                return Err("upload failed".to_string());
            }

            if self.0.faults.corrupt.load(Ordering::SeqCst) {
                let last = bytes.len() - 1;
                bytes[last] ^= 0xff;
            }

            self.0.objects.lock().unwrap().insert(key, bytes);
            Ok(())
        })
    }

    fn get(&self, key: String) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move { Ok(self.0.objects.lock().unwrap().get(&key).cloned()) })
    }

    fn size(&self, key: String) -> BoxFuture<'_, Result<Option<u64>, String>> {
        Box::pin(async move {
            Ok(self
                .0
                .objects
                .lock()
                .unwrap()
                .get(&key)
                .map(|o| o.len() as u64))
        })
    }

    fn delete(&self, key: String) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.0.objects.lock().unwrap().remove(&key);
            Ok(())
        })
    }
}

impl Index for MemIndex {
    fn epoch(&self) -> i64 {
        self.1
    }

    fn record(&self, snapshot: Snapshot) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if self.0.faults.record.load(Ordering::SeqCst) {
                return Err("index unavailable".to_string());
            }

            if snapshot.epoch != *self.0.latest_epoch.lock().unwrap() {
                return Err("epoch is no longer the latest".to_string());
            }

            self.0.rows.lock().unwrap().push(snapshot);
            Ok(())
        })
    }

    fn list(&self, database_id: String) -> BoxFuture<'_, Result<Vec<Snapshot>, String>> {
        Box::pin(async move {
            let mut rows: Vec<Snapshot> = self
                .0
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.database_id == database_id)
                .cloned()
                .collect();

            rows.sort_by_key(|s| std::cmp::Reverse(s.seq));
            Ok(rows)
        })
    }

    fn forget(&self, database_id: String, seq: i64) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.0
                .rows
                .lock()
                .unwrap()
                .retain(|s| !(s.database_id == database_id && s.seq == seq));
            Ok(())
        })
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> TempDir {
    TempDir(std::env::temp_dir().join(format!("d1-archive-test-{}", uuid::Uuid::new_v4())))
}

fn archive(memory: &Memory, epoch: i64, pause_after: Duration) -> Archive {
    *memory.latest_epoch.lock().unwrap() = epoch;

    Archive {
        config: ArchiveConfig {
            storage: StorageConfig {
                id: String::new(),
                bucket: String::new(),
                prefix: None,
                access_key_id: String::new(),
                secret_access_key: String::new(),
                endpoint: String::new(),
                region: None,
                public_url: None,
            },
            interval: Duration::from_secs(300),
            alert_after: Duration::from_secs(900),
            pause_after,
            keep: 3,
        },
        store: Box::new(MemStore(memory.clone())),
        index: Box::new(MemIndex(memory.clone(), epoch)),
    }
}

fn service(dir: &TempDir, memory: &Memory) -> Service {
    service_with(dir, memory, 1, Duration::from_secs(3600))
}

fn service_with(dir: &TempDir, memory: &Memory, epoch: i64, pause_after: Duration) -> Service {
    Service::new(Config {
        data_dir: dir.0.clone(),
        limits: Limits::default(),
        timeout: Duration::from_secs(5),
        max_queue: 64,
        max_open: 16,
        idle: Duration::from_secs(300),
        min_free_bytes: 0,
    })
    .unwrap()
    .with_archive(archive(memory, epoch, pause_after))
}

async fn exec(service: &Service, sql: &str) -> D1Result {
    service.execute(DB, D1Op::Exec { sql: sql.into() }).await
}

/// The values of column 0, as JSON
async fn values(service: &Service, sql: &str) -> String {
    let op = D1Op::Query {
        statement: D1Statement {
            sql: sql.to_string(),
            params: vec![],
        },
        rows: D1Rows::All,
    };

    match service.execute(DB, op).await {
        D1Result::Query(result) => serde_json::to_string(&result.rows).unwrap(),
        D1Result::Error(err) => format!("error: {:?}: {}", err.kind, err.message),
        other => panic!("unexpected {other:?}"),
    }
}

fn remove_volume(dir: &TempDir) {
    for entry in std::fs::read_dir(&dir.0).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with(DB) {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
}

#[tokio::test]
async fn a_lost_volume_comes_back_from_the_last_snapshot() {
    let dir = temp_dir();
    let memory = Memory::default();

    {
        let service = service(&dir, &memory);

        exec(
            &service,
            "CREATE TABLE t (a); INSERT INTO t VALUES ('saved');",
        )
        .await;
        service.snapshot_all().await;
        exec(&service, "INSERT INTO t VALUES ('lost');").await;
    }

    remove_volume(&dir);

    let service = service(&dir, &memory);

    assert_eq!(values(&service, "SELECT a FROM t").await, r#"[["saved"]]"#);
}

#[tokio::test]
async fn no_snapshot_without_new_commits() {
    let dir = temp_dir();
    let memory = Memory::default();
    let service = service(&dir, &memory);

    exec(&service, "CREATE TABLE t (a)").await;
    service.snapshot_all().await;
    values(&service, "SELECT * FROM t").await;
    service.snapshot_all().await;

    assert_eq!(memory.rows.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_upload_keeps_the_previous_snapshot() {
    let dir = temp_dir();
    let memory = Memory::default();
    let service = service(&dir, &memory);

    exec(&service, "CREATE TABLE t (a); INSERT INTO t VALUES (1);").await;
    service.snapshot_all().await;
    exec(&service, "INSERT INTO t VALUES (2);").await;

    memory.faults.put.store(true, Ordering::SeqCst);
    service.snapshot_all().await;

    assert_eq!(memory.rows.lock().unwrap().len(), 1);

    memory.faults.put.store(false, Ordering::SeqCst);
    service.snapshot_all().await;

    assert_eq!(memory.rows.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_unrecorded_upload_is_not_restored() {
    let dir = temp_dir();
    let memory = Memory::default();

    {
        let service = service(&dir, &memory);

        exec(&service, "CREATE TABLE t (a); INSERT INTO t VALUES (1);").await;
        service.snapshot_all().await;
        exec(&service, "INSERT INTO t VALUES (2);").await;

        // The object is uploaded, then the index refuses it
        memory.faults.record.store(true, Ordering::SeqCst);
        service.snapshot_all().await;
        memory.faults.record.store(false, Ordering::SeqCst);
    }

    assert_eq!(memory.objects.lock().unwrap().len(), 2);

    remove_volume(&dir);

    let service = service(&dir, &memory);

    assert_eq!(values(&service, "SELECT a FROM t").await, "[[1]]");
}

#[tokio::test]
async fn a_corrupt_snapshot_is_refused() {
    let dir = temp_dir();
    let memory = Memory::default();

    {
        let service = service(&dir, &memory);

        memory.faults.corrupt.store(true, Ordering::SeqCst);
        exec(&service, "CREATE TABLE t (a); INSERT INTO t VALUES (1);").await;
        service.snapshot_all().await;
    }

    remove_volume(&dir);

    let service = service(&dir, &memory);
    let result = values(&service, "SELECT a FROM t").await;

    assert!(result.starts_with("error: Unavailable"), "{result}");
    assert!(!dir.0.join(format!("{DB}.sqlite")).exists());
}

#[tokio::test]
async fn a_truncated_snapshot_is_refused() {
    let dir = temp_dir();
    let memory = Memory::default();

    {
        let service = service(&dir, &memory);

        exec(&service, "CREATE TABLE t (a); INSERT INTO t VALUES (1);").await;
        service.snapshot_all().await;
    }

    // Truncate the object and its recorded checksum alike: only the
    // decompression and SQLite checks can catch it
    {
        let mut objects = memory.objects.lock().unwrap();
        let (key, object) = objects.iter_mut().next().unwrap();
        let key = key.clone();

        object.truncate(object.len() / 2);

        let checksum = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&object[..]));
        let mut rows = memory.rows.lock().unwrap();
        let row = rows.iter_mut().find(|s| s.key == key).unwrap();

        row.checksum = checksum;
    }

    remove_volume(&dir);

    let service = service(&dir, &memory);
    let result = values(&service, "SELECT a FROM t").await;

    assert!(result.starts_with("error: Unavailable"), "{result}");
}

#[tokio::test]
async fn only_the_latest_snapshots_are_kept() {
    let dir = temp_dir();
    let memory = Memory::default();
    let service = service(&dir, &memory);

    exec(&service, "CREATE TABLE t (a)").await;

    for value in 0..5 {
        exec(&service, &format!("INSERT INTO t VALUES ({value})")).await;
        service.snapshot_all().await;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    assert_eq!(memory.rows.lock().unwrap().len(), 3);
    assert_eq!(memory.objects.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn an_older_epoch_cannot_publish() {
    let dir = temp_dir();
    let memory = Memory::default();
    let service = service_with(&dir, &memory, 1, Duration::from_secs(3600));

    exec(&service, "CREATE TABLE t (a)").await;

    // A newer runner started
    *memory.latest_epoch.lock().unwrap() = 2;
    service.snapshot_all().await;

    assert!(memory.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn writes_pause_when_snapshots_fail_too_long() {
    let dir = temp_dir();
    let memory = Memory::default();
    let service = service_with(&dir, &memory, 1, Duration::from_millis(50));

    memory.faults.put.store(true, Ordering::SeqCst);

    exec(&service, "CREATE TABLE t (a)").await;
    service.snapshot_all().await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let write = exec(&service, "INSERT INTO t VALUES (1)").await;

    match write {
        D1Result::Error(err) => {
            assert_eq!(err.kind, D1ErrorKind::Overloaded);
            assert!(err.message.contains("writes are paused"), "{}", err.message);
        }
        other => panic!("expected a refused write, got {other:?}"),
    }

    assert_eq!(values(&service, "SELECT count(*) FROM t").await, "[[0]]");

    memory.faults.put.store(false, Ordering::SeqCst);
    service.snapshot_all().await;

    assert!(matches!(
        exec(&service, "INSERT INTO t VALUES (1)").await,
        D1Result::Exec { .. }
    ));
}

#[tokio::test]
async fn a_reset_restores_the_last_snapshot() {
    let dir = temp_dir();
    let memory = Memory::default();
    let service = service(&dir, &memory);

    exec(
        &service,
        "CREATE TABLE t (a); INSERT INTO t VALUES ('saved');",
    )
    .await;
    service.snapshot_all().await;
    exec(&service, "INSERT INTO t VALUES ('after');").await;

    service.reset_to_snapshot(DB).unwrap();

    assert_eq!(values(&service, "SELECT a FROM t").await, r#"[["saved"]]"#);

    let aside = std::fs::read_dir(&dir.0).unwrap().flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .contains(".sqlite.replaced-")
    });

    assert!(aside, "the replaced file is kept aside");
}

#[tokio::test]
async fn a_database_without_snapshot_starts_empty() {
    let dir = temp_dir();
    let memory = Memory::default();
    let service = service(&dir, &memory);

    assert_eq!(values(&service, "SELECT 1").await, "[[1]]");
}
