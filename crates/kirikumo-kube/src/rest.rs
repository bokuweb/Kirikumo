//! The [`Cluster`] trait over a real apiserver.
//!
//! Blocking `ureq` over rustls, run by the caller on a background thread: no
//! async runtime, which is what lets this crate be linked into a host that
//! has one of its own (`AGENTS.md` rule 3). TLS is built from the
//! kubeconfig — private roots, a client certificate, or the reader's decision
//! to skip verification — so a `kind` cluster on a self-signed certificate
//! works with no further configuration.
//!
//! Two agents, not one. Requests carry a global timeout, because a list that
//! hangs is a table that never fills; a watch must not, because a watch that
//! says nothing for an hour is a watch working exactly as designed.

use crate::auth::Authenticator;
use crate::discovery::{self, ResourceListWire};
use crate::error::{Error, Result};
use crate::kubeconfig::ClusterAccess;
use crate::logs::{Lines, LogStream};
use crate::model::{
    ApiResource, Catalogue, ClusterVersion, EventRecord, LogRequest, Metrics, Object, ObjectList,
    Patch,
};
use crate::watch::{JsonLines, WatchStream};
use crate::{Cluster, quantity};
use serde_json::Value;
use std::io::BufReader;
use std::sync::Mutex;
use std::time::Duration;
use ureq::tls::{Certificate, ClientCert, PemItem, PrivateKey, RootCerts, TlsConfig};

/// How long one ordinary request may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How long a watch asks the apiserver to hold the connection open.
///
/// Kubernetes closes a watch on a timeout of its own — five minutes to an
/// hour, jittered — and asking for a number makes the reconnect cadence ours
/// rather than the cluster's.
///
/// Five minutes, and the reason is not the reconnect: it is how long an
/// *abandoned* watch lives. The reader is a blocking read on a thread of its
/// own, and a thread parked in `read` cannot be interrupted from outside, so
/// a watch that has been told to stop only notices at its next event or at
/// this timeout. Five minutes bounds that; half an hour did not.
const WATCH_SECONDS: u32 = 300;

/// The most a single answer may be, in bytes.
///
/// `ureq` caps a body read anyway; naming the number here is what makes a
/// four-thousand-pod list work — the default is an order of magnitude too
/// small for one.
const BODY_LIMIT: u64 = 256 * 1024 * 1024;

/// Where the metrics API lives, when it is installed at all.
const METRICS_PREFIX: &str = "/apis/metrics.k8s.io/v1beta1";

/// The verbs that carry a body.
#[derive(Debug, Clone, Copy)]
enum Method {
    /// `POST`, for the access review.
    Post,
    /// `PUT`, for applying a whole object.
    Put,
    /// `PATCH`, for everything else that writes.
    Patch,
}

/// A cluster over HTTPS.
pub struct Rest {
    base: String,
    auth: Authenticator,
    access: ClusterAccess,
    agents: Mutex<Agents>,
}

/// The agents in use, and the client certificate they were built with.
///
/// An exec plugin may hand back a *certificate* rather than a token, and a
/// certificate is part of a TLS configuration rather than of a request — so
/// when the plugin rotates one, the agents have to be rebuilt. Keeping the
/// certificate that built them is how that is noticed.
struct Agents {
    client_cert: Option<Vec<u8>>,
    request: ureq::Agent,
    stream: ureq::Agent,
}

impl Rest {
    /// Connect to the cluster a context resolves to.
    ///
    /// Nothing is sent here: this builds the TLS configuration and the
    /// agents. The first request is what discovers whether the cluster is
    /// reachable, which is deliberate — a window that refuses to open cannot
    /// tell the reader which cluster is down.
    pub fn connect(access: ClusterAccess) -> Result<Self> {
        if access.server.is_empty() {
            return Err(Error::Config("the context names no server".into()));
        }
        let auth = Authenticator::new(access.auth.clone());
        let client_cert = auth.static_client_cert();
        let agents = Agents::build(&access, client_cert.as_ref())?;
        Ok(Self {
            base: access.server.clone(),
            auth,
            access,
            agents: Mutex::new(agents),
        })
    }

    /// The namespace the context defaults to, which is what the window opens
    /// on.
    pub fn default_namespace(&self) -> Option<&str> {
        self.access.namespace.as_deref()
    }

    /// Whether this connection skips certificate verification, which the
    /// window says out loud (`docs/ui.md` §3.2).
    pub fn is_insecure(&self) -> bool {
        self.access.insecure
    }

    /// The agents to use right now, rebuilt if the credential's certificate
    /// has rotated, and the `Authorization` header to send.
    fn prepare(&self) -> Result<(ureq::Agent, ureq::Agent, Option<String>)> {
        let credential = self.auth.credential()?;
        let cert = credential
            .client_cert
            .as_ref()
            .map(|(chain, _)| chain.clone());
        let mut agents = self
            .agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if agents.client_cert != cert {
            *agents = Agents::build(&self.access, credential.client_cert.as_ref())?;
        }
        Ok((
            agents.request.clone(),
            agents.stream.clone(),
            credential.header,
        ))
    }

    /// A path made absolute against the apiserver.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// One GET, as text.
    fn get_text(&self, path: &str) -> Result<String> {
        let (agent, _, header) = self.prepare()?;
        let mut request = agent
            .get(self.url(path))
            .header("Accept", "application/json");
        if let Some(header) = header {
            request = request.header("Authorization", header);
        }
        let mut response = request.call()?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()?;
        match (200..300).contains(&status) {
            true => Ok(body),
            false => Err(Error::from_status(status, &body)),
        }
    }

    /// One GET, decoded.
    fn get_json(&self, path: &str) -> Result<Value> {
        let body = self.get_text(path)?;
        serde_json::from_str(&body).map_err(|error| Error::Malformed(format!("{path}: {error}")))
    }

    /// One request that carries a body.
    ///
    /// `ureq` types its builders by whether the verb takes a body, so the
    /// three that do are dispatched here and `DELETE` — which does not —
    /// goes through [`Self::delete_path`].
    fn send(&self, method: Method, path: &str, content_type: &str, body: &Value) -> Result<String> {
        let (agent, _, header) = self.prepare()?;
        let url = self.url(path);
        let mut request = match method {
            Method::Post => agent.post(url),
            Method::Put => agent.put(url),
            Method::Patch => agent.patch(url),
        }
        .header("Accept", "application/json")
        .header("Content-Type", content_type);
        if let Some(header) = header {
            request = request.header("Authorization", header);
        }
        Self::answer(request.send(body.to_string())?)
    }

    /// One DELETE.
    fn delete_path(&self, path: &str) -> Result<String> {
        let (agent, _, header) = self.prepare()?;
        let mut request = agent
            .delete(self.url(path))
            .header("Accept", "application/json");
        if let Some(header) = header {
            request = request.header("Authorization", header);
        }
        Self::answer(request.call()?)
    }

    /// Read a response, turning a non-2xx into the `Status` it carries.
    fn answer(mut response: ureq::http::Response<ureq::Body>) -> Result<String> {
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()?;
        match (200..300).contains(&status) {
            true => Ok(body),
            false => Err(Error::from_status(status, &body)),
        }
    }

    /// Every resource list for one group version, tolerating a group that
    /// answers with an error.
    ///
    /// An aggregated apiserver that is registered and down makes `/apis`
    /// name a group that cannot be listed. `kubectl` prints a warning and
    /// carries on, and so does this: one broken extension must not cost the
    /// whole sidebar.
    fn resources_of(&self, group_version: &str) -> Vec<ApiResource> {
        let path = match group_version {
            "v1" => "/api/v1".to_string(),
            other => format!("/apis/{other}"),
        };
        match self
            .get_json(&path)
            .and_then(|value| ResourceListWire::parse(value, group_version))
        {
            Ok(resources) => resources,
            Err(error) => {
                tracing::warn!(%error, group_version, "skipping an API group that would not list");
                Vec::new()
            }
        }
    }

    /// The events involving one object.
    fn events_path(&self, uid: &str, namespace: Option<&str>) -> String {
        let selector = format!("involvedObject.uid={uid}");
        match namespace {
            Some(namespace) if !namespace.is_empty() => {
                format!("/api/v1/namespaces/{namespace}/events?fieldSelector={selector}")
            }
            _ => format!("/api/v1/events?fieldSelector={selector}"),
        }
    }

    /// Read a metrics list into [`Metrics`].
    ///
    /// A pod's use is the sum of its containers', because that is the number
    /// a person compares against the pod's requests.
    fn metrics(&self, path: &str) -> Result<Vec<Metrics>> {
        let list = ObjectList::parse(self.get_json(path)?)?;
        Ok(list
            .items
            .iter()
            .map(|object| {
                let (cpu, memory) = match object.at("usage") {
                    // A node reports one `usage` block.
                    Some(usage) => (
                        usage.get("cpu").and_then(Value::as_str).unwrap_or_default(),
                        usage
                            .get("memory")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    None => ("", ""),
                };
                let mut cpu_milli = quantity::cpu_milli(cpu).unwrap_or_default();
                let mut memory_bytes = quantity::bytes(memory).unwrap_or_default();
                // A pod reports one block per container instead.
                for container in object.array_at("containers") {
                    let usage = container.get("usage");
                    let read = |name: &str| {
                        usage
                            .and_then(|usage| usage.get(name))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    cpu_milli += quantity::cpu_milli(&read("cpu")).unwrap_or_default();
                    memory_bytes += quantity::bytes(&read("memory")).unwrap_or_default();
                }
                Metrics {
                    name: object.meta.name.clone(),
                    namespace: object.meta.namespace.clone(),
                    cpu_milli,
                    memory_bytes,
                }
            })
            .collect())
    }
}

impl Agents {
    /// Build both agents for a cluster, with a client certificate if there
    /// is one.
    fn build(access: &ClusterAccess, client_cert: Option<&(Vec<u8>, Vec<u8>)>) -> Result<Self> {
        let mut tls = TlsConfig::builder().disable_verification(access.insecure);
        if !access.roots.is_empty() {
            let mut roots = Vec::new();
            for pem in &access.roots {
                roots.extend(certificates(pem)?);
            }
            if !roots.is_empty() {
                tls = tls.root_certs(RootCerts::new_with_certs(&roots));
            }
        }
        if let Some((chain, key)) = client_cert {
            let chain = certificates(chain)?;
            if chain.is_empty() {
                return Err(Error::Credentials(
                    "the client certificate holds no certificate".into(),
                ));
            }
            let key = PrivateKey::from_pem(key)
                .map_err(|error| Error::Credentials(format!("the client key: {error}")))?;
            tls = tls.client_cert(Some(ClientCert::new_with_certs(&chain, key)));
        }
        let tls = tls.build();
        let request = ureq::Agent::config_builder()
            // Non-2xx answers are read for their `Status` rather than thrown
            // away as an error with no body.
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .tls_config(tls.clone())
            .user_agent("kirikumo")
            .build();
        let stream = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // No global timeout: a watch that says nothing for an hour is a
            // watch working as designed.
            .timeout_global(None)
            .tls_config(tls)
            .user_agent("kirikumo")
            .build();
        Ok(Self {
            client_cert: client_cert.map(|(chain, _)| chain.clone()),
            request: request.into(),
            stream: stream.into(),
        })
    }
}

/// Every certificate in a PEM bundle.
///
/// A kubeconfig's CA field is routinely a bundle — a root and an
/// intermediate — so taking the first would work until the day it did not.
fn certificates(pem: &[u8]) -> Result<Vec<Certificate<'static>>> {
    let mut certificates = Vec::new();
    for item in ureq::tls::parse_pem(pem) {
        match item {
            Ok(PemItem::Certificate(certificate)) => certificates.push(certificate),
            Ok(_) => {}
            Err(error) => {
                return Err(Error::Credentials(format!("a certificate: {error}")));
            }
        }
    }
    Ok(certificates)
}

impl Cluster for Rest {
    fn version(&self) -> Result<ClusterVersion> {
        let value = self.get_json("/version")?;
        serde_json::from_value(value)
            .map_err(|error| Error::Malformed(format!("/version: {error}")))
    }

    fn catalogue(&self) -> Result<Catalogue> {
        // The core group is the one that is not under `/apis`, so it is asked
        // for by name rather than discovered.
        let mut lists = vec![self.resources_of("v1")];
        let apis = self.get_json("/apis")?;
        for group_version in discovery::preferred_group_versions(&apis) {
            lists.push(self.resources_of(&group_version));
        }
        Ok(discovery::catalogue(lists))
    }

    fn namespaces(&self) -> Result<Vec<String>> {
        let list = ObjectList::parse(self.get_json("/api/v1/namespaces")?)?;
        let mut names: Vec<String> = list
            .items
            .into_iter()
            .map(|object| object.meta.name)
            .collect();
        names.sort();
        Ok(names)
    }

    fn list(&self, resource: &ApiResource, namespace: Option<&str>) -> Result<ObjectList> {
        ObjectList::parse(self.get_json(&resource.collection_path(namespace))?)
    }

    fn get(&self, resource: &ApiResource, namespace: Option<&str>, name: &str) -> Result<Object> {
        Object::new(self.get_json(&resource.object_path(namespace, name))?)
    }

    fn events_for(&self, uid: &str, namespace: Option<&str>) -> Result<Vec<EventRecord>> {
        let list = ObjectList::parse(self.get_json(&self.events_path(uid, namespace))?)?;
        let mut events: Vec<EventRecord> = list.items.iter().map(EventRecord::parse).collect();
        // Newest first: the reason a person opens this tab is the thing that
        // just happened.
        events.sort_by_key(|event| std::cmp::Reverse(event.last));
        Ok(events)
    }

    fn logs(&self, request: &LogRequest) -> Result<String> {
        let path = format!(
            "/api/v1/namespaces/{}/pods/{}/log?{}",
            request.namespace,
            request.pod,
            request.query()
        );
        self.get_text(&path)
    }

    fn follow_logs(&self, request: &LogRequest) -> Result<Box<dyn LogStream>> {
        let path = format!(
            "/api/v1/namespaces/{}/pods/{}/log?{}",
            request.namespace,
            request.pod,
            request.clone().follow(true).query()
        );
        // The streaming agent, for the same reason a watch uses it: a log
        // that says nothing for an hour is a log working as designed.
        let (_, agent, header) = self.prepare()?;
        let mut builder = agent.get(self.url(&path)).header("Accept", "text/plain");
        if let Some(header) = header {
            builder = builder.header("Authorization", header);
        }
        let mut response = builder.call()?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response
                .body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_string()
                .unwrap_or_default();
            return Err(Error::from_status(status, &body));
        }
        let reader = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        Ok(Box::new(Lines::new(BufReader::new(reader))))
    }

    fn node_metrics(&self) -> Result<Vec<Metrics>> {
        self.metrics(&format!("{METRICS_PREFIX}/nodes"))
    }

    fn pod_metrics(&self, namespace: Option<&str>) -> Result<Vec<Metrics>> {
        let path = match namespace.filter(|namespace| !namespace.is_empty()) {
            Some(namespace) => format!("{METRICS_PREFIX}/namespaces/{namespace}/pods"),
            None => format!("{METRICS_PREFIX}/pods"),
        };
        self.metrics(&path)
    }

    fn watch(
        &self,
        resource: &ApiResource,
        namespace: Option<&str>,
        from: &str,
    ) -> Result<Box<dyn WatchStream>> {
        let (_, agent, header) = self.prepare()?;
        let path = format!(
            "{}?watch=true&allowWatchBookmarks=true&timeoutSeconds={WATCH_SECONDS}&resourceVersion={from}",
            resource.collection_path(namespace)
        );
        let mut request = agent
            .get(self.url(&path))
            .header("Accept", "application/json");
        if let Some(header) = header {
            request = request.header("Authorization", header);
        }
        let mut response = request.call()?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response
                .body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_string()
                .unwrap_or_default();
            return Err(Error::from_status(status, &body));
        }
        let reader = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        Ok(Box::new(JsonLines::new(BufReader::new(reader))))
    }

    fn delete(&self, resource: &ApiResource, namespace: Option<&str>, name: &str) -> Result<()> {
        self.delete_path(&resource.object_path(namespace, name))
            .map(|_| ())
    }

    fn patch(
        &self,
        resource: &ApiResource,
        namespace: Option<&str>,
        name: &str,
        patch: Patch,
    ) -> Result<Object> {
        let method = match patch {
            Patch::Replace(_) => Method::Put,
            _ => Method::Patch,
        };
        let answer = self.send(
            method,
            &resource.object_path(namespace, name),
            patch.content_type(),
            patch.body(),
        )?;
        let value: Value =
            serde_json::from_str(&answer).map_err(|error| Error::Malformed(error.to_string()))?;
        Object::new(value)
    }

    fn can_i(&self, resource: &ApiResource, namespace: Option<&str>, verb: &str) -> Result<bool> {
        let review = serde_json::json!({
            "apiVersion": "authorization.k8s.io/v1",
            "kind": "SelfSubjectAccessReview",
            "spec": {"resourceAttributes": {
                "group": resource.group,
                "resource": resource.name,
                "verb": verb,
                "namespace": namespace.unwrap_or_default(),
            }}
        });
        let answer = self.send(
            Method::Post,
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            "application/json",
            &review,
        )?;
        let value: Value =
            serde_json::from_str(&answer).map_err(|error| Error::Malformed(error.to_string()))?;
        Ok(value
            .pointer("/status/allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthMethod;

    fn access() -> ClusterAccess {
        ClusterAccess {
            server: "https://127.0.0.1:6443".into(),
            roots: Vec::new(),
            insecure: true,
            server_name: None,
            auth: AuthMethod::Token("t".into()),
            namespace: Some("default".into()),
        }
    }

    #[test]
    fn a_context_with_no_server_is_refused_before_anything_is_sent() {
        let mut access = access();
        access.server = String::new();
        assert!(Rest::connect(access).is_err());
    }

    #[test]
    fn connecting_sends_nothing_so_an_unreachable_cluster_still_opens_a_window() {
        // No apiserver is listening on this port; connecting must still work,
        // because the window has to open to say which cluster is down.
        let rest = Rest::connect(access()).unwrap();
        assert_eq!(rest.default_namespace(), Some("default"));
        assert!(rest.is_insecure());
    }

    #[test]
    fn events_are_asked_for_by_the_uid_of_the_object_they_are_about() {
        let rest = Rest::connect(access()).unwrap();
        assert_eq!(
            rest.events_path("abc", Some("kube-system")),
            "/api/v1/namespaces/kube-system/events?fieldSelector=involvedObject.uid=abc"
        );
        assert_eq!(
            rest.events_path("abc", None),
            "/api/v1/events?fieldSelector=involvedObject.uid=abc"
        );
    }

    #[test]
    fn a_pem_bundle_yields_every_certificate_in_it() {
        // Two certificates, as a kubeconfig's CA field routinely holds.
        let pem = concat!(
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
            "-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n"
        );
        // The bodies are not valid DER, which `parse_pem` does not check —
        // it is the TLS provider that validates. What matters here is that
        // both sections are found rather than only the first.
        assert_eq!(certificates(pem.as_bytes()).unwrap().len(), 2);
        assert!(certificates(b"not a pem").unwrap().is_empty());
    }
}
