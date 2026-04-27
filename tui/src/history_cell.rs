//! Transcript/history cells for the Codex TUI.
//!
//! This crate is intentionally pared down for the single-turn runner used by `codex-potter`.

use std::any::Any;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::approvals::ElicitationRequest;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::approvals::GuardianAssessmentEvent;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::FileChange;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use codex_protocol::request_user_input::RequestUserInputEvent;
use ratatui::prelude::*;
use ratatui::style::Style;
use ratatui::style::Styled;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::CODEX_POTTER_VERSION;
use crate::diff_render::create_compact_diff_summary;
use crate::diff_render::create_diff_summary;
use crate::diff_render::display_path_for;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::OutputLinesParams;
use crate::exec_cell::TOOL_CALL_MAX_LINES;
use crate::exec_cell::output_lines;
use crate::live_wrap::take_prefix_by_width;
use crate::render::line_utils::prefix_lines;
use crate::render::line_utils::push_owned_lines;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;
use crate::ui_consts::LIVE_PREFIX_COLS;
use crate::update_action::UpdateAction;
use crate::verbosity::Verbosity;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_lines;

/// Represents an event to display in the conversation history. Returns its
/// `Vec<Line<'static>>` representation to make it easier to display in a
/// scrollable list.
pub trait HistoryCell: std::fmt::Debug + Send + Sync + Any {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    fn desired_height(&self, width: u16) -> u16 {
        Paragraph::new(Text::from(self.display_lines(width)))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .try_into()
            .unwrap_or(0)
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines(width)
    }

    fn desired_transcript_height(&self, width: u16) -> u16 {
        let lines = self.transcript_lines(width);

        // Workaround for ratatui bug: if there's only one line and it's whitespace-only, ratatui
        // gives 2 lines.
        if let [line] = &lines[..]
            && line
                .spans
                .iter()
                .all(|span| span.content.chars().all(char::is_whitespace))
        {
            return 1;
        }

        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .try_into()
            .unwrap_or(0)
    }

    fn is_stream_continuation(&self) -> bool {
        false
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        None
    }
}

impl Renderable for Box<dyn HistoryCell> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let lines = self.display_lines(area.width);
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let y = if area.height == 0 {
            0
        } else {
            let overflow = paragraph
                .line_count(area.width)
                .saturating_sub(usize::from(area.height));
            u16::try_from(overflow).unwrap_or(u16::MAX)
        };
        paragraph.scroll((y, 0)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        HistoryCell::desired_height(self.as_ref(), width)
    }
}

#[derive(Debug)]
pub struct UserHistoryCell {
    pub message: String,
}

impl HistoryCell for UserHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();

        let wrap_width = width
            .saturating_sub(
                LIVE_PREFIX_COLS + 1, /* keep a one-column right margin for wrapping */
            )
            .max(1);

        let style = user_message_style();

        let wrapped = adaptive_wrap_lines(
            self.message.lines().map(|l| Line::from(l).style(style)),
            // Wrap algorithm matches textarea.rs.
            RtOptions::new(usize::from(wrap_width))
                .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit),
        );

        lines.push(Line::from("").style(style));
        lines.extend(prefix_lines(wrapped, "› ".bold().dim(), "  ".into()));
        lines.push(Line::from("").style(style));
        lines
    }
}

pub fn new_user_prompt(message: String) -> UserHistoryCell {
    UserHistoryCell { message }
}

#[derive(Debug)]
pub struct AgentMessageCell {
    lines: Vec<Line<'static>>,
    is_first_line: bool,
}

impl AgentMessageCell {
    pub fn new(lines: Vec<Line<'static>>, is_first_line: bool) -> Self {
        Self {
            lines,
            is_first_line,
        }
    }
}

impl HistoryCell for AgentMessageCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        adaptive_wrap_lines(
            &self.lines,
            RtOptions::new(width as usize)
                .initial_indent(if self.is_first_line {
                    "• ".dim().into()
                } else {
                    "  ".into()
                })
                .subsequent_indent("  ".into()),
        )
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}

#[derive(Debug)]
pub struct PlainHistoryCell {
    lines: Vec<Line<'static>>,
}

impl PlainHistoryCell {
    pub fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }
}

impl HistoryCell for PlainHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines.clone()
    }
}

#[cfg_attr(debug_assertions, allow(dead_code))]
#[derive(Debug)]
pub struct UpdateAvailableHistoryCell {
    latest_version: String,
    update_action: Option<UpdateAction>,
}

#[cfg_attr(debug_assertions, allow(dead_code))]
impl UpdateAvailableHistoryCell {
    pub fn new(latest_version: String, update_action: Option<UpdateAction>) -> Self {
        Self {
            latest_version,
            update_action,
        }
    }
}

impl HistoryCell for UpdateAvailableHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let update_instruction = if let Some(update_action) = self.update_action {
            Line::from(vec![
                "Run ".into(),
                Span::styled(
                    update_action.command_str(),
                    Style::default().cyan().add_modifier(Modifier::BOLD),
                ),
                " to update.".into(),
            ])
        } else {
            Line::from(vec![
                "See ".into(),
                Span::styled(
                    "https://github.com/breezewish/CodexPotter",
                    Style::default().cyan().underlined(),
                ),
                " for installation options.".into(),
            ])
        };

        let content = vec![
            Line::from(vec![
                Span::styled(
                    padded_emoji("✨"),
                    Style::default().cyan().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Update available!",
                    Style::default().cyan().add_modifier(Modifier::BOLD),
                ),
                " ".into(),
                Span::styled(
                    format!("{CODEX_POTTER_VERSION} -> {}", self.latest_version),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            update_instruction,
            "".into(),
            "See full release notes:".into(),
            Line::from(vec![Span::styled(
                "https://github.com/breezewish/CodexPotter/releases/latest",
                Style::default().cyan().underlined(),
            )]),
        ];

        with_border_clamped(content, width)
    }
}

#[derive(Debug)]
pub struct PrefixedWrappedHistoryCell {
    text: Text<'static>,
    initial_prefix: Line<'static>,
    subsequent_prefix: Line<'static>,
}

impl PrefixedWrappedHistoryCell {
    pub fn new(
        text: impl Into<Text<'static>>,
        initial_prefix: impl Into<Line<'static>>,
        subsequent_prefix: impl Into<Line<'static>>,
    ) -> Self {
        Self {
            text: text.into(),
            initial_prefix: initial_prefix.into(),
            subsequent_prefix: subsequent_prefix.into(),
        }
    }
}

/// Render `lines` inside a border sized to the widest span in the content, but clamped so it
/// never exceeds the available terminal `width`.
fn with_border_clamped(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    if width < 4 {
        return Vec::new();
    }

    let max_line_width = lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);

    // Mirrors upstream behavior: clamp to `min(content_width, width - 4)` to avoid border
    // overflow on narrow terminals.
    let max_inner_width = usize::from(width.saturating_sub(4));
    let content_width = max_line_width.min(max_inner_width);

    let mut out = Vec::with_capacity(lines.len() + 2);
    let border_inner_width = content_width + 2;
    out.push(vec![format!("╭{}╮", "─".repeat(border_inner_width)).dim()].into());

    for line in lines.into_iter() {
        let used_width: usize = line
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        let span_count = line.spans.len();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(span_count + 4);
        spans.push(Span::from("│ ").dim());
        spans.extend(line.into_iter());
        if used_width < content_width {
            spans.push(Span::from(" ".repeat(content_width - used_width)).dim());
        }
        spans.push(Span::from(" │").dim());
        out.push(Line::from(spans));
    }

    out.push(vec![format!("╰{}╯", "─".repeat(border_inner_width)).dim()].into());

    out
}

/// Return the emoji followed by a hair space (U+200A).
/// Using only the hair space avoids excessive padding after the emoji while
/// still providing a small visual gap across terminals.
fn padded_emoji(emoji: &str) -> String {
    format!("{emoji}\u{200A}")
}

impl HistoryCell for PrefixedWrappedHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let opts = RtOptions::new(width.max(1) as usize)
            .initial_indent(self.initial_prefix.clone())
            .subsequent_indent(self.subsequent_prefix.clone());
        let wrapped = adaptive_wrap_lines(&self.text, opts);
        let mut out = Vec::new();
        push_owned_lines(&wrapped, &mut out);
        out
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.display_lines(width).len() as u16
    }
}

#[derive(Debug)]
pub struct WebSearchToolCallsCell {
    queries: Vec<String>,
}

pub fn new_web_search_tool_calls(queries: Vec<String>) -> WebSearchToolCallsCell {
    WebSearchToolCallsCell { queries }
}

impl HistoryCell for WebSearchToolCallsCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 || self.queries.is_empty() {
            return Vec::new();
        }

        let wrap_width = width.max(1) as usize;
        let mut out: Vec<Line<'static>> = vec![vec!["• ".dim(), "Searched".bold()].into()];

        for (idx, query) in self.queries.iter().enumerate() {
            let initial_prefix: Line<'static> = if idx == 0 {
                "  └ ".dim().into()
            } else {
                "    ".dim().into()
            };
            let opts = RtOptions::new(wrap_width)
                .initial_indent(initial_prefix)
                .subsequent_indent("    ".dim().into());
            let wrapped = adaptive_wrap_lines(query.lines(), opts);
            out.extend(wrapped);
        }

        out
    }
}

#[derive(Debug)]
pub struct RequestPermissionsCell {
    reason: Option<String>,
    network: Option<codex_protocol::models::NetworkPermissions>,
    file_system: Option<codex_protocol::models::FileSystemPermissions>,
}

pub fn new_request_permissions_event(event: RequestPermissionsEvent) -> RequestPermissionsCell {
    RequestPermissionsCell {
        reason: event.reason,
        network: event.permissions.network,
        file_system: event.permissions.file_system,
    }
}

impl HistoryCell for RequestPermissionsCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let wrap_width = width.max(1) as usize;
        let mut out: Vec<Line<'static>> =
            vec![vec!["• ".dim(), "Requested permissions".bold()].into()];

        let mut details: Vec<String> = Vec::new();
        if let Some(reason) = &self.reason {
            details.push(format!("Reason: {reason}"));
        }
        if let Some(network) = &self.network {
            let label = match network.enabled {
                Some(true) => "Network: enabled",
                Some(false) => "Network: disabled",
                None => "Network: <unspecified>",
            };
            details.push(label.to_string());
        }
        if let Some(file_system) = &self.file_system {
            if let Some(write) = &file_system.write {
                for root in write {
                    details.push(format!("FileSystem write: {}", root.display()));
                }
            }
            if let Some(read) = &file_system.read {
                for root in read {
                    details.push(format!("FileSystem read: {}", root.display()));
                }
            }
        }

        for (idx, detail) in details.iter().enumerate() {
            let initial_prefix: Line<'static> = if idx == 0 {
                "  └ ".dim().into()
            } else {
                "    ".dim().into()
            };
            let opts = RtOptions::new(wrap_width)
                .initial_indent(initial_prefix)
                .subsequent_indent("    ".dim().into());
            out.extend(adaptive_wrap_lines(detail.lines(), opts));
        }

        out
    }
}

#[derive(Debug)]
pub struct RequestUserInputCell {
    question_summaries: Vec<String>,
}

pub fn new_request_user_input_event(event: RequestUserInputEvent) -> RequestUserInputCell {
    let question_summaries = event
        .questions
        .into_iter()
        .map(|question| format!("{}: {}", question.header, question.question))
        .collect();
    RequestUserInputCell { question_summaries }
}

impl HistoryCell for RequestUserInputCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let wrap_width = width.max(1) as usize;
        let mut out: Vec<Line<'static>> =
            vec![vec!["• ".dim(), "User input requested".bold()].into()];

        for (idx, summary) in self.question_summaries.iter().enumerate() {
            let initial_prefix: Line<'static> = if idx == 0 {
                "  └ ".dim().into()
            } else {
                "    ".dim().into()
            };
            let opts = RtOptions::new(wrap_width)
                .initial_indent(initial_prefix)
                .subsequent_indent("    ".dim().into());
            out.extend(adaptive_wrap_lines(summary.lines(), opts));
        }

        out
    }
}

#[derive(Debug)]
pub struct ElicitationRequestCell {
    server_name: String,
    mode: Option<&'static str>,
    message: Option<String>,
    url: Option<String>,
}

pub fn new_elicitation_request_event(event: ElicitationRequestEvent) -> ElicitationRequestCell {
    let mut mode = None;
    let mut message = event.message;
    let mut url = None;

    if let Some(request) = event.request {
        match request {
            ElicitationRequest::Form { message: msg, .. } => {
                mode = Some("form");
                message = Some(msg);
            }
            ElicitationRequest::Url {
                message: msg,
                url: link,
                ..
            } => {
                mode = Some("url");
                message = Some(msg);
                url = Some(link);
            }
        }
    }

    ElicitationRequestCell {
        server_name: event.server_name,
        mode,
        message,
        url,
    }
}

impl HistoryCell for ElicitationRequestCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let wrap_width = width.max(1) as usize;
        let mut out: Vec<Line<'static>> = vec![
            vec![
                "• ".dim(),
                "Elicitation requested".bold(),
                format!(" ({})", self.server_name).dim(),
            ]
            .into(),
        ];

        let mut details: Vec<String> = Vec::new();
        if let Some(mode) = self.mode {
            details.push(format!("Mode: {mode}"));
        }
        if let Some(message) = &self.message {
            details.push(format!("Message: {message}"));
        }
        if let Some(url) = &self.url {
            details.push(format!("URL: {url}"));
        }

        for (idx, detail) in details.iter().enumerate() {
            let initial_prefix: Line<'static> = if idx == 0 {
                "  └ ".dim().into()
            } else {
                "    ".dim().into()
            };
            let opts = RtOptions::new(wrap_width)
                .initial_indent(initial_prefix)
                .subsequent_indent("    ".dim().into());
            out.extend(adaptive_wrap_lines(detail.lines(), opts));
        }

        out
    }
}

#[derive(Debug)]
pub struct GuardianAssessmentCell {
    status: String,
    risk_score: Option<u8>,
    risk_level: Option<String>,
    rationale: Option<String>,
}

pub fn new_guardian_assessment_event(event: GuardianAssessmentEvent) -> GuardianAssessmentCell {
    GuardianAssessmentCell {
        status: format!("{:?}", event.status),
        risk_score: event.risk_score,
        risk_level: event.risk_level.map(|level| format!("{level:?}")),
        rationale: event.rationale,
    }
}

impl HistoryCell for GuardianAssessmentCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let wrap_width = width.max(1) as usize;
        let mut out: Vec<Line<'static>> =
            vec![vec!["• ".dim(), "Guardian assessment".bold()].into()];

        let mut details: Vec<String> = vec![format!("Status: {}", self.status)];
        if let Some(score) = self.risk_score {
            details.push(format!("Risk score: {score}"));
        }
        if let Some(level) = &self.risk_level {
            details.push(format!("Risk level: {level}"));
        }
        if let Some(rationale) = &self.rationale {
            details.push(format!("Rationale: {rationale}"));
        }

        for (idx, detail) in details.iter().enumerate() {
            let initial_prefix: Line<'static> = if idx == 0 {
                "  └ ".dim().into()
            } else {
                "    ".dim().into()
            };
            let opts = RtOptions::new(wrap_width)
                .initial_indent(initial_prefix)
                .subsequent_indent("    ".dim().into());
            out.extend(adaptive_wrap_lines(detail.lines(), opts));
        }

        out
    }
}

pub fn new_info_event(message: String, hint: Option<String>) -> PlainHistoryCell {
    let mut line = vec!["• ".dim(), message.into()];
    if let Some(hint) = hint {
        line.push(" ".into());
        line.push(hint.dim());
    }
    let lines: Vec<Line<'static>> = vec![line.into()];
    PlainHistoryCell { lines }
}

#[derive(Debug, Clone)]
/// Summary information for a background unified-exec terminal entry.
pub struct UnifiedExecProcessDetails {
    /// Command preview shown in the listing.
    pub command_display: String,
    /// Recent output chunks rendered under the command, if available.
    pub recent_chunks: Vec<String>,
}

#[derive(Debug)]
/// History cell that renders the `/ps` output.
pub struct UnifiedExecProcessesOutputCell {
    processes: Vec<UnifiedExecProcessDetails>,
}

/// Build a `/ps` output cell that lists background unified-exec terminals.
pub fn new_unified_exec_processes_output(
    processes: Vec<UnifiedExecProcessDetails>,
) -> UnifiedExecProcessesOutputCell {
    UnifiedExecProcessesOutputCell { processes }
}

impl HistoryCell for UnifiedExecProcessesOutputCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let wrap_width = width as usize;
        let max_processes = 16usize;
        let mut out: Vec<Line<'static>> = vec![
            Line::from("/ps".magenta()),
            "".into(),
            vec!["Background terminals".bold()].into(),
            "".into(),
        ];

        if self.processes.is_empty() {
            out.push("  • No background terminals running.".italic().into());
            return out;
        }

        let prefix = "  • ";
        let prefix_width = UnicodeWidthStr::width(prefix);
        let truncation_suffix = " [...]";
        let truncation_suffix_width = UnicodeWidthStr::width(truncation_suffix);
        let mut shown = 0usize;

        for process in &self.processes {
            if shown >= max_processes {
                break;
            }
            let command = &process.command_display;
            let (snippet, snippet_truncated) = {
                let (first_line, has_more_lines) = match command.split_once('\n') {
                    Some((first, _)) => (first, true),
                    None => (command.as_str(), false),
                };
                let max_graphemes = 80;
                let mut graphemes = first_line.grapheme_indices(true);
                if let Some((byte_index, _)) = graphemes.nth(max_graphemes) {
                    (first_line[..byte_index].to_string(), true)
                } else {
                    (first_line.to_string(), has_more_lines)
                }
            };

            if wrap_width <= prefix_width {
                out.push(Line::from(prefix.dim()));
                shown += 1;
                continue;
            }

            let budget = wrap_width.saturating_sub(prefix_width);
            let mut needs_suffix = snippet_truncated;
            if !needs_suffix {
                let (_, remainder, _) = take_prefix_by_width(&snippet, budget);
                if !remainder.is_empty() {
                    needs_suffix = true;
                }
            }
            if needs_suffix && budget > truncation_suffix_width {
                let available = budget.saturating_sub(truncation_suffix_width);
                let (truncated, _, _) = take_prefix_by_width(&snippet, available);
                out.push(
                    vec![
                        prefix.dim(),
                        Span::from(truncated).cyan(),
                        truncation_suffix.dim(),
                    ]
                    .into(),
                );
            } else {
                let (truncated, _, _) = take_prefix_by_width(&snippet, budget);
                out.push(vec![prefix.dim(), Span::from(truncated).cyan()].into());
            }

            let chunk_prefix_first = "    ↳ ";
            let chunk_prefix_next = "      ";
            for (idx, chunk) in process.recent_chunks.iter().enumerate() {
                let chunk_prefix = if idx == 0 {
                    chunk_prefix_first
                } else {
                    chunk_prefix_next
                };
                let chunk_prefix_width = UnicodeWidthStr::width(chunk_prefix);
                if wrap_width <= chunk_prefix_width {
                    out.push(Line::from(chunk_prefix.dim()));
                    continue;
                }
                let budget = wrap_width.saturating_sub(chunk_prefix_width);
                let (truncated, remainder, _) = take_prefix_by_width(chunk, budget);
                if !remainder.is_empty() && budget > truncation_suffix_width {
                    let available = budget.saturating_sub(truncation_suffix_width);
                    let (shorter, _, _) = take_prefix_by_width(chunk, available);
                    out.push(
                        vec![
                            chunk_prefix.dim(),
                            Span::from(shorter).dim(),
                            truncation_suffix.dim(),
                        ]
                        .into(),
                    );
                } else {
                    out.push(vec![chunk_prefix.dim(), Span::from(truncated).dim()].into());
                }
            }

            shown += 1;
        }

        let remaining = self.processes.len().saturating_sub(shown);
        if remaining > 0 {
            let more_text = format!("... and {remaining} more running");
            if wrap_width <= prefix_width {
                out.push(Line::from(prefix.dim()));
            } else {
                let budget = wrap_width.saturating_sub(prefix_width);
                let (truncated, _, _) = take_prefix_by_width(&more_text, budget);
                out.push(vec![prefix.dim(), Span::from(truncated).dim()].into());
            }
        }

        out
    }
}

#[allow(clippy::disallowed_methods)]
pub fn new_warning_event(message: String) -> PrefixedWrappedHistoryCell {
    PrefixedWrappedHistoryCell::new(message.yellow(), "⚠ ".yellow(), "  ")
}

#[derive(Debug)]
pub struct DeprecationNoticeCell {
    summary: String,
    details: Option<String>,
}

pub fn new_deprecation_notice(summary: String, details: Option<String>) -> DeprecationNoticeCell {
    DeprecationNoticeCell { summary, details }
}

impl HistoryCell for DeprecationNoticeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(vec!["⚠ ".red().bold(), self.summary.clone().red()].into());

        let wrap_width = width.saturating_sub(4).max(1) as usize;

        if let Some(details) = &self.details {
            let line = textwrap::wrap(details, wrap_width)
                .into_iter()
                .map(|s| s.to_string().dim().into())
                .collect::<Vec<_>>();
            lines.extend(line);
        }

        lines
    }
}

pub fn new_error_event(message: String) -> PlainHistoryCell {
    // Use a hair space (U+200A) to create a subtle, near-invisible separation
    // before the text. VS16 is intentionally omitted to keep spacing tighter
    // in terminals like Ghostty.
    let lines: Vec<Line<'static>> = vec![vec![format!("■ {message}").red()].into()];
    PlainHistoryCell { lines }
}

pub fn new_proposed_plan_stream(
    lines: Vec<Line<'static>>,
    is_stream_continuation: bool,
) -> ProposedPlanStreamCell {
    ProposedPlanStreamCell {
        lines,
        is_stream_continuation,
    }
}

/// Render a user‑friendly plan update styled like a checkbox todo list.
pub fn new_plan_update(update: UpdatePlanArgs) -> PlanUpdateCell {
    let UpdatePlanArgs { explanation, plan } = update;
    PlanUpdateCell { explanation, plan }
}

#[derive(Debug)]
pub struct ProposedPlanStreamCell {
    lines: Vec<Line<'static>>,
    is_stream_continuation: bool,
}

impl HistoryCell for ProposedPlanStreamCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines.clone()
    }

    fn is_stream_continuation(&self) -> bool {
        self.is_stream_continuation
    }
}

#[derive(Debug)]
pub struct PlanUpdateCell {
    explanation: Option<String>,
    plan: Vec<PlanItemArg>,
}

impl HistoryCell for PlanUpdateCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let render_note = |text: &str| -> Vec<Line<'static>> {
            let wrap_width = width.saturating_sub(4).max(1) as usize;
            textwrap::wrap(text, wrap_width)
                .into_iter()
                .map(|s| s.to_string().dim().italic().into())
                .collect()
        };

        let render_step = |status: &StepStatus, text: &str| -> Vec<Line<'static>> {
            let (box_str, step_style) = match status {
                StepStatus::Completed => ("✔ ", Style::default().crossed_out().dim()),
                StepStatus::InProgress => ("□ ", Style::default().cyan().bold()),
                StepStatus::Pending => ("□ ", Style::default().dim()),
            };
            let wrap_width = (width as usize)
                .saturating_sub(4)
                .saturating_sub(box_str.width())
                .max(1);
            let parts = textwrap::wrap(text, wrap_width);
            let step_text = parts
                .into_iter()
                .map(|s| s.to_string().set_style(step_style).into())
                .collect();
            prefix_lines(step_text, box_str.into(), "  ".into())
        };

        let mut lines: Vec<Line<'static>> = vec![];
        lines.push(vec!["• ".dim(), "Updated Plan".bold()].into());

        let mut indented_lines = vec![];
        let note = self
            .explanation
            .as_ref()
            .map(|s| s.trim())
            .filter(|t| !t.is_empty());
        if let Some(expl) = note {
            indented_lines.extend(render_note(expl));
        };

        if self.plan.is_empty() {
            indented_lines.push(Line::from("(no steps provided)".dim().italic()));
        } else {
            for PlanItemArg { step, status } in self.plan.iter() {
                indented_lines.extend(render_step(status, step));
            }
        }
        lines.extend(prefix_lines(indented_lines, "  └ ".dim(), "    ".into()));

        lines
    }
}

#[derive(Debug)]
pub struct PatchHistoryCell {
    changes: HashMap<PathBuf, FileChange>,
    cwd: PathBuf,
    compact: bool,
}

impl HistoryCell for PatchHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.compact {
            create_compact_diff_summary(&self.changes, &self.cwd, width as usize)
        } else {
            create_diff_summary(&self.changes, &self.cwd, width as usize)
        }
    }
}

/// Create a new patch cell that lists the file-level summary of a proposed patch.
pub fn new_patch_event(
    changes: HashMap<PathBuf, FileChange>,
    cwd: &Path,
    verbosity: Verbosity,
) -> PatchHistoryCell {
    PatchHistoryCell {
        changes,
        cwd: cwd.to_path_buf(),
        compact: verbosity == Verbosity::Minimal,
    }
}

/// Create a compact patch summary cell that folds multiple patch-apply events into a single
/// transcript item.
///
/// This is used by the `Verbosity::Minimal` renderer to avoid repeating many one-line `Edited ...`
/// items when an agent applies several small patches consecutively.
///
/// Divergence (codex-potter): file paths are listed in patch event order (first occurrence),
/// not sorted alphabetically.
pub fn new_coalesced_compact_patch_event(
    change_sets: &[HashMap<PathBuf, FileChange>],
    cwd: &Path,
) -> PlainHistoryCell {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Verb {
        Added,
        Deleted,
        Edited,
    }

    #[derive(Clone, Debug)]
    struct Summary {
        last_verb: Verb,
        saw_add: bool,
        saw_delete: bool,
        move_path: Option<PathBuf>,
        added: usize,
        removed: usize,
    }

    impl Summary {
        fn new(verb: Verb) -> Self {
            Self {
                last_verb: verb,
                saw_add: verb == Verb::Added,
                saw_delete: verb == Verb::Deleted,
                move_path: None,
                added: 0,
                removed: 0,
            }
        }

        fn update_verb(&mut self, verb: Verb) {
            self.last_verb = verb;
            self.saw_add |= verb == Verb::Added;
            self.saw_delete |= verb == Verb::Deleted;
        }

        fn display_verb(&self) -> Verb {
            if self.last_verb == Verb::Deleted {
                return Verb::Deleted;
            }
            if self.saw_add && self.saw_delete {
                // A delete followed by an add (or vice versa) is best presented as an edit, since
                // the final on-disk state is a replacement rather than a net add/delete.
                return Verb::Edited;
            }
            if self.saw_add {
                return Verb::Added;
            }
            Verb::Edited
        }
    }

    fn add_remove_counts_from_unified_diff(diff: &str) -> (usize, usize) {
        let Ok(patch) = diffy::Patch::from_str(diff) else {
            return (0, 0);
        };

        patch
            .hunks()
            .iter()
            .flat_map(diffy::Hunk::lines)
            .fold((0, 0), |(added, removed), line| match line {
                diffy::Line::Insert(_) => (added + 1, removed),
                diffy::Line::Delete(_) => (added, removed + 1),
                diffy::Line::Context(_) => (added, removed),
            })
    }

    fn render_line_count_summary(added: usize, removed: usize) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        spans.push("(".into());
        spans.push(format!("+{added}").green());
        spans.push(" ".into());
        spans.push(format!("-{removed}").red());
        spans.push(")".into());
        spans
    }

    let mut merged: HashMap<PathBuf, Summary> = HashMap::new();
    // Track output ordering in the same order patch events arrive. Use first occurrence so the
    // list remains stable as counts are updated by later patch events.
    let mut path_order: Vec<PathBuf> = Vec::new();

    for changes in change_sets {
        // `HashMap` iteration order is intentionally non-deterministic; sort the keys so that
        // multi-file patch events render stably, while preserving the higher-level event ordering
        // from the `change_sets` slice.
        let mut paths: Vec<&PathBuf> = changes.keys().collect();
        paths.sort();

        for path in paths {
            let Some(change) = changes.get(path) else {
                continue;
            };
            let verb = match &change {
                FileChange::Add { .. } => Verb::Added,
                FileChange::Delete { .. } => Verb::Deleted,
                FileChange::Update { .. } => Verb::Edited,
            };
            let (added, removed) = match &change {
                FileChange::Add { content } => (content.lines().count(), 0),
                FileChange::Delete { content } => (0, content.lines().count()),
                FileChange::Update { unified_diff, .. } => {
                    add_remove_counts_from_unified_diff(unified_diff)
                }
            };
            let move_path = match &change {
                FileChange::Update {
                    move_path: Some(new_path),
                    ..
                } => Some(new_path.clone()),
                _ => None,
            };

            use std::collections::hash_map::Entry;

            let entry = match merged.entry(path.clone()) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    path_order.push(path.clone());
                    entry.insert(Summary::new(verb))
                }
            };
            entry.update_verb(verb);
            if move_path.is_some() {
                entry.move_path = move_path;
            }
            entry.added = entry.added.saturating_add(added);
            entry.removed = entry.removed.saturating_add(removed);
        }
    }

    if merged.is_empty() {
        return PlainHistoryCell::new(Vec::new());
    }

    let mut rows: Vec<(PathBuf, Summary)> = Vec::with_capacity(merged.len());
    for path in path_order {
        match merged.remove(&path) {
            Some(summary) => rows.push((path, summary)),
            None => debug_assert!(
                false,
                "coalesced patch summary missing expected path in merged map: {}",
                path.display()
            ),
        }
    }
    if !merged.is_empty() {
        debug_assert!(
            false,
            "coalesced patch summary missing expected paths in output order"
        );
        let mut leftovers: Vec<(PathBuf, Summary)> = merged.into_iter().collect();
        leftovers.sort_by(|(a, _), (b, _)| a.cmp(b));
        rows.extend(leftovers);
    }

    let file_count = rows.len();
    let noun = if file_count == 1 { "file" } else { "files" };

    let total_added: usize = rows.iter().map(|(_, s)| s.added).sum();
    let total_removed: usize = rows.iter().map(|(_, s)| s.removed).sum();

    let render_path = |path: &Path, move_path: Option<&PathBuf>| -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        spans.push(display_path_for(path, cwd).into());
        if let Some(move_path) = move_path {
            spans.push(format!(" → {}", display_path_for(move_path, cwd)).into());
        }
        spans
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut header_spans: Vec<Span<'static>> = vec!["• ".dim()];
    if let [(path, summary)] = &rows[..] {
        let verb = match summary.display_verb() {
            Verb::Added => "Added",
            Verb::Deleted => "Deleted",
            Verb::Edited => "Edited",
        };
        header_spans.push(verb.bold());
        header_spans.push(" ".into());
        header_spans.extend(render_path(path, summary.move_path.as_ref()));
        header_spans.push(" ".into());
        header_spans.extend(render_line_count_summary(summary.added, summary.removed));
    } else {
        header_spans.push("Changed".bold());
        header_spans.push(format!(" {file_count} {noun} ").into());
        header_spans.extend(render_line_count_summary(total_added, total_removed));
    }
    out.push(Line::from(header_spans));

    if file_count > 1 {
        for (idx, (path, summary)) in rows.into_iter().enumerate() {
            let prefix = if idx == 0 { "  └ " } else { "    " };
            let mut file_spans: Vec<Span<'static>> = vec![prefix.dim()];
            let verb = match summary.display_verb() {
                Verb::Added => "Added",
                Verb::Deleted => "Deleted",
                Verb::Edited => "Edited",
            };
            file_spans.push(verb.bold());
            file_spans.push(" ".into());
            file_spans.extend(render_path(&path, summary.move_path.as_ref()));
            file_spans.push(" ".into());
            file_spans.extend(render_line_count_summary(summary.added, summary.removed));
            out.push(Line::from(file_spans));
        }
    }

    PlainHistoryCell::new(out)
}

pub fn new_patch_apply_failure(stderr: String) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Failure title
    lines.push(Line::from("✘ Failed to apply patch".magenta().bold()));

    if !stderr.trim().is_empty() {
        let output = output_lines(
            Some(&CommandOutput {
                exit_code: 1,
                aggregated_output: stderr,
                formatted_output: String::new(),
            }),
            OutputLinesParams {
                line_limit: TOOL_CALL_MAX_LINES,
                only_err: true,
                include_angle_pipe: true,
                include_prefix: true,
            },
        );
        lines.extend(output.lines);
    }

    PlainHistoryCell { lines }
}

/// Create a `Viewed Image` cell.
///
/// Divergence (codex-potter): consecutive image tool calls are folded into one transcript item,
/// preserving event order and rendering newly arrived paths into the live viewport immediately.
pub fn new_view_image_tool_calls(paths: &[PathBuf], cwd: &Path) -> PlainHistoryCell {
    if paths.is_empty() {
        return PlainHistoryCell::new(Vec::new());
    }

    let mut lines: Vec<Line<'static>> = vec![vec!["• ".dim(), "Viewed Image".bold()].into()];
    for (idx, path) in paths.iter().enumerate() {
        let prefix = if idx == 0 { "  └ " } else { "    " };
        lines.push(vec![prefix.dim(), display_path_for(path, cwd).dim()].into());
    }

    PlainHistoryCell { lines }
}

#[derive(Debug)]
/// A visual divider between turns, optionally showing how long the assistant "worked for".
///
/// This separator is only emitted for turns that performed concrete work (e.g., running commands,
/// applying patches, making web searches), so purely conversational turns do not show an empty
/// divider.
pub struct FinalMessageSeparator {
    elapsed_seconds: Option<u64>,
}

impl FinalMessageSeparator {
    /// Creates a separator; `elapsed_seconds` typically comes from the status indicator timer.
    pub fn new(elapsed_seconds: Option<u64>) -> Self {
        Self { elapsed_seconds }
    }
}

impl HistoryCell for FinalMessageSeparator {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let elapsed_seconds = self
            .elapsed_seconds
            .map(crate::status_indicator_widget::fmt_elapsed_compact);
        if let Some(elapsed_seconds) = elapsed_seconds {
            let worked_for = format!("─ Worked for {elapsed_seconds} ─");
            let worked_for_width = worked_for.width();
            vec![
                Line::from_iter([
                    worked_for,
                    "─".repeat((width as usize).saturating_sub(worked_for_width)),
                ])
                .dim(),
            ]
        } else {
            vec![Line::from_iter(["─".repeat(width as usize).dim()])]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn display_width(line: &Line<'_>) -> usize {
        line.iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum()
    }

    fn render_lines(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn with_border_clamped_limits_border_to_terminal_width() {
        let width = 20u16;
        let lines = with_border_clamped(vec![Line::from("x".repeat(100))], width);

        assert!(!lines.is_empty());
        assert_eq!(display_width(&lines[0]), usize::from(width));
        assert_eq!(
            display_width(lines.last().expect("missing bottom border")),
            usize::from(width)
        );
    }

    #[test]
    fn with_border_clamped_returns_empty_when_width_too_small() {
        assert!(with_border_clamped(vec![Line::from("hello")], 3).is_empty());
    }

    #[test]
    fn ps_output_empty_snapshot() {
        let cell = new_unified_exec_processes_output(Vec::new());
        let rendered = render_lines(&cell.display_lines(/*width*/ 60)).join("\n");
        insta::assert_snapshot!(rendered);
    }
}
