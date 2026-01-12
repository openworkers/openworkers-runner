/// Returns the first 8 characters of an ID for readable logs
pub fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}
