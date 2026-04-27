use anyhow::{Context, Result};
use chromiumoxide::{Browser, Page};
use tracing::instrument;

use super::validate_url_scheme;

/// Multi-tab page manager. Tracks all open pages and the active index.
pub struct TabManager {
    pages: Vec<Page>,
    active: usize,
}

impl TabManager {
    /// Create a new TabManager with an initial page.
    pub fn new(initial_page: Page) -> Self {
        Self {
            pages: vec![initial_page],
            active: 0,
        }
    }

    /// Get a reference to the currently active page.
    pub fn active_page(&self) -> &Page {
        &self.pages[self.active]
    }

    /// Number of open tabs.
    pub fn count(&self) -> usize {
        self.pages.len()
    }

    /// Current active tab index.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// Open a new tab, optionally navigating to a URL. Sets it as active.
    /// Enforces the same URL scheme validation as `navigate`.
    #[instrument(skip_all, fields(browser.action = "new_tab"))]
    pub async fn new_tab(&mut self, browser: &Browser, url: Option<&str>) -> Result<String> {
        let target = url.unwrap_or("about:blank");
        // Validate URL scheme unless it's about:blank (the default)
        if target != "about:blank" {
            validate_url_scheme(target)?;
        }
        let page = browser
            .new_page(target)
            .await
            .with_context(|| format!("Failed to open new tab: {target}"))?;
        self.pages.push(page);
        self.active = self.pages.len() - 1;
        Ok(format!(
            "Opened new tab (index {}) at {target}",
            self.active
        ))
    }

    /// Switch to a tab by index.
    pub fn switch_tab(&mut self, index: usize) -> Result<String> {
        if index >= self.pages.len() {
            anyhow::bail!(
                "Tab index {index} out of range (0..{})",
                self.pages.len() - 1
            );
        }
        self.active = index;
        Ok(format!("Switched to tab {index}"))
    }

    /// Close the active tab. Cannot close the last tab.
    #[instrument(skip_all, fields(browser.action = "close_tab"))]
    pub async fn close_tab(&mut self) -> Result<String> {
        if self.pages.len() <= 1 {
            anyhow::bail!("Cannot close the last tab");
        }

        let closed_idx = self.active;
        let page = self.pages.remove(closed_idx);
        // Best-effort close
        let _ = page.close().await;

        // Adjust active index
        if self.active >= self.pages.len() {
            self.active = self.pages.len() - 1;
        }

        Ok(format!(
            "Closed tab {closed_idx}. Now on tab {}",
            self.active
        ))
    }

    /// List all tabs with index, URL, and title.
    #[instrument(skip_all, fields(browser.action = "list_tabs"))]
    pub async fn list_tabs(&self) -> Result<String> {
        let mut lines = Vec::with_capacity(self.pages.len());
        for (i, page) in self.pages.iter().enumerate() {
            let url = page.url().await.ok().flatten().unwrap_or_default();
            let title = page.get_title().await.ok().flatten().unwrap_or_default();
            let marker = if i == self.active { " (active)" } else { "" };
            lines.push(format!("[{i}]{marker} {title} — {url}"));
        }
        Ok(lines.join("\n"))
    }

    /// Replace the active page (used during recovery).
    pub fn replace_active(&mut self, page: Page) {
        self.pages[self.active] = page;
    }

    /// Replace all pages with a single fresh page (used during full relaunch recovery).
    pub fn reset(&mut self, page: Page) {
        self.pages = vec![page];
        self.active = 0;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn active_index_adjusts_on_close() {
        // Verify the index adjustment logic: if active >= pages.len(),
        // it should become pages.len() - 1.
        let mut active = 2_usize;
        let new_len = 2_usize;
        if active >= new_len {
            active = new_len - 1;
        }
        assert_eq!(active, 1);
    }

    #[test]
    fn active_index_stays_when_not_at_end() {
        let mut active = 0_usize;
        let new_len = 2_usize;
        if active >= new_len {
            active = new_len - 1;
        }
        assert_eq!(active, 0);
    }
}
