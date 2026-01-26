use openworkers_core::LogEvent;
use std::time::Duration;

pub enum LogMessage {
    Log { worker_id: String, event: LogEvent },
    FlushWorker { worker_id: String }, // Signal to flush logs for a specific worker
}

// Batching configuration
const BATCH_SIZE: usize = 10; // Flush after 10 messages
const BATCH_TIMEOUT_MS: u64 = 100; // Or flush every 100ms

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
            log::info!(
                "Global log publisher started (batching: {}msg/{}ms)",
                BATCH_SIZE,
                BATCH_TIMEOUT_MS
            );

            let mut batch: Vec<LogMessage> = Vec::with_capacity(BATCH_SIZE);
            let timeout = Duration::from_millis(BATCH_TIMEOUT_MS);

            loop {
                // Try to receive a message with timeout
                match rx.recv_timeout(timeout) {
                    Ok(log_msg) => {
                        match log_msg {
                            LogMessage::Log { .. } => {
                                batch.push(log_msg);

                                // Drain all immediately available messages
                                while batch.len() < BATCH_SIZE {
                                    match rx.try_recv() {
                                        Ok(msg) => match msg {
                                            LogMessage::Log { .. } => batch.push(msg),
                                            LogMessage::FlushWorker { worker_id } => {
                                                // Flush logs for this worker immediately
                                                flush_worker(&nc, &mut batch, &worker_id).await;
                                            }
                                        },
                                        Err(_) => break,
                                    }
                                }

                                // Flush if batch is full
                                if batch.len() >= BATCH_SIZE {
                                    flush_batch(&nc, &mut batch).await;
                                }
                            }
                            LogMessage::FlushWorker { worker_id } => {
                                // Flush all pending logs for this specific worker
                                flush_worker(&nc, &mut batch, &worker_id).await;
                            }
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
    if batch.is_empty() {
        return;
    }

    log::debug!("Flushing batch of {} log messages", batch.len());

    for log_msg in batch.drain(..) {
        if let LogMessage::Log { worker_id, event } = log_msg {
            // Also log locally for debugging
            log::debug!(
                "[worker:{}] [{}] {}",
                &worker_id[..8.min(worker_id.len())],
                event.level,
                event.message
            );

            let level = event.level.to_string();
            let subject = format!("{}.console.{}", worker_id, level);

            if let Err(err) = nc.publish(subject, event.message.into()).await {
                log::error!("failed to publish log to NATS: {:?}", err);
            }
        }
    }

    // Single flush for the entire batch instead of one per message
    if let Err(err) = nc.flush().await {
        log::error!("failed to flush NATS batch: {:?}", err);
    }
}

async fn flush_worker(nc: &async_nats::Client, batch: &mut Vec<LogMessage>, worker_id: &str) {
    // Extract and flush only logs from this specific worker
    let (worker_logs, remaining): (Vec<_>, Vec<_>) = batch
        .drain(..)
        .partition(|msg| matches!(msg, LogMessage::Log { worker_id: id, .. } if id == worker_id));

    if !worker_logs.is_empty() {
        log::debug!(
            "Flushing {} logs for worker {}",
            worker_logs.len(),
            crate::utils::short_id(worker_id)
        );

        for log_msg in worker_logs {
            if let LogMessage::Log { worker_id, event } = log_msg {
                // Also log locally for debugging
                log::debug!(
                    "[worker:{}] [{}] {}",
                    &worker_id[..8.min(worker_id.len())],
                    event.level,
                    event.message
                );

                let level = event.level.to_string();
                let subject = format!("{}.console.{}", worker_id, level);

                if let Err(err) = nc.publish(subject, event.message.into()).await {
                    log::error!("failed to publish log to NATS: {:?}", err);
                }
            }
        }

        // Flush only this worker's logs
        if let Err(err) = nc.flush().await {
            log::error!("failed to flush worker logs: {:?}", err);
        }
    }

    // Put back remaining logs
    *batch = remaining;
}

pub struct WorkerLogHandler {
    tx: std::sync::mpsc::Sender<LogEvent>,
    worker_id: String,
    global_tx: std::sync::mpsc::Sender<LogMessage>,
    forwarder_handle: Option<std::thread::JoinHandle<()>>,
}

impl WorkerLogHandler {
    /// Signal that this worker is done and logs should be flushed
    pub fn flush(self) {
        log::debug!(
            "Flushing logs for worker: {}",
            crate::utils::short_id(&self.worker_id)
        );
        // Drop tx first to close the channel
        drop(self.tx);
        // Wait for forwarder thread to finish forwarding all pending logs
        if let Some(handle) = self.forwarder_handle {
            let _ = handle.join();
        }
        // Then send flush signal
        let _ = self.global_tx.send(LogMessage::FlushWorker {
            worker_id: self.worker_id,
        });
    }
}

// Create a worker-specific log handler that sends directly to global publisher
pub fn create_log_handler(
    worker_id: String,
    global_tx: std::sync::mpsc::Sender<LogMessage>,
) -> (std::sync::mpsc::Sender<LogEvent>, WorkerLogHandler) {
    let (tx, rx) = std::sync::mpsc::channel::<LogEvent>();

    let worker_id_clone = worker_id.clone();
    let global_tx_clone = global_tx.clone();

    // Forwarder thread: forwards logs from worker channel to global publisher
    let forwarder_handle = std::thread::spawn(move || {
        for event in rx {
            if let Err(e) = global_tx_clone.send(LogMessage::Log {
                worker_id: worker_id_clone.clone(),
                event,
            }) {
                log::error!("Failed to send log to global publisher: {:?}", e);
                break;
            }
        }
    });

    let handler = WorkerLogHandler {
        tx: tx.clone(),
        worker_id,
        global_tx,
        forwarder_handle: Some(forwarder_handle),
    };

    (tx, handler)
}
