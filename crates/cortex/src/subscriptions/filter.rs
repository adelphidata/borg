//! Which events a subscription reacts to.
//!
//! Every field is optional and every set field must match (AND). An empty
//! filter matches everything the source produces, which is why the UI asks
//! the user to keep it narrow: every match is a model run.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::presets::Severity;

/// One `path == value` test against the event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldEquals {
    /// Dot path into the payload (`data.issue.level`).
    pub path: String,
    /// Value the field must equal.
    pub value: Value,
}

/// A subscription's filter. Stored as JSON in `subscriptions.filter`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    /// Event kinds to react to. An entry matches the kind itself or any kind
    /// under it: `pull_request` matches `pull_request.opened`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// Case-insensitive text the message, email body or summary must contain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phrase: Option<String>,
    /// Channel sources: only messages that mention Borg.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mention_only: bool,
    /// Channel sources: the channel id the message must be in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// Channel sources: the sender id the message must be from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// Channel sources: the reaction (emoji name) that must be added.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reaction: Option<String>,
    /// Lowest severity to react to (alert sources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity_min: Option<Severity>,
    /// GitHub: repositories (`owner/name`) to react to.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// GitHub: branch a push or pull request must target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// GitHub: at least one of these labels must be on the issue or PR.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// GitHub: login of the person who caused the event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// GitHub: ignore draft pull requests.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub exclude_drafts: bool,
    /// Generic payload tests for sources the named fields do not cover.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub field_equals: Vec<FieldEquals>,
}

/// What the filter looks at for one event.
#[derive(Debug, Clone, Copy)]
pub struct EventView<'a> {
    /// Normalised kind (`pull_request.opened`, `alert.triggered`, …).
    pub kind: Option<&'a str>,
    /// One-line summary.
    pub summary: &'a str,
    /// Free text of the event (message text, email body), when it has one.
    pub text: Option<&'a str>,
    /// Severity, when the source reports one.
    pub severity: Option<Severity>,
    /// The stored payload.
    pub payload: &'a Value,
}

impl Filter {
    /// Parse the stored JSON. An unreadable filter matches nothing, so a
    /// corrupt row cannot turn into "react to everything".
    pub fn parse(json: &str) -> Option<Self> {
        if json.trim().is_empty() {
            return Some(Self::default());
        }
        serde_json::from_str(json).ok()
    }

    /// Whether `event` passes every set field.
    pub fn matches(&self, event: &EventView<'_>) -> bool {
        self.kind_ok(event.kind)
            && self.phrase_ok(event)
            && self.channel_ok(event.payload)
            && self.severity_ok(event.severity)
            && self.github_ok(event.payload)
            && self
                .field_equals
                .iter()
                .all(|f| lookup(event.payload, &f.path) == Some(&f.value))
    }

    fn kind_ok(&self, kind: Option<&str>) -> bool {
        if self.kinds.is_empty() {
            return true;
        }
        let Some(kind) = kind else { return false };
        self.kinds.iter().any(|want| {
            kind == want
                || kind
                    .strip_prefix(want.as_str())
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    fn phrase_ok(&self, event: &EventView<'_>) -> bool {
        let Some(phrase) = self
            .phrase
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return true;
        };
        let needle = phrase.to_lowercase();
        let haystack = event.text.unwrap_or(event.summary).to_lowercase();
        haystack.contains(&needle)
    }

    fn channel_ok(&self, payload: &Value) -> bool {
        let field = |name: &str| payload.get(name).and_then(Value::as_str);
        if self.mention_only && payload.get("mentioned").and_then(Value::as_bool) != Some(true) {
            return false;
        }
        let same = |want: &Option<String>, name: &str| {
            want.as_deref()
                .is_none_or(|w| field(name).is_some_and(|v| v.eq_ignore_ascii_case(w)))
        };
        same(&self.channel_id, "channel_id")
            && same(&self.sender, "sender")
            && same(&self.reaction, "reaction")
    }

    fn severity_ok(&self, severity: Option<Severity>) -> bool {
        match (self.severity_min, severity) {
            (None, _) => true,
            // An event that carries no severity is not silently dropped by a
            // severity floor; the floor is for sources that report one.
            (Some(_), None) => true,
            (Some(min), Some(got)) => got >= min,
        }
    }

    fn github_ok(&self, payload: &Value) -> bool {
        let text = |path: &str| lookup(payload, path).and_then(Value::as_str);
        if !self.repos.is_empty() {
            let Some(repo) = text("repository.full_name") else {
                return false;
            };
            if !self.repos.iter().any(|r| r.eq_ignore_ascii_case(repo)) {
                return false;
            }
        }
        if let Some(branch) = self.branch.as_deref() {
            let target = text("ref")
                .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r))
                .or_else(|| text("pull_request.base.ref"));
            if target != Some(branch) {
                return false;
            }
        }
        if let Some(author) = self.author.as_deref() {
            if !text("sender.login").is_some_and(|l| l.eq_ignore_ascii_case(author)) {
                return false;
            }
        }
        if self.exclude_drafts
            && lookup(payload, "pull_request.draft").and_then(Value::as_bool) == Some(true)
        {
            return false;
        }
        if !self.labels.is_empty() {
            let names = github_labels(payload);
            if !self
                .labels
                .iter()
                .any(|want| names.iter().any(|n| n.eq_ignore_ascii_case(want)))
            {
                return false;
            }
        }
        true
    }
}

/// Every label name on the event: the issue's or PR's labels plus the label
/// just added or removed.
fn github_labels(payload: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    for path in ["pull_request.labels", "issue.labels"] {
        if let Some(list) = lookup(payload, path).and_then(Value::as_array) {
            out.extend(list.iter().filter_map(|l| l.get("name")?.as_str()));
        }
    }
    if let Some(name) = lookup(payload, "label.name").and_then(Value::as_str) {
        out.push(name);
    }
    out
}

/// Walk `value` along a dot path. `a.b.0.c` indexes arrays with numbers; a
/// leading `$.` is accepted for people used to JSONPath.
pub fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.strip_prefix("$.").unwrap_or(path);
    if path.is_empty() || path == "$" {
        return Some(value);
    }
    path.split('.').try_fold(value, |cur, part| match cur {
        Value::Array(items) => items.get(part.parse::<usize>().ok()?),
        Value::Object(map) => map.get(part),
        _ => None,
    })
}

/// [`lookup`] as an owned string. Numbers and booleans are rendered, so an id
/// stored as a number still works as a dedup key.
pub fn lookup_string(value: &Value, path: &str) -> Option<String> {
    match lookup(value, path)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view<'a>(kind: Option<&'a str>, payload: &'a Value) -> EventView<'a> {
        EventView {
            kind,
            summary: "summary line",
            text: payload.get("text").and_then(Value::as_str),
            severity: None,
            payload,
        }
    }

    #[test]
    fn lookup_walks_objects_and_arrays_and_rejects_bad_steps() {
        let v = json!({"a": {"b": [{"c": 7}, {"c": "x"}]}});
        assert_eq!(lookup(&v, "a.b.0.c"), Some(&json!(7)));
        assert_eq!(lookup(&v, "$.a.b.1.c"), Some(&json!("x")));
        assert_eq!(lookup(&v, "a.b.9.c"), None);
        assert_eq!(lookup(&v, "a.b.c"), None, "an array needs an index");
        assert_eq!(lookup_string(&v, "a.b.0.c").as_deref(), Some("7"));
    }

    #[test]
    fn an_unreadable_filter_matches_nothing_and_an_empty_one_everything() {
        assert_eq!(Filter::parse("not json"), None);
        assert_eq!(Filter::parse(""), Some(Filter::default()));
        let payload = json!({});
        assert!(Filter::default().matches(&view(None, &payload)));
    }

    #[test]
    fn filter_table() {
        let pr = json!({
            "action": "opened",
            "repository": {"full_name": "octo/app"},
            "sender": {"login": "mona"},
            "pull_request": {"draft": true, "base": {"ref": "main"}, "labels": [{"name": "bug"}]}
        });
        let push = json!({"ref": "refs/heads/release", "repository": {"full_name": "octo/app"}});
        let slack = json!({"text": "Needs REPRO on checkout", "channel_id": "C1", "sender": "U9", "mentioned": false});

        let cases: Vec<(&str, Filter, Option<&str>, &Value, bool)> = vec![
            (
                "kind prefix",
                Filter {
                    kinds: vec!["pull_request".into()],
                    ..Default::default()
                },
                Some("pull_request.opened"),
                &pr,
                true,
            ),
            (
                "kind prefix needs a dot boundary",
                Filter {
                    kinds: vec!["pull".into()],
                    ..Default::default()
                },
                Some("pull_request.opened"),
                &pr,
                false,
            ),
            (
                "kind set but event has none",
                Filter {
                    kinds: vec!["push".into()],
                    ..Default::default()
                },
                None,
                &pr,
                false,
            ),
            (
                "repo match ignores case",
                Filter {
                    repos: vec!["Octo/App".into()],
                    ..Default::default()
                },
                None,
                &pr,
                true,
            ),
            (
                "repo mismatch",
                Filter {
                    repos: vec!["octo/other".into()],
                    ..Default::default()
                },
                None,
                &pr,
                false,
            ),
            (
                "branch from push ref",
                Filter {
                    branch: Some("release".into()),
                    ..Default::default()
                },
                Some("push"),
                &push,
                true,
            ),
            (
                "branch from PR base",
                Filter {
                    branch: Some("main".into()),
                    ..Default::default()
                },
                None,
                &pr,
                true,
            ),
            (
                "branch mismatch",
                Filter {
                    branch: Some("main".into()),
                    ..Default::default()
                },
                Some("push"),
                &push,
                false,
            ),
            (
                "drafts excluded",
                Filter {
                    exclude_drafts: true,
                    ..Default::default()
                },
                None,
                &pr,
                false,
            ),
            (
                "label present",
                Filter {
                    labels: vec!["BUG".into()],
                    ..Default::default()
                },
                None,
                &pr,
                true,
            ),
            (
                "label absent",
                Filter {
                    labels: vec!["docs".into()],
                    ..Default::default()
                },
                None,
                &pr,
                false,
            ),
            (
                "author",
                Filter {
                    author: Some("mona".into()),
                    ..Default::default()
                },
                None,
                &pr,
                true,
            ),
            (
                "phrase is case-insensitive",
                Filter {
                    phrase: Some("needs repro".into()),
                    ..Default::default()
                },
                None,
                &slack,
                true,
            ),
            (
                "phrase absent",
                Filter {
                    phrase: Some("deploy".into()),
                    ..Default::default()
                },
                None,
                &slack,
                false,
            ),
            (
                "mention required",
                Filter {
                    mention_only: true,
                    ..Default::default()
                },
                None,
                &slack,
                false,
            ),
            (
                "channel id",
                Filter {
                    channel_id: Some("C1".into()),
                    ..Default::default()
                },
                None,
                &slack,
                true,
            ),
            (
                "other channel",
                Filter {
                    channel_id: Some("C2".into()),
                    ..Default::default()
                },
                None,
                &slack,
                false,
            ),
            (
                "field equals",
                Filter {
                    field_equals: vec![FieldEquals {
                        path: "sender".into(),
                        value: json!("U9"),
                    }],
                    ..Default::default()
                },
                None,
                &slack,
                true,
            ),
            (
                "field differs",
                Filter {
                    field_equals: vec![FieldEquals {
                        path: "sender".into(),
                        value: json!("U1"),
                    }],
                    ..Default::default()
                },
                None,
                &slack,
                false,
            ),
        ];
        for (name, filter, kind, payload, want) in cases {
            assert_eq!(filter.matches(&view(kind, payload)), want, "{name}");
        }
    }

    #[test]
    fn severity_floor_drops_lower_events_but_not_unrated_ones() {
        let payload = json!({});
        let floor = Filter {
            severity_min: Some(Severity::Error),
            ..Default::default()
        };
        let at = |s| EventView {
            severity: s,
            ..view(None, &payload)
        };
        assert!(!floor.matches(&at(Some(Severity::Warning))));
        assert!(floor.matches(&at(Some(Severity::Error))));
        assert!(floor.matches(&at(Some(Severity::Critical))));
        assert!(floor.matches(&at(None)));
    }

    #[test]
    fn stored_json_round_trips_and_omits_unset_fields() {
        let f = Filter {
            kinds: vec!["push".into()],
            branch: Some("main".into()),
            ..Default::default()
        };
        let text = serde_json::to_string(&f).unwrap();
        assert_eq!(text, r#"{"kinds":["push"],"branch":"main"}"#);
        assert_eq!(Filter::parse(&text), Some(f));
    }
}
