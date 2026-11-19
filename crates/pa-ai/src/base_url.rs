//! Request-time base URL placeholders.
//!
//! The catalog names per-account path segments as `{NAME}` placeholders (the Cloudflare
//! transports), so its `(provider, api, baseUrl)` pinning tuples stay fixed. Every outbound
//! path fills them here, after pinning, from the environment.

use std::borrow::Cow;

/// Environment variables a base URL may name as `{NAME}` placeholders.
const PLACEHOLDERS: [&str; 2] = ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"];

/// Fill the base URL's placeholders from the environment. A missing variable is an error naming
/// it, so no URL that still carries a placeholder reaches the network. Values become path
/// segments, so anything beyond letters, digits, `-` and `_` is refused rather than allowed to
/// reshape the URL. Error messages name variables only, never their values.
pub(crate) fn resolve_placeholders(base_url: &str) -> Result<Cow<'_, str>, String> {
    let mut resolved = Cow::Borrowed(base_url);
    for name in PLACEHOLDERS {
        let placeholder = format!("{{{name}}}");
        if !resolved.contains(&placeholder) {
            continue;
        }
        let value = std::env::var(name).unwrap_or_default();
        let value = value.trim();
        if value.is_empty() {
            return Err(format!(
                "needs the {name} environment variable to fill its base URL"
            ));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "needs {name} to hold only letters, digits, '-' and '_'"
            ));
        }
        resolved = Cow::Owned(resolved.replace(&placeholder, value));
    }
    Ok(resolved)
}
