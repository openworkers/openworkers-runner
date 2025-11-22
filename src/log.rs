use openworkers_runtime::LogEvent;
use std::time::Duration;

pub struct LogMessage {
    pub worker_id: String,
    pub event: LogEvent,
}

// Batching configuration
const BATCH_SIZE: usize = 10;           // Flush after 10 messages
const BATCH_TIMEOUT_MS: u64 = 100;       // Or flush every 100ms

// Create a global log publisher that handles all NATS publishing with batching
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
            log::info!("Global log publisher started (batching: {}msg/{}ms)", BATCH_SIZE, BATCH_TIMEOUT_MS);

            let mut batch: Vec<LogMessage> = Vec::with_capacity(BATCH_SIZE);
            let timeout = Duration::from_millis(BATCH_TIMEOUT_MS);

            loop {
                // Try to receive a message with timeout
                match rx.recv_timeout(timeout) {
                    Ok(log_msg) => {
                        batch.push(log_msg);

                        // Drain all immediately available messages
                        while batch.len() < BATCH_SIZE {
                            match rx.try_recv() {
                                Ok(log_msg) => batch.push(log_msg),
                                Err(_) => break,
                            }
                        }

                        // Flush if batch is full or we stopped receiving
                        if batch.len() >= BATCH_SIZE {
                            flush_batch(&nc, &mut batch).await;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Timeout: flush if we have any messages
                        if !batch.is_empty() {
                            flush_batch(&nc, &mut batch).await;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        // Channel closed, flush remaining and exit
                        if !batch.is_empty() {
                            flush_batch(&nc, &mut batch).await;
                        }
                        break;
                    }
                }
            }

            log::warn!("Global log publisher stopped");
        });
    });

    tx
}

async fn flush_batch(nc: &async_nats::Client, batch: &mut Vec<LogMessage>) {
    log::debug!("Flushing batch of {} log messages", batch.len());

    for log_msg in batch.drain(..) {
        let level = log_msg.event.level.to_string();
        let subject = format!("{}.console.{}", log_msg.worker_id, level);

        if let Err(err) = nc.publish(subject, log_msg.event.message.into()).await {
            log::error!("failed to publish log to NATS: {:?}", err);
        }
    }

    // Single flush for the entire batch instead of one per message
    if let Err(err) = nc.flush().await {
        log::error!("failed to flush NATS batch: {:?}", err);
    }
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
