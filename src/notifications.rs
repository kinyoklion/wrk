//! Notification banners. Anything that can reach wrk's socket — a widget, a
//! hook, a script (`wrk api notify`) — can post one. The newest shows on the
//! screen's top row (over the pane borders, so nothing resizes) until it's
//! opened, dismissed, or its `ttl` runs out; opening one shows what it points
//! at (a session, a project, or a widget).

use std::time::{Duration, Instant};

use crate::ipc::NotifyRequest;

/// The banner's rightmost cells (` ✕ `): a click there dismisses instead of
/// opening.
pub const CLOSE_HIT_WIDTH: u16 = 3;

/// At most this many notifications are kept; posting past it drops the oldest,
/// so a chatty widget can't grow the queue without bound.
const MAX_QUEUED: usize = 50;

/// What activating a notification opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyTarget {
    /// A session id, as `wrk api sessions` lists it.
    Session(String),
    Project(String),
    Widget(String),
    /// Nothing: activating it just dismisses it.
    None,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u64,
    pub title: String,
    pub body: Option<String>,
    pub target: NotifyTarget,
    pub expires: Option<Instant>,
}

impl Notification {
    /// The banner text: `title — body`, on one line.
    pub fn text(&self) -> String {
        let one_line = |s: &str| -> String {
            s.chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect()
        };
        match &self.body {
            Some(body) if !body.trim().is_empty() => {
                format!("{} — {}", one_line(&self.title), one_line(body))
            }
            _ => one_line(&self.title),
        }
    }
}

/// The posted notifications, oldest first.
#[derive(Debug, Default)]
pub struct Notifications {
    list: Vec<Notification>,
    next_id: u64,
}

impl Notifications {
    /// Queue a notification; returns its id (for `dismiss`).
    pub fn push(&mut self, req: NotifyRequest, now: Instant) -> u64 {
        self.next_id += 1;
        let target = match (req.session, req.project, req.widget) {
            (Some(s), _, _) => NotifyTarget::Session(s),
            (None, Some(p), _) => NotifyTarget::Project(p),
            (None, None, Some(w)) => NotifyTarget::Widget(w),
            (None, None, None) => NotifyTarget::None,
        };
        self.list.push(Notification {
            id: self.next_id,
            title: req.title,
            body: req.body,
            target,
            expires: req.ttl.map(|secs| now + Duration::from_secs(secs)),
        });
        if self.list.len() > MAX_QUEUED {
            self.list.remove(0);
        }
        self.next_id
    }

    /// The one on screen.
    pub fn newest(&self) -> Option<&Notification> {
        self.list.last()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Take the one on screen (to open or dismiss it); the next newest shows.
    pub fn pop_newest(&mut self) -> Option<Notification> {
        self.list.pop()
    }

    /// Remove notification `id`; returns whether it was still queued.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.list.len();
        self.list.retain(|n| n.id != id);
        self.list.len() != before
    }

    /// Drop the notifications whose `ttl` has run out.
    pub fn expire(&mut self, now: Instant) {
        self.list.retain(|n| n.expires.is_none_or(|t| t > now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(title: &str) -> NotifyRequest {
        NotifyRequest {
            title: title.into(),
            body: None,
            session: None,
            project: None,
            widget: None,
            ttl: None,
        }
    }

    #[test]
    fn newest_shows_and_popping_reveals_the_next() {
        let now = Instant::now();
        let mut n = Notifications::default();
        let a = n.push(req("first"), now);
        let b = n.push(req("second"), now);
        assert_ne!(a, b);
        assert_eq!(n.newest().unwrap().title, "second");
        assert_eq!(n.pop_newest().unwrap().id, b);
        assert_eq!(n.newest().unwrap().id, a);
        assert!(n.dismiss(a));
        assert!(!n.dismiss(a), "already gone");
        assert!(n.is_empty());
    }

    #[test]
    fn the_target_prefers_session_then_project_then_widget() {
        let now = Instant::now();
        let mut n = Notifications::default();
        n.push(
            NotifyRequest {
                session: Some("tab1".into()),
                project: Some("wrk".into()),
                ..req("x")
            },
            now,
        );
        assert_eq!(
            n.newest().unwrap().target,
            NotifyTarget::Session("tab1".into())
        );
        n.push(
            NotifyRequest {
                project: Some("wrk".into()),
                widget: Some("prs".into()),
                ..req("x")
            },
            now,
        );
        assert_eq!(
            n.newest().unwrap().target,
            NotifyTarget::Project("wrk".into())
        );
        n.push(req("x"), now);
        assert_eq!(n.newest().unwrap().target, NotifyTarget::None);
    }

    #[test]
    fn a_ttl_expires_it() {
        let now = Instant::now();
        let mut n = Notifications::default();
        n.push(
            NotifyRequest {
                ttl: Some(5),
                ..req("brief")
            },
            now,
        );
        n.push(req("sticky"), now);
        n.expire(now + Duration::from_secs(4));
        assert_eq!(n.len(), 2);
        n.expire(now + Duration::from_secs(5));
        assert_eq!(n.len(), 1);
        assert_eq!(n.newest().unwrap().title, "sticky");
    }

    #[test]
    fn the_queue_is_bounded() {
        let now = Instant::now();
        let mut n = Notifications::default();
        for i in 0..MAX_QUEUED + 5 {
            n.push(req(&format!("n{i}")), now);
        }
        assert_eq!(n.len(), MAX_QUEUED);
        assert_eq!(n.newest().unwrap().title, format!("n{}", MAX_QUEUED + 4));
    }

    #[test]
    fn banner_text_is_one_line() {
        let now = Instant::now();
        let mut n = Notifications::default();
        n.push(
            NotifyRequest {
                body: Some("line one\nline two".into()),
                ..req("CI\tdone")
            },
            now,
        );
        assert_eq!(n.newest().unwrap().text(), "CI done — line one line two");
    }
}
