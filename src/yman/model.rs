//! Serde types of yman's JSON reads (`ls --json -a`, `show --json`,
//! `tags --json`, `status --json`). Unknown fields are ignored and all but
//! the keys are optional, so a field a later yman drops does not break
//! parsing.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TaskRow {
    pub id: String,
    #[serde(default)]
    pub priority: u8,
    pub status: String,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub attachments: u32,
    #[serde(default)]
    pub comments: u32,
    #[serde(default)]
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TaskDetails {
    pub id: String,
    #[serde(default)]
    pub priority: u8,
    pub status: String,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub related_by: Vec<String>,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub discussion: Vec<Comment>,
    #[serde(default)]
    pub discussion_total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Attachment {
    pub name: String,
    #[serde(default)]
    pub added: String,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub author: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TagCount {
    pub tag: String,
    #[serde(default)]
    pub tasks: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TrackerStatus {
    #[serde(default)]
    pub local: LocalRef,
    #[serde(default)]
    pub remote: RemoteRef,
    #[serde(default)]
    pub refresh: String,
    #[serde(default)]
    pub autosync: String,
    #[serde(default)]
    pub hooks: bool,
    #[serde(default)]
    pub worktree: WorktreeState,
    #[serde(default)]
    pub merge: MergeState,
    #[serde(default)]
    pub tasks: TaskCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LocalRef {
    #[serde(rename = "ref", default)]
    pub name: String,
    #[serde(default)]
    pub head: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RemoteRef {
    #[serde(rename = "ref", default)]
    pub name: String,
    #[serde(default)]
    pub fetched: bool,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub ahead: usize,
    #[serde(default)]
    pub behind: usize,
    #[serde(default)]
    pub origin: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WorktreeState {
    #[serde(default)]
    pub dirty: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MergeState {
    #[serde(default)]
    pub in_progress: bool,
    #[serde(default)]
    pub unmerged: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct TaskCounts {
    #[serde(default)]
    pub by_status: std::collections::BTreeMap<String, usize>,
    #[serde(default)]
    pub other: usize,
    #[serde(default)]
    pub broken: usize,
}

/// Every task (closed ones too, in yman's `ls` order) and the tracker status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub tasks: Vec<TaskRow>,
    pub status: Option<TrackerStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LS: &str = r#"[
{"id":"1","priority":1,"status":"done","title":"First","tags":["phase0","ui"],"assignee":null,"links":[],"related":["2"],"created":"2026-10-01T14:28:03Z","updated":"2026-10-01T14:42:36Z","attachments":0,"comments":2,"dir":"1.1.first","future":1},
{"id":"2","priority":2,"status":"todo","title":"Second","tags":[],"assignee":"claude","links":[],"related":[],"created":"2026-10-02T10:00:00Z","updated":"2026-10-02T10:00:00Z","attachments":1,"comments":0,"dir":"2.2.second"}
]"#;

    const SHOW: &str = r#"{"id":"1","priority":1,"status":"doing","title":"First","tags":["a"],"assignee":"claude","links":[],"related":["2"],"related_by":["3"],"created":"2026-10-01T14:28:03Z","updated":"2026-10-01T14:42:36Z","dir":"1.1.first","body":"line one\nline two\n","attachments":[{"name":"x.md","added":"2026-10-01T15:00:00Z","by":"claude","missing":false}],"discussion":[{"ts":"2026-10-01T15:00:00Z","author":"claude","text":"hello"},{"ts":"2026-10-01T16:00:00Z","author":null,"text":"anon"}],"discussion_total":2}"#;

    const TAGS: &str = r#"[{"tag":"bug","tasks":6},{"tag":"ui","tasks":2}]"#;

    const STATUS_REMOTE: &str = r#"{"local":{"ref":"refs/yman/local","head":"126ddaa"},"remote":{"ref":"refs/tasks/main","fetched":true,"head":"c5c1d32","ahead":2,"behind":0,"origin":true},"refresh":"lazy","autosync":"off","hooks":false,"worktree":{"dirty":0},"merge":{"in_progress":false,"unmerged":[]},"tasks":{"by_status":{"todo":33,"doing":2,"done":276},"other":0,"broken":0}}"#;

    const STATUS_LOCAL: &str = r#"{"local":{"ref":"refs/yman/local","head":"126ddaa"},"remote":{"ref":"refs/tasks/main","fetched":false,"head":null,"ahead":0,"behind":0,"origin":false},"refresh":"lazy","autosync":"off","hooks":false,"worktree":{"dirty":1},"merge":{"in_progress":false,"unmerged":[]},"tasks":{"by_status":{"todo":1,"cancelled":3},"other":0,"broken":0}}"#;

    #[test]
    fn ls_rows_parse_in_order() {
        let rows: Vec<TaskRow> = serde_json::from_str(LS).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].id.as_str(), rows[0].title.as_str(), rows[0].status.as_str()), ("1", "First", "done"));
        assert_eq!(rows[0].assignee, None);
        assert_eq!(rows[0].tags, ["phase0", "ui"]);
        assert_eq!(rows[0].related, ["2"]);
        assert_eq!(rows[0].comments, 2);
        assert_eq!(rows[1].assignee.as_deref(), Some("claude"));
        assert_eq!((rows[1].priority, rows[1].attachments), (2, 1));
        assert!(serde_json::from_str::<Vec<TaskRow>>("[]").unwrap().is_empty());
    }

    #[test]
    fn show_parses() {
        let t: TaskDetails = serde_json::from_str(SHOW).unwrap();
        assert_eq!(t.related_by, ["3"]);
        assert_eq!(t.body, "line one\nline two\n");
        assert_eq!(t.attachments.len(), 1);
        assert!(!t.attachments[0].missing);
        assert_eq!(t.attachments[0].by.as_deref(), Some("claude"));
        assert_eq!(t.discussion.len(), 2);
        assert_eq!(t.discussion[1].author, None);
        assert_eq!(t.discussion_total, 2);
    }

    #[test]
    fn tags_parse() {
        let t: Vec<TagCount> = serde_json::from_str(TAGS).unwrap();
        assert_eq!(t, [TagCount { tag: "bug".into(), tasks: 6 }, TagCount { tag: "ui".into(), tasks: 2 }]);
        assert!(serde_json::from_str::<Vec<TagCount>>("[]").unwrap().is_empty());
    }

    #[test]
    fn status_parses() {
        let s: TrackerStatus = serde_json::from_str(STATUS_REMOTE).unwrap();
        assert_eq!(s.remote.head.as_deref(), Some("c5c1d32"));
        assert_eq!(s.remote.ahead, 2);
        assert!(s.remote.origin);
        assert_eq!(s.tasks.by_status["todo"], 33);
        assert_eq!(s.local.name, "refs/yman/local");
        let s: TrackerStatus = serde_json::from_str(STATUS_LOCAL).unwrap();
        assert_eq!(s.remote.head, None);
        assert!(!s.remote.origin);
        assert_eq!(s.worktree.dirty, 1);
        assert_eq!(s.tasks.by_status["cancelled"], 3);
    }

    #[test]
    fn minimal_row_takes_defaults() {
        let r: TaskRow = serde_json::from_str(r#"{"id":"5","status":"todo","title":"t"}"#).unwrap();
        assert_eq!((r.priority, r.attachments, r.comments), (0, 0, 0));
        assert!(r.tags.is_empty() && r.dir.is_empty() && r.assignee.is_none());
    }
}
