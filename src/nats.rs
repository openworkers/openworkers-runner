use base64::engine::Engine;
use base64::engine::general_purpose::STANDARD;

pub async fn nats_connect() -> async_nats::Client {
    let nats_servers = std::env::var("NATS_SERVERS").expect("env NATS_SERVERS is required");

    log::debug!("connecting to nats: {nats_servers}");

    match std::env::var("NATS_CREDENTIALS") {
        Ok(credentials) => {
            let credentials = STANDARD
                .decode(credentials)
                .expect("env NATS_CREDENTIALS must be valid base64");
            let credentials_str =
                String::from_utf8(credentials).expect("env NATS_CREDENTIALS must be valid UTF-8");

            async_nats::ConnectOptions::with_credentials(&credentials_str)
                .expect("invalid NATS credentials format")
                .connect(&nats_servers)
                .await
                .expect("failed to connect to NATS with credentials")
        }
        Err(_) => async_nats::connect(&nats_servers)
            .await
            .expect("failed to connect to NATS"),
    }
}
