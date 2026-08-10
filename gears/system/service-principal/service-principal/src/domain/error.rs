//! Domain error model for the service-principal REST facade.
//!
//! Fail-closed. Each variant carries the typed data its `#[error(...)]` message
//! needs; the human-readable message is never assembled at the call site. Mapped
//! to a canonical `Problem` at the REST boundary in `api::rest::error`.

use service_principal_sdk::ServicePrincipalFailure;
use thiserror::Error;
use toolkit_macros::domain_model;

/// Errors raised by the service-principal domain layer.
#[domain_model]
#[derive(Debug, Error)]
pub enum DomainError {
    /// The SPI rejected the input with no state retained (bad name, scope not in
    /// allowlist, quota exceeded, client id taken) → `400`.
    #[error("invalid input: {detail}")]
    InvalidInput {
        /// Human-readable detail of the invalid-input rejection.
        detail: String,
        /// The offending field, when the SPI attributes one.
        field: Option<String>,
    },

    /// The addressed principal does not exist within the tenant → `404`.
    /// (revoke treats this as success-equivalent before conversion — see `service`.)
    #[error("service principal not found")]
    NotFound,

    /// The PDP denied the request (or its constraints failed to compile) → `403`.
    #[error("access denied")]
    AccessDenied,

    /// No SPI provider is registered in the `ClientHub` → `503`.
    #[error("service-principal provider unavailable")]
    ProviderUnavailable,

    /// A clean upstream failure or a PDP evaluation failure — no state retained,
    /// retry is harmless → `503`.
    #[error("upstream unavailable: {detail}")]
    Upstream {
        /// Human-readable detail of the upstream failure.
        detail: String,
    },

    /// Transport uncertainty — the vendor may have retained state → `409`.
    /// A naive retry would hit `InvalidInput` ("name taken"); recovery for a
    /// create is revoke + create, which `409` signals over `503`'s retry-same.
    #[error("upstream outcome ambiguous: {detail}")]
    Ambiguous {
        /// Human-readable detail of the ambiguous outcome.
        detail: String,
    },
}

/// Maximum length (bytes, post-ASCII-filtering) of a provider `detail` string
/// allowed to reach the wire. Long enough for a useful operator sentence,
/// short enough to bound the size of anything an adapter might accidentally
/// paste into `detail` (e.g. a truncated stack trace).
const MAX_PROVIDER_DETAIL_LEN: usize = 200;

/// Marker appended when `detail` is truncated, so callers can tell the text
/// was cut rather than assume it ended naturally.
const TRUNCATION_MARKER: &str = "...(truncated)";

/// Sanitize a provider-supplied `detail` string before it can reach the wire.
///
/// `ServicePrincipalFailure::{CleanFailure,Ambiguous}` are constructed by
/// out-of-tree adapters we do not control; nothing upstream of this boundary
/// guarantees `detail` is free of control characters, ANSI escapes, or
/// internal diagnostics (hostnames, connection strings, stack traces). This
/// is the last point before `detail` is embedded in an RFC-9457 `Problem`
/// response (see `api::rest::error`), so it is where we fail closed: keep
/// only ASCII graphic characters, map whitespace runs to a single space, and
/// drop everything else — including legitimate non-ASCII text. Rejecting
/// non-ASCII outright is an intentional extra defense against Unicode
/// obfuscation (bidi override characters, zero-width characters) smuggled in
/// by an out-of-tree adapter, not merely a byte-level control-character
/// filter. The caller logs the sanitized text alongside the original length
/// (via `tracing::warn!`) before this function's result reaches the wire, so
/// operators retain a bounded diagnostic without ever persisting raw,
/// untrusted adapter text in logs.
fn sanitize_provider_detail(detail: &str) -> String {
    // Map every non-graphic whitespace character (tab, newline, CR, ...) to a
    // plain space first, so runs of them collapse below instead of gluing
    // adjacent words together; drop every other non-printable-ASCII byte
    // outright (control characters, ANSI escape bytes, non-ASCII bytes).
    let printable: String = detail
        .chars()
        .filter_map(|c| {
            if c.is_ascii_graphic() {
                Some(c)
            } else if c.is_whitespace() {
                Some(' ')
            } else {
                None
            }
        })
        .collect();
    let collapsed = printable.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= MAX_PROVIDER_DETAIL_LEN {
        return collapsed;
    }
    // The string is now ASCII-only (filtered above), so every byte index is
    // also a char boundary — slicing at `MAX_PROVIDER_DETAIL_LEN` is safe.
    let mut truncated = collapsed[..MAX_PROVIDER_DETAIL_LEN].to_owned();
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

/// General SPI-failure → domain mapping. NOTE: `revoke` handles `NotFound` as
/// success *before* calling this (idempotent delete), so this blanket mapping is
/// only reached for the non-idempotent operations.
impl From<ServicePrincipalFailure> for DomainError {
    fn from(err: ServicePrincipalFailure) -> Self {
        match err {
            ServicePrincipalFailure::InvalidInput { detail, field } => {
                // `detail` and `field` are both wire-visible violation
                // metadata originating from the same untrusted adapter
                // boundary as `CleanFailure`/`Ambiguous`, so both are routed
                // through the same sanitizer before reaching `DomainError`.
                Self::InvalidInput {
                    detail: sanitize_provider_detail(&detail),
                    field: field.map(|f| sanitize_provider_detail(&f)),
                }
            }
            ServicePrincipalFailure::NotFound { .. } => Self::NotFound,
            ServicePrincipalFailure::CleanFailure { detail } => {
                // Never log the raw, untrusted adapter text: log the
                // sanitized text plus the original length instead, so
                // operators can still gauge signal (and detect truncation)
                // without secrets or control bytes ever reaching the logs.
                let sanitized = sanitize_provider_detail(&detail);
                tracing::warn!(
                    provider_detail = %sanitized,
                    original_len = detail.len(),
                    "service-principal provider reported a clean failure"
                );
                Self::Upstream { detail: sanitized }
            }
            ServicePrincipalFailure::Ambiguous { detail } => {
                let sanitized = sanitize_provider_detail(&detail);
                tracing::warn!(
                    provider_detail = %sanitized,
                    original_len = detail.len(),
                    "service-principal provider reported an ambiguous outcome"
                );
                Self::Ambiguous { detail: sanitized }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use service_principal_sdk::ServicePrincipalFailure as F;

    use super::*;

    #[test]
    fn maps_sdk_failures_to_domain() {
        assert!(matches!(
            DomainError::from(F::InvalidInput {
                detail: "bad".into(),
                field: Some("name".into())
            }),
            DomainError::InvalidInput { field: Some(_), .. }
        ));
        assert!(matches!(
            DomainError::from(F::NotFound { detail: "x".into() }),
            DomainError::NotFound
        ));
        assert!(matches!(
            DomainError::from(F::CleanFailure { detail: "x".into() }),
            DomainError::Upstream { .. }
        ));
        assert!(matches!(
            DomainError::from(F::Ambiguous { detail: "x".into() }),
            DomainError::Ambiguous { .. }
        ));
    }

    #[test]
    fn sanitize_passes_through_normal_text() {
        assert_eq!(
            sanitize_provider_detail("vendor rejected the request: quota exceeded"),
            "vendor rejected the request: quota exceeded"
        );
    }

    #[test]
    fn sanitize_strips_control_characters_and_ansi_escapes() {
        // \u{1b} is the ESC byte that opens an ANSI escape sequence and
        // \u{7} (BEL) and NUL are other control bytes that must be dropped
        // outright, while whitespace-class control characters (\n, \r) are
        // collapsed into a single separating space rather than glued away.
        let raw = "conn\u{1b}[31mfailed\u{7}\n\rretry\0now";
        let cleaned = sanitize_provider_detail(raw);
        assert!(!cleaned.chars().any(char::is_control));
        assert_eq!(cleaned, "conn[31mfailed retrynow");
    }

    #[test]
    fn sanitize_collapses_whitespace_runs() {
        assert_eq!(
            sanitize_provider_detail("too   many\t\tspaces"),
            "too many spaces"
        );
    }

    #[test]
    fn sanitize_truncates_overlong_detail_with_marker() {
        let raw = "x".repeat(500);
        let cleaned = sanitize_provider_detail(&raw);
        assert!(cleaned.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            cleaned.len(),
            MAX_PROVIDER_DETAIL_LEN + TRUNCATION_MARKER.len()
        );
    }

    #[test]
    fn sanitize_empty_input_is_empty() {
        assert_eq!(sanitize_provider_detail(""), "");
    }

    #[test]
    fn sanitize_all_control_chars_is_empty() {
        let raw = "\0\u{1}\u{7}\u{1b}\u{7f}";
        assert_eq!(sanitize_provider_detail(raw), "");
    }

    #[test]
    fn sanitize_exactly_at_max_len_is_unchanged() {
        let raw = "x".repeat(MAX_PROVIDER_DETAIL_LEN);
        let cleaned = sanitize_provider_detail(&raw);
        assert_eq!(cleaned, raw);
        assert!(!cleaned.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn sanitize_one_over_max_len_is_truncated_with_marker() {
        let raw = "x".repeat(MAX_PROVIDER_DETAIL_LEN + 1);
        let cleaned = sanitize_provider_detail(&raw);
        assert!(cleaned.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            cleaned,
            format!(
                "{}{}",
                "x".repeat(MAX_PROVIDER_DETAIL_LEN),
                TRUNCATION_MARKER
            )
        );
    }

    #[test]
    fn invalid_input_conversion_sanitizes_detail_and_field() {
        let dirty_detail = format!("bad\u{1b}[0mname\n{}", "z".repeat(500));
        let dirty_field = format!("na\u{7}me\t{}", "f".repeat(500));

        let domain_err = DomainError::from(F::InvalidInput {
            detail: dirty_detail,
            field: Some(dirty_field),
        });

        let DomainError::InvalidInput { detail, field } = domain_err else {
            panic!("expected InvalidInput variant");
        };
        assert!(!detail.contains('\u{1b}'));
        assert!(detail.ends_with(TRUNCATION_MARKER));

        let field = field.expect("field must be preserved");
        assert!(!field.contains('\u{7}'));
        assert!(field.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn clean_failure_and_ambiguous_conversions_sanitize_detail() {
        let raw = format!("secret=abc123\u{1b}[0m\n{}", "y".repeat(500));

        let upstream = DomainError::from(F::CleanFailure {
            detail: raw.clone(),
        });
        let DomainError::Upstream { detail } = upstream else {
            panic!("expected Upstream variant");
        };
        assert!(!detail.contains('\u{1b}'));
        assert!(detail.ends_with(TRUNCATION_MARKER));

        let ambiguous = DomainError::from(F::Ambiguous { detail: raw });
        let DomainError::Ambiguous { detail } = ambiguous else {
            panic!("expected Ambiguous variant");
        };
        assert!(!detail.contains('\u{1b}'));
        assert!(detail.ends_with(TRUNCATION_MARKER));
    }
}
