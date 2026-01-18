//! Worker benchmarks for V8 runtime

#![cfg(feature = "v8")]

use criterion::{Criterion, criterion_group, criterion_main};
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

pub fn worker_benchmarks(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("Worker");

    group.bench_function("new", |b| {
        b.iter(|| {
            rt.block_on(async {
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async {
                        let script = Script::new(
                            r#"addEventListener('fetch', (e) => e.respondWith(new Response('OK')));"#,
                        );
                        Worker::new(script, None).await.unwrap()
                    })
                    .await
            })
        })
    });

    // For exec benchmarks, we need to keep the same LocalSet alive across iterations
    // because the Worker's scheduler tasks are spawned on it.
    group.bench_function("exec_simple_response", |b| {
        let local = tokio::task::LocalSet::new();

        let mut worker = rt.block_on(local.run_until(async {
            let script = Script::new(
                r#"addEventListener('fetch', (e) => e.respondWith(new Response('OK')));"#,
            );
            Worker::new(script, None).await.unwrap()
        }));

        b.iter(|| {
            rt.block_on(local.run_until(async {
                let req = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };
                let (task, rx) = Event::fetch(req);
                worker.exec(task).await.unwrap();
                rx.await.unwrap()
            }))
        })
    });

    group.bench_function("exec_json_response", |b| {
        let local = tokio::task::LocalSet::new();

        let mut worker = rt.block_on(local.run_until(async {
            let script = Script::new(
                r#"addEventListener('fetch', (e) => e.respondWith(new Response(JSON.stringify({a:1,b:2}))));"#,
            );
            Worker::new(script, None).await.unwrap()
        }));

        b.iter(|| {
            rt.block_on(local.run_until(async {
                let req = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };
                let (task, rx) = Event::fetch(req);
                worker.exec(task).await.unwrap();
                rx.await.unwrap()
            }))
        })
    });

    group.bench_function("exec_with_headers", |b| {
        let local = tokio::task::LocalSet::new();

        let mut worker = rt.block_on(local.run_until(async {
            let script = Script::new(
                r#"addEventListener('fetch', (e) => e.respondWith(new Response('OK', {headers: {'X-A': '1', 'X-B': '2'}})));"#,
            );
            Worker::new(script, None).await.unwrap()
        }));

        b.iter(|| {
            rt.block_on(local.run_until(async {
                let req = HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: RequestBody::None,
                };
                let (task, rx) = Event::fetch(req);
                worker.exec(task).await.unwrap();
                rx.await.unwrap()
            }))
        })
    });

    group.finish();
}

criterion_group!(benches, worker_benchmarks);
criterion_main!(benches);
