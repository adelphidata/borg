//! Short-term (working) memory that lives in-memory for the duration of a session.
//!
//! Accumulates facts from tool calls and pre-compaction extraction.
//! Rendered into the system prompt's dynamic suffix as `<working_memory>`.
//! Never persisted directly — flushed to daily log entries on session end,
//! then consolidated into long-term memory by the nightly job.

use crate::tokenizer::estimate_tokens;
use crate::xml_util::escape_xml_attr;

/// Context about the currently active project.
#[derive(Debug, Clone)]
pub struct ProjectContext {
    /// Project ID from the projects table.
    pub id: String,
    /// Human-readable project name.
    pub name: String,
    /// Brief project description.
    pub description: String,
    /// Project status (e.g. "active", "archived").
    pub status: String,
}

/// Category of a short-term memory fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactCategory {
    /// A decision made during the session.
    Decision,
    /// A user preference or correction.
    Preference,
    /// Outcome of a task or tool execution.
    TaskOutcome,
    /// A code/environment fact discovered.
    CodeFact,
    /// A correction to previous behavior.
    Correction,
}

impl std::fmt::Display for FactCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decision => write!(f, "Decision"),
            Self::Preference => write!(f, "Preference"),
            Self::TaskOutcome => write!(f, "TaskOutcome"),
            Self::CodeFact => write!(f, "CodeFact"),
            Self::Correction => write!(f, "Correction"),
        }
    }
}

/// A single fact extracted during the session.
#[derive(Debug, Clone)]
pub struct MemoryFact {
    /// What kind of fact this is.
    pub category: FactCategory,
    /// The fact content.
    pub content: String,
    /// Which conversation turn produced this fact (0-indexed).
    pub source_turn: u32,
    /// Estimated token count for this fact.
    pub tokens: usize,
}

/// Short-term working memory for the current session.
///
/// Accumulates facts and project context in-memory. Rendered into the
/// system prompt on each turn and flushed to a daily log on session end.
#[derive(Debug)]
pub struct ShortTermMemory {
    session_id: String,
    active_project: Option<ProjectContext>,
    facts: Vec<MemoryFact>,
    facts_tokens: usize,
    max_tokens: usize,
}

impl ShortTermMemory {
    /// Create a new empty short-term memory buffer.
    pub fn new(session_id: String, max_tokens: usize) -> Self {
        Self {
            session_id,
            active_project: None,
            facts: Vec::new(),
            facts_tokens: 0,
            max_tokens,
        }
    }

    /// Set the active project context.
    pub fn set_active_project(&mut self, project: ProjectContext) {
        self.active_project = Some(project);
    }

    /// Add a fact to working memory. Drops the oldest fact if over budget.
    pub fn add_fact(&mut self, category: FactCategory, content: String, turn: u32) {
        // Overhead must match what `facts_as_text()` / `render()` emit per fact:
        // `"- [<Category>] "` prefix + trailing newline. Using a static `+5`
        // undersells longer categories (`TaskOutcome`) and lets the tracked
        // `facts_tokens` drift below the real rendered size.
        let overhead = estimate_tokens(&format!("- [{category}] ")) + 1;
        let tokens = estimate_tokens(&content) + overhead;
        self.facts.push(MemoryFact {
            category,
            content,
            source_turn: turn,
            tokens,
        });
        self.facts_tokens += tokens;

        // Evict oldest facts if over budget
        while self.facts_tokens > self.max_tokens && self.facts.len() > 1 {
            let removed = self.facts.remove(0);
            self.facts_tokens = self.facts_tokens.saturating_sub(removed.tokens);
        }
    }

    /// Returns true if there are no facts and no active project.
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty() && self.active_project.is_none()
    }

    /// Estimated token count of the rendered output.
    pub fn token_estimate(&self) -> usize {
        if self.is_empty() {
            return 0;
        }
        let project_tokens = self
            .active_project
            .as_ref()
            .map(|p| estimate_tokens(&p.name) + estimate_tokens(&p.description) + 20)
            .unwrap_or(0);
        self.facts_tokens + project_tokens + 30 // overhead for XML tags
    }

    /// Render as XML for inclusion in the system prompt.
    /// Returns an empty string if there's nothing to render.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }

        let mut out = format!(
            "\n<working_memory session=\"{}\">\n",
            escape_xml_attr(&self.session_id)
        );

        if let Some(ref project) = self.active_project {
            out.push_str(&format!(
                "  <active_project name=\"{}\" status=\"{}\">\n    {}\n  </active_project>\n",
                escape_xml_attr(&project.name),
                escape_xml_attr(&project.status),
                escape_xml_attr(&project.description),
            ));
        }

        if !self.facts.is_empty() {
            out.push_str("  <session_facts>\n");
            for fact in &self.facts {
                out.push_str(&format!(
                    "  - [{}] {}\n",
                    fact.category,
                    escape_xml_attr(&fact.content)
                ));
            }
            out.push_str("  </session_facts>\n");
        }

        out.push_str("</working_memory>\n");
        out
    }

    /// Render facts as plain text for flushing to a daily log entry.
    pub fn facts_as_text(&self) -> String {
        if self.facts.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        for fact in &self.facts {
            out.push_str(&format!("- [{}] {}\n", fact.category, fact.content));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_empty() {
        let stm = ShortTermMemory::new("sess-1".into(), 2000);
        assert!(stm.is_empty());
        assert_eq!(stm.token_estimate(), 0);
    }

    #[test]
    fn add_fact_increments_count() {
        let mut stm = ShortTermMemory::new("sess-1".into(), 2000);
        stm.add_fact(FactCategory::Decision, "chose option A".into(), 0);
        assert!(!stm.is_empty());
        assert_eq!(stm.facts.len(), 1);
        assert!(stm.token_estimate() > 0);
    }

    #[test]
    fn add_fact_respects_budget() {
        let mut stm = ShortTermMemory::new("sess-1".into(), 20);
        // Add facts that exceed the small budget
        for i in 0..10 {
            stm.add_fact(
                FactCategory::CodeFact,
                format!("fact number {i} with some extra text to use tokens"),
                i,
            );
        }
        // Should have evicted older facts
        assert!(stm.facts.len() < 10);
        // Last fact should be the most recent
        assert!(stm.facts.last().unwrap().content.contains("9"));
    }

    #[test]
    fn render_empty_returns_empty() {
        let stm = ShortTermMemory::new("sess-1".into(), 2000);
        assert_eq!(stm.render(), "");
    }

    #[test]
    fn render_with_project() {
        let mut stm = ShortTermMemory::new("sess-1".into(), 2000);
        stm.set_active_project(ProjectContext {
            id: "proj-1".into(),
            name: "borg".into(),
            description: "AI assistant".into(),
            status: "active".into(),
        });

        let rendered = stm.render();
        assert!(rendered.contains("<working_memory"));
        assert!(rendered.contains("<active_project"));
        assert!(rendered.contains("borg"));
        assert!(rendered.contains("AI assistant"));
        assert!(rendered.contains("</working_memory>"));
    }

    #[test]
    fn render_with_facts() {
        let mut stm = ShortTermMemory::new("sess-1".into(), 2000);
        stm.add_fact(FactCategory::Decision, "chose Rust".into(), 0);
        stm.add_fact(FactCategory::Correction, "use snake_case".into(), 1);

        let rendered = stm.render();
        assert!(rendered.contains("<session_facts>"));
        assert!(rendered.contains("[Decision] chose Rust"));
        assert!(rendered.contains("[Correction] use snake_case"));
    }

    #[test]
    fn set_active_project() {
        let mut stm = ShortTermMemory::new("sess-1".into(), 2000);
        assert!(stm.is_empty());

        stm.set_active_project(ProjectContext {
            id: "p1".into(),
            name: "test".into(),
            description: "desc".into(),
            status: "active".into(),
        });
        assert!(!stm.is_empty());
    }

    #[test]
    fn facts_as_text() {
        let mut stm = ShortTermMemory::new("sess-1".into(), 2000);
        stm.add_fact(FactCategory::TaskOutcome, "build succeeded".into(), 0);
        stm.add_fact(FactCategory::Preference, "prefers dark mode".into(), 1);

        let text = stm.facts_as_text();
        assert!(text.contains("[TaskOutcome] build succeeded"));
        assert!(text.contains("[Preference] prefers dark mode"));
    }

    #[test]
    fn facts_as_text_empty() {
        let stm = ShortTermMemory::new("sess-1".into(), 2000);
        assert_eq!(stm.facts_as_text(), "");
    }

    #[test]
    fn facts_tokens_covers_rendered_text_size() {
        // `facts_tokens` must not *undersell* the real rendered size — that
        // was the `+5`-hardcoded-overhead bug: eviction kicked in too late
        // and `<working_memory>` overran its budget. Per-fact estimation is
        // allowed to overshoot slightly (subword packing across facts makes
        // the concatenated string tokenize tighter than the sum-of-prefixes),
        // but it must never undershoot.
        let mut stm = ShortTermMemory::new("s".into(), 10_000);
        stm.add_fact(FactCategory::Decision, "use rust".into(), 0);
        stm.add_fact(FactCategory::TaskOutcome, "migration shipped".into(), 1);
        stm.add_fact(FactCategory::Preference, "dark mode".into(), 2);
        stm.add_fact(FactCategory::CodeFact, "tokio runtime".into(), 3);
        stm.add_fact(FactCategory::Correction, "use snake_case".into(), 4);

        let rendered_tokens = estimate_tokens(&stm.facts_as_text());
        let tracked = stm.facts_tokens;

        assert!(
            tracked >= rendered_tokens,
            "tracked facts_tokens ({tracked}) must not undershoot rendered \
             size ({rendered_tokens}) — that lets eviction run too late"
        );
        // Sanity: shouldn't grossly overshoot either (>2× would mean the
        // overhead accounting is wildly wrong and would evict too aggressively).
        assert!(
            tracked <= rendered_tokens * 2,
            "tracked facts_tokens ({tracked}) is more than 2× rendered \
             size ({rendered_tokens}) — overhead estimate is broken"
        );
    }

    #[test]
    fn tight_budget_keeps_rendered_size_within_bound() {
        // With a tight budget and long-category facts, eviction must leave the
        // rendered output at or below the configured budget. Regression guard
        // for the old `+5` overhead that let `facts_tokens` understate reality
        // (so eviction fired too late and rendered text exceeded max_tokens).
        let mut stm = ShortTermMemory::new("s".into(), 30);
        for i in 0..20 {
            stm.add_fact(
                FactCategory::TaskOutcome,
                format!("outcome number {i} with some padding text"),
                i,
            );
        }
        let rendered = stm.facts_as_text();
        let rendered_tokens = estimate_tokens(&rendered);
        assert!(
            rendered_tokens <= 30 + 2,
            "rendered size {rendered_tokens} must not exceed budget 30 \
             (plus 2-token tolerance for estimator granularity)"
        );
    }

    #[test]
    fn token_estimate_grows_with_facts() {
        let mut stm = ShortTermMemory::new("sess-1".into(), 5000);
        let t0 = stm.token_estimate();
        stm.add_fact(FactCategory::Decision, "first fact".into(), 0);
        let t1 = stm.token_estimate();
        stm.add_fact(FactCategory::Decision, "second fact".into(), 1);
        let t2 = stm.token_estimate();
        assert!(t1 > t0);
        assert!(t2 > t1);
    }
}
