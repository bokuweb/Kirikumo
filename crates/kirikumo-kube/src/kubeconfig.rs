//! Reading, merging and resolving a kubeconfig.
//!
//! `KUBECONFIG` is a `:`-separated list and it *merges*, first-wins: the
//! first file to name a context, cluster or user is the one that defines it,
//! and `current-context` comes from the first file that sets one. That rule
//! is `kubectl`'s, and getting it wrong means opening on someone else's
//! cluster, so it is the first thing tested here.
//!
//! Paths inside a file — a CA bundle, a client key, a token file — are
//! resolved relative to *that file's* directory, not to the working
//! directory, which is why every entry remembers where it came from.
//!
//! Nothing here writes. This app never edits a kubeconfig
//! (`AGENTS.md` rule 10).

use crate::auth::{AuthMethod, ExecConfig};
use crate::error::{Error, Result};
use crate::model::ContextRef;
use base64::Engine as _;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where to look for a kubeconfig, in order of authority.
///
/// `KIRIKUMO_KUBECONFIG` first, so a test or a second profile can keep out of
/// the real one; then `KUBECONFIG`, which is a list; then `~/.kube/config`.
/// An entry that is empty is skipped, because `KUBECONFIG=":/a"` is a thing
/// shells produce.
pub fn kubeconfig_paths() -> Vec<PathBuf> {
    if let Some(value) = std::env::var_os("KIRIKUMO_KUBECONFIG") {
        return split_paths(&value);
    }
    if let Some(value) = std::env::var_os("KUBECONFIG")
        && !value.is_empty()
    {
        return split_paths(&value);
    }
    match dirs::home_dir() {
        Some(home) => vec![home.join(".kube").join("config")],
        None => Vec::new(),
    }
}

fn split_paths(value: &std::ffi::OsStr) -> Vec<PathBuf> {
    std::env::split_paths(value)
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

/// A merged kubeconfig.
#[derive(Debug, Clone, Default)]
pub struct KubeConfig {
    clusters: BTreeMap<String, Entry<ClusterSpec>>,
    users: BTreeMap<String, Entry<UserSpec>>,
    contexts: BTreeMap<String, Entry<ContextSpec>>,
    /// The order contexts appeared in, because a picker sorted by a
    /// `BTreeMap`'s keys would not be the order the file lists them and
    /// people navigate their own file by memory.
    order: Vec<String>,
    current: Option<String>,
}

/// One named entry, and the file it came from.
#[derive(Debug, Clone)]
struct Entry<T> {
    spec: T,
    /// The directory the defining file sits in; relative paths hang off it.
    base: PathBuf,
}

/// What it takes to reach one cluster, resolved from a context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterAccess {
    /// The apiserver's address, without a trailing slash.
    pub server: String,
    /// PEM roots to verify the server against; empty means the system's.
    pub roots: Vec<Vec<u8>>,
    /// Whether the context turns verification off. Honoured, and said out
    /// loud in the window (`docs/ui.md` §3.2).
    pub insecure: bool,
    /// The name to send in TLS SNI and to verify against, when the file
    /// overrides it (`tls-server-name`).
    pub server_name: Option<String>,
    /// How to prove who we are.
    pub auth: AuthMethod,
    /// The namespace the context defaults to.
    pub namespace: Option<String>,
}

impl KubeConfig {
    /// Read and merge every file that exists, in order.
    ///
    /// A file that is missing is skipped, because `KUBECONFIG` routinely
    /// names one that is not there. A file that exists and is not a
    /// kubeconfig is an error, because silently ignoring it would open the
    /// window on the wrong cluster.
    pub fn load(paths: &[PathBuf]) -> Result<Self> {
        let mut merged = Self::default();
        for path in paths {
            let text = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(Error::Config(format!("{}: {error}", path.display())));
                }
            };
            let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            let file = Self::parse(&text, &base)
                .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?;
            merged.absorb(file);
        }
        Ok(merged)
    }

    /// Parse one file, with the directory its relative paths hang off.
    pub fn parse(text: &str, base: &Path) -> Result<Self> {
        let file: ConfigFile =
            serde_norway::from_str(text).map_err(|error| Error::Config(error.to_string()))?;
        let mut config = Self {
            current: file.current_context.filter(|name| !name.is_empty()),
            ..Self::default()
        };
        for named in file.clusters {
            config.clusters.entry(named.name).or_insert(Entry {
                spec: named.cluster,
                base: base.to_path_buf(),
            });
        }
        for named in file.users {
            config.users.entry(named.name).or_insert(Entry {
                spec: named.user,
                base: base.to_path_buf(),
            });
        }
        for named in file.contexts {
            if config.contexts.contains_key(&named.name) {
                continue;
            }
            config.order.push(named.name.clone());
            config.contexts.insert(
                named.name,
                Entry {
                    spec: named.context,
                    base: base.to_path_buf(),
                },
            );
        }
        Ok(config)
    }

    /// Merge another file *under* this one: first-wins, as `kubectl` does.
    fn absorb(&mut self, other: Self) {
        for (name, entry) in other.clusters {
            self.clusters.entry(name).or_insert(entry);
        }
        for (name, entry) in other.users {
            self.users.entry(name).or_insert(entry);
        }
        for name in other.order {
            if let Some(entry) = other.contexts.get(&name)
                && !self.contexts.contains_key(&name)
            {
                self.order.push(name.clone());
                self.contexts.insert(name, entry.clone());
            }
        }
        if self.current.is_none() {
            self.current = other.current;
        }
    }

    /// Every context, in the order the files list them.
    pub fn contexts(&self) -> Vec<ContextRef> {
        self.order
            .iter()
            .filter_map(|name| {
                let entry = self.contexts.get(name)?;
                let cluster = self.clusters.get(&entry.spec.cluster);
                Some(ContextRef {
                    name: name.clone(),
                    cluster: entry.spec.cluster.clone(),
                    user: entry.spec.user.clone().filter(|user| !user.is_empty()),
                    namespace: entry
                        .spec
                        .namespace
                        .clone()
                        .filter(|namespace| !namespace.is_empty()),
                    server: cluster
                        .map(|cluster| cluster.spec.server.clone())
                        .unwrap_or_default(),
                    insecure: cluster.is_some_and(|cluster| cluster.spec.insecure),
                })
            })
            .collect()
    }

    /// The context the file says to open on.
    ///
    /// Falls back to the first context there is: a file with contexts and no
    /// `current-context` is common enough (a merged `KUBECONFIG` where the
    /// first file sets none), and opening on *something* beats opening on the
    /// no-cluster screen.
    pub fn current_context(&self) -> Option<&str> {
        self.current
            .as_deref()
            .filter(|name| self.contexts.contains_key(*name))
            .or_else(|| self.order.first().map(String::as_str))
    }

    /// Resolve a context into what it takes to reach its cluster.
    pub fn access(&self, context: &str) -> Result<ClusterAccess> {
        let entry = self
            .contexts
            .get(context)
            .ok_or_else(|| Error::NoContext(format!("no context named {context:?}")))?;
        let cluster = self.clusters.get(&entry.spec.cluster).ok_or_else(|| {
            Error::Config(format!(
                "context {context:?} names cluster {:?}, which is not in the file",
                entry.spec.cluster
            ))
        })?;
        let mut roots = Vec::new();
        if let Some(data) = &cluster.spec.certificate_authority_data {
            roots.push(decode(data, "certificate-authority-data")?);
        } else if let Some(path) = &cluster.spec.certificate_authority {
            roots.push(read_file(&cluster.base, path)?);
        }
        let user = entry
            .spec
            .user
            .as_ref()
            .filter(|name| !name.is_empty())
            .and_then(|name| self.users.get(name));
        let auth = match user {
            Some(user) => user.spec.method(&user.base)?,
            None => AuthMethod::Anonymous,
        };
        Ok(ClusterAccess {
            server: cluster.spec.server.trim_end_matches('/').to_string(),
            roots,
            insecure: cluster.spec.insecure,
            server_name: cluster
                .spec
                .tls_server_name
                .clone()
                .filter(|name| !name.is_empty()),
            auth,
            namespace: entry
                .spec
                .namespace
                .clone()
                .filter(|namespace| !namespace.is_empty()),
        })
    }
}

/// Decode a base64 field, naming it when it is not base64.
fn decode(data: &str, field: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|error| Error::Config(format!("{field} is not base64: {error}")))
}

/// Read a file named by a kubeconfig, resolving it against the file's own
/// directory the way `kubectl` does.
fn read_file(base: &Path, path: &str) -> Result<Vec<u8>> {
    let resolved = resolve(base, path);
    std::fs::read(&resolved)
        .map_err(|error| Error::Config(format!("{}: {error}", resolved.display())))
}

/// A path from a kubeconfig, made absolute against the file's directory.
pub(crate) fn resolve(base: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(shellexpand_home(path));
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// Expand a leading `~`, which kubeconfigs written by hand routinely carry.
fn shellexpand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest).to_string_lossy().into_owned(),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

// The wire shapes. Deliberately lenient — `deny_unknown_fields` would refuse
// a kubeconfig carrying an extension this app does not know about, and real
// files are full of them (`extensions`, cloud-provider blocks, `preferences`).

#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(rename = "current-context")]
    current_context: Option<String>,
    #[serde(default)]
    clusters: Vec<Named<ClusterSpec>>,
    #[serde(default)]
    users: Vec<NamedUser>,
    #[serde(default)]
    contexts: Vec<NamedContext>,
}

#[derive(Debug, Deserialize)]
struct Named<T> {
    name: String,
    cluster: T,
}

#[derive(Debug, Deserialize)]
struct NamedUser {
    name: String,
    #[serde(default)]
    user: UserSpec,
}

#[derive(Debug, Deserialize)]
struct NamedContext {
    name: String,
    context: ContextSpec,
}

#[derive(Debug, Clone, Deserialize)]
struct ClusterSpec {
    #[serde(default)]
    server: String,
    #[serde(default, rename = "certificate-authority")]
    certificate_authority: Option<String>,
    #[serde(default, rename = "certificate-authority-data")]
    certificate_authority_data: Option<String>,
    #[serde(default, rename = "insecure-skip-tls-verify")]
    insecure: bool,
    #[serde(default, rename = "tls-server-name")]
    tls_server_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ContextSpec {
    #[serde(default)]
    cluster: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UserSpec {
    #[serde(default, rename = "client-certificate")]
    client_certificate: Option<String>,
    #[serde(default, rename = "client-certificate-data")]
    client_certificate_data: Option<String>,
    #[serde(default, rename = "client-key")]
    client_key: Option<String>,
    #[serde(default, rename = "client-key-data")]
    client_key_data: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default, rename = "tokenFile")]
    token_file: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    exec: Option<ExecSpec>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExecSpec {
    #[serde(default, rename = "apiVersion")]
    api_version: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<ExecEnv>,
    #[serde(default, rename = "installHint")]
    install_hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExecEnv {
    name: String,
    #[serde(default)]
    value: String,
}

impl UserSpec {
    /// Which of the ways in this entry describes.
    ///
    /// The order is `kubectl`'s: an exec plugin wins, because a file that has
    /// both an expired token and a plugin to refresh it must use the plugin;
    /// then client certificates, then a token, then a token file, then basic
    /// auth, which is a decade deprecated and still in real files.
    fn method(&self, base: &Path) -> Result<AuthMethod> {
        if let Some(exec) = &self.exec {
            return Ok(AuthMethod::Exec(ExecConfig {
                api_version: exec.api_version.clone(),
                command: exec.command.clone(),
                args: exec.args.clone(),
                env: exec
                    .env
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.value.clone()))
                    .collect(),
                install_hint: exec.install_hint.clone().unwrap_or_default(),
            }));
        }
        let certificate = match (&self.client_certificate_data, &self.client_certificate) {
            (Some(data), _) => Some(decode(data, "client-certificate-data")?),
            (None, Some(path)) => Some(read_file(base, path)?),
            (None, None) => None,
        };
        let key = match (&self.client_key_data, &self.client_key) {
            (Some(data), _) => Some(decode(data, "client-key-data")?),
            (None, Some(path)) => Some(read_file(base, path)?),
            (None, None) => None,
        };
        match (certificate, key) {
            (Some(certificate), Some(key)) => {
                return Ok(AuthMethod::ClientCert { certificate, key });
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(Error::Credentials(
                    "a client certificate without its key, or the other way round".into(),
                ));
            }
            (None, None) => {}
        }
        if let Some(token) = self.token.as_ref().filter(|token| !token.is_empty()) {
            return Ok(AuthMethod::Token(token.clone()));
        }
        if let Some(path) = self.token_file.as_ref().filter(|path| !path.is_empty()) {
            return Ok(AuthMethod::TokenFile(resolve(base, path)));
        }
        if let Some(username) = self.username.as_ref().filter(|name| !name.is_empty()) {
            return Ok(AuthMethod::Basic {
                username: username.clone(),
                password: self.password.clone().unwrap_or_default(),
            });
        }
        Ok(AuthMethod::Anonymous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIND: &str = r#"
apiVersion: v1
kind: Config
current-context: kind-dev
clusters:
- name: kind-dev
  cluster:
    server: https://127.0.0.1:6443/
    certificate-authority-data: Y2E=
- name: homelab
  cluster:
    server: https://10.0.0.2:6443
    insecure-skip-tls-verify: true
contexts:
- name: kind-dev
  context:
    cluster: kind-dev
    user: kind-dev
    namespace: default
- name: homelab
  context:
    cluster: homelab
    user: homelab
users:
- name: kind-dev
  user:
    client-certificate-data: Y2VydA==
    client-key-data: a2V5
- name: homelab
  user:
    token: sha256~abc
"#;

    fn parse(text: &str) -> KubeConfig {
        KubeConfig::parse(text, Path::new("/tmp/kube")).unwrap()
    }

    #[test]
    fn contexts_come_out_in_the_order_the_file_lists_them() {
        let config = parse(KIND);
        let names: Vec<_> = config
            .contexts()
            .into_iter()
            .map(|context| context.name)
            .collect();
        assert_eq!(names, vec!["kind-dev", "homelab"]);
        assert_eq!(config.current_context(), Some("kind-dev"));
    }

    #[test]
    fn a_context_carries_the_server_and_whether_it_is_insecure() {
        let contexts = parse(KIND).contexts();
        assert_eq!(contexts[0].server, "https://127.0.0.1:6443/");
        assert!(!contexts[0].insecure);
        assert!(contexts[1].insecure);
        assert_eq!(contexts[0].namespace.as_deref(), Some("default"));
        assert_eq!(contexts[1].namespace, None);
    }

    #[test]
    fn a_trailing_slash_on_the_server_is_dropped_so_paths_do_not_double_up() {
        let access = parse(KIND).access("kind-dev").unwrap();
        assert_eq!(access.server, "https://127.0.0.1:6443");
    }

    #[test]
    fn client_certificates_win_and_arrive_decoded() {
        let access = parse(KIND).access("kind-dev").unwrap();
        assert_eq!(access.roots, vec![b"ca".to_vec()]);
        match access.auth {
            AuthMethod::ClientCert { certificate, key } => {
                assert_eq!(certificate, b"cert");
                assert_eq!(key, b"key");
            }
            other => panic!("expected a client certificate, got {other:?}"),
        }
    }

    #[test]
    fn a_token_is_read_and_an_insecure_cluster_says_so() {
        let access = parse(KIND).access("homelab").unwrap();
        assert!(access.insecure);
        assert_eq!(access.auth, AuthMethod::Token("sha256~abc".into()));
    }

    #[test]
    fn half_a_client_certificate_is_refused_rather_than_ignored() {
        let text = r#"
clusters: [{name: c, cluster: {server: https://x}}]
contexts: [{name: c, context: {cluster: c, user: u}}]
users: [{name: u, user: {client-certificate-data: Y2VydA==}}]
"#;
        assert!(parse(text).access("c").is_err());
    }

    #[test]
    fn an_exec_plugin_wins_over_a_token_that_is_already_in_the_file() {
        let text = r#"
clusters: [{name: c, cluster: {server: https://x}}]
contexts: [{name: c, context: {cluster: c, user: u}}]
users:
- name: u
  user:
    token: stale
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: aws
      args: [eks, get-token]
      env: [{name: AWS_PROFILE, value: work}]
"#;
        match parse(text).access("c").unwrap().auth {
            AuthMethod::Exec(exec) => {
                assert_eq!(exec.command, "aws");
                assert_eq!(exec.args, vec!["eks", "get-token"]);
                assert_eq!(exec.env, vec![("AWS_PROFILE".into(), "work".into())]);
            }
            other => panic!("expected an exec plugin, got {other:?}"),
        }
    }

    #[test]
    fn a_relative_path_hangs_off_the_file_it_was_written_in() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ca.pem"), b"root").unwrap();
        let text = r#"
clusters: [{name: c, cluster: {server: https://x, certificate-authority: ca.pem}}]
contexts: [{name: c, context: {cluster: c}}]
"#;
        let config = KubeConfig::parse(text, dir.path()).unwrap();
        assert_eq!(config.access("c").unwrap().roots, vec![b"root".to_vec()]);
    }

    #[test]
    fn merging_is_first_wins_and_current_context_comes_from_the_first_file_to_set_one() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::write(
            &first,
            r#"
clusters: [{name: shared, cluster: {server: https://first}}]
contexts: [{name: shared, context: {cluster: shared}}, {name: only-first, context: {cluster: shared}}]
"#,
        )
        .unwrap();
        std::fs::write(
            &second,
            r#"
current-context: only-second
clusters: [{name: shared, cluster: {server: https://second}}]
contexts: [{name: shared, context: {cluster: shared}}, {name: only-second, context: {cluster: shared}}]
"#,
        )
        .unwrap();

        let config = KubeConfig::load(&[first, second]).unwrap();
        let names: Vec<_> = config.contexts().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["shared", "only-first", "only-second"]);
        // The first file defines the shared cluster…
        assert_eq!(config.access("shared").unwrap().server, "https://first");
        // …and the first file to set a current context wins, which here is
        // the second, because the first sets none.
        assert_eq!(config.current_context(), Some("only-second"));
    }

    #[test]
    fn a_missing_file_in_the_list_is_skipped_and_a_broken_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::write(&real, "contexts: [{name: c, context: {cluster: c}}]").unwrap();
        let config = KubeConfig::load(&[dir.path().join("absent"), real.clone()]).unwrap();
        assert_eq!(config.contexts().len(), 1);

        let broken = dir.path().join("broken");
        std::fs::write(&broken, "\tnot: [valid").unwrap();
        assert!(KubeConfig::load(&[broken]).is_err());
    }

    #[test]
    fn a_file_with_contexts_and_no_current_one_opens_on_the_first() {
        let config =
            parse("contexts: [{name: a, context: {cluster: a}}, {name: b, context: {cluster: b}}]");
        assert_eq!(config.current_context(), Some("a"));
    }

    #[test]
    fn a_current_context_naming_something_absent_is_not_believed() {
        let config = parse("current-context: gone\ncontexts: [{name: a, context: {cluster: a}}]");
        assert_eq!(config.current_context(), Some("a"));
    }

    #[test]
    fn an_empty_config_has_nothing_and_says_so_when_asked_for_a_context() {
        let config = parse("apiVersion: v1\nkind: Config\n");
        assert!(config.contexts().is_empty());
        assert_eq!(config.current_context(), None);
        assert!(matches!(
            config.access("anything"),
            Err(Error::NoContext(_))
        ));
    }

    #[test]
    fn a_context_naming_a_cluster_that_is_not_there_is_an_error_with_both_names_in_it() {
        let config = parse("contexts: [{name: c, context: {cluster: missing}}]");
        match config.access("c") {
            Err(Error::Config(message)) => {
                assert!(message.contains("\"c\""), "{message}");
                assert!(message.contains("\"missing\""), "{message}");
            }
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn extensions_and_unknown_blocks_do_not_stop_a_file_from_loading() {
        let config = parse(
            r#"
apiVersion: v1
kind: Config
preferences: {colors: true}
clusters:
- name: c
  cluster:
    server: https://x
    extensions: [{name: cloud, extension: {provider: gke}}]
contexts: [{name: c, context: {cluster: c}}]
"#,
        );
        assert_eq!(config.contexts().len(), 1);
    }
}
