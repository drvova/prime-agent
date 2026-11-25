//! Header helpers.

/// Convert a header map into a plain record with lowercase keys.
#[expect(
    dead_code,
    reason = "the SDK header conversion is kept for the providers still to port"
)]
pub fn headers_to_record(
    headers: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
        .collect()
}
