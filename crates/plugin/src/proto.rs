//! Wire types for the wrk session-plugin protocol.
//!
//! wrk connects to the plugin's socket, writes one [`Request`] as a line of
//! JSON, reads one reply line, and closes. A reply is the method's response
//! object plus an `ok` envelope field; `{"ok": false, "error": "…"}` reports a
//! failure, which wrk shows to the user. Use [`ok_reply`] / [`error_reply`] to
//! write replies and [`parse_reply`] to read them so both sides agree on the
//! envelope.
//!
//! Every optional field defaults, and unknown fields are ignored, so a plugin
//! built against an older version of these types keeps working.

use std::collections::BTreeMap;
use std::fmt::Display;

use anyhow::{Result, anyhow};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// One request from wrk to a plugin, internally tagged by `method`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    /// Handshake: report name, version, and supported [`Capabilities`]
    /// ([`HelloResponse`]). Optional — a plugin that doesn't answer it is
    /// treated as having no optional capabilities.
    Hello,
    /// List the sessions available for a project, for the new-tab picker
    /// ([`ListResponse`]). Only sent when the plugin advertises `list`.
    ListSessions { project: ProjectPayload },
    /// Resolve the command to spawn for one pane ([`AttachResponse`]). The only
    /// method a plugin must implement.
    Attach(AttachRequest),
    /// Best-effort notice that a tab hosting `session_id` closed. Only sent
    /// when the plugin advertises `close`; the reply body is ignored.
    Close {
        project: ProjectPayload,
        session_id: String,
    },
}

/// The project a request is about: its name, its local directory, and the
/// opaque `[project.plugin_config]` table from wrk's `projects.toml`. A plugin
/// uses these to decide which box / session the request maps to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPayload {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
}

/// Which pane an [`AttachRequest`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachKind {
    /// A primary-pane agent tab.
    Agent,
    /// The project's shell pane. Only sent when the plugin advertises `shell`;
    /// a reply with an empty `command` makes wrk fall back to a local shell.
    Shell,
}

/// Ask the plugin for the process to spawn for one pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRequest {
    pub project: ProjectPayload,
    /// The session to re-attach, or `None` to let the plugin pick or create one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub kind: AttachKind,
    /// True only when the user explicitly asked for a brand-new session (the
    /// picker's "New session"). With `session_id: None` this means "create a
    /// new one"; `fresh: false` with no id means "attach the project's
    /// current/default session".
    #[serde(default)]
    pub fresh: bool,
    /// The tab's display name — a hint the plugin may use when creating.
    #[serde(default)]
    pub name: String,
    /// wrk's IPC socket (`WRK_SOCK`), sent only when the plugin advertised the
    /// `status` capability. To report the tab's state, connect here and send
    /// the same line `wrk hook` sends —
    /// `{"cmd":"status","tab":"<wrk_tab>","kind":"busy"|"stopped"|"waiting"}` —
    /// and wrk shows it in the sidebar exactly like a local hook. Omit a
    /// `session_id` from those lines unless the session really changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrk_sock: Option<String>,
    /// The tab id (`WRK_TAB`) to tag pushed status with. `None` for the shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrk_tab: Option<String>,
}

/// Optional features a plugin supports, from its [`HelloResponse`]. All
/// default to `false`, so a plugin that only implements `attach` degrades
/// gracefully: "New session" only, a local shell, no close notices, and the
/// idle-time status heuristic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    /// `list_sessions` is supported (the picker can show real sessions).
    pub list: bool,
    /// `attach` with `kind: "shell"` can return a shell command for the project.
    pub shell: bool,
    /// The plugin pushes tab status over `wrk_sock` (see
    /// [`AttachRequest::wrk_sock`]).
    pub status: bool,
    /// `close` notices are wanted.
    pub close: bool,
}

/// Reply to [`Request::Hello`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HelloResponse {
    pub name: String,
    pub version: String,
    pub capabilities: Capabilities,
}

/// Lifecycle state of a plugin session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Running,
    Stopped,
    #[default]
    Unknown,
}

/// One session the plugin offers for a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSession {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub state: SessionState,
}

/// Reply to [`Request::ListSessions`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListResponse {
    pub sessions: Vec<PluginSession>,
}

/// Reply to [`Request::Attach`]: the process wrk should spawn in the pane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachResponse {
    /// Argv of the process to spawn. Empty ⇒ the plugin declined (for a shell
    /// attach, wrk falls back to a local shell; for an agent it's an error).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Extra environment for the process, merged over wrk's own `WRK_*`
    /// variables (so the plugin can override or forward them).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory; `None` ⇒ the project's local path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The session id the plugin attached or created. wrk records it and sends
    /// it back on the next attach for this tab, so reopening resumes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl AttachResponse {
    /// The plugin returned nothing to spawn.
    pub fn is_declined(&self) -> bool {
        self.command
            .first()
            .is_none_or(|program| program.is_empty())
    }
}

/// Render a success reply line for `body` (which must serialize to a JSON
/// object): its fields with `"ok": true` in front.
pub fn ok_reply<T: Serialize>(body: &T) -> Result<String> {
    let serde_json::Value::Object(fields) = serde_json::to_value(body)? else {
        return Err(anyhow!("a reply body must serialize to a JSON object"));
    };
    let mut reply = serde_json::Map::with_capacity(fields.len() + 1);
    reply.insert("ok".into(), true.into());
    reply.extend(fields);
    Ok(serde_json::Value::Object(reply).to_string())
}

/// Render an error reply line: `{"ok":false,"error":"<message>"}`.
pub fn error_reply(message: impl Display) -> String {
    serde_json::json!({ "ok": false, "error": message.to_string() }).to_string()
}

/// Parse a reply line into `T`, turning an `{"ok": false}` envelope into an
/// `Err` carrying the plugin's message. The envelope's `ok` field is otherwise
/// ignored.
pub fn parse_reply<T: DeserializeOwned>(line: &str) -> Result<T> {
    let line = line.trim();
    if line.is_empty() {
        return Err(anyhow!("the plugin closed the connection without replying"));
    }
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| anyhow!("malformed reply ({e}): {line}"))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        let message = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the plugin reported an unspecified error");
        return Err(anyhow!("{message}"));
    }
    serde_json::from_value(value).map_err(|e| anyhow!("unexpected reply ({e}): {line}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> ProjectPayload {
        ProjectPayload {
            name: "svc".into(),
            path: "/home/me/svc".into(),
            config: BTreeMap::from([("box".to_string(), "i-0abc".to_string())]),
        }
    }

    #[test]
    fn requests_round_trip_with_method_tags() {
        let attach = Request::Attach(AttachRequest {
            project: project(),
            session_id: Some("sess-1".into()),
            kind: AttachKind::Agent,
            fresh: false,
            name: "svc".into(),
            wrk_sock: Some("/run/user/1000/wrk/sock/wrk-7.sock".into()),
            wrk_tab: Some("tab0".into()),
        });
        let json = serde_json::to_string(&attach).unwrap();
        assert!(json.contains(r#""method":"attach""#), "{json}");
        assert!(json.contains(r#""kind":"agent""#), "{json}");
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), attach);

        assert_eq!(
            serde_json::to_string(&Request::Hello).unwrap(),
            r#"{"method":"hello"}"#
        );
        let close = Request::Close {
            project: project(),
            session_id: "s".into(),
        };
        let json = serde_json::to_string(&close).unwrap();
        assert!(json.contains(r#""method":"close""#), "{json}");
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), close);
    }

    #[test]
    fn a_minimal_attach_request_parses() {
        // A plugin must accept a request carrying only the required fields.
        let req: Request = serde_json::from_str(
            r#"{"method":"attach","project":{"name":"p","path":"/p"},"kind":"shell"}"#,
        )
        .unwrap();
        let Request::Attach(a) = req else {
            panic!("not an attach")
        };
        assert_eq!(a.kind, AttachKind::Shell);
        assert!(a.session_id.is_none() && !a.fresh && a.wrk_sock.is_none());
    }

    #[test]
    fn ok_reply_round_trips_through_parse_reply() {
        let resp = AttachResponse {
            command: vec!["ssh".into(), "-t".into(), "box".into()],
            env: BTreeMap::from([("FOO".to_string(), "bar".to_string())]),
            cwd: Some("/srv".into()),
            session_id: Some("sess-9".into()),
        };
        let line = ok_reply(&resp).unwrap();
        assert!(line.starts_with(r#"{"ok":true,"#), "{line}");
        assert_eq!(parse_reply::<AttachResponse>(&line).unwrap(), resp);
    }

    #[test]
    fn error_envelope_becomes_err_with_the_plugins_message() {
        let err = parse_reply::<AttachResponse>(&error_reply("no such box")).unwrap_err();
        assert_eq!(err.to_string(), "no such box");
    }

    #[test]
    fn empty_or_garbage_replies_are_errors() {
        assert!(parse_reply::<HelloResponse>("").is_err());
        assert!(parse_reply::<HelloResponse>("not json").is_err());
        // A non-object body can't be a reply.
        assert!(ok_reply(&"just a string").is_err());
    }

    #[test]
    fn declined_means_nothing_to_run() {
        assert!(AttachResponse::default().is_declined());
        let blank = AttachResponse {
            command: vec![String::new()],
            ..Default::default()
        };
        assert!(blank.is_declined());
        let real = AttachResponse {
            command: vec!["bash".into()],
            ..Default::default()
        };
        assert!(!real.is_declined());
    }

    #[test]
    fn hello_defaults_when_capabilities_are_omitted() {
        let hello: HelloResponse = parse_reply(r#"{"ok":true,"name":"x"}"#).unwrap();
        assert_eq!(hello.capabilities, Capabilities::default());
    }
}
