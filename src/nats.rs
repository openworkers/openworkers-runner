use base64::engine::general_purpose::STANDARD;
use base64::engine::Engine;

pub async fn nats_connect() -> async_nats::Client {
    let nats_servers = std::env::var("NATS_SERVERS").expect("NATS_SERVERS must be set");

    log::debug!("connecting to nats: {nats_servers}");

    let conn = match std::env::var("NATS_CREDS") {
        Ok(credentials) => {
            // Decode the base64 encoded credentials
            let credentials: Vec<u8> = STANDARD
                .decode(credentials)
                .expect("failed to decode credentials");
            let credentials_str =
                String::from_utf8(credentials).expect("failed to convert credentials to string");

            async_nats::ConnectOptions::with_credentials(&credentials_str)
                .expect("failed to create nats options")
                .connect(&nats_servers)
                .await
        }
        Err(_) => async_nats::connect(&nats_servers).await,
    };

    conn.expect("failed to connect to nats")
}
