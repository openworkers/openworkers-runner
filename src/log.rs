use openworkers_runtime::LogEvent;

pub fn create_log_handler(worker_id: String) -> std::sync::mpsc::Sender<LogEvent> {
  let (tx, rx) = std::sync::mpsc::channel::<LogEvent>();

  std::thread::spawn(move || {
    let nc = crate::nats::nats_connect();

    for event in rx {
      log::debug!("{:?}", event);

      let level = event.level.to_string();
      
      match nc.publish(format!("{worker_id}.console.{level}").as_str(), event.message) {
        Ok(()) => {}
        Err(err) => log::error!("failed to publish log event: {:?}", err),
      };
    }

    log::debug!("log handler stopped");
  });

  tx
}
