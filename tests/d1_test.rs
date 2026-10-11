//! A V8 worker uses a D1 binding through RunnerOperations, down to SQLite.

#![cfg(feature = "v8")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Once;

use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::RequestBody;
use openworkers_core::Script;
use openworkers_runner::Binding;
use openworkers_runner::ops::RunnerOperations;
use openworkers_runner::store::bindings_to_infos;

const DB: &str = "33333333-3333-4333-8333-333333333333";

static INIT: Once = Once::new();

fn init_d1() {
    INIT.call_once(|| {
        let dir = std::env::temp_dir().join(format!("d1-worker-test-{}", uuid::Uuid::new_v4()));

        // SAFETY: set once, before any thread of this test binary reads it
        unsafe { std::env::set_var("D1_DATA_DIR", &dir) };

        futures::executor::block_on(openworkers_runner::d1::init(None)).unwrap();
    });
}

async fn fetch(code: &str, path: &str) -> String {
    let bindings = vec![Binding::D1 {
        key: "DB".to_string(),
        database_id: DB.to_string(),
    }];

    let script = Script::with_bindings(code, None, bindings_to_infos(&bindings));
    let ops = Arc::new(RunnerOperations::new().with_bindings(bindings));

    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let mut worker = openworkers_runtime_v8::Worker::new_with_ops(script, None, ops)
                .await
                .unwrap();

            let (task, rx) = Event::fetch(HttpRequest {
                method: HttpMethod::Get,
                url: format!("http://localhost{path}"),
                headers: HashMap::new(),
                body: RequestBody::None,
            });

            worker.exec(task).await.unwrap();

            let response = rx.await.unwrap();
            let body = response.body.collect().await.unwrap().unwrap();

            String::from_utf8_lossy(&body).into_owned()
        })
        .await
}

const GUESTBOOK: &str = r#"
    const handle = async (url) => {
        const db = env.DB;

        switch (url.pathname) {
            case '/setup':
                return db.exec(`
                    CREATE TABLE IF NOT EXISTS entries (
                        id INTEGER PRIMARY KEY,
                        author TEXT NOT NULL,
                        message TEXT NOT NULL,
                        avatar BLOB
                    );
                    DELETE FROM entries;
                `);

            case '/sign': {
                const insert = db.prepare('INSERT INTO entries (author, message, avatar) VALUES (?1, ?2, ?3)');
                const results = await db.batch([
                    insert.bind('ada', 'hello; world', new Uint8Array([1, 2, 3])),
                    insert.bind('alan', 'second', null),
                ]);
                return results.map((r) => [r.success, r.meta.changes, r.meta.last_row_id]);
            }

            case '/read':
                return {
                    all: (await db.prepare('SELECT author, message, avatar FROM entries ORDER BY id').all()).results,
                    first: await db.prepare('SELECT author FROM entries WHERE id = ?').bind(2).first('author'),
                    count: await db.prepare('SELECT count(*) AS n FROM entries').first('n'),
                    raw: await db.prepare('SELECT id, author FROM entries ORDER BY id').raw({ columnNames: true }),
                };

            case '/rollback': {
                const insert = db.prepare('INSERT INTO entries (id, author, message) VALUES (?1, ?2, ?3)');
                let error;
                try {
                    await db.batch([insert.bind(10, 'eve', 'kept?'), insert.bind(1, 'dup', 'conflict')]);
                } catch (err) {
                    error = err.message;
                }
                const count = await db.prepare('SELECT count(*) AS n FROM entries').first('n');
                return { error, count };
            }

            case '/denied':
                try {
                    await db.prepare('PRAGMA journal_mode = DELETE').run();
                    return 'allowed';
                } catch (err) {
                    return err.message;
                }
        }
    };

    addEventListener('fetch', (event) => {
        event.respondWith(
            handle(new URL(event.request.url))
                .then((out) => new Response(JSON.stringify(out)))
                .catch((err) => new Response('error: ' + err.message)),
        );
    });
"#;

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|_| panic!("not JSON: {body}"))
}

#[tokio::test]
async fn guestbook_runs_on_sqlite() {
    init_d1();

    let setup = json(&fetch(GUESTBOOK, "/setup").await);

    assert_eq!(setup["count"], 2);
    assert!(setup["duration"].is_number());

    assert_eq!(fetch(GUESTBOOK, "/sign").await, "[[true,1,1],[true,1,2]]");

    let read = json(&fetch(GUESTBOOK, "/read").await);

    assert_eq!(
        read["all"].to_string(),
        r#"[{"author":"ada","avatar":[1,2,3],"message":"hello; world"},{"author":"alan","avatar":null,"message":"second"}]"#
    );
    assert_eq!(read["first"], "alan");
    assert_eq!(read["count"], 2);
    assert_eq!(
        read["raw"].to_string(),
        r#"[["id","author"],[1,"ada"],[2,"alan"]]"#
    );

    let rollback = json(&fetch(GUESTBOOK, "/rollback").await);

    assert_eq!(
        rollback["error"],
        "D1_ERROR: UNIQUE constraint failed: entries.id: SQLITE_CONSTRAINT"
    );
    assert_eq!(rollback["count"], 2);

    assert_eq!(
        fetch(GUESTBOOK, "/denied").await,
        r#""D1_ERROR: not authorized: SQLITE_AUTH""#
    );
}

#[tokio::test]
async fn unknown_binding_is_unavailable() {
    init_d1();

    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(
                env.DB.prepare('SELECT 1').all()
                    .then(() => new Response('ok'))
                    .catch((err) => new Response(err.message)),
            );
        });
    "#;

    let script = Script::with_bindings(code, None, vec![openworkers_core::BindingInfo::d1("DB")]);
    let ops = Arc::new(RunnerOperations::new());

    let local = tokio::task::LocalSet::new();
    let body = local
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

    assert_eq!(
        String::from_utf8_lossy(&body),
        "D1_ERROR: D1 binding 'DB' not configured"
    );
}
