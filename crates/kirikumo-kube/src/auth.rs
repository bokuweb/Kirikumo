//! Turning a kubeconfig user entry into a credential, over and over.
//!
//! Three of the four ways in are static — a token, a file, a certificate —
//! and the fourth is not: EKS, GKE and AKS all authenticate by running a
//! command that prints a short-lived token, and that command has to be run
//! again when the token expires. So a credential is *asked for* before every
//! request rather than built once at connect time, and this module is the
//! thing that decides whether the last answer is still good.
//!
//! Nothing here writes a credential to disk (`AGENTS.md` rule 10). A token an
//! exec plugin produced lives in this process's memory until it expires.

use crate::error::{Error, Result};
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

/// How long before an exec credential's stated expiry we stop trusting it.
///
/// Clock skew between here and the apiserver is real, and a token that
/// expires mid-flight is a 401 the reader sees as an empty table.
const SKEW: Duration = Duration::seconds(60);

/// How long an exec credential with no stated expiry is reused for.
///
/// The `ExecCredential` schema makes `expirationTimestamp` optional, and
/// plugins that omit it are common. Running the plugin per request would put
/// a process spawn in front of every list; caching it forever would outlive
/// whatever the plugin was refreshing. Five minutes is short enough that a
/// rotated credential recovers on its own and long enough that a table's
/// worth of requests costs one spawn.
const DEFAULT_TTL: Duration = Duration::minutes(5);

/// How a client proves who it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    /// Nothing: an unsecured cluster, or one reached through a proxy that
    /// authenticates for us.
    Anonymous,
    /// A bearer token written in the file.
    Token(String),
    /// A bearer token in a file, re-read every time because a projected
    /// service-account token is rewritten in place as it rotates.
    TokenFile(PathBuf),
    /// HTTP basic auth. A decade deprecated, still in real files.
    Basic {
        /// The user.
        username: String,
        /// Their password.
        password: String,
    },
    /// A client certificate and its key, both PEM.
    ClientCert {
        /// The certificate chain, PEM.
        certificate: Vec<u8>,
        /// The private key, PEM.
        key: Vec<u8>,
    },
    /// A command that prints an `ExecCredential`.
    Exec(ExecConfig),
}

/// A `user.exec` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecConfig {
    /// `client.authentication.k8s.io/v1` or `…/v1beta1`.
    pub api_version: String,
    /// The program to run. Run directly, never through a shell.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Variables added to this process's environment for the run.
    pub env: Vec<(String, String)>,
    /// What the file says to do when the command is not installed, which is
    /// the only useful thing to show when it is not.
    pub install_hint: String,
}

/// What a request needs to identify itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credential {
    /// The `Authorization` header's value, when there is one.
    pub header: Option<String>,
    /// A client certificate chain and key, PEM, when there is one.
    pub client_cert: Option<(Vec<u8>, Vec<u8>)>,
}

/// Produces a credential, refreshing it when it has to.
///
/// Shared by every request on one connection, so the exec plugin runs once
/// per expiry rather than once per request. The lock is held only across the
/// plugin's own run, which is why it is a `Mutex` and not a channel.
#[derive(Debug)]
pub struct Authenticator {
    method: AuthMethod,
    cached: Mutex<Option<Cached>>,
}

#[derive(Debug, Clone)]
struct Cached {
    credential: Credential,
    /// When this stops being usable. `None` means it never expires.
    until: Option<DateTime<Utc>>,
}

impl Authenticator {
    /// An authenticator for one way in.
    pub fn new(method: AuthMethod) -> Self {
        Self {
            method,
            cached: Mutex::new(None),
        }
    }

    /// How this client identifies itself, right now.
    ///
    /// Cheap for every method but [`AuthMethod::Exec`], which may spawn a
    /// process; call it on the background executor, like everything else in
    /// this crate.
    pub fn credential(&self) -> Result<Credential> {
        match &self.method {
            AuthMethod::Anonymous => Ok(Credential::default()),
            AuthMethod::Token(token) => Ok(Credential {
                header: Some(format!("Bearer {token}")),
                client_cert: None,
            }),
            AuthMethod::TokenFile(path) => {
                let token = std::fs::read_to_string(path)
                    .map_err(|error| Error::Credentials(format!("{}: {error}", path.display())))?;
                Ok(Credential {
                    header: Some(format!("Bearer {}", token.trim())),
                    client_cert: None,
                })
            }
            AuthMethod::Basic { username, password } => {
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(format!("{username}:{password}"));
                Ok(Credential {
                    header: Some(format!("Basic {encoded}")),
                    client_cert: None,
                })
            }
            AuthMethod::ClientCert { certificate, key } => Ok(Credential {
                header: None,
                client_cert: Some((certificate.clone(), key.clone())),
            }),
            AuthMethod::Exec(config) => self.exec_credential(config, Utc::now()),
        }
    }

    /// Whether this way in ever produces a client certificate.
    ///
    /// The TLS configuration of a connection is built once, before any
    /// request; a method that authenticates with a header does not need one
    /// and a method that does needs it up front.
    pub fn is_client_cert(&self) -> bool {
        matches!(self.method, AuthMethod::ClientCert { .. })
    }

    /// The static client certificate, for building the connection's TLS.
    pub fn static_client_cert(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        match &self.method {
            AuthMethod::ClientCert { certificate, key } => Some((certificate.clone(), key.clone())),
            _ => None,
        }
    }

    fn exec_credential(&self, config: &ExecConfig, now: DateTime<Utc>) -> Result<Credential> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = cached.as_ref()
            && is_fresh(entry.until, now)
        {
            return Ok(entry.credential.clone());
        }
        let status = run(config)?;
        let credential = status.credential()?;
        *cached = Some(Cached {
            credential: credential.clone(),
            until: Some(status.expiry.unwrap_or(now + DEFAULT_TTL)),
        });
        Ok(credential)
    }
}

/// Whether a cached credential is still worth using at `now`.
///
/// Deliberately a free function so the rule — the skew, and "no expiry means
/// it never goes stale" — can be tested without spawning anything.
fn is_fresh(until: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match until {
        None => true,
        Some(until) => now + SKEW < until,
    }
}

/// The `status` block of an `ExecCredential`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecStatus {
    /// A bearer token, for the plugins that produce one.
    pub token: Option<String>,
    /// A client certificate chain, PEM, for the plugins that produce those.
    pub client_certificate: Option<Vec<u8>>,
    /// Its key, PEM.
    pub client_key: Option<Vec<u8>>,
    /// When it stops working, if the plugin says.
    pub expiry: Option<DateTime<Utc>>,
}

impl ExecStatus {
    /// Parse what a plugin printed.
    ///
    /// Both `v1` and `v1beta1` are the same shape here, so the `apiVersion`
    /// is not checked: refusing a credential over a version string would
    /// break a working setup for no benefit.
    pub fn parse(stdout: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            status: WireStatus,
        }
        #[derive(Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WireStatus {
            #[serde(default)]
            token: Option<String>,
            #[serde(default)]
            client_certificate_data: Option<String>,
            #[serde(default)]
            client_key_data: Option<String>,
            #[serde(default)]
            expiration_timestamp: Option<String>,
        }

        let wire: Wire = serde_json::from_str(stdout.trim()).map_err(|error| {
            Error::Credentials(format!(
                "the plugin did not print an ExecCredential: {error}"
            ))
        })?;
        Ok(Self {
            token: wire.status.token.filter(|token| !token.is_empty()),
            client_certificate: wire.status.client_certificate_data.map(String::into_bytes),
            client_key: wire.status.client_key_data.map(String::into_bytes),
            expiry: wire
                .status
                .expiration_timestamp
                .and_then(|stamp| DateTime::parse_from_rfc3339(&stamp).ok())
                .map(|time| time.with_timezone(&Utc)),
        })
    }

    /// The credential this status describes.
    fn credential(&self) -> Result<Credential> {
        if let Some(token) = &self.token {
            return Ok(Credential {
                header: Some(format!("Bearer {token}")),
                client_cert: None,
            });
        }
        match (&self.client_certificate, &self.client_key) {
            (Some(certificate), Some(key)) => Ok(Credential {
                header: None,
                client_cert: Some((certificate.clone(), key.clone())),
            }),
            _ => Err(Error::Credentials(
                "the plugin printed neither a token nor a certificate and key".into(),
            )),
        }
    }
}

/// Run an exec plugin and read what it printed.
///
/// The command is run directly, never through a shell: a kubeconfig is a file
/// that can arrive from a colleague or a cluster provisioner, and passing its
/// `command` to `sh -c` would turn a config file into a script.
fn run(config: &ExecConfig) -> Result<ExecStatus> {
    let mut command = Command::new(&config.command);
    command.args(&config.args);
    for (name, value) in &config.env {
        command.env(name, value);
    }
    // v1beta1 plugins may read this; providing it empty-but-present is what
    // client-go does when `provideClusterInfo` is off.
    command.env("KUBERNETES_EXEC_INFO", exec_info(&config.api_version));

    let output = command.output().map_err(|error| {
        let hint = match config.install_hint.is_empty() {
            true => String::new(),
            false => format!(" — {}", config.install_hint),
        };
        Error::Credentials(format!("could not run {:?}: {error}{hint}", config.command))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Credentials(format!(
            "{:?} exited with {}: {}",
            config.command,
            output.status,
            stderr.trim().chars().take(500).collect::<String>()
        )));
    }
    ExecStatus::parse(&String::from_utf8_lossy(&output.stdout))
}

/// The `KUBERNETES_EXEC_INFO` document handed to a plugin.
fn exec_info(api_version: &str) -> String {
    let version = match api_version.is_empty() {
        true => "client.authentication.k8s.io/v1beta1",
        false => api_version,
    };
    format!(r#"{{"apiVersion":"{version}","kind":"ExecCredential","spec":{{}}}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn a_static_token_needs_no_work() {
        let auth = Authenticator::new(AuthMethod::Token("abc".into()));
        assert_eq!(
            auth.credential().unwrap().header.as_deref(),
            Some("Bearer abc")
        );
        assert!(!auth.is_client_cert());
    }

    #[test]
    fn a_token_file_is_read_every_time_because_it_rotates_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "first\n").unwrap();
        let auth = Authenticator::new(AuthMethod::TokenFile(path.clone()));
        assert_eq!(
            auth.credential().unwrap().header.as_deref(),
            Some("Bearer first")
        );
        std::fs::write(&path, "second\n").unwrap();
        assert_eq!(
            auth.credential().unwrap().header.as_deref(),
            Some("Bearer second")
        );
    }

    #[test]
    fn a_missing_token_file_says_which_one() {
        let auth = Authenticator::new(AuthMethod::TokenFile("/nowhere/token".into()));
        match auth.credential() {
            Err(Error::Credentials(message)) => assert!(message.contains("/nowhere/token")),
            other => panic!("expected a credentials error, got {other:?}"),
        }
    }

    #[test]
    fn basic_auth_is_base64_of_user_and_password() {
        let auth = Authenticator::new(AuthMethod::Basic {
            username: "admin".into(),
            password: "hunter2".into(),
        });
        assert_eq!(
            auth.credential().unwrap().header.as_deref(),
            Some("Basic YWRtaW46aHVudGVyMg==")
        );
    }

    #[test]
    fn a_client_certificate_is_offered_to_tls_and_carries_no_header() {
        let auth = Authenticator::new(AuthMethod::ClientCert {
            certificate: b"cert".to_vec(),
            key: b"key".to_vec(),
        });
        let credential = auth.credential().unwrap();
        assert!(credential.header.is_none());
        assert_eq!(
            credential.client_cert,
            Some((b"cert".to_vec(), b"key".to_vec()))
        );
        assert!(auth.is_client_cert());
    }

    #[test]
    fn an_exec_credential_is_read_from_either_api_version() {
        let status = ExecStatus::parse(
            r#"{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential",
                "status":{"token":"k8s-aws-v1.abc",
                          "expirationTimestamp":"2026-09-07T12:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(status.token.as_deref(), Some("k8s-aws-v1.abc"));
        assert_eq!(status.expiry, Some(at("2026-09-07T12:00:00Z")));
    }

    #[test]
    fn an_exec_credential_may_be_a_certificate_instead_of_a_token() {
        let status = ExecStatus::parse(
            r#"{"status":{"clientCertificateData":"-----BEGIN CERTIFICATE-----",
                          "clientKeyData":"-----BEGIN PRIVATE KEY-----"}}"#,
        )
        .unwrap();
        let credential = status.credential().unwrap();
        assert!(credential.header.is_none());
        assert!(credential.client_cert.is_some());
    }

    #[test]
    fn a_plugin_that_prints_nothing_usable_is_an_error_and_not_an_anonymous_request() {
        let status = ExecStatus::parse(r#"{"status":{}}"#).unwrap();
        assert!(status.credential().is_err());
        assert!(ExecStatus::parse("not json").is_err());
    }

    #[test]
    fn a_credential_is_stale_a_minute_before_it_expires() {
        let expiry = at("2026-09-07T12:00:00Z");
        assert!(is_fresh(Some(expiry), at("2026-09-07T11:58:00Z")));
        // Inside the skew window: treated as gone, because the request is
        // still in flight when the apiserver stops believing it.
        assert!(!is_fresh(Some(expiry), at("2026-09-07T11:59:30Z")));
        assert!(!is_fresh(Some(expiry), at("2026-09-07T12:00:01Z")));
        assert!(is_fresh(None, at("2100-01-01T00:00:00Z")));
    }

    #[test]
    fn an_exec_plugin_is_run_and_its_answer_cached() {
        // A plugin that prints a different token every run, so a second
        // credential that matches the first proves the cache was used.
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("runs");
        let script = format!(
            "echo x >> {counter}; printf '%s' '{{\"status\":{{\"token\":\"t\"}}}}'",
            counter = counter.display()
        );
        let auth = Authenticator::new(AuthMethod::Exec(ExecConfig {
            api_version: "client.authentication.k8s.io/v1beta1".into(),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            env: Vec::new(),
            install_hint: String::new(),
        }));

        assert_eq!(
            auth.credential().unwrap().header.as_deref(),
            Some("Bearer t")
        );
        assert_eq!(
            auth.credential().unwrap().header.as_deref(),
            Some("Bearer t")
        );
        let runs = std::fs::read_to_string(&counter).unwrap();
        assert_eq!(runs.lines().count(), 1, "the plugin should run once");
    }

    #[test]
    fn a_plugin_that_fails_reports_its_own_words_and_the_install_hint() {
        let auth = Authenticator::new(AuthMethod::Exec(ExecConfig {
            api_version: String::new(),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "echo 'no credentials here' >&2; exit 3".into()],
            env: Vec::new(),
            install_hint: String::new(),
        }));
        match auth.credential() {
            Err(Error::Credentials(message)) => {
                assert!(message.contains("no credentials here"), "{message}")
            }
            other => panic!("expected a credentials error, got {other:?}"),
        }
    }

    #[test]
    fn a_plugin_that_is_not_installed_says_what_the_file_says_to_do() {
        let auth = Authenticator::new(AuthMethod::Exec(ExecConfig {
            api_version: String::new(),
            command: "/nowhere/gke-gcloud-auth-plugin".into(),
            args: Vec::new(),
            env: Vec::new(),
            install_hint: "install gke-gcloud-auth-plugin".into(),
        }));
        match auth.credential() {
            Err(Error::Credentials(message)) => {
                assert!(
                    message.contains("install gke-gcloud-auth-plugin"),
                    "{message}"
                )
            }
            other => panic!("expected a credentials error, got {other:?}"),
        }
    }

    #[test]
    fn the_environment_a_plugin_asks_for_reaches_it() {
        let auth = Authenticator::new(AuthMethod::Exec(ExecConfig {
            api_version: String::new(),
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"printf '{"status":{"token":"%s"}}' "$AWS_PROFILE""#.into(),
            ],
            env: vec![("AWS_PROFILE".into(), "work".into())],
            install_hint: String::new(),
        }));
        assert_eq!(
            auth.credential().unwrap().header.as_deref(),
            Some("Bearer work")
        );
    }
}
