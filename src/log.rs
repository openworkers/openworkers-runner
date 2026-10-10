//! The console lines of the workers. A live log stream gets each line when
//! the worker writes it; the logs table gets the lines in batches.

use chrono::{DateTime, Utc};
use openworkers_core::{LogEvent, LogLevel};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use tokio::sync::{broadcast, mpsc};

/// Lines a live stream can fall behind before the runner closes it.
const LIVE_CAPACITY: usize = 1024;

/// Lines that wait for the logs table. When the table is slower than the
/// workers, the runner drops new lines, so the queue cannot fill the memory.
const STORE_CAPACITY: usize = 10_000;

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub date: DateTime<Utc>,
    pub worker_id: String,
    pub level: LogLevel,
    pub message: String,
}

/// The name of a level in the logs table and in the log streams.
pub fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Log => "log",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    }
}

/// Takes the console lines of all workers.
#[derive(Clone)]
pub struct LogSink {
    live: broadcast::Sender<LogEntry>,
    store: mpsc::Sender<LogEntry>,
    dropped: Arc<AtomicU64>,
    /// The date of the last line, in microseconds, as the logs table keeps it.
    last_date: Arc<AtomicI64>,
}

/// The lines for the logs table, and the count of lines the sink dropped
/// because the queue was full.
pub struct LogStore {
    pub lines: mpsc::Receiver<LogEntry>,
    pub dropped: Arc<AtomicU64>,
}

impl LogSink {
    pub fn new() -> (Self, LogStore) {
        let (live, _) = broadcast::channel(LIVE_CAPACITY);
        let (store, lines) = mpsc::channel(STORE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));

        let sink = Self {
            live,
            store,
            dropped: dropped.clone(),
            last_date: Arc::new(AtomicI64::new(0)),
        };

        (sink, LogStore { lines, dropped })
    }

    pub fn send(&self, worker_id: &str, event: LogEvent) {
        let entry = LogEntry {
            date: self.next_date(),
            worker_id: worker_id.to_string(),
            level: event.level,
            message: event.message,
        };

        if self.live.receiver_count() > 0 {
            self.live.send(entry.clone()).ok();
        }

        if let Err(mpsc::error::TrySendError::Full(_)) = self.store.try_send(entry) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.live.subscribe()
    }

    /// Now, or 1 microsecond after the last line: the history sorts lines by
    /// date, and lines of one microsecond would come out in any order.
    fn next_date(&self) -> DateTime<Utc> {
        let now = Utc::now().timestamp_micros();
        let mut last = self.last_date.load(Ordering::Relaxed);

        let date = loop {
            let date = now.max(last + 1);

            match self.last_date.compare_exchange_weak(
                last,
                date,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break date,
                Err(current) => last = current,
            }
        };

        DateTime::from_timestamp_micros(date).expect("a date near now is in range")
    }
}

/// Sends the console lines of one worker to the sink.
#[derive(Clone)]
pub struct LogSender {
    worker_id: String,
    sink: LogSink,
}

impl LogSender {
    pub fn new(worker_id: String, sink: LogSink) -> Self {
        Self { worker_id, sink }
    }

    pub fn send(&self, event: LogEvent) {
        self.sink.send(&self.worker_id, event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(message: &str) -> LogEvent {
        LogEvent {
            level: LogLevel::Warn,
            message: message.to_string(),
        }
    }

    #[test]
    fn a_line_goes_to_the_live_streams_and_to_the_store() {
        let (sink, mut store) = LogSink::new();
        let mut live = sink.subscribe();

        LogSender::new("worker".to_string(), sink).send(line("hello"));

        let streamed = live.try_recv().unwrap();
        let stored = store.lines.try_recv().unwrap();

        for entry in [streamed, stored] {
            assert_eq!(entry.worker_id, "worker");
            assert_eq!(entry.level, LogLevel::Warn);
            assert_eq!(entry.message, "hello");
        }
    }

    #[test]
    fn each_line_gets_a_later_date_than_the_line_before() {
        let (sink, mut store) = LogSink::new();

        for i in 0..5000 {
            sink.send("worker", line(&i.to_string()));
        }

        let mut previous = None;

        while let Ok(entry) = store.lines.try_recv() {
            assert_eq!(
                entry.date.timestamp_subsec_nanos() % 1000,
                0,
                "whole microseconds"
            );
            assert!(
                previous < Some(entry.date),
                "{previous:?} then {}",
                entry.date
            );
            previous = Some(entry.date);
        }
    }

    #[test]
    fn a_full_store_queue_drops_new_lines_and_counts_them() {
        let (sink, mut store) = LogSink::new();

        for i in 0..STORE_CAPACITY + 3 {
            sink.send("worker", line(&i.to_string()));
        }

        assert_eq!(store.dropped.load(Ordering::Relaxed), 3);
        assert_eq!(store.lines.try_recv().unwrap().message, "0");
    }

    #[test]
    fn a_closed_store_does_not_count_as_a_drop() {
        let (sink, store) = LogSink::new();
        let dropped = store.dropped.clone();
        drop(store);

        sink.send("worker", line("lost"));

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn level_names_match_the_logs_table_enum() {
        let names: Vec<_> = [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Log,
            LogLevel::Debug,
            LogLevel::Trace,
        ]
        .into_iter()
        .map(level_name)
        .collect();

        assert_eq!(names, ["error", "warn", "info", "log", "debug", "trace"]);
    }
}
