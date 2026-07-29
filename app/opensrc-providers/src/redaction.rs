pub(crate) const REDACTED_SECRET: &str = "[REDACTED]";

#[must_use]
pub(crate) fn redact_known_secret(message: &str, secret: &str) -> String {
    let secret = secret.trim();
    if secret.is_empty() {
        return message.to_string();
    }
    message.replace(secret, REDACTED_SECRET)
}

#[cfg(test)]
mod tests {
    use super::{REDACTED_SECRET, redact_known_secret};

    #[test]
    fn redacts_every_occurrence_of_a_sentinel_provider_secret() {
        const SECRET: &str = "sk-opensource-sentinel-never-log";
        let message = format!("Bearer {SECRET}; upstream echoed api_key={SECRET}");
        let redacted = redact_known_secret(&message, SECRET);

        assert!(!redacted.contains(SECRET));
        assert_eq!(redacted.matches(REDACTED_SECRET).count(), 2);
    }

    #[test]
    fn an_empty_secret_does_not_rewrite_provider_diagnostics() {
        assert_eq!(
            redact_known_secret("provider temporarily unavailable", ""),
            "provider temporarily unavailable"
        );
    }
}
