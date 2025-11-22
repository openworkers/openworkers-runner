use openworkers_runtime::LogEvent;

pub struct LogMessage {
    pub worker_id: String,
    pub event: LogEvent,
}

// Create a global log publisher that handles all NATS publishing
pub fn start_log_publisher() -> std::sync::mpsc::Sender<LogMessage> {
    let (tx, rx) = std::sync::mpsc::channel::<LogMessage>();

    std::thread::spawn(move || {
        // Create a tokio runtime for this thread
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let nc = crate::nats::nats_connect().await;
            log::info!("Global log publisher started");

            for log_msg in rx {
                let level = log_msg.event.level.to_string();
                let subject = format!("{}.console.{}", log_msg.worker_id, level);

                match nc.publish(subject, log_msg.event.message.into()).await {
                    Ok(()) => {
                        // IMPORTANT: flush() is required to ensure messages are sent immediately.
                        // Without it, async-nats buffers messages internally and they may not
                        // be transmitted before the worker completes, causing logs to be lost.
                        if let Err(err) = nc.flush().await {
                            log::error!("failed to flush NATS message: {:?}", err);
                        }
                    }
                    Err(err) => log::error!("failed to publish log to NATS: {:?}", err),
                };
            }

            log::warn!("Global log publisher stopped");
        });
    });

    tx
}

// Create a worker-specific log handler that wraps the global sender
pub fn create_log_handler(
    worker_id: String,
    global_tx: std::sync::mpsc::Sender<LogMessage>,
) -> std::sync::mpsc::Sender<LogEvent> {
    // Create a new channel that will forward to the global one
    let (tx, rx) = std::sync::mpsc::channel::<LogEvent>();

    // Clone the worker_id for the thread
    let worker_id_clone = worker_id.clone();

    std::thread::spawn(move || {
        for event in rx {
            if let Err(e) = global_tx.send(LogMessage {
                worker_id: worker_id_clone.clone(),
                event,
            }) {
                log::error!("Failed to send log to global publisher: {:?}", e);
            }
        }
    });

    tx
}
