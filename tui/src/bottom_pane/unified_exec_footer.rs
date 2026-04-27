//! Renders and formats unified-exec background session summary text.
//!
//! This module provides one canonical summary string so the bottom pane can
//! reuse the same copy/grammar logic across different status surfaces.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::live_wrap::take_prefix_by_width;
use crate::render::renderable::Renderable;

/// Tracks active unified-exec processes and renders a compact summary.
pub struct UnifiedExecFooter {
    processes: Vec<String>,
}

impl UnifiedExecFooter {
    pub fn new() -> Self {
        Self {
            processes: Vec::new(),
        }
    }

    pub fn set_processes(&mut self, processes: Vec<String>) -> bool {
        if self.processes == processes {
            return false;
        }
        self.processes = processes;
        true
    }

    /// Returns the unindented summary text used by both footer and status-row rendering.
    ///
    /// The returned string intentionally omits leading spaces and separators so
    /// callers can choose layout-specific framing (inline separator vs. row
    /// indentation). Returning `None` means there is nothing to surface.
    pub fn summary_text(&self) -> Option<String> {
        if self.processes.is_empty() {
            return None;
        }

        let count = self.processes.len();
        let plural = if count == 1 { "" } else { "s" };
        Some(format!(
            "{count} background terminal{plural} running · /ps to view · /stop to close"
        ))
    }

    fn render_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width < 4 {
            return Vec::new();
        }
        let Some(summary) = self.summary_text() else {
            return Vec::new();
        };
        let message = format!("  {summary}");
        let (truncated, _, _) = take_prefix_by_width(&message, width as usize);
        vec![Line::from(truncated.dim())]
    }
}

impl Default for UnifiedExecFooter {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderable for UnifiedExecFooter {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        Paragraph::new(self.render_lines(area.width)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.render_lines(width).len() as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn summary_text_empty() {
        let footer = UnifiedExecFooter::new();
        assert_eq!(footer.summary_text(), None);
    }

    #[test]
    fn summary_text_pluralizes() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_processes(vec!["sleep 1".to_string()]);
        assert_eq!(
            footer.summary_text(),
            Some("1 background terminal running · /ps to view · /stop to close".to_string())
        );

        footer.set_processes(vec!["sleep 1".to_string(), "sleep 2".to_string()]);
        assert_eq!(
            footer.summary_text(),
            Some("2 background terminals running · /ps to view · /stop to close".to_string())
        );
    }
}
