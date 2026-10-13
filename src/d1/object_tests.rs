use std::path::PathBuf;
use std::time::Duration;

use openworkers_core::ObjectStorageOp;
use openworkers_core::ObjectStorageResult;

use super::Config;
use super::Limits;
use super::Service;

const NAMESPACE: &str = "33333333-3333-4333-8333-333333333333";
const OBJECT: &str = "abababababababababababababababababababababababababababababababab";

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn service() -> (Service, TempDir) {
    let dir = TempDir(std::env::temp_dir().join(format!("do-test-{}", uuid::Uuid::new_v4())));

    (service_in(&dir, u64::MAX), dir)
}

fn service_in(dir: &TempDir, namespace_bytes: u64) -> Service {
    let config = Config {
        data_dir: dir.0.clone(),
        limits: Limits::default(),
        timeout: Duration::from_secs(5),
        max_queue: 64,
        max_open: 16,
        idle: Duration::from_secs(300),
        min_free_bytes: 0,
        namespace_bytes,
    };

    Service::new(config).unwrap()
}

fn put(entries: &[(&str, &[u8])]) -> ObjectStorageOp {
    ObjectStorageOp::Write {
        puts: entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_vec()))
            .collect(),
        deletes: Vec::new(),
        delete_all: false,
    }
}

fn list() -> ObjectStorageOp {
    ObjectStorageOp::List {
        start: None,
        start_after: None,
        end: None,
        prefix: None,
        reverse: false,
        limit: None,
    }
}

async fn keys(service: &Service, op: ObjectStorageOp) -> Vec<String> {
    match service.object_storage(NAMESPACE, OBJECT, op).await {
        ObjectStorageResult::Entries(entries) => entries.into_iter().map(|(key, _)| key).collect(),
        other => panic!("not entries: {other:?}"),
    }
}

#[tokio::test]
async fn values_round_trip_in_their_object_file() {
    let (service, dir) = service();

    service
        .object_storage(NAMESPACE, OBJECT, put(&[("a", b"one"), ("b", &[0, 255])]))
        .await;

    let get = ObjectStorageOp::Get {
        keys: vec!["b".to_string(), "missing".to_string(), "a".to_string()],
    };

    assert_eq!(
        service.object_storage(NAMESPACE, OBJECT, get).await,
        ObjectStorageResult::Values(vec![Some(vec![0, 255]), None, Some(b"one".to_vec())])
    );
    assert!(
        dir.0
            .join(format!("objects/{NAMESPACE}/{OBJECT}.sqlite"))
            .exists()
    );
}

#[tokio::test]
async fn a_list_follows_its_range_order_and_limit() {
    let (service, _dir) = service();
    let values: &[u8] = b"v";

    service
        .object_storage(
            NAMESPACE,
            OBJECT,
            put(&[
                ("a:1", values),
                ("a:2", values),
                ("a:3", values),
                ("b", values),
                ("\u{e9}", values),
            ]),
        )
        .await;

    let range =
        |start: Option<&str>, after: Option<&str>, end: Option<&str>, prefix: Option<&str>| {
            ObjectStorageOp::List {
                start: start.map(String::from),
                start_after: after.map(String::from),
                end: end.map(String::from),
                prefix: prefix.map(String::from),
                reverse: false,
                limit: None,
            }
        };

    // UTF-8 byte order, as on Cloudflare
    assert_eq!(
        keys(&service, list()).await,
        ["a:1", "a:2", "a:3", "b", "\u{e9}"]
    );
    assert_eq!(
        keys(&service, range(None, None, None, Some("a:"))).await,
        ["a:1", "a:2", "a:3"]
    );
    assert_eq!(
        keys(&service, range(Some("a:2"), None, Some("b"), None)).await,
        ["a:2", "a:3"]
    );
    assert_eq!(
        keys(&service, range(None, Some("a:3"), None, None)).await,
        ["b", "\u{e9}"]
    );

    let last_two = ObjectStorageOp::List {
        start: None,
        start_after: None,
        end: None,
        prefix: None,
        reverse: true,
        limit: Some(2),
    };

    assert_eq!(keys(&service, last_two).await, ["\u{e9}", "b"]);
}

#[tokio::test]
async fn a_write_is_atomic_and_counts_its_deletes() {
    let (service, _dir) = service();

    service
        .object_storage(NAMESPACE, OBJECT, put(&[("a", b"1"), ("b", b"2")]))
        .await;

    let write = ObjectStorageOp::Write {
        puts: vec![("c".to_string(), b"3".to_vec())],
        deletes: vec!["a".to_string(), "missing".to_string()],
        delete_all: false,
    };

    assert_eq!(
        service.object_storage(NAMESPACE, OBJECT, write).await,
        ObjectStorageResult::Written { deleted: 1 }
    );
    assert_eq!(keys(&service, list()).await, ["b", "c"]);

    // A put over the limit fails the whole write
    let too_big = ObjectStorageOp::Write {
        puts: vec![
            ("d".to_string(), b"4".to_vec()),
            ("e".to_string(), vec![0; 128 * 1024 + 1]),
        ],
        deletes: vec!["b".to_string()],
        delete_all: true,
    };

    assert!(matches!(
        service.object_storage(NAMESPACE, OBJECT, too_big).await,
        ObjectStorageResult::Error(message) if message.contains("over 131072 bytes")
    ));
    assert_eq!(keys(&service, list()).await, ["b", "c"]);

    let delete_all = ObjectStorageOp::Write {
        puts: Vec::new(),
        deletes: Vec::new(),
        delete_all: true,
    };

    service.object_storage(NAMESPACE, OBJECT, delete_all).await;
    assert!(keys(&service, list()).await.is_empty());
}

#[tokio::test]
async fn keys_have_the_limits_of_cloudflare() {
    let (service, _dir) = service();
    let long = "k".repeat(2049);
    let many: Vec<String> = (0..129).map(|n| n.to_string()).collect();

    for op in [
        put(&[(long.as_str(), b"v")]),
        ObjectStorageOp::Get { keys: many },
    ] {
        assert!(matches!(
            service.object_storage(NAMESPACE, OBJECT, op).await,
            ObjectStorageResult::Error(_)
        ));
    }
}

#[tokio::test]
async fn a_deleted_namespace_loses_its_files() {
    let (service, dir) = service();

    service
        .object_storage(NAMESPACE, OBJECT, put(&[("a", b"1")]))
        .await;
    assert!(service.remove_namespace_files(NAMESPACE));
    assert!(!dir.0.join(format!("objects/{NAMESPACE}")).exists());

    // A namespace with no file is already clean
    assert!(service.remove_namespace_files(NAMESPACE));
}

#[tokio::test]
async fn an_object_file_stays_in_the_data_directory() {
    let (service, _dir) = service();

    for (namespace, object) in [
        ("../escape", OBJECT),
        (NAMESPACE, "../../escape"),
        (NAMESPACE, &OBJECT.to_uppercase()),
        (NAMESPACE, &OBJECT[1..]),
    ] {
        assert!(matches!(
            service.object_storage(namespace, object, list()).await,
            ObjectStorageResult::Error(_)
        ));
    }
}

#[tokio::test]
async fn an_alarm_lives_in_the_object_file_apart_from_the_keys() {
    let (service, _dir) = service();
    let storage = |op| service.object_storage(NAMESPACE, OBJECT, op);

    assert_eq!(
        storage(ObjectStorageOp::GetAlarm).await,
        ObjectStorageResult::Alarm(None)
    );
    assert_eq!(
        storage(ObjectStorageOp::SetAlarm(Some(5000))).await,
        ObjectStorageResult::Alarm(Some(5000))
    );
    assert_eq!(
        storage(ObjectStorageOp::SetAlarm(Some(7000))).await,
        ObjectStorageResult::Alarm(Some(7000))
    );

    // deleteAll() leaves the alarm, as on Cloudflare
    storage(put(&[("a", b"1")])).await;
    storage(ObjectStorageOp::Write {
        puts: Vec::new(),
        deletes: Vec::new(),
        delete_all: true,
    })
    .await;

    assert_eq!(
        storage(ObjectStorageOp::GetAlarm).await,
        ObjectStorageResult::Alarm(Some(7000))
    );

    storage(ObjectStorageOp::SetAlarm(None)).await;
    assert_eq!(
        storage(ObjectStorageOp::GetAlarm).await,
        ObjectStorageResult::Alarm(None)
    );
}

#[tokio::test]
async fn clearing_an_old_alarm_keeps_a_newer_one() {
    let (service, _dir) = service();

    service
        .object_storage(NAMESPACE, OBJECT, ObjectStorageOp::SetAlarm(Some(5000)))
        .await;

    // The handler of the alarm at 5000 set the next one
    service
        .object_storage(NAMESPACE, OBJECT, ObjectStorageOp::SetAlarm(Some(9000)))
        .await;
    assert_eq!(
        service.clear_alarm(NAMESPACE, OBJECT, 5000).await,
        ObjectStorageResult::Alarm(Some(9000))
    );

    assert_eq!(
        service.clear_alarm(NAMESPACE, OBJECT, 9000).await,
        ObjectStorageResult::Alarm(None)
    );
}

const OTHER: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

/// Puts values of 100 KiB in `object` until the namespace is full, and
/// answers how many it took.
async fn fill(service: &Service, object: &str) -> usize {
    let value = vec![7; 100 * 1024];

    for count in 0..64 {
        let key = format!("key-{count}");

        match service
            .object_storage(NAMESPACE, object, put(&[(&key, &value)]))
            .await
        {
            ObjectStorageResult::Written { .. } => {}
            ObjectStorageResult::Error(err) => {
                assert!(err.contains("the storage is full"), "{err}");
                return count;
            }
            other => panic!("{other:?}"),
        }
    }

    panic!("the namespace takes more than 6 MiB");
}

#[tokio::test]
async fn a_full_namespace_takes_deletes_and_no_puts() {
    let dir = TempDir(std::env::temp_dir().join(format!("do-test-{}", uuid::Uuid::new_v4())));
    let service = service_in(&dir, 1 << 20);

    let count = fill(&service, OBJECT).await;
    assert!((8..=11).contains(&count), "{count} values of 100 KiB");

    // Another object of the namespace gets no file, even for a read
    let read = service
        .object_storage(NAMESPACE, OTHER, ObjectStorageOp::GetAlarm)
        .await;
    assert!(matches!(read, ObjectStorageResult::Error(_)), "{read:?}");
    assert!(
        !dir.0
            .join(format!("objects/{NAMESPACE}/{OTHER}.sqlite"))
            .exists()
    );

    // Reads still run, and a delete frees room at once
    assert_eq!(keys(&service, list()).await.len(), count);

    let clear = ObjectStorageOp::Write {
        puts: Vec::new(),
        deletes: Vec::new(),
        delete_all: true,
    };
    assert!(matches!(
        service.object_storage(NAMESPACE, OBJECT, clear).await,
        ObjectStorageResult::Written { .. }
    ));

    // The emptied object keeps the pages of its tables
    assert!(fill(&service, OTHER).await >= count - 1);
}

#[tokio::test]
async fn the_limit_counts_the_files_already_on_disk() {
    let dir = TempDir(std::env::temp_dir().join(format!("do-test-{}", uuid::Uuid::new_v4())));

    let service = service_in(&dir, 1 << 20);
    let count = fill(&service, OBJECT).await;
    drop(service);

    // A runner that starts again knows what the namespace holds
    let service = service_in(&dir, 1 << 20);
    let value = vec![7; 100 * 1024];
    let refused = service
        .object_storage(NAMESPACE, OBJECT, put(&[("one-more", &value)]))
        .await;
    assert!(
        matches!(&refused, ObjectStorageResult::Error(err) if err.contains("the storage is full")),
        "{refused:?} after {count} values"
    );

    // Another namespace has its own limit
    let other_namespace = "44444444-4444-4444-8444-444444444444";
    let written = service
        .object_storage(other_namespace, OBJECT, put(&[("one", &value)]))
        .await;
    assert!(
        matches!(written, ObjectStorageResult::Written { .. }),
        "{written:?}"
    );
}

#[tokio::test]
async fn without_snapshots_written_objects_do_not_stay_open() {
    let (service, _dir) = service();

    for index in 0..32 {
        let object = format!("{index:064x}");
        let written = service
            .object_storage(NAMESPACE, &object, put(&[("key", b"value")]))
            .await;
        assert!(matches!(written, ObjectStorageResult::Written { .. }));
    }

    let open = service.databases.lock().unwrap().len();
    assert!(open <= 16, "{open} databases open, over D1_MAX_OPEN");
}
