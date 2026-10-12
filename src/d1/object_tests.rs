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
    let config = Config {
        data_dir: dir.0.clone(),
        limits: Limits::default(),
        timeout: Duration::from_secs(5),
        max_queue: 64,
        max_open: 16,
        idle: Duration::from_secs(300),
        min_free_bytes: 0,
    };

    (Service::new(config).unwrap(), dir)
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
