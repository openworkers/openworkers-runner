use openworkers_runtime::ScheduledInit;
use openworkers_runtime::Script;
use openworkers_runtime::Task;
use openworkers_runtime::Worker;

use serde::Deserialize;
use serde::Serialize;

use crate::store;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledData {
    pub id: String,
    pub cron: String,
    pub scheduled_time: u64,
    pub worker_id: String,
}

fn run_scheduled(data: ScheduledData, script: Script) {
    let (res_tx, res_rx) = tokio::sync::oneshot::channel::<()>();

    let task = Task::Scheduled(Some(ScheduledInit::new(res_tx, data.scheduled_time)));

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let local = tokio::task::LocalSet::new();

        local.spawn_local(async move {
            log::debug!("create worker");
            let mut worker = Worker::new(script).await.unwrap();

            log::debug!("exec scheduled task");
            match worker.exec(task).await {
                Ok(()) => log::debug!("exec completed"),
                Err(err) => log::error!("exec did not complete: {err}"),
            }
        });

        log::debug!("scheduled task listener started");

        match local.block_on(&rt, async { res_rx.await } ) {
            Ok(()) => {}
            Err(err) => log::error!("failed to wait for end: {err}"),
        }

        log::debug!("scheduled task listener stopped");
    });
}

pub fn handle_scheduled(db: store::Database) {
    std::thread::spawn(move || {
        let local = tokio::task::LocalSet::new();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let handle = local.spawn_local(async move {
            let nc = nats::connect("nats://127.0.0.1:4222").expect("failed to connect to nats");
            let sub = nc
                .queue_subscribe("scheduled", "runner")
                .expect("failed to subscribe to scheduled");

            log::debug!("listening for scheduled tasks");

            while let Some(msg) = sub.next() {
                log::debug!("scheduled task received: {:?}", msg);

                let data: ScheduledData = match serde_json::from_slice::<ScheduledData>(&msg.data) {
                    Ok(msg) => msg,
                    Err(err) => {
                        log::error!("failed to parse scheduled task: {:?}", err);
                        continue;
                    }
                };

                log::debug!("scheduled task parsed: {:?}", data);

                let worker_id = store::WorkerIdentifier::Id(data.worker_id.clone());
                let worker = match store::get_worker(&db, worker_id).await {
                    Some(worker) => worker,
                    None => {
                        log::error!("worker not found: {:?}", data.worker_id);
                        continue;
                    }
                };

                let script = Script {
                    specifier: openworkers_runtime::module_url("script.js"),
                    code: Some(openworkers_runtime::FastString::from(worker.script)),
                    env: match worker.env {
                        Some(env) => Some(env.encode_to_string()),
                        None => None,                        
                    },
                };

                run_scheduled(data, script);
            }

            log::debug!("scheduled task listener stopped");
        });

        log::debug!("subscribing to scheduled {:?}", handle);

        match local.block_on(&rt, handle) {
            Ok(()) => {}
            Err(err) => log::error!("failed to wait for end: {err}"),
        }
    });
}
