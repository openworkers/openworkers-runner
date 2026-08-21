//! Guest component for the runner's end-to-end binding test
//!
//! Build with: cargo build --target wasm32-wasip2 --release
//!
//! One GET touches KV, the database and storage through
//! `openworkers:bindings`, then reports what came back, so the test can check
//! both the values and the operations the runner was asked to run.

wit_bindgen::generate!({
    world: "worker",
    path: "../../../../openworkers-runtime-wasm/wit",
    generate_all,
});

use exports::openworkers::worker::scheduled::Guest as ScheduledGuest;
use exports::wasi::http0_2_0::incoming_handler::Guest as HttpGuest;
use openworkers::bindings::database;
use openworkers::bindings::database::SqlParam;
use openworkers::bindings::database::SqlValue;
use openworkers::bindings::kv;
use openworkers::bindings::storage;
use wasi::http0_2_0::types::Fields;
use wasi::http0_2_0::types::IncomingRequest;
use wasi::http0_2_0::types::OutgoingBody;
use wasi::http0_2_0::types::OutgoingResponse;
use wasi::http0_2_0::types::ResponseOutparam;

/// wasi:io caps `blocking-write-and-flush` at this many bytes
const CHUNK_SIZE: usize = 4096;

const OBJECT: &[u8] = b"round trip";

struct BindingsWorker;

impl HttpGuest for BindingsWorker {
    fn handle(_request: IncomingRequest, response_out: ResponseOutparam) {
        match probe() {
            Ok(body) => respond(response_out, 200, body.as_bytes()),
            Err(e) => respond(response_out, 500, e.as_bytes()),
        }
    }
}

impl ScheduledGuest for BindingsWorker {
    fn handle_scheduled(_scheduled_time: u64) {}
}

fn probe() -> Result<String, String> {
    kv::put("CACHE", "visits", r#"{"count":1}"#, Some(60))?;

    let cached = kv::get("CACHE", "visits")?.ok_or_else(|| "visits is missing".to_string())?;

    // One parameter per sql-value shape the test asserts on
    let params = vec![
        SqlParam::Value(SqlValue::Integer(7)),
        SqlParam::Value(SqlValue::Boolean(true)),
        SqlParam::Value(SqlValue::Text("widget".to_string())),
    ];

    let row = database::first(
        "DB",
        "SELECT name FROM items WHERE id = $1 AND active = $2 AND name = $3",
        &params,
    )?
    .ok_or_else(|| "no row".to_string())?;

    storage::put("BUCKET", "hello.txt", OBJECT)?;

    let object =
        storage::get("BUCKET", "hello.txt")?.ok_or_else(|| "hello.txt is missing".to_string())?;

    Ok(format!(
        "kv={} db={} storage={}",
        cached,
        row,
        String::from_utf8_lossy(&object)
    ))
}

fn respond(response_out: ResponseOutparam, status: u16, body: &[u8]) {
    let headers = Fields::new();

    headers
        .set("content-type", &[b"text/plain".to_vec()])
        .unwrap();

    let response = OutgoingResponse::new(headers);
    response.set_status_code(status).unwrap();

    let response_body = response.body().unwrap();

    ResponseOutparam::set(response_out, Ok(response));

    {
        let stream = response_body.write().unwrap();

        for chunk in body.chunks(CHUNK_SIZE) {
            stream.blocking_write_and_flush(chunk).unwrap();
        }
    }

    OutgoingBody::finish(response_body, None).unwrap();
}

export!(BindingsWorker);
