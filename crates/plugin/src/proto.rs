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
    /// List the projects the plugin provides itself, for its own group in
    /// wrk's sidebar ([`ProjectsResponse`]). Only sent when the plugin
    /// advertises `projects`. These projects are not in `projects.toml`: wrk
    /// shows exactly what the latest listing says, and asks again periodically.
    ListProjects,
    /// Describe the form for creating a project or session ([`FormResponse`]).
    /// `project` is the listed project the person was in, when there was one,
    /// so the form can preselect it. Only sent with the `create` capability.
    CreateForm {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<ProjectPayload>,
    },
    /// Create what the filled-in form describes ([`CreateResponse`]): `values`
    /// maps each visible field's `id` to its value. Only sent with `create`.
    Create {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<ProjectPayload>,
        #[serde(default)]
        values: BTreeMap<String, String>,
    },
    /// Describe the form for removing one of the plugin's listed projects, or
    /// the session `session_id` in it ([`FormResponse`]): what removing means
    /// is the plugin's to say, and the form is how the person confirms it.
    /// Only sent with the `remove` capability.
    RemoveForm {
        project: ProjectPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// Remove what the filled-in removal form describes ([`RemoveResponse`]),
    /// for the same `project` and `session_id`. Only sent with `remove`.
    Remove {
        project: ProjectPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default)]
        values: BTreeMap<String, String>,
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
    /// On a shell attach for a project the plugin listed (`projects`), it is
    /// the session of the tab on screen, so the shell can follow the tab.
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
    /// `list_projects` is supported: the plugin lists its own projects, which
    /// wrk shows as a sidebar group of their own.
    pub projects: bool,
    /// `create_form` and `create` are supported: wrk can create projects and
    /// sessions with a form the plugin describes.
    pub create: bool,
    /// `remove_form` and `remove` are supported: wrk can remove the plugin's
    /// listed projects and sessions, with a form the plugin describes.
    pub remove: bool,
    /// Closing one of the plugin's listed tabs (`projects`) ends its session:
    /// wrk sends `close` for it and drops the tab, and the plugin is expected
    /// to stop the session so later listings no longer include it. Without
    /// it, closing a listed tab only detaches its pane and the tab stays.
    /// Enough on its own: it does not need `close` as well.
    pub close_listed: bool,
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

/// What a session's agent is doing, as a listing reports it. wrk shows it on
/// tabs that have no live pane (a live pane's own status reports win).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    /// Working on a turn.
    Busy,
    /// Blocked on the person (a permission prompt, a question).
    Waiting,
    /// Done with its turn, or not running.
    Idle,
    /// A status this version of the protocol doesn't know.
    #[serde(other)]
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
    /// The agent's activity, when the plugin knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
}

/// Reply to [`Request::ListSessions`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListResponse {
    pub sessions: Vec<PluginSession>,
}

/// One project a plugin lists for its sidebar group ([`Request::ListProjects`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteProject {
    /// Stable across listings: wrk keys the project (and its live panes) by it.
    pub id: String,
    /// Shown in the sidebar.
    pub name: String,
    /// A word or two shown dimmed beside the name ("stopped", "starting"), or
    /// nothing for the ordinary state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Sent back as [`ProjectPayload::config`] on every request about this
    /// project, so the plugin can tell what it is (a box id, say).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
    /// The project's sessions, in the order wrk lists them as tabs. Attaching
    /// one sends its `id` as the attach's `session_id`.
    pub sessions: Vec<PluginSession>,
}

/// Reply to [`Request::ListProjects`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectsResponse {
    /// The sidebar group's heading; `None` ⇒ the plugin's name in settings.toml.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub projects: Vec<RemoteProject>,
}

/// How a [`FormField`] is edited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    /// One of `options`, cycled with ←/→.
    Select,
    /// A line of text. (Also what an unknown kind falls back to.)
    #[default]
    #[serde(other)]
    Text,
}

/// One choice of a select field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormOption {
    /// What `create` receives.
    pub value: String,
    /// What the person sees; empty ⇒ `value`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
}

/// Show a field only while another field has one of some values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowIf {
    pub field: String,
    pub equals: Vec<String>,
}

/// One input of a [`FormResponse`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormField {
    /// The key of this field's value in [`Request::Create`]'s `values`.
    pub id: String,
    pub label: String,
    pub kind: FieldKind,
    /// wrk refuses to submit while a visible required field is empty.
    pub required: bool,
    /// The initial value (for a select, an option's `value`; else its first).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Shown dimmed in an empty text field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// A line of guidance shown while the field has focus.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// A select's choices.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<FormOption>,
    /// When set, the field is shown (and sent) only while it holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_if: Option<ShowIf>,
}

impl FormField {
    /// Whether the field is shown, given the form's current values.
    pub fn visible(&self, values: &BTreeMap<String, String>) -> bool {
        self.show_if.as_ref().is_none_or(|cond| {
            values
                .get(&cond.field)
                .is_some_and(|v| cond.equals.iter().any(|e| e == v))
        })
    }
}

/// Reply to [`Request::CreateForm`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormResponse {
    /// The dialog's title.
    pub title: String,
    /// A paragraph shown above the fields: what submitting will do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<FormField>,
    /// The submit action's label; `None` ⇒ "Create" (or "Remove", for a
    /// removal form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit: Option<String>,
}

/// Reply to [`Request::Create`]. wrk lists the plugin's projects again, then
/// opens `project` (and its tab `session`) once a listing includes them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CreateResponse {
    /// The listed project's `id` to open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The session (tab) to show in it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// A line for the status bar ("starting box-3; the session appears once
    /// it's up").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Reply to [`Request::Remove`]. wrk lists the plugin's projects again, so
/// what was removed leaves the sidebar once a listing no longer has it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoveResponse {
    /// A line for the status bar ("terminating box-3").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
    fn project_listings_and_forms_round_trip_and_tolerate_the_unknown() {
        assert_eq!(
            serde_json::to_string(&Request::ListProjects).unwrap(),
            r#"{"method":"list_projects"}"#
        );
        let create = Request::Create {
            project: Some(project()),
            values: BTreeMap::from([("box".to_string(), "new".to_string())]),
        };
        let json = serde_json::to_string(&create).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), create);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"method":"create_form"}"#).unwrap(),
            Request::CreateForm { project: None }
        );
        let remove = Request::Remove {
            project: project(),
            session_id: Some("p/main".into()),
            values: BTreeMap::from([("confirm".to_string(), "box-1".to_string())]),
        };
        let json = serde_json::to_string(&remove).unwrap();
        assert!(json.starts_with(r#"{"method":"remove","#), "{json}");
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), remove);
        let form = Request::RemoveForm {
            project: project(),
            session_id: None,
        };
        let json = serde_json::to_string(&form).unwrap();
        assert!(!json.contains("session_id"), "{json}");
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), form);
        let removed: RemoveResponse = parse_reply(r#"{"ok":true,"message":"gone"}"#).unwrap();
        assert_eq!(removed.message.as_deref(), Some("gone"));

        let listing: ProjectsResponse = parse_reply(
            r#"{"ok":true,"group":"boxes","projects":[{"id":"b1","name":"box-1","state":"stopped",
                "config":{"box":"b1"},"sessions":[{"id":"p/main","name":"svc-main","status":"thinking"},
                {"id":"p/x","status":"busy","extra":1}]}]}"#,
        )
        .unwrap();
        assert_eq!(listing.group.as_deref(), Some("boxes"));
        let sessions = &listing.projects[0].sessions;
        assert_eq!(sessions[0].status, Some(AgentStatus::Unknown));
        assert_eq!(sessions[1].status, Some(AgentStatus::Busy));

        let form: FormResponse = parse_reply(
            r#"{"ok":true,"title":"New session","fields":[
                {"id":"box","label":"Box","kind":"select","options":[{"value":"b1","label":"box-1"},{"value":"new"}]},
                {"id":"box_name","label":"Name","required":true,"show_if":{"field":"box","equals":["new"]}},
                {"id":"when","label":"When","kind":"calendar"}]}"#,
        )
        .unwrap();
        assert_eq!(form.fields[0].kind, FieldKind::Select);
        assert_eq!(
            form.fields[2].kind,
            FieldKind::Text,
            "unknown kinds read as text"
        );
        let mut values = BTreeMap::from([("box".to_string(), "b1".to_string())]);
        assert!(!form.fields[1].visible(&values));
        values.insert("box".into(), "new".into());
        assert!(form.fields[1].visible(&values));
        assert!(form.fields[0].visible(&values));
    }

    #[test]
    fn hello_defaults_when_capabilities_are_omitted() {
        let hello: HelloResponse = parse_reply(r#"{"ok":true,"name":"x"}"#).unwrap();
        assert_eq!(hello.capabilities, Capabilities::default());
    }

    #[test]
    fn close_listed_is_its_own_capability() {
        let hello: HelloResponse =
            parse_reply(r#"{"ok":true,"name":"x","capabilities":{"close_listed":true}}"#).unwrap();
        assert!(hello.capabilities.close_listed);
        assert!(!hello.capabilities.close, "it doesn't imply `close`");
    }
}
