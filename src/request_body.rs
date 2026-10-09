//! The size limit of a request body, on the buffered and the streamed path.

use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use once_cell::sync::Lazy;

/// The largest request body the runner takes, in bytes: MAX_REQUEST_BODY_BYTES,
/// 10 MiB by default. A buffered body is held whole in memory and copied
/// into the worker, so the limit bounds both.
pub static MAX_REQUEST_BODY_BYTES: Lazy<usize> = Lazy::new(|| {
    std::env::var("MAX_REQUEST_BODY_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10 * 1024 * 1024)
});

#[derive(Debug, PartialEq, Eq)]
pub enum BodyError {
    TooLarge,
    Read(String),
}

/// Whether the content-length header announces more than `limit` bytes, so
/// the request can be refused before its body is read.
pub fn announces_more_than(headers: &hyper::HeaderMap, limit: usize) -> bool {
    headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
}

/// Reads a whole body of at most `limit` bytes.
pub async fn read<B>(body: B, limit: usize) -> Result<Bytes, BodyError>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.is::<LengthLimitError>() => Err(BodyError::TooLarge),
        Err(error) => Err(BodyError::Read(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    #[tokio::test]
    async fn a_body_up_to_the_limit_is_read_and_one_byte_more_is_refused() {
        let at_limit = Full::new(Bytes::from(vec![b'a'; 16]));
        let over_limit = Full::new(Bytes::from(vec![b'a'; 17]));

        assert_eq!(read(at_limit, 16).await.unwrap().len(), 16);
        assert_eq!(read(over_limit, 16).await, Err(BodyError::TooLarge));
    }

    #[test]
    fn a_content_length_above_the_limit_is_refused_before_the_body() {
        let mut headers = hyper::HeaderMap::new();
        assert!(!announces_more_than(&headers, 16));

        headers.insert(hyper::header::CONTENT_LENGTH, "16".parse().unwrap());
        assert!(!announces_more_than(&headers, 16));

        headers.insert(hyper::header::CONTENT_LENGTH, "17".parse().unwrap());
        assert!(announces_more_than(&headers, 16));
    }
}
