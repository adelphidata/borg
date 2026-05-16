//! Composer submit / queued submit / paste handling and the small
//! standalone helpers they rely on (`@path` query extraction and the
//! clipboard-image paste path).

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use cortex::agent::AgentEvent;

use super::super::composer::Composer;
use super::super::history::HistoryCell;
use super::{App, AppAction, AppState, QueuedMessage};

impl<'a> App<'a> {
    /// Handle a bracketed paste event (entire pasted text as a single string).
    ///
    /// Popups consume the paste exclusively — either into their text-input
    /// buffer or as a no-op when no input field is active. Paste never leaks
    /// to the composer while any popup is open, regardless of `AppState`.
    pub fn handle_paste(&mut self, text: String) -> AppAction {
        // Popups get absolute first priority across ALL states.
        // A popup can be visible during Streaming if drain_queued_if_idle
        // started a turn while a popup was open.
        if self.dispatch_paste_to_popup(&text) {
            return AppAction::Continue;
        }

        match self.state {
            AppState::Idle | AppState::AwaitingInput { .. } => {
                self.composer.handle_paste(&text);
                let composer_text = self.composer.text();
                if composer_text.starts_with('/') {
                    self.command_popup.update_filter(&composer_text);
                }
                AppAction::Continue
            }
            _ => {
                // While streaming, queue the pasted text as a steer.
                self.pending_steers.push_back(text);
                AppAction::Continue
            }
        }
    }

    /// If `trimmed` invokes a user-defined slash command (`/name [args]`),
    /// return the expanded template body. Returns `None` for built-ins,
    /// non-slash input, or unknown names — those proceed to the existing
    /// dispatcher / user-message path.
    pub(super) fn expand_custom_command(&self, trimmed: &str) -> Option<String> {
        if !trimmed.starts_with('/') {
            return None;
        }
        let (name, args) = match trimmed.split_once(char::is_whitespace) {
            Some((n, rest)) => (n, rest.trim_start()),
            None => (trimmed, ""),
        };
        let cmd = self.command_popup.lookup(name)?;
        let template = cmd.template.as_deref()?;
        Some(super::super::custom_commands::expand_template(template, args))
    }

    pub(in crate::tui) fn handle_submit(&mut self, input: &str) -> Result<AppAction> {
        // User-defined slash commands (`~/.borg/commands/<name>.md`) expand
        // their template and flow down the normal user-message path. Must run
        // BEFORE built-in dispatch so a custom command isn't rejected as
        // "unknown command", and so it can never shadow a built-in (the
        // popup already drops collisions; this is a second guard).
        let expanded_owned = self.expand_custom_command(input.trim());
        let input: &str = match &expanded_owned {
            Some(s) => s.as_str(),
            None => input,
        };
        if expanded_owned.is_none() {
            if let Some(result) = self.try_handle_command(input.trim()) {
                return result;
            }
        }

        self.cells.push(HistoryCell::User {
            text: input.to_string(),
        });
        self.cells.push(HistoryCell::assistant(String::new(), true));

        let images = self.composer.take_image_attachments();

        // Client-side `@path` expansion. Files get read (≤50 KB),
        // directories get listed (depth 1), blocked/missing paths
        // produce skip cards. Cards render as one-line rows above the
        // assistant response; raw content goes only to the LLM via
        // `augmented_text`.
        let expansion = cortex::mentions::expand_mentions(input, &self.config);
        if !expansion.cards.is_empty() {
            let insert_at = self.cells.len().saturating_sub(1);
            for card in &expansion.cards {
                self.cells.insert(
                    insert_at,
                    HistoryCell::MentionCard {
                        label: card.label(),
                    },
                );
            }
        }
        let final_input = expansion.augmented_text;

        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        self.event_rx = Some(event_rx);

        let cancel = CancellationToken::new();
        self.cancel_token = Some(cancel.clone());

        self.enter_streaming();

        Ok(AppAction::SendMessage {
            input: final_input,
            images,
            event_tx,
            cancel,
        })
    }

    /// Pop the next queued message (FIFO) for dispatch.
    pub fn pop_next_queued(&mut self) -> Option<QueuedMessage> {
        self.queued_messages.pop_front()
    }

    /// Submit a queued message (called from the event loop when a turn completes).
    /// Pushes the User cell to scrollback now that the queued message is
    /// actually being sent — at queue time the message lives only in the
    /// queue preview widget.
    pub fn handle_queued_submit(&mut self, qm: QueuedMessage) -> Result<AppAction> {
        self.cells.push(HistoryCell::User {
            text: qm.text.clone(),
        });
        // Expand `@path` mentions in the queued text. Cards render
        // between the user cell and the assistant cell we're about to push.
        // See `handle_submit` for the same pattern.
        let expansion = cortex::mentions::expand_mentions(&qm.text, &self.config);
        for card in &expansion.cards {
            self.cells.push(HistoryCell::MentionCard {
                label: card.label(),
            });
        }
        let final_input = expansion.augmented_text;

        self.cells.push(HistoryCell::assistant(String::new(), true));

        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        self.event_rx = Some(event_rx);

        let cancel = CancellationToken::new();
        self.cancel_token = Some(cancel.clone());

        self.enter_streaming();

        Ok(AppAction::SendMessage {
            input: final_input,
            images: qm.images,
            event_tx,
            cancel,
        })
    }
}

/// Extract the query portion after the last `@` in text, if it looks like a file
/// mention in progress (preceded by whitespace or at position 0, and no space after it).
pub(super) fn extract_at_query(text: &str) -> Option<String> {
    let at_pos = text.rfind('@')?;
    if at_pos > 0 {
        let prev = text.as_bytes()[at_pos - 1];
        if prev != b' ' && prev != b'\t' && prev != b'\n' {
            return None;
        }
    }
    let after = &text[at_pos + 1..];
    if after.contains(' ') {
        return None;
    }
    Some(after.to_string())
}

/// Try to paste a clipboard image into the composer. Returns true if an image was pasted.
pub(super) fn try_paste_clipboard_image(composer: &mut Composer) -> bool {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return false;
    };

    if let Ok(img_data) = clipboard.get_image() {
        if let Some(png_bytes) = rgba_to_png(
            &img_data.bytes,
            img_data.width as u32,
            img_data.height as u32,
        ) {
            composer.add_image(png_bytes, "image/png".to_string());
            return true;
        }
    }

    if let Ok(text) = clipboard.get_text() {
        let trimmed = text.trim();
        let path = std::path::Path::new(trimmed);
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            const IMG_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "heic", "heif"];
            if IMG_EXTS.contains(&ext_lower.as_str()) && path.exists() {
                if let Ok(bytes) = std::fs::read(path) {
                    let mime = match ext_lower.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        "bmp" => "image/bmp",
                        "heic" | "heif" => "image/heic",
                        _ => "application/octet-stream",
                    };
                    composer.add_image(bytes, mime.to_string());
                    return true;
                }
            }
        }
    }

    false
}

/// Convert raw RGBA pixel data to PNG bytes.
fn rgba_to_png(rgba_bytes: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(width, height, rgba_bytes.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}
