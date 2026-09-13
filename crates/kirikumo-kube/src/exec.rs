//! Running a command in a container.
//!
//! Non-interactive: a command goes in, its output and exit status come back,
//! the way `kubectl exec pod -- cmd` behaves without `-it`. That is most of
//! what a viewer needs from exec — what is in that file, what does `env`
//! say, is the process there — and it needs no terminal emulator, which an
//! interactive shell does (roadmap Q6).
//!
//! The same WebSocket a port-forward uses, with five channels instead of
//! two: stdin, stdout, stderr, a status channel that carries a `Status`
//! object at the end, and a resize channel this app never writes. No
//! opening-port frame; that is port-forward's alone.

use crate::error::Error;
use serde_json::Value;

/// The channels of an exec session, per `v4.channel.k8s.io`.
pub const STDIN: u8 = 0;
/// What the command wrote to its standard output.
pub const STDOUT: u8 = 1;
/// What it wrote to its standard error.
pub const STDERR: u8 = 2;
/// The apiserver's `Status` for the run, sent once at the end.
pub const STATUS: u8 = 3;

/// What to run, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// The pod's namespace.
    pub namespace: String,
    /// The pod's name.
    pub pod: String,
    /// Which container, when the pod has more than one.
    pub container: Option<String>,
    /// The command and its arguments, as `argv`. Not a shell line: a shell
    /// is asked for by naming one, `["sh", "-c", "…"]`, which is what
    /// [`Self::shell`] does.
    pub command: Vec<String>,
    /// Attached rather than collected: a tty, stdin open, and stderr folded
    /// into stdout the way a terminal has it. What [`Self::attach`] asks for.
    pub interactive: bool,
}

impl ExecRequest {
    /// Run a line through `sh -c` in a pod.
    ///
    /// `sh` rather than `bash`, because every image that has a shell has
    /// `sh`, and many that have `sh` — `alpine`, `busybox`, distroless-with-
    /// a-shell — have no `bash`.
    pub fn shell(namespace: impl Into<String>, pod: impl Into<String>, line: &str) -> Self {
        Self {
            namespace: namespace.into(),
            pod: pod.into(),
            container: None,
            command: vec!["sh".into(), "-c".into(), line.to_string()],
            interactive: false,
        }
    }

    /// An interactive shell in a pod: `sh` on a tty with stdin open.
    ///
    /// `sh` for the reason [`Self::shell`] gives; a person who wants `bash`
    /// can type it.
    pub fn attach(namespace: impl Into<String>, pod: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            pod: pod.into(),
            container: None,
            command: vec!["sh".into()],
            interactive: true,
        }
    }

    /// Run in one container.
    pub fn container(mut self, container: impl Into<String>) -> Self {
        self.container = Some(container.into());
        self
    }

    /// Attach rather than collect.
    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self
    }

    /// The query string this request becomes.
    ///
    /// Every argument is its own `command=` parameter, percent-encoded, which
    /// is how the apiserver takes an `argv`. No stdin and no tty: the output
    /// is collected, not attached to.
    pub fn query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(container) = &self.container {
            parts.push(format!("container={}", encode(container)));
        }
        for argument in &self.command {
            parts.push(format!("command={}", encode(argument)));
        }
        parts.push("stdout=true".into());
        // With a tty there is no separate stderr: the kernel merges the two
        // on the terminal, as it does on any terminal.
        match self.interactive {
            true => {
                parts.push("stderr=false".into());
                parts.push("stdin=true".into());
                parts.push("tty=true".into());
            }
            false => {
                parts.push("stderr=true".into());
                parts.push("stdin=false".into());
                parts.push("tty=false".into());
            }
        }
        parts.join("&")
    }
}

/// Percent-encode a query value.
///
/// Everything but the unreserved set, because a command line is full of
/// spaces, quotes and slashes and every one of them has to survive the
/// query string.
pub fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// What a run produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    /// Standard output, as text.
    pub stdout: String,
    /// Standard error, as text.
    pub stderr: String,
    /// The exit code, when the status channel said one. `None` means the
    /// run ended without saying — a connection dropped, or an image whose
    /// runtime does not report codes.
    pub exit_code: Option<i32>,
    /// The apiserver's own words when the run failed for a reason other
    /// than the command's exit code: no such container, no such command.
    pub failure: Option<String>,
}

impl ExecOutput {
    /// Whether the command ran and exited zero.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && self.failure.is_none()
    }

    /// Take in what the status channel said at the end.
    pub fn read_status(&mut self, status: &[u8]) {
        let (exit_code, failure) = parse_status(status);
        self.exit_code = exit_code;
        self.failure = failure;
    }
}

/// Read the `Status` the apiserver sends on the status channel.
///
/// Two shapes. `{"status":"Success"}` is exit zero. A non-zero exit is a
/// `Failure` with `reason: NonZeroExitCode` and the code in
/// `details.causes[].message` under `reason: ExitCode` — which is where it
/// is, however odd that is. Any other `Failure` is the apiserver's own
/// words about why the command never ran.
pub fn parse_status(status: &[u8]) -> (Option<i32>, Option<String>) {
    let Ok(value) = serde_json::from_slice::<Value>(status) else {
        return (None, None);
    };
    match value.get("status").and_then(Value::as_str) {
        Some("Success") => (Some(0), None),
        Some("Failure") => {
            let reason = value.get("reason").and_then(Value::as_str).unwrap_or("");
            if reason == "NonZeroExitCode" {
                let code = value
                    .pointer("/details/causes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|cause| cause.get("reason").and_then(Value::as_str) == Some("ExitCode"))
                    .and_then(|cause| cause.get("message"))
                    .and_then(Value::as_str)
                    .and_then(|code| code.parse().ok());
                return (code, None);
            }
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .map(str::to_string)
                .or_else(|| Some(format!("the run failed: {reason}")));
            (None, message)
        }
        _ => (None, None),
    }
}

/// Fold one frame into the output.
///
/// Returns `true` when the frame was the status, which is the last thing
/// the apiserver sends and the sign to stop reading.
pub fn take_frame(output: &mut ExecOutput, channel: u8, payload: &[u8]) -> bool {
    match channel {
        STDOUT => output.stdout.push_str(&String::from_utf8_lossy(payload)),
        STDERR => output.stderr.push_str(&String::from_utf8_lossy(payload)),
        STATUS => {
            output.read_status(payload);
            return true;
        }
        _ => {}
    }
    false
}

/// The message the resize channel takes: the apiserver's own field names.
pub fn resize_message(cols: u16, rows: u16) -> Vec<u8> {
    format!("{{\"Width\":{cols},\"Height\":{rows}}}").into_bytes()
}

/// The channel a resize goes on.
pub const RESIZE: u8 = 4;

/// The resource an access review asks about for exec: the `pods/exec`
/// subresource, which is not in the catalogue because it cannot be listed.
pub fn review_resource(pods: &crate::model::ApiResource) -> crate::model::ApiResource {
    let mut resource = pods.clone();
    resource.name = "pods/exec".into();
    resource
}

/// The error a caller gets when the run itself could not start.
pub fn could_not_run(reason: impl std::fmt::Display) -> Error {
    Error::Transport(format!("exec: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_line_becomes_argv_through_sh() {
        let request = ExecRequest::shell("shop", "api", "ls -la /etc | head");
        assert_eq!(request.command, vec!["sh", "-c", "ls -la /etc | head"]);
    }

    #[test]
    fn every_argument_is_its_own_encoded_parameter() {
        let query = ExecRequest::shell("shop", "api", "echo \"a b\"")
            .container("proxy")
            .query();
        assert!(
            query.starts_with("container=proxy&command=sh&command=-c&command=echo%20%22a%20b%22"),
            "{query}"
        );
        assert!(query.ends_with("stdout=true&stderr=true&stdin=false&tty=false"));
    }

    #[test]
    fn an_attached_shell_asks_for_a_tty_and_stdin_and_no_separate_stderr() {
        let query = ExecRequest::attach("shop", "api").query();
        assert!(query.contains("command=sh"), "{query}");
        assert!(
            query.ends_with("stdout=true&stderr=false&stdin=true&tty=true"),
            "{query}"
        );
        assert_eq!(
            String::from_utf8(resize_message(120, 40)).unwrap(),
            r#"{"Width":120,"Height":40}"#
        );
    }

    #[test]
    fn encoding_keeps_what_is_safe_and_escapes_the_rest() {
        assert_eq!(encode("abc-_.~09"), "abc-_.~09");
        assert_eq!(encode("a b/c=d&e"), "a%20b%2Fc%3Dd%26e");
        assert_eq!(encode("日"), "%E6%97%A5");
    }

    #[test]
    fn success_is_exit_zero() {
        assert_eq!(
            parse_status(br#"{"status":"Success","metadata":{}}"#),
            (Some(0), None)
        );
    }

    #[test]
    fn a_non_zero_exit_is_read_out_of_the_causes() {
        let status = br#"{"status":"Failure","message":"command terminated with non-zero exit code: error executing command [sh -c false], exit code 1","reason":"NonZeroExitCode","details":{"causes":[{"reason":"ExitCode","message":"1"}]}}"#;
        assert_eq!(parse_status(status), (Some(1), None));
    }

    #[test]
    fn a_command_that_never_ran_is_the_apiservers_words() {
        let status = br#"{"status":"Failure","message":"container not found (\"nope\")","reason":"NotFound"}"#;
        let (code, failure) = parse_status(status);
        assert_eq!(code, None);
        assert!(failure.is_some_and(|f| f.contains("container not found")));
    }

    #[test]
    fn something_that_is_not_a_status_says_nothing() {
        assert_eq!(parse_status(b"garbage"), (None, None));
        assert_eq!(parse_status(br#"{"status":"Odd"}"#), (None, None));
    }

    #[test]
    fn frames_fold_into_the_output_and_the_status_is_the_end() {
        let mut output = ExecOutput::default();
        assert!(!take_frame(&mut output, STDOUT, b"hello\n"));
        assert!(!take_frame(&mut output, STDERR, b"warn\n"));
        assert!(!take_frame(&mut output, STDIN, b"ignored"));
        assert!(take_frame(&mut output, STATUS, br#"{"status":"Success"}"#));
        assert_eq!(output.stdout, "hello\n");
        assert_eq!(output.stderr, "warn\n");
        assert!(output.succeeded());
    }

    #[test]
    fn a_failure_is_not_a_success_whatever_stdout_says() {
        let mut output = ExecOutput::default();
        take_frame(&mut output, STDOUT, b"fine\n");
        take_frame(&mut output, STATUS, br#"{"status":"Failure","reason":"NonZeroExitCode","details":{"causes":[{"reason":"ExitCode","message":"2"}]}}"#);
        assert_eq!(output.exit_code, Some(2));
        assert!(!output.succeeded());
    }

    #[test]
    fn the_review_asks_about_the_exec_subresource() {
        let pods = crate::model::ApiResource {
            group: String::new(),
            version: "v1".into(),
            kind: "Pod".into(),
            name: "pods".into(),
            singular: "pod".into(),
            namespaced: true,
            verbs: vec!["list".into()],
            short_names: Vec::new(),
            categories: Vec::new(),
        };
        assert_eq!(review_resource(&pods).name, "pods/exec");
    }
}
