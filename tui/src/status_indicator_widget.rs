//! A live status indicator that shows the *latest* log line emitted by the
//! application while the agent is processing a long‑running task.
//!
//! # Divergences from upstream Codex TUI
//!
//! `codex-potter` uses this widget a bit differently:
//!
//! - The header may include an optional round prefix with a total timer (e.g.
//!   `Round 2/10 (4m 13s) · Working`).
//! - Remaining context is rendered in the status line while a task is running.
//! - We do not show an "esc to interrupt" hint (even though <kbd>Esc</kbd> interrupts running tasks).

use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use unicode_width::UnicodeWidthStr;

use crate::line_truncation::truncate_line_to_width;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::render::renderable::Renderable;
use crate::status_line::StatusLine;
use crate::status_line::render_status_line;
use crate::text_formatting::capitalize_first;
use crate::tui::FrameRequester;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

pub const STATUS_DETAILS_DEFAULT_MAX_LINES: usize = 3;
const DETAILS_PREFIX: &str = "  └ ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDetailsCapitalization {
    CapitalizeFirst,
    Preserve,
}

/// Renders a live status line (with optional details) while a task is running.
pub struct StatusIndicatorWidget {
    /// Animated header text (defaults to "Working").
    header: String,
    header_prefix: Option<String>,
    header_prefix_elapsed_offset: Option<Duration>,
    details: Option<String>,
    details_max_lines: usize,
    inline_message: Option<String>,
    context_window_percent: Option<i64>,
    context_window_used_tokens: Option<i64>,
    // Single-turn runners (for example `codex-potter`) may want a context indicator even when the
    // bottom composer uses custom hints or is temporarily disabled.
    show_context_window: bool,

    elapsed_running: Duration,
    last_resume_at: Instant,
    is_paused: bool,
    frame_requester: FrameRequester,
    animations_enabled: bool,
}

/// Format elapsed seconds into a compact, human-friendly form used by the status line.
///
/// Examples: `0s`, `59s`, `1m 00s`, `59m 59s`, `1h 00m 00s`, `2h 03m 09s`.
pub fn fmt_elapsed_compact(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        return format!("{elapsed_secs}s");
    }
    if elapsed_secs < 3600 {
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

impl StatusIndicatorWidget {
    pub fn new(frame_requester: FrameRequester, animations_enabled: bool) -> Self {
        Self {
            header: String::from("Working"),
            header_prefix: None,
            header_prefix_elapsed_offset: None,
            details: None,
            details_max_lines: STATUS_DETAILS_DEFAULT_MAX_LINES,
            inline_message: None,
            context_window_percent: None,
            context_window_used_tokens: None,
            show_context_window: false,
            elapsed_running: Duration::ZERO,
            last_resume_at: Instant::now(),
            is_paused: false,
            frame_requester,
            animations_enabled,
        }
    }

    /// Update the animated header label (left of the brackets).
    pub fn update_header(&mut self, header: String) {
        self.header = header;
    }

    pub fn update_header_prefix(&mut self, prefix: Option<String>) {
        self.header_prefix = prefix.filter(|prefix| !prefix.is_empty());
    }

    /// Configure an additional elapsed timer shown after the header prefix (when present).
    ///
    /// This is primarily used by `codex-potter` to display the total elapsed time since the
    /// current project started, while still keeping the existing per-turn timer.
    pub fn set_header_prefix_elapsed_start(&mut self, started_at: Option<Instant>) {
        self.header_prefix_elapsed_offset =
            started_at.map(|started_at| self.last_resume_at.saturating_duration_since(started_at));
    }

    /// Update the details text shown below the header.
    pub fn update_details(
        &mut self,
        details: Option<String>,
        capitalization: StatusDetailsCapitalization,
        max_lines: usize,
    ) {
        self.details_max_lines = max_lines.max(1);
        self.details = details
            .filter(|details| !details.is_empty())
            .map(|details| {
                let trimmed = details.trim_start();
                match capitalization {
                    StatusDetailsCapitalization::CapitalizeFirst => capitalize_first(trimmed),
                    StatusDetailsCapitalization::Preserve => trimmed.to_string(),
                }
            });
    }

    pub fn update_inline_message(&mut self, message: Option<String>) {
        self.inline_message = message
            .map(|message| message.trim().to_string())
            .filter(|message| !message.is_empty());
    }

    #[cfg(test)]
    pub fn header(&self) -> &str {
        &self.header
    }

    #[cfg(test)]
    pub fn details(&self) -> Option<&str> {
        self.details.as_deref()
    }

    #[cfg(test)]
    pub fn inline_message(&self) -> Option<&str> {
        self.inline_message.as_deref()
    }

    pub fn set_context_window_percent(&mut self, percent: Option<i64>) {
        self.context_window_percent = percent;
    }

    pub fn set_context_window_used_tokens(&mut self, used_tokens: Option<i64>) {
        self.context_window_used_tokens = used_tokens;
    }

    pub fn set_context_window_visible(&mut self, visible: bool) {
        self.show_context_window = visible;
    }

    #[cfg(test)]
    pub fn pause_timer_at(&mut self, now: Instant) {
        if self.is_paused {
            return;
        }
        self.elapsed_running += now.saturating_duration_since(self.last_resume_at);
        self.is_paused = true;
    }

    #[cfg(test)]
    pub fn resume_timer_at(&mut self, now: Instant) {
        if !self.is_paused {
            return;
        }
        self.last_resume_at = now;
        self.is_paused = false;
        self.frame_requester.schedule_frame();
    }

    fn elapsed_duration_at(&self, now: Instant) -> Duration {
        let mut elapsed = self.elapsed_running;
        if !self.is_paused {
            elapsed += now.saturating_duration_since(self.last_resume_at);
        }
        elapsed
    }

    #[cfg(test)]
    fn elapsed_seconds_at(&self, now: Instant) -> u64 {
        self.elapsed_duration_at(now).as_secs()
    }

    /// Returns the elapsed time shown in the status indicator, in seconds.
    pub fn elapsed_seconds(&self) -> u64 {
        self.elapsed_duration_at(Instant::now()).as_secs()
    }

    /// Wrap the details text into a fixed width and return the lines, truncating if necessary.
    fn wrapped_details_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(details) = self.details.as_deref() else {
            return Vec::new();
        };
        if width == 0 {
            return Vec::new();
        }

        let prefix_width = UnicodeWidthStr::width(DETAILS_PREFIX);
        let opts = RtOptions::new(usize::from(width))
            .initial_indent(Line::from(DETAILS_PREFIX.dim()))
            .subsequent_indent(Line::from(Span::from(" ".repeat(prefix_width)).dim()))
            .break_words(true);

        let mut out = word_wrap_lines(details.lines().map(|line| vec![line.dim()]), opts);

        if out.len() > self.details_max_lines {
            out.truncate(self.details_max_lines);
            if let Some(last) = out.last_mut() {
                let available_width = usize::from(width).saturating_sub(1);
                let mut truncated = truncate_line_to_width(last.clone(), available_width);
                truncated.spans.push(Span::from("…").dim());
                *last = truncated;
            }
        }

        out
    }
}

impl Renderable for StatusIndicatorWidget {
    fn desired_height(&self, width: u16) -> u16 {
        1 + u16::try_from(self.wrapped_details_lines(width).len()).unwrap_or(0)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        // Schedule next animation frame.
        self.frame_requester
            .schedule_frame_in(Duration::from_millis(32));
        let now = Instant::now();
        let elapsed_duration = self.elapsed_duration_at(now);

        let mut lines = Vec::new();
        let status_line = render_status_line(
            &StatusLine {
                header: self.header.clone(),
                header_prefix: self.header_prefix.clone(),
                header_prefix_elapsed: self
                    .header_prefix_elapsed_offset
                    .map(|offset| offset.saturating_add(elapsed_duration)),
                elapsed: elapsed_duration,
                inline_message: self.inline_message.clone(),
                context_window_percent: self.context_window_percent,
                context_window_used_tokens: self.context_window_used_tokens,
                show_context_window: self.show_context_window,
            },
            Some(self.last_resume_at),
            self.animations_enabled,
        );
        lines.push(truncate_line_with_ellipsis_if_overflow(
            status_line,
            usize::from(area.width),
        ));
        if area.height > 1 {
            // If there is enough space, add the details lines below the header.
            let details = self.wrapped_details_lines(area.width);
            let max_details = usize::from(area.height.saturating_sub(1));
            lines.extend(details.into_iter().take(max_details));
        }

        Paragraph::new(Text::from(lines)).render_ref(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;
    use std::time::Instant;

    use pretty_assertions::assert_eq;

    #[test]
    fn fmt_elapsed_compact_formats_seconds_minutes_hours() {
        assert_eq!(fmt_elapsed_compact(0), "0s");
        assert_eq!(fmt_elapsed_compact(1), "1s");
        assert_eq!(fmt_elapsed_compact(59), "59s");
        assert_eq!(fmt_elapsed_compact(60), "1m 00s");
        assert_eq!(fmt_elapsed_compact(61), "1m 01s");
        assert_eq!(fmt_elapsed_compact(3 * 60 + 5), "3m 05s");
        assert_eq!(fmt_elapsed_compact(59 * 60 + 59), "59m 59s");
        assert_eq!(fmt_elapsed_compact(3600), "1h 00m 00s");
        assert_eq!(fmt_elapsed_compact(3600 + 60 + 1), "1h 01m 01s");
        assert_eq!(fmt_elapsed_compact(25 * 3600 + 2 * 60 + 3), "25h 02m 03s");
    }

    #[test]
    fn renders_with_working_header() {
        let w = StatusIndicatorWidget::new(crate::tui::FrameRequester::test_dummy(), true);

        // Render into a fixed-size test terminal and snapshot the backend.
        let mut terminal = Terminal::new(TestBackend::new(80, 2)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn renders_truncated() {
        let w = StatusIndicatorWidget::new(crate::tui::FrameRequester::test_dummy(), true);

        // Render into a fixed-size test terminal and snapshot the backend.
        let mut terminal = Terminal::new(TestBackend::new(20, 2)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn renders_with_context_window_percent() {
        let mut w = StatusIndicatorWidget::new(crate::tui::FrameRequester::test_dummy(), false);
        w.set_context_window_visible(true);
        w.set_context_window_percent(Some(85));

        // Freeze time-dependent rendering (elapsed + spinner) to keep the snapshot stable.
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        let mut terminal = Terminal::new(TestBackend::new(80, 1)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn renders_wrapped_details_panama_two_lines() {
        let mut w = StatusIndicatorWidget::new(crate::tui::FrameRequester::test_dummy(), false);
        w.update_details(
            Some("A man a plan a canal panama".to_string()),
            StatusDetailsCapitalization::CapitalizeFirst,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );

        // Freeze time-dependent rendering (elapsed + spinner) to keep the snapshot stable.
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        // Prefix is 4 columns, so a width of 30 yields a content width of 26: one column
        // short of fitting the whole phrase (27 cols), forcing exactly one wrap without ellipsis.
        let mut terminal = Terminal::new(TestBackend::new(30, 3)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn renders_with_round_prefix() {
        let mut w = StatusIndicatorWidget::new(crate::tui::FrameRequester::test_dummy(), false);
        w.update_header_prefix(Some("Round 1/10".to_string()));

        // Freeze time-dependent rendering (elapsed + spinner) to keep the snapshot stable.
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        let mut terminal = Terminal::new(TestBackend::new(80, 1)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn timer_pauses_when_requested() {
        let mut widget = StatusIndicatorWidget::new(crate::tui::FrameRequester::test_dummy(), true);

        let baseline = Instant::now();
        widget.last_resume_at = baseline;

        let before_pause = widget.elapsed_seconds_at(baseline + Duration::from_secs(5));
        assert_eq!(before_pause, 5);

        widget.pause_timer_at(baseline + Duration::from_secs(5));
        let paused_elapsed = widget.elapsed_seconds_at(baseline + Duration::from_secs(10));
        assert_eq!(paused_elapsed, before_pause);

        widget.resume_timer_at(baseline + Duration::from_secs(10));
        let after_resume = widget.elapsed_seconds_at(baseline + Duration::from_secs(13));
        assert_eq!(after_resume, before_pause + 3);
    }

    #[test]
    fn details_overflow_adds_ellipsis() {
        let mut w = StatusIndicatorWidget::new(crate::tui::FrameRequester::test_dummy(), true);
        w.update_details(
            Some("abcd abcd abcd abcd".to_string()),
            StatusDetailsCapitalization::CapitalizeFirst,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );

        let lines = w.wrapped_details_lines(6);
        assert_eq!(lines.len(), STATUS_DETAILS_DEFAULT_MAX_LINES);
        let last = lines.last().expect("expected last details line");
        assert_eq!(
            last.spans.last().map(|span| span.content.as_ref()),
            Some("…"),
            "expected ellipsis in last line: {last:?}",
        );
    }
}
