//! Minimal bottom-pane implementation used by the single-turn runner.
//!
//! The original Codex TUI has a large interactive bottom pane (popups, approvals, etc). For
//! `codex-potter` we only need the legacy composer UX (textarea, file search, paste burst handling)
//! both for capturing the initial prompt and for queuing follow-up prompts while a turn is
//! running.
//!
//! This crate also includes a small subset of upstream selection-style views for CodexPotter's
//! resume action picker.

mod chat_composer;
mod chat_composer_history;
mod command_popup;
mod file_search_popup;
mod footer;
mod fuzzy_match;
mod list_selection_view;
mod paste_burst;
pub mod popup_consts;
mod prompt_args;
mod prompt_footer;
mod queued_user_messages;
mod scroll_state;
mod selection_popup_common;
mod skill_popup;
mod slash_commands;
mod textarea;
mod unified_exec_footer;
mod word_boundary;

pub use chat_composer::ChatComposer;
pub use chat_composer::ChatComposerDraft;
pub use chat_composer::InputResult;
pub use list_selection_view::ListSelectionView;
pub use list_selection_view::SelectionItem;
pub use list_selection_view::SelectionViewParams;
pub use list_selection_view::SideContentWidth;
pub use list_selection_view::popup_content_width;
pub use list_selection_view::side_by_side_layout_widths;
pub use prompt_footer::PromptFooterContext;
pub use prompt_footer::PromptFooterOverride;
#[cfg(test)]
pub use prompt_footer::render_prompt_footer_for_test;
pub use queued_user_messages::QueuedUserMessages;

use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app_event_sender::AppEventSender;
use crate::render::renderable::Renderable;
use crate::status_indicator_widget::STATUS_DETAILS_DEFAULT_MAX_LINES;
use crate::status_indicator_widget::StatusDetailsCapitalization;
use crate::status_indicator_widget::StatusIndicatorWidget;
use crate::tui::FrameRequester;
use prompt_footer::render_prompt_footer;
use unified_exec_footer::UnifiedExecFooter;

/// How long the "press again to quit" hint stays visible.
#[cfg(test)]
pub const QUIT_SHORTCUT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

pub struct BottomPaneParams {
    pub frame_requester: FrameRequester,
    pub enhanced_keys_supported: bool,
    pub app_event_tx: AppEventSender,
    pub animations_enabled: bool,
    pub placeholder_text: String,
    pub disable_paste_burst: bool,
}

/// Pane displayed in the lower half of the inline viewport.
///
/// This is a minimal subset of upstream Codex's `BottomPane`: it owns the prompt input
/// (`ChatComposer`), renders queued user messages while a task is running, and optionally renders
/// a status indicator above the composer.
pub struct BottomPane {
    frame_requester: FrameRequester,
    animations_enabled: bool,

    status: Option<StatusIndicatorWidget>,
    status_header: String,
    status_header_prefix: Option<String>,
    project_started_at: Option<Instant>,
    status_details: Option<String>,
    status_details_capitalization: StatusDetailsCapitalization,
    status_details_max_lines: usize,
    unified_exec_footer: UnifiedExecFooter,
    context_window_percent: Option<i64>,
    context_window_used_tokens: Option<i64>,

    queued_user_messages: QueuedUserMessages,
    composer: ChatComposer,
    prompt_footer_override: Option<PromptFooterOverride>,
    prompt_footer: PromptFooterContext,
}

impl BottomPane {
    pub fn new(params: BottomPaneParams) -> Self {
        let BottomPaneParams {
            frame_requester,
            enhanced_keys_supported,
            app_event_tx,
            animations_enabled,
            placeholder_text,
            disable_paste_burst,
        } = params;

        let mut composer = ChatComposer::new(
            true,
            app_event_tx,
            enhanced_keys_supported,
            placeholder_text,
            disable_paste_burst,
        );
        composer.set_footer_hint_override(Some(Vec::new()));

        Self {
            frame_requester,
            animations_enabled,
            status: None,
            status_header: String::from("Working"),
            status_header_prefix: None,
            project_started_at: None,
            status_details: None,
            status_details_capitalization: StatusDetailsCapitalization::CapitalizeFirst,
            status_details_max_lines: STATUS_DETAILS_DEFAULT_MAX_LINES,
            unified_exec_footer: UnifiedExecFooter::new(),
            context_window_percent: None,
            context_window_used_tokens: None,
            queued_user_messages: QueuedUserMessages::new(),
            composer,
            prompt_footer_override: None,
            // Avoid deriving this from the process cwd so tests stay deterministic. Callers are
            // expected to set this explicitly via `set_prompt_footer_context`.
            prompt_footer: PromptFooterContext::new(PathBuf::from("."), None),
        }
    }

    pub fn composer(&self) -> &ChatComposer {
        &self.composer
    }

    pub fn composer_mut(&mut self) -> &mut ChatComposer {
        &mut self.composer
    }

    pub fn is_task_running(&self) -> bool {
        self.status.is_some()
    }

    pub fn prompt_working_dir(&self) -> &Path {
        &self.prompt_footer.working_dir
    }

    /// Return the current footer context rendered beneath the composer.
    pub fn prompt_footer_context(&self) -> &PromptFooterContext {
        &self.prompt_footer
    }

    pub fn set_task_running(&mut self, running: bool) {
        if running {
            if self.status.is_none() {
                self.status = Some(self.new_status_indicator());
            }
            self.request_redraw();
        } else if self.status.take().is_some() {
            self.request_redraw();
        }
    }

    pub fn set_context_window(&mut self, percent: Option<i64>, used_tokens: Option<i64>) {
        if self.context_window_percent == percent && self.context_window_used_tokens == used_tokens
        {
            return;
        }

        self.context_window_percent = percent;
        self.context_window_used_tokens = used_tokens;

        if let Some(status) = self.status.as_mut() {
            status.set_context_window_visible(true);
            status.set_context_window_percent(percent);
            status.set_context_window_used_tokens(used_tokens);
        }

        self.request_redraw();
    }

    pub fn update_status_header(&mut self, header: String) {
        self.update_status_header_with_details(header, None);
    }

    pub fn update_status_header_with_details(&mut self, header: String, details: Option<String>) {
        self.update_status_header_with_details_options(
            header,
            details,
            StatusDetailsCapitalization::CapitalizeFirst,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );
    }

    pub fn update_status_header_with_details_options(
        &mut self,
        header: String,
        details: Option<String>,
        capitalization: StatusDetailsCapitalization,
        max_lines: usize,
    ) {
        self.status_header = header;
        self.status_details = details.filter(|details| !details.trim().is_empty());
        self.status_details_capitalization = capitalization;
        self.status_details_max_lines = max_lines.max(1);

        if let Some(status) = self.status.as_mut() {
            status.update_header_prefix(self.status_header_prefix.clone());
            status.update_header(self.status_header.clone());
            status.update_details(
                self.status_details.clone(),
                self.status_details_capitalization,
                self.status_details_max_lines,
            );
            status.update_inline_message(self.unified_exec_footer.summary_text());
        }

        self.request_redraw();
    }

    /// Returns the current status indicator header text.
    ///
    /// This is the logical header string tracked by the bottom pane, regardless of whether the
    /// status indicator is currently visible.
    pub fn status_header(&self) -> &str {
        &self.status_header
    }

    pub fn set_status_header_prefix(&mut self, prefix: Option<String>) {
        let prefix = prefix.filter(|value| !value.is_empty());
        if self.status_header_prefix == prefix {
            return;
        }

        self.status_header_prefix = prefix;

        if let Some(status) = self.status.as_mut() {
            status.update_header_prefix(self.status_header_prefix.clone());
        }

        self.request_redraw();
    }

    /// Set the start time for the current CodexPotter project.
    ///
    /// When configured, the live status indicator renders a dim elapsed timer after the round
    /// prefix (e.g. `Round 3/10 (4m 13s) · ...`).
    pub fn set_project_started_at(&mut self, started_at: Option<Instant>) {
        self.project_started_at = started_at;

        if let Some(status) = self.status.as_mut() {
            status.set_header_prefix_elapsed_start(self.project_started_at);
        }

        self.request_redraw();
    }

    pub fn set_queued_user_messages(&mut self, queued: Vec<String>) {
        self.queued_user_messages.messages = queued;
    }

    /// Set a temporary footer override beneath the composer.
    ///
    /// This setter intentionally does not schedule a redraw on its own. Current callers either
    /// draw immediately before blocking on the external editor, or clear the override and request
    /// a frame explicitly once the editor returns.
    pub fn set_prompt_footer_override(&mut self, override_mode: Option<PromptFooterOverride>) {
        self.prompt_footer_override = override_mode;
    }

    /// Set the working directory and optional git branch shown in the prompt footer.
    pub fn set_prompt_footer_context(&mut self, context: PromptFooterContext) {
        if self.prompt_footer == context {
            return;
        }

        self.prompt_footer = context;
        self.request_redraw();
    }

    fn new_status_indicator(&self) -> StatusIndicatorWidget {
        let mut status =
            StatusIndicatorWidget::new(self.frame_requester.clone(), self.animations_enabled);
        status.update_header_prefix(self.status_header_prefix.clone());
        status.set_header_prefix_elapsed_start(self.project_started_at);
        status.update_header(self.status_header.clone());
        status.update_details(
            self.status_details.clone(),
            self.status_details_capitalization,
            self.status_details_max_lines,
        );
        status.update_inline_message(self.unified_exec_footer.summary_text());
        status.set_context_window_visible(true);
        status.set_context_window_percent(self.context_window_percent);
        status.set_context_window_used_tokens(self.context_window_used_tokens);
        status
    }

    pub fn set_unified_exec_processes(&mut self, processes: Vec<String>) {
        if self.unified_exec_footer.set_processes(processes) {
            self.sync_status_inline_message();
            self.request_redraw();
        }
    }

    fn sync_status_inline_message(&mut self) {
        if let Some(status) = self.status.as_mut() {
            status.update_inline_message(self.unified_exec_footer.summary_text());
        }
    }

    fn request_redraw(&self) {
        self.frame_requester.schedule_frame();
    }

    pub fn status_widget(&self) -> Option<&StatusIndicatorWidget> {
        self.status.as_ref()
    }

    #[cfg(test)]
    pub fn status_indicator_mut(&mut self) -> Option<&mut StatusIndicatorWidget> {
        self.status.as_mut()
    }

    #[cfg(test)]
    pub fn context_window_percent(&self) -> Option<i64> {
        self.context_window_percent
    }

    #[cfg(test)]
    pub fn context_window_used_tokens(&self) -> Option<i64> {
        self.context_window_used_tokens
    }
}

impl Renderable for BottomPane {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        const PROMPT_FOOTER_HEIGHT: u16 = 1;
        let width = area.width;
        let prompt_footer_height =
            if !self.composer.selection_popup_visible() && area.height > PROMPT_FOOTER_HEIGHT {
                PROMPT_FOOTER_HEIGHT
            } else {
                0
            };

        let composer_height = self
            .composer
            .desired_height(width)
            .min(area.height.saturating_sub(prompt_footer_height));
        let composer_area = Rect::new(
            area.x,
            area.bottom()
                .saturating_sub(prompt_footer_height.saturating_add(composer_height)),
            area.width,
            composer_height,
        );
        self.composer.render(composer_area, buf);

        if prompt_footer_height > 0 {
            let footer_area = Rect::new(area.x, composer_area.bottom(), area.width, 1);
            render_prompt_footer(
                footer_area,
                buf,
                self.prompt_footer_override,
                &self.prompt_footer.working_dir,
                self.prompt_footer.git_branch.as_deref(),
                self.prompt_footer.yolo_active,
            );
        }

        let height_above_composer = area
            .height
            .saturating_sub(composer_height.saturating_add(prompt_footer_height));
        if height_above_composer == 0 {
            return;
        }

        let top_area = Rect::new(area.x, area.y, area.width, height_above_composer);

        let footer_height = if self.status.is_none() && !self.unified_exec_footer.is_empty() {
            self.unified_exec_footer
                .desired_height(width)
                .saturating_add(1)
        } else {
            0
        };
        let top_status_height = self
            .status
            .as_ref()
            .map(|status| status.desired_height(width).saturating_add(2))
            .unwrap_or(footer_height)
            .min(top_area.height);
        let status_area = Rect::new(top_area.x, top_area.y, top_area.width, top_status_height);
        if let Some(status) = self.status.as_ref() {
            // Leave one blank line above and below the status indicator so it matches the legacy
            // Codex TUI spacing (shimmer should not touch the transcript directly).
            let available_height = status_area.height.saturating_sub(2);
            if available_height > 0 {
                let height = status
                    .desired_height(status_area.width)
                    .min(available_height);
                if height > 0 {
                    status.render(
                        Rect::new(status_area.x, status_area.y + 1, status_area.width, height),
                        buf,
                    );
                }
            }
        } else if !self.unified_exec_footer.is_empty() {
            let height = self
                .unified_exec_footer
                .desired_height(status_area.width)
                .min(status_area.height);
            if height > 0 {
                self.unified_exec_footer.render(
                    Rect::new(status_area.x, status_area.y, status_area.width, height),
                    buf,
                );
            }
        }

        let queue_height = top_area.height.saturating_sub(top_status_height);
        if queue_height == 0 {
            return;
        }
        let queue_area = Rect::new(
            top_area.x,
            top_area.y + top_status_height,
            top_area.width,
            queue_height,
        );
        self.queued_user_messages.render(queue_area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let status_height = self
            .status
            .as_ref()
            .map(|status| status.desired_height(width).saturating_add(2))
            .unwrap_or_else(|| {
                if self.unified_exec_footer.is_empty() {
                    0
                } else {
                    self.unified_exec_footer
                        .desired_height(width)
                        .saturating_add(1)
                }
            });

        status_height
            + self.queued_user_messages.desired_height(width)
            + self.composer.desired_height(width)
            + if self.composer.selection_popup_visible() {
                0
            } else {
                1
            }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        const PROMPT_FOOTER_HEIGHT: u16 = 1;
        let width = area.width;
        let prompt_footer_height =
            if !self.composer.selection_popup_visible() && area.height > PROMPT_FOOTER_HEIGHT {
                PROMPT_FOOTER_HEIGHT
            } else {
                0
            };
        let composer_height = self
            .composer
            .desired_height(width)
            .min(area.height.saturating_sub(prompt_footer_height));
        let composer_area = Rect::new(
            area.x,
            area.bottom()
                .saturating_sub(prompt_footer_height.saturating_add(composer_height)),
            area.width,
            composer_height,
        );
        self.composer.cursor_pos(composer_area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event::AppEvent;
    use tokio::sync::mpsc::unbounded_channel;

    fn new_test_pane() -> BottomPane {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        BottomPane::new(BottomPaneParams {
            frame_requester: FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: AppEventSender::new(tx_raw),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        })
    }

    fn render_snapshot(pane: &BottomPane, area: Rect) -> String {
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf);
        format!("{buf:?}")
    }

    #[test]
    fn unified_exec_footer_renders_when_status_hidden() {
        let mut pane = new_test_pane();
        pane.set_unified_exec_processes(vec!["sleep 5".to_string()]);

        let width = 80;
        let height = pane.desired_height(width);
        let area = Rect::new(0, 0, width, height);
        insta::assert_snapshot!(
            "unified_exec_footer_renders_when_status_hidden",
            render_snapshot(&pane, area)
        );
    }
}
