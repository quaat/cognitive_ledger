//! Projection error taxonomy (ADR-0020): every failure is retryable or permanent and carries
//! a stable code that is recorded on the stream and exported as a metric label. Messages
//! never contain endpoints, credentials or response bodies.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorClass {
    /// Transient: retry with backoff.
    Retryable,
    /// Configuration or protocol: the stream blocks until an operator acts.
    Permanent,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProjectionErrorCode {
    TargetUnavailable,
    TargetTimeout,
    TargetThrottled,
    TargetServerError,
    TargetAuth,
    TargetProtocol,
    TargetNotTransactional,
    NamedGraphUnsupported,
    StateTooLarge,
    MarkerAhead,
    VerificationFailed,
    InvalidTargetGraph,
    LedgerUnavailable,
    LedgerState,
    /// The target holds another stream's projection in this stream's cognitive graph, or
    /// the dataset is bound to another target id.
    TargetConflict,
}

impl ProjectionErrorCode {
    /// Stable, bounded code stored in `projection_state.last_error_code`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TargetUnavailable => "TARGET_UNAVAILABLE",
            Self::TargetTimeout => "TARGET_TIMEOUT",
            Self::TargetThrottled => "TARGET_THROTTLED",
            Self::TargetServerError => "TARGET_SERVER_ERROR",
            Self::TargetAuth => "TARGET_AUTH",
            Self::TargetProtocol => "TARGET_PROTOCOL",
            Self::TargetNotTransactional => "TARGET_NOT_TRANSACTIONAL",
            Self::NamedGraphUnsupported => "NAMED_GRAPH_UNSUPPORTED",
            Self::StateTooLarge => "STATE_TOO_LARGE",
            Self::MarkerAhead => "MARKER_AHEAD",
            Self::VerificationFailed => "VERIFICATION_FAILED",
            Self::InvalidTargetGraph => "INVALID_TARGET_GRAPH",
            Self::LedgerUnavailable => "LEDGER_UNAVAILABLE",
            Self::LedgerState => "LEDGER_STATE",
            Self::TargetConflict => "TARGET_CONFLICT",
        }
    }

    pub const ALL: [Self; 15] = [
        Self::TargetUnavailable,
        Self::TargetTimeout,
        Self::TargetThrottled,
        Self::TargetServerError,
        Self::TargetAuth,
        Self::TargetProtocol,
        Self::TargetNotTransactional,
        Self::NamedGraphUnsupported,
        Self::StateTooLarge,
        Self::MarkerAhead,
        Self::VerificationFailed,
        Self::InvalidTargetGraph,
        Self::LedgerUnavailable,
        Self::LedgerState,
        Self::TargetConflict,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub struct ProjectionError {
    class: ErrorClass,
    code: ProjectionErrorCode,
    message: String,
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl ProjectionError {
    pub fn retryable(code: ProjectionErrorCode, message: impl Into<String>) -> Self {
        Self {
            class: ErrorClass::Retryable,
            code,
            message: message.into(),
        }
    }

    pub fn permanent(code: ProjectionErrorCode, message: impl Into<String>) -> Self {
        Self {
            class: ErrorClass::Permanent,
            code,
            message: message.into(),
        }
    }

    pub fn class(&self) -> ErrorClass {
        self.class
    }

    pub fn code(&self) -> ProjectionErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_retryable(&self) -> bool {
        self.class == ErrorClass::Retryable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_bounded_tokens() {
        let mut seen = std::collections::HashSet::new();
        for code in ProjectionErrorCode::ALL {
            let s = code.as_str();
            assert!(seen.insert(s), "{s} duplicated");
            assert!(
                s.len() <= 64 && s.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
                "{s}"
            );
        }
    }
}
