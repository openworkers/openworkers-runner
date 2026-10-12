//! Durable Object ids: 32 bytes in hex. The first 16 bytes name the object,
//! and the last 16 are a MAC of the first 16 under the key of the namespace,
//! so a namespace refuses the ids of every other one.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Bytes of each half of an id.
const PART: usize = 16;

/// The key a namespace makes its ids with.
#[derive(Clone, PartialEq, Eq)]
pub struct IdKey(pub [u8; 32]);

impl std::fmt::Debug for IdKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IdKey(..)")
    }
}

fn mac(key: &IdKey, label: &[u8], data: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC takes a key of any size");
    mac.update(label);
    mac.update(&[0]);
    mac.update(data);
    mac
}

fn with_tag(key: &IdKey, body: &[u8]) -> String {
    let tag = mac(key, b"id", body).finalize().into_bytes();

    let mut id = body.to_vec();
    id.extend_from_slice(&tag[..PART]);

    hex::encode(id)
}

/// The id of the object of `name`: the same for each call.
pub fn from_name(key: &IdKey, name: &str) -> String {
    let body = mac(key, b"name", name.as_bytes()).finalize().into_bytes();

    with_tag(key, &body[..PART])
}

/// A new id, which no name gives.
pub fn unique(key: &IdKey) -> String {
    with_tag(key, uuid::Uuid::new_v4().as_bytes())
}

/// The id in `text`, in lower case, if this namespace made it.
pub fn parse(key: &IdKey, text: &str) -> Option<String> {
    if text.len() != PART * 4 {
        return None;
    }

    let bytes = hex::decode(text).ok()?;
    let (body, tag) = bytes.split_at(PART);

    mac(key, b"id", body).verify_truncated_left(tag).ok()?;

    Some(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: IdKey = IdKey([7; 32]);
    const OTHER: IdKey = IdKey([8; 32]);

    #[test]
    fn a_name_gives_one_id_of_64_hex_digits() {
        let id = from_name(&KEY, "room");

        assert_eq!(id, from_name(&KEY, "room"));
        assert_ne!(id, from_name(&KEY, "other room"));
        assert_ne!(id, from_name(&OTHER, "room"));
        assert_eq!(id.len(), 64);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn unique_ids_differ() {
        assert_ne!(unique(&KEY), unique(&KEY));
    }

    #[test]
    fn the_namespace_takes_back_its_own_ids_only() {
        let named = from_name(&KEY, "room");
        let new = unique(&KEY);

        assert_eq!(parse(&KEY, &named), Some(named.clone()));
        assert_eq!(parse(&KEY, &new), Some(new.clone()));
        assert_eq!(parse(&KEY, &named.to_uppercase()), Some(named.clone()));

        assert_eq!(parse(&OTHER, &named), None);
        assert_eq!(parse(&OTHER, &new), None);
    }

    #[test]
    fn a_changed_id_is_refused() {
        let id = from_name(&KEY, "room");

        // One bit off in the body, then in the tag
        for at in [0, 63] {
            let mut bytes = id.clone().into_bytes();
            bytes[at] = if bytes[at] == b'0' { b'1' } else { b'0' };

            assert_eq!(parse(&KEY, &String::from_utf8(bytes).unwrap()), None);
        }

        for text in ["", "room", &id[..62], &format!("{id}00"), &"zz".repeat(32)] {
            assert_eq!(parse(&KEY, text), None, "{text}");
        }
    }
}
