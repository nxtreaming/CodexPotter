//! Shared status-line formatting used by the live shimmer and append-only exec hints.

use std::time::Duration;
use std::time::Instant;

use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::exec_cell::spinner;
use crate::shimmer::shimmer_spans;
use crate::token_format::format_tokens_compact;
use crate::ui_colors::secondary_color;

/// A fully resolved single-line status snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub header: String,
    pub header_prefix: Option<String>,
    pub header_prefix_elapsed: Option<Duration>,
    pub elapsed: Duration,
    pub inline_message: Option<String>,
    pub context_window_percent: Option<i64>,
    pub context_window_used_tokens: Option<i64>,
    pub show_context_window: bool,
}

/// Format a single status line using the same text layout as the live shimmer area.
pub fn render_status_line(
    status: &StatusLine,
    spinner_started_at: Option<Instant>,
    animations_enabled: bool,
) -> Line<'static> {
    let pretty_elapsed =
        crate::status_indicator_widget::fmt_elapsed_compact(status.elapsed.as_secs());

    let mut spans = Vec::with_capacity(8);
    spans.push(spinner(spinner_started_at, animations_enabled));
    spans.push(" ".into());
    if let Some(prefix) = status.header_prefix.as_deref() {
        spans.push(Span::styled(
            prefix.to_string(),
            Style::default().fg(secondary_color()).bold(),
        ));
        if let Some(prefix_elapsed) = status.header_prefix_elapsed {
            let pretty_prefix_elapsed =
                crate::status_indicator_widget::fmt_elapsed_compact(prefix_elapsed.as_secs());
            spans.push(format!(" ({pretty_prefix_elapsed})").dim());
        }
        if !status.header.is_empty() {
            spans.push(" · ".dim());
        }
    }
    if animations_enabled {
        spans.extend(shimmer_spans(&status.header));
    } else if !status.header.is_empty() {
        spans.push(status.header.clone().into());
    }
    spans.push(" ".into());
    spans.push(format!("({pretty_elapsed})").dim());

    if let Some(message) = status.inline_message.as_deref() {
        spans.push(" · ".dim());
        spans.push(Span::from(message.to_string()).dim());
    }

    if status.show_context_window {
        spans.push(" · ".dim());
        if let Some(percent) = status.context_window_percent {
            let percent = percent.clamp(0, 100);
            spans.push(format!("{percent}% context left").dim());
        } else if let Some(used_tokens) = status.context_window_used_tokens {
            let used_fmt = format_tokens_compact(used_tokens);
            spans.push(format!("{used_fmt} used").dim());
        } else {
            spans.push("100% context left".dim());
        }
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn line_to_plain_string(line: &Line<'_>) -> String {
        let mut out = String::new();
        for span in &line.spans {
            out.push_str(span.content.as_ref());
        }
        out
    }

    #[test]
    fn render_status_line_formats_round_prefix_elapsed_and_context() {
        let line = render_status_line(
            &StatusLine {
                header: "Updating progress file".to_string(),
                header_prefix: Some("Round 1/10".to_string()),
                header_prefix_elapsed: Some(Duration::from_secs(2650)),
                elapsed: Duration::from_secs(2650),
                inline_message: None,
                context_window_percent: Some(12),
                context_window_used_tokens: None,
                show_context_window: true,
            },
            None,
            false,
        );

        assert_eq!(
            line_to_plain_string(&line),
            "• Round 1/10 (44m 10s) · Updating progress file (44m 10s) · 12% context left"
        );
    }
}
