//! Runs `tests/d1/conformance.js` in V8 against two real D1 databases and
//! reports each test. The battery lives here rather than in
//! openworkers-conformance: D1 behavior is in the runner, and that suite runs
//! runtimes without a host that serves bindings.

#![cfg(feature = "v8")]

use std::collections::HashMap;
use std::sync::Arc;

use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::RequestBody;
use openworkers_core::Script;
use openworkers_runner::Binding;
use openworkers_runner::ops::RunnerOperations;
use openworkers_runner::store::bindings_to_infos;

const HARNESS: &str = r#"
const __tests = [];

function test(name, fn) {
    __tests.push({ name, fn });
}

function assert(condition, message) {
    if (!condition) {
        throw new Error(message || 'assertion failed');
    }
}

function assertEqual(actual, expected, message) {
    if (actual !== expected) {
        throw new Error((message ? message + ': ' : '') + 'expected ' + JSON.stringify(expected) + ', got ' + JSON.stringify(actual));
    }
}

function assertJSON(actual, expected) {
    assertEqual(JSON.stringify(actual), JSON.stringify(expected));
}

function assertType(value, expected) {
    assertEqual(typeof value, expected, 'type');
}

function describe(err) {
    return (err && err.constructor ? err.constructor.name : typeof err) + ': ' + (err && err.message);
}

function assertThrows(fn, expected) {
    try {
        fn();
    } catch (err) {
        assert(describe(err).includes(expected), 'expected ' + expected + ', got ' + describe(err));
        return;
    }

    throw new Error('expected a throw with ' + expected);
}

async function assertRejects(fn, expected) {
    try {
        await fn();
    } catch (err) {
        assert(describe(err).includes(expected), 'expected ' + expected + ', got ' + describe(err));
        return;
    }

    throw new Error('expected a rejection with ' + expected);
}

addEventListener('fetch', (event) => {
    event.respondWith((async () => {
        const results = [];

        for (const { name, fn } of __tests) {
            try {
                await fn();
                results.push({ name, ok: true });
            } catch (err) {
                results.push({ name, ok: false, error: describe(err) });
            }
        }

        return new Response(JSON.stringify(results));
    })());
});
"#;

#[derive(serde::Deserialize)]
struct Outcome {
    name: String,
    ok: bool,
    error: Option<String>,
}

#[tokio::test]
async fn d1_conformance() {
    let dir = std::env::temp_dir().join(format!("d1-conformance-{}", uuid::Uuid::new_v4()));
    let service = openworkers_runner::d1::Service::new(openworkers_runner::d1::Config {
        data_dir: dir.clone(),
        limits: openworkers_runner::d1::Limits::default(),
        timeout: std::time::Duration::from_secs(10),
        max_queue: 64,
        max_open: 16,
        idle: std::time::Duration::from_secs(300),
        min_free_bytes: 0,
    })
    .unwrap();

    openworkers_runner::d1::install(service);

    let bindings = vec![
        Binding::D1 {
            key: "DB".to_string(),
            database_id: uuid::Uuid::new_v4().to_string(),
        },
        Binding::D1 {
            key: "OTHER".to_string(),
            database_id: uuid::Uuid::new_v4().to_string(),
        },
    ];

    let code = format!("{}\n{}", HARNESS, include_str!("d1/conformance.js"));
    let script = Script::with_bindings(code, None, bindings_to_infos(&bindings));
    let ops = Arc::new(RunnerOperations::new().with_bindings(bindings));

    let body = tokio::task::LocalSet::new()
        .run_until(async move {
            let mut worker = openworkers_runtime_v8::Worker::new_with_ops(script, None, ops)
                .await
                .unwrap();

            let (task, rx) = Event::fetch(HttpRequest {
                method: HttpMethod::Get,
                url: "http://localhost/".to_string(),
                headers: HashMap::new(),
                body: RequestBody::None,
            });

            worker.exec(task).await.unwrap();

            let response = rx.await.unwrap();

            response.body.collect().await.unwrap().unwrap()
        })
        .await;

    let _ = std::fs::remove_dir_all(&dir);

    let outcomes: Vec<Outcome> = serde_json::from_slice(&body)
        .unwrap_or_else(|_| panic!("not a report: {}", String::from_utf8_lossy(&body)));

    let failed: Vec<&Outcome> = outcomes.iter().filter(|o| !o.ok).collect();

    for outcome in &failed {
        eprintln!(
            "FAIL {}: {}",
            outcome.name,
            outcome.error.as_deref().unwrap_or("")
        );
    }

    eprintln!(
        "D1 conformance: {}/{} passed",
        outcomes.len() - failed.len(),
        outcomes.len()
    );

    assert!(outcomes.len() >= 40, "only {} tests ran", outcomes.len());
    assert!(
        failed.is_empty(),
        "{} D1 conformance tests failed",
        failed.len()
    );
}
