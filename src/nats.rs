use base64::{engine::general_purpose::STANDARD, Engine as _};

pub fn nats_connect() -> nats::Connection {
    let nats_servers = std::env::var("NATS_SERVERS").expect("NATS_SERVERS must be set");

    log::debug!("connecting to nats: {nats_servers}");
    
    let conn = match std::env::var("NATS_CREDS") {
        Ok(credentials) => {
          // Decode the base64 encoded credentials
          let credentials: Vec<u8> = STANDARD.decode(credentials).expect("failed to decode credentials");
          let credentials = String::from_utf8(credentials).expect("failed to convert credentials to string");
          let options = nats::Options::with_static_credentials(&credentials).expect("failed to create nats options");
        
          options.connect(nats_servers)
        },
        Err(_) => nats::connect(nats_servers),
    };

    conn.expect("failed to connect to nats")
}
