/// Returns the first 8 characters of an ID for readable logs
pub fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

/// The first 2 KiB of a guest-supplied string, marked when it was cut.
///
/// A worker decides how long its SQL is, and an OTLP record above the gRPC
/// message limit is refused forever, taking its whole batch with it.
pub fn truncated(text: &str) -> std::borrow::Cow<'_, str> {
    const LIMIT: usize = 2048;

    if text.len() <= LIMIT {
        return std::borrow::Cow::Borrowed(text);
    }

    let mut end = LIMIT;

    while !text.is_char_boundary(end) {
        end -= 1;
    }

    std::borrow::Cow::Owned(format!("{}... [{} bytes]", &text[..end], text.len()))
}

#[cfg(test)]
mod tests {
    use super::truncated;

    #[test]
    fn a_short_string_is_left_alone() {
        assert_eq!(truncated("select 1"), "select 1");
    }

    #[test]
    fn a_long_string_is_cut_and_says_so() {
        let long = "x".repeat(5000);

        assert_eq!(
            truncated(&long),
            format!("{}... [5000 bytes]", "x".repeat(2048))
        );
    }

    #[test]
    fn a_cut_between_the_bytes_of_one_character_steps_back() {
        // Three bytes each: 2048 is not a multiple of 3, so the limit lands
        // inside the 683rd character and the cut has to step back to 2046.
        let long = "\u{20ac}".repeat(2000);
        let out = truncated(&long);

        assert!(out.ends_with("... [6000 bytes]"));
        assert_eq!(
            out.matches('\u{20ac}').count(),
            682,
            "cut before the character the limit fell inside"
        );
    }
}
