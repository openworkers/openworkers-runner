//! The size limit of a request body, on the buffered and the streamed path.

use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use once_cell::sync::Lazy;

pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;

/// The limit of a worker upload to the dashboard: the 30 MiB that nginx let
/// through on that route.
pub const UPLOAD_MAX_BODY_BYTES: usize = 30 * 1024 * 1024;

/// A request extension that raises the body limit of one request above
/// MAX_REQUEST_BODY_BYTES.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyLimit(pub usize);

/// The largest request body the runner takes, in bytes: MAX_REQUEST_BODY_BYTES,
/// or the default. A buffered body is held whole in memory and copied into
/// the worker, so the limit bounds both. A value that is not valid stops the
/// runner when it first reads the limit.
pub static MAX_REQUEST_BODY_BYTES: Lazy<usize> = Lazy::new(|| {
    let value = std::env::var("MAX_REQUEST_BODY_BYTES").ok();

    limit_from(value.as_deref()).unwrap_or_else(|error| panic!("{error}"))
});

/// The limit that a MAX_REQUEST_BODY_BYTES value gives: a count of bytes
/// above 0, or the default when the variable is not set.
pub fn limit_from(value: Option<&str>) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_MAX_REQUEST_BODY_BYTES);
    };

    match value.trim().parse::<usize>() {
        Ok(limit) if limit > 0 => Ok(limit),
        _ => Err(format!(
            "MAX_REQUEST_BODY_BYTES is not a count of bytes above 0: '{value}'"
        )),
    }
}

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
    fn the_default_limit_is_10_mib_and_a_value_replaces_it() {
        assert_eq!(limit_from(None), Ok(10 * 1024 * 1024));
        assert_eq!(limit_from(Some("1048576")), Ok(1024 * 1024));
        assert_eq!(limit_from(Some(" 42 ")), Ok(42));
    }

    #[test]
    fn a_limit_that_is_not_a_count_above_0_is_refused() {
        for value in ["", "0", "-1", "10MB", "1.5", "99999999999999999999999"] {
            assert!(limit_from(Some(value)).is_err(), "{value:?}");
        }
    }

    #[tokio::test]
    async fn a_25_mib_body_passes_the_upload_limit_only() {
        let body = || Full::new(Bytes::from(vec![0u8; 25 * 1024 * 1024]));

        assert_eq!(
            read(body(), UPLOAD_MAX_BODY_BYTES).await.unwrap().len(),
            25 * 1024 * 1024
        );
        assert_eq!(
            read(body(), limit_from(None).unwrap()).await,
            Err(BodyError::TooLarge)
        );
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
