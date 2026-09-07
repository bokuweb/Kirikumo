//! What can go wrong between here and an apiserver.
//!
//! The variants exist to be *told apart by a caller*, not to be exhaustive:
//! a `Gone` restarts a watch, a `Forbidden` greys a control, an `Unsupported`
//! hides a whole tab, and everything else is a sentence for the reader.

use std::fmt;

/// The result of anything that talks to a cluster.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure reaching or reading a cluster.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No kubeconfig was found, or it named no contexts.
    #[error("no kubeconfig context: {0}")]
    NoContext(String),

    /// The kubeconfig is not readable, or not a kubeconfig.
    #[error("kubeconfig: {0}")]
    Config(String),

    /// A credential could not be produced: an exec plugin that failed, a
    /// token file that is not there, a key that is not a key.
    #[error("credentials: {0}")]
    Credentials(String),

    /// The connection failed, or TLS did.
    #[error("could not reach the cluster: {0}")]
    Transport(String),

    /// The apiserver answered, and said no.
    #[error("{status} from the apiserver: {message}")]
    Api {
        /// The HTTP status.
        status: u16,
        /// The `Status` object's message, or the body.
        message: String,
        /// The `Status` object's `reason`, when it sent one.
        reason: String,
    },

    /// The client is not allowed to do this.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// There is nothing there.
    #[error("not found: {0}")]
    NotFound(String),

    /// A watch's `resourceVersion` has aged out of the apiserver's window;
    /// the caller must list again and start a new watch (roadmap §4.7).
    #[error("the watch is too old and must be restarted")]
    Gone,

    /// The answer was not the shape it should have been.
    #[error("unexpected answer from the cluster: {0}")]
    Malformed(String),

    /// This implementation, or this cluster, does not do that.
    #[error("not supported here")]
    Unsupported,
}

impl Error {
    /// Turn an HTTP status and body into the most specific variant it fits.
    ///
    /// Kubernetes answers a failure with a `Status` object carrying `message`
    /// and `reason`, which is far more useful than the status line; when it
    /// does not, the body stands in.
    pub fn from_status(status: u16, body: &str) -> Self {
        let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
        let field = |name: &str| -> String {
            parsed
                .as_ref()
                .and_then(|value| value.get(name))
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let mut message = field("message");
        let reason = field("reason");
        if message.is_empty() {
            message = body.trim().chars().take(500).collect();
        }
        match status {
            403 => Self::Forbidden(message),
            404 => Self::NotFound(message),
            // 410 is the watch's own failure: the version we asked to resume
            // from is older than the apiserver still remembers.
            410 => Self::Gone,
            _ => Self::Api {
                status,
                message,
                reason,
            },
        }
    }

    /// Whether retrying could plausibly succeed, which is what a watch's
    /// backoff needs to know: a transport hiccup is worth another go, a 403
    /// is not.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Api { status, .. } => *status >= 500 || *status == 429,
            _ => false,
        }
    }
}

impl From<ureq::Error> for Error {
    fn from(error: ureq::Error) -> Self {
        Self::Transport(error.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Transport(error.to_string())
    }
}

/// A short label for a failure, for a header or a tooltip.
///
/// The full message belongs in the body of a panel; a strip has room for a
/// word.
pub struct ShortError<'a>(pub &'a Error);

impl fmt::Display for ShortError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Error::NoContext(_) => write!(f, "no context"),
            Error::Config(_) => write!(f, "kubeconfig"),
            Error::Credentials(_) => write!(f, "credentials"),
            Error::Transport(_) => write!(f, "unreachable"),
            Error::Forbidden(_) => write!(f, "forbidden"),
            Error::NotFound(_) => write!(f, "not found"),
            Error::Gone => write!(f, "restarting"),
            Error::Api { status, .. } => write!(f, "{status}"),
            Error::Malformed(_) => write!(f, "unreadable"),
            Error::Unsupported => write!(f, "unsupported"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_object_is_preferred_to_the_status_line() {
        let body = r#"{"kind":"Status","status":"Failure",
            "message":"pods \"api\" not found","reason":"NotFound","code":404}"#;
        match Error::from_status(404, body) {
            Error::NotFound(message) => assert!(message.contains("api")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn a_body_that_is_not_a_status_still_says_something() {
        match Error::from_status(502, "<html>bad gateway</html>") {
            Error::Api {
                status, message, ..
            } => {
                assert_eq!(status, 502);
                assert!(message.contains("bad gateway"));
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn four_ten_is_the_watch_restarting_and_not_an_error_to_show() {
        assert!(matches!(Error::from_status(410, "{}"), Error::Gone));
    }

    #[test]
    fn only_the_failures_worth_retrying_say_so() {
        assert!(Error::Transport("dns".into()).is_retryable());
        assert!(Error::from_status(503, "{}").is_retryable());
        assert!(Error::from_status(429, "{}").is_retryable());
        assert!(!Error::from_status(403, "{}").is_retryable());
        assert!(!Error::Gone.is_retryable());
    }
}
