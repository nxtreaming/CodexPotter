use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_protocol::protocol::PotterProjectDetails;
use codex_protocol::protocol::PotterProjectListEntry;
use codex_protocol::protocol::PotterProjectListStatus;
use codex_protocol::protocol::PotterProjectRoundSummary;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::prelude::Widget as _;
use ratatui::style::Style;
use ratatui::style::Styled as _;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use textwrap::Options as WrapOptions;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::human_time::human_time_ago;
use crate::ui_colors::orange_color;

const USER_TASK_PREVIEW_MAX_LINES: usize = 5;
// CodexPotter divergence: in split-pane mode, keep the details text narrower than the pane so
// long task descriptions and round summaries stay easier to scan. Maximized mode still uses the
// full pane width.
const NON_MAXIMIZED_DETAILS_MAX_CONTENT_WIDTH: usize = 100;

#[derive(Debug, Default, Clone, Copy)]
struct OverlayMetrics {
    left_inner_width: u16,
    left_inner_height: u16,
    right_inner_height: u16,
    right_total_lines: usize,
}

/// Controls which footer hints are rendered in the projects overlay.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ProjectsOverlayFooterMode {
    #[default]
    ListOverlay,
    ResumePicker,
}

/// UI-only state machine for the inline projects list overlay (`Ctrl+L` / `/list`).
///
/// Press `Tab` to toggle a maximized details view (hides the left projects list).
///
/// This struct intentionally contains no filesystem/business logic; the CLI workflow layer owns
/// discovery/detail loading and feeds results back through the provider channels.
#[derive(Debug, Default)]
pub struct ProjectsOverlay {
    open: bool,
    maximized: bool,
    footer_mode: ProjectsOverlayFooterMode,

    list_loading: bool,
    list_error: Option<String>,
    projects: Vec<PotterProjectListEntry>,
    selected: usize,
    scroll_top: usize,

    right_scroll: usize,
    /// Cached details payloads keyed by the workdir-relative project directory.
    details_by_project: HashMap<PathBuf, PotterProjectDetails>,

    metrics: OverlayMetrics,
}

impl ProjectsOverlay {
    pub fn set_footer_mode(&mut self, mode: ProjectsOverlayFooterMode) {
        self.footer_mode = mode;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Returns the currently highlighted project's directory, if any.
    pub fn selected_project_dir(&self) -> Option<PathBuf> {
        self.projects
            .get(self.selected)
            .map(|p| p.project_dir.clone())
    }

    /// Open the overlay, or start refreshing its contents if already open.
    ///
    /// When refreshing an already-open overlay, this keeps the current selection and scroll
    /// offsets, and keeps any existing list/details visible until the new responses arrive.
    pub fn open_or_refresh(&mut self) -> crate::ProjectsOverlayRequest {
        let was_open = self.open;
        self.open = true;
        if !was_open {
            self.maximized = false;
        }
        self.list_loading = true;
        self.list_error = None;
        if !was_open {
            self.projects.clear();
            self.selected = 0;
            self.scroll_top = 0;
            self.right_scroll = 0;
            self.details_by_project.clear();
        }
        crate::ProjectsOverlayRequest::List
    }

    pub fn close(&mut self) {
        self.open = false;
        self.maximized = false;
    }

    pub fn on_projects_list(
        &mut self,
        projects: Vec<PotterProjectListEntry>,
        error: Option<String>,
    ) -> Option<crate::ProjectsOverlayRequest> {
        if !self.open {
            return None;
        }

        self.list_loading = false;
        self.list_error = error;
        let previous_selected_dir = self.selected_project_dir();
        let previous_selected = self.selected;
        let previous_scroll_top = self.scroll_top;
        let previous_scroll_top_dir = self
            .projects
            .get(self.scroll_top)
            .map(|project| project.project_dir.clone());
        let previous_right_scroll = self.right_scroll;

        self.projects = projects;

        if self.projects.is_empty() {
            self.selected = 0;
            self.scroll_top = 0;
            self.right_scroll = 0;
            return None;
        }

        let max_selected = self.projects.len().saturating_sub(1);
        self.selected = previous_selected.min(max_selected);

        if let Some(project_dir) = previous_selected_dir.as_ref()
            && let Some(found) = self
                .projects
                .iter()
                .position(|project| &project.project_dir == project_dir)
        {
            self.selected = found;
        }

        let next_selected_dir = self
            .projects
            .get(self.selected)
            .map(|project| &project.project_dir);
        let selection_changed = next_selected_dir != previous_selected_dir.as_ref();
        if selection_changed {
            self.right_scroll = 0;
        } else {
            self.right_scroll = previous_right_scroll;
        }

        self.scroll_top = previous_scroll_top.min(max_selected);
        if let Some(top_dir) = previous_scroll_top_dir
            && let Some(found) = self
                .projects
                .iter()
                .position(|project| project.project_dir == top_dir)
        {
            self.scroll_top = found;
        }

        // Keep the selection visible if the refreshed list shifted item heights.
        self.ensure_selected_visible();
        // If selection snapped above the previous scroll anchor, ensure `scroll_top` doesn't move
        // beyond it.
        self.scroll_top = self.scroll_top.min(self.selected);

        self.selected_project_dir()
            .map(|project_dir| crate::ProjectsOverlayRequest::Details { project_dir })
    }

    pub fn on_project_details(&mut self, details: PotterProjectDetails) {
        // Ignore details responses while a list refresh is in flight. In practice this avoids a
        // race where an older details response (queued before the refresh request) lands after we
        // started refreshing, causing the right pane to flicker before the refreshed list and
        // follow-up details request complete.
        if self.list_loading {
            return;
        }

        self.details_by_project
            .insert(details.project_dir.clone(), details);
    }

    pub fn handle_key_event(
        &mut self,
        key_event: KeyEvent,
    ) -> Option<crate::ProjectsOverlayRequest> {
        if !self.open {
            return None;
        }

        if matches!(key_event.kind, crossterm::event::KeyEventKind::Press)
            && (self.is_ctrl_char(key_event, 'l') || self.is_ctrl_char(key_event, 'c'))
        {
            self.close();
            return None;
        }

        if key_event.modifiers == KeyModifiers::NONE {
            match key_event.code {
                KeyCode::Esc => {
                    self.close();
                    return None;
                }
                KeyCode::Tab if matches!(key_event.kind, crossterm::event::KeyEventKind::Press) => {
                    self.maximized = !self.maximized;
                    return None;
                }
                KeyCode::Up => return self.bump_selection(-1),
                KeyCode::Down => return self.bump_selection(1),
                _ => {}
            }
        }

        if key_event.modifiers == KeyModifiers::SHIFT {
            match key_event.code {
                KeyCode::Up => {
                    self.bump_right_scroll(-3);
                    return None;
                }
                KeyCode::Down => {
                    self.bump_right_scroll(3);
                    return None;
                }
                _ => {}
            }
        }

        if self.is_ctrl_char(key_event, 'u') {
            self.page_right_scroll(-1);
            return None;
        }
        if self.is_ctrl_char(key_event, 'd') {
            self.page_right_scroll(1);
            return None;
        }

        None
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, now: SystemTime) {
        if !self.open || area.is_empty() {
            return;
        }

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        let body_area = layout[0];
        let footer_area = layout[1];

        let separator_width: u16 = 3;
        let available_width = body_area.width.saturating_sub(separator_width);
        let mut left_width =
            u16::try_from(u32::from(available_width) * 38 / 100).unwrap_or_default();
        if available_width > 0 && left_width == 0 {
            left_width = 1;
        }
        left_width = left_width.min(40).min(available_width);
        let right_width = available_width.saturating_sub(left_width);

        let left_area = Rect::new(body_area.x, body_area.y, left_width, body_area.height);
        let list_right_area = Rect::new(
            body_area
                .x
                .saturating_add(left_width)
                .saturating_add(separator_width),
            body_area.y,
            right_width,
            body_area.height,
        );
        let right_area = if self.maximized {
            body_area
        } else {
            list_right_area
        };

        let left_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(left_area);
        let left_list_area = left_layout[0];
        let left_pager_area = left_layout[1];

        let left_content_area = Rect::new(
            left_list_area.x,
            left_list_area.y.saturating_add(1),
            left_list_area.width,
            left_list_area.height.saturating_sub(1),
        );

        let right_content_area = if self.maximized {
            Rect::new(
                right_area.x.saturating_add(2),
                right_area.y.saturating_add(1),
                right_area.width.saturating_sub(4),
                right_area.height.saturating_sub(2),
            )
        } else {
            Rect::new(
                right_area.x,
                right_area.y.saturating_add(1),
                right_area.width.saturating_sub(2),
                right_area.height.saturating_sub(2),
            )
        };

        self.metrics = OverlayMetrics {
            left_inner_width: left_content_area.width,
            left_inner_height: left_content_area.height,
            right_inner_height: right_content_area.height,
            right_total_lines: 0,
        };

        if self.maximized {
            self.ensure_selected_visible();
        } else {
            let left_lines = self.render_left_lines(left_content_area, now);
            Paragraph::new(Text::from(left_lines)).render(left_content_area, buf);
            self.render_left_pager(left_pager_area, buf);
        }

        let right_lines = self.build_right_lines(right_content_area, now);
        self.metrics.right_total_lines = right_lines.len();
        self.right_scroll = self.right_scroll.min(self.max_right_scroll());
        Paragraph::new(Text::from(right_lines))
            .scroll((u16::try_from(self.right_scroll).unwrap_or(u16::MAX), 0))
            .render(right_content_area, buf);

        self.render_footer(footer_area, buf);
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        let footer_area = Rect::new(
            area.x.saturating_add(2),
            area.y,
            area.width.saturating_sub(4),
            area.height,
        );
        if footer_area.is_empty() {
            return;
        }

        let pager_line = self.detail_pager_line(footer_area.width);
        let pager_width = u16::try_from(pager_line.width())
            .unwrap_or(u16::MAX)
            .min(footer_area.width);

        if pager_width == 0 {
            let hint_line = self.footer_hint_line(footer_area.width);
            Paragraph::new(hint_line).render(footer_area, buf);
            return;
        }

        if footer_area.width <= pager_width {
            Paragraph::new(pager_line).render(footer_area, buf);
            return;
        }

        let hint_area = Rect::new(
            footer_area.x,
            footer_area.y,
            footer_area.width.saturating_sub(pager_width),
            footer_area.height,
        );
        let pager_area = Rect::new(
            hint_area.right(),
            footer_area.y,
            pager_width,
            footer_area.height,
        );

        let hint_line = self.footer_hint_line(hint_area.width);
        Paragraph::new(hint_line).render(hint_area, buf);
        Paragraph::new(pager_line).render(pager_area, buf);
    }

    fn footer_hint_line(&self, width: u16) -> Line<'static> {
        let variants = self.footer_hint_variants();
        let fallback = variants.last().cloned().unwrap_or_default();
        variants
            .into_iter()
            .find(|line| line.width() <= usize::from(width))
            .unwrap_or(fallback)
    }

    fn footer_hint_variants(&self) -> Vec<Line<'static>> {
        let tab_hint = if self.maximized {
            " exit maximize"
        } else {
            " maximize"
        };

        match self.footer_mode {
            ProjectsOverlayFooterMode::ListOverlay => vec![
                Line::from(vec![
                    "Esc".into(),
                    " close".dim(),
                    "  ".into(),
                    "Tab".into(),
                    tab_hint.dim(),
                    "  ".into(),
                    "↑↓".into(),
                    " switch".dim(),
                    "  ".into(),
                    "shift+↑↓".into(),
                    " scroll".dim(),
                    "  ".into(),
                    "ctrl+u/d".into(),
                    " page".dim(),
                ]),
                Line::from(vec![
                    "Esc".into(),
                    "  ".into(),
                    "Tab".into(),
                    tab_hint.dim(),
                    "  ".into(),
                    "↑↓".into(),
                    " switch".dim(),
                    " ".into(),
                    "⇧↑↓".into(),
                    " scroll".dim(),
                    " ".into(),
                    "^U/^D".into(),
                    " page".dim(),
                ]),
                Line::from(vec![
                    "Esc".into(),
                    "  ".into(),
                    "Tab".into(),
                    tab_hint.dim(),
                    "  ".into(),
                    "↑↓".into(),
                    " ".into(),
                    "⇧↑↓".into(),
                    " ".into(),
                    "^U/^D".into(),
                ]),
                Line::from(vec!["Esc".into()]),
            ],
            ProjectsOverlayFooterMode::ResumePicker => vec![
                Line::from(vec![
                    "Enter".into(),
                    " resume".dim(),
                    "  ".into(),
                    "Esc".into(),
                    " start new".dim(),
                    "  ".into(),
                    "Tab".into(),
                    tab_hint.dim(),
                    "  ".into(),
                    "↑↓".into(),
                    " switch".dim(),
                    "  ".into(),
                    "shift+↑↓".into(),
                    " scroll".dim(),
                    "  ".into(),
                    "ctrl+u/d".into(),
                    " page".dim(),
                ]),
                Line::from(vec![
                    "Enter".into(),
                    " resume".dim(),
                    "  ".into(),
                    "Esc".into(),
                    " start new".dim(),
                    "  ".into(),
                    "Tab".into(),
                    tab_hint.dim(),
                    "  ".into(),
                    "↑↓".into(),
                    " switch".dim(),
                    "  ".into(),
                    "⇧↑↓".into(),
                    " scroll".dim(),
                    "  ".into(),
                    "^U/^D".into(),
                    " page".dim(),
                ]),
                Line::from(vec![
                    "Enter".into(),
                    " resume".dim(),
                    "  ".into(),
                    "Esc".into(),
                    " start new".dim(),
                    "  ".into(),
                    "Tab".into(),
                    tab_hint.dim(),
                    "  ".into(),
                    "↑↓".into(),
                    " switch".dim(),
                ]),
                Line::from(vec![
                    "Enter".into(),
                    " resume".dim(),
                    "  ".into(),
                    "Esc".into(),
                    " start new".dim(),
                ]),
            ],
        }
    }

    fn render_left_pager(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        Paragraph::new(self.left_pager_line(area.width)).render(area, buf);
    }

    fn left_pager_line(&self, width: u16) -> Line<'static> {
        if self.list_loading
            || self.list_error.is_some()
            || self.projects.is_empty()
            || self.metrics.left_inner_height == 0
            || width == 0
        {
            return Line::from("");
        }

        let wrap_width = project_list_description_wrap_width(self.metrics.left_inner_width);
        let viewport_height = usize::from(self.metrics.left_inner_height);
        let pages = project_list_page_starts(&self.projects, wrap_width, viewport_height);
        let page_count = pages.len().max(1);
        if page_count <= 1 {
            return Line::from("");
        }
        let selected = self.selected.min(self.projects.len().saturating_sub(1));
        let current_page = pages
            .iter()
            .rposition(|page_start| *page_start <= selected)
            .unwrap_or(0);

        let max_width = usize::from(width);
        let max_dots = page_count.min(max_width).clamp(1, 5);
        let mut spans = dots_pager_spans(current_page, page_count, max_dots);

        let dot_count = spans.len();
        let pad = if dot_count < max_width {
            (max_width - dot_count) / 2
        } else {
            0
        };

        if pad > 0 {
            spans.insert(0, Span::from(" ".repeat(pad)));
        }

        Line::from(spans)
    }

    fn detail_pager_line(&self, width: u16) -> Line<'static> {
        if width == 0 || self.metrics.right_total_lines == 0 {
            return Line::from("");
        }

        let viewport_height = usize::from(self.metrics.right_inner_height);
        if viewport_height == 0 {
            return Line::from("");
        }

        let page_count =
            (self.metrics.right_total_lines + viewport_height.saturating_sub(1)) / viewport_height;
        let page_count = page_count.max(1);
        if page_count <= 1 {
            return Line::from("");
        }
        // The details pager is a progress indicator over the rendered content, not a strict page
        // index. When the final "page" is shorter than the viewport, `max_right_scroll()` will be
        // less than `(page_count - 1) * viewport_height`, so integer-dividing by viewport height
        // would prevent the last dot from ever activating.
        let max_scroll = self
            .metrics
            .right_total_lines
            .saturating_sub(viewport_height);
        let current_page = if max_scroll == 0 {
            0
        } else {
            (self.right_scroll.min(max_scroll) * page_count.saturating_sub(1)) / max_scroll
        };

        let max_dots = page_count.min(usize::from(width)).clamp(1, 5);
        Line::from(dots_pager_spans(current_page, page_count, max_dots))
    }

    fn render_left_lines(&mut self, area: Rect, now: SystemTime) -> Vec<Line<'static>> {
        if area.is_empty() {
            return Vec::new();
        }

        if self.projects.is_empty() && self.list_loading {
            return vec![Line::from("Loading projects...")];
        }

        if let Some(err) = self.list_error.as_deref() {
            return vec![
                Line::from(vec![
                    Span::from("Failed to load projects list:").red().bold(),
                ]),
                Line::from(vec![Span::from(err.to_string()).red()]),
            ];
        }

        if self.projects.is_empty() {
            return vec![Line::from(vec![
                Span::from("No projects found under .codexpotter/projects").dim(),
            ])];
        }

        self.ensure_selected_visible();

        let wrap_width = project_list_description_wrap_width(area.width);

        let mut out: Vec<Line<'static>> = Vec::new();
        let mut remaining = usize::from(area.height);

        for (idx, project) in self.projects.iter().enumerate().skip(self.scroll_top) {
            if remaining == 0 {
                break;
            }

            let item_lines =
                render_project_list_item(project, wrap_width, now, idx == self.selected);
            let height = item_lines.len();
            if height > remaining {
                break;
            }

            out.extend(item_lines);

            remaining = remaining.saturating_sub(height);
        }

        out
    }

    fn build_right_lines(&self, area: Rect, now: SystemTime) -> Vec<Line<'static>> {
        if area.is_empty() {
            return Vec::new();
        }

        let wrap_width = if self.maximized {
            usize::from(area.width.max(1))
        } else {
            usize::from(area.width.max(1)).min(NON_MAXIMIZED_DETAILS_MAX_CONTENT_WIDTH)
        };

        if self.projects.is_empty() && self.list_loading {
            return wrap_plain_lines(
                vec![Line::from(vec![Span::from("Loading projects...").dim()])],
                wrap_width,
            );
        }

        if let Some(err) = self.list_error.as_deref() {
            return wrap_plain_lines(
                vec![
                    Line::from(vec![
                        Span::from("Failed to load projects list:").red().bold(),
                    ]),
                    Line::from(vec![Span::from(err.to_string()).red()]),
                ],
                wrap_width,
            );
        }

        if self.projects.is_empty() {
            return wrap_plain_lines(
                vec![Line::from(vec![
                    Span::from("No projects found under .codexpotter/projects").dim(),
                ])],
                wrap_width,
            );
        }

        let Some(selected) = self.projects.get(self.selected) else {
            return wrap_plain_lines(
                vec![Line::from(vec![Span::from("No project selected").dim()])],
                wrap_width,
            );
        };

        let Some(details) = self.details_by_project.get(&selected.project_dir) else {
            return wrap_plain_lines(
                vec![Line::from(vec![
                    Span::from("Loading project details...").dim(),
                ])],
                wrap_width,
            );
        };

        if let Some(err) = details.error.as_deref() {
            return wrap_plain_lines(
                vec![
                    Line::from(vec![
                        Span::from("Failed to load project details:").red().bold(),
                    ]),
                    Line::from(vec![Span::from(err.to_string()).red()]),
                ],
                wrap_width,
            );
        }

        let mut header_spans: Vec<Span<'static>> = Vec::new();
        if let Some(branch) = details
            .git_branch
            .as_deref()
            .filter(|branch| !branch.trim().is_empty())
        {
            header_spans.push(Span::from(branch.to_string()).cyan().dim());
            header_spans.push("  ".dim());
        }
        header_spans.push(Span::from(details.progress_file.to_string_lossy().to_string()).dim());

        let mut lines =
            wrap_plain_lines(vec![Line::from(header_spans), Line::from("")], wrap_width);
        lines.extend(user_task_preview_lines(
            details.user_message.as_deref(),
            wrap_width,
        ));
        if !details.rounds.is_empty() {
            lines.push(Line::from(""));
        }
        for (idx, round) in details.rounds.iter().enumerate() {
            append_round_details(&mut lines, round, wrap_width, now);
            if idx + 1 < details.rounds.len() {
                lines.push(Line::from(""));
            }
        }

        lines
    }

    fn ensure_selected_visible(&mut self) {
        let height = usize::from(self.metrics.left_inner_height);
        // Selection visibility is computed using metrics captured during the last render pass.
        // This keeps key handling decoupled from the terminal layout system.
        if height == 0 || self.projects.is_empty() {
            self.scroll_top = self.scroll_top.min(self.selected);
            return;
        }

        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
            return;
        }

        let wrap_width = project_list_description_wrap_width(self.metrics.left_inner_width);
        let mut used = 0usize;
        let mut last_visible = self.scroll_top;

        for (idx, project) in self.projects.iter().enumerate().skip(self.scroll_top) {
            let item_height = project_list_item_height(project, wrap_width);
            if used + item_height > height {
                break;
            }
            used += item_height;
            last_visible = idx;
        }

        if self.selected <= last_visible {
            return;
        }

        // Scrolling keeps the selection visible by snapping it to the top when it moves past the
        // viewport. (If a single list item is taller than the viewport, no earlier `scroll_top`
        // can make it fully visible, so snapping remains the least surprising behavior.)
        self.scroll_top = self.selected;
    }

    fn bump_selection(&mut self, delta: i32) -> Option<crate::ProjectsOverlayRequest> {
        if self.projects.is_empty() {
            return None;
        }

        let max = self.projects.len().saturating_sub(1);
        let selected = i32::try_from(self.selected).unwrap_or(0);
        let next = (selected + delta).clamp(0, i32::try_from(max).unwrap_or(0));
        let next = usize::try_from(next).unwrap_or_default();

        if next == self.selected {
            return None;
        }

        self.selected = next;
        self.right_scroll = 0;
        self.ensure_selected_visible();

        let project_dir = self.projects.get(self.selected)?.project_dir.clone();
        // Keep showing any cached details for the newly selected project, but always refresh them
        // in the background so revisiting a project in a long-lived overlay session does not leave
        // stale content stuck on screen until the next auto-refresh tick.
        Some(crate::ProjectsOverlayRequest::Details { project_dir })
    }

    fn bump_right_scroll(&mut self, delta: i32) {
        if self.metrics.right_inner_height == 0 {
            return;
        }

        let current = i32::try_from(self.right_scroll).unwrap_or(0);
        let max = i32::try_from(self.max_right_scroll()).unwrap_or(0);
        self.right_scroll = usize::try_from((current + delta).clamp(0, max)).unwrap_or_default();
    }

    fn page_right_scroll(&mut self, delta_pages: i32) {
        let height = usize::from(self.metrics.right_inner_height);
        if height == 0 {
            return;
        }

        let step = i32::try_from((height / 3).max(1)).unwrap_or(1);
        self.bump_right_scroll(step * delta_pages);
    }

    fn max_right_scroll(&self) -> usize {
        let height = usize::from(self.metrics.right_inner_height);
        self.metrics.right_total_lines.saturating_sub(height)
    }

    fn is_ctrl_char(&self, key_event: KeyEvent, expected: char) -> bool {
        key_event.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key_event.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected))
    }
}

fn project_list_description_wrap_width(area_width: u16) -> usize {
    usize::from(area_width.max(1)).saturating_sub(4).max(1)
}

fn project_list_item_height(project: &PotterProjectListEntry, wrap_width: usize) -> usize {
    1 + description_lines(project, wrap_width).len() + 1
}

fn project_list_page_starts(
    projects: &[PotterProjectListEntry],
    wrap_width: usize,
    viewport_height: usize,
) -> Vec<usize> {
    if projects.is_empty() {
        return vec![0];
    }
    if viewport_height == 0 {
        return vec![0];
    }

    let mut page_starts = Vec::new();
    let mut start = 0usize;
    while start < projects.len() {
        page_starts.push(start);

        let mut used = 0usize;
        let mut next = start;
        while next < projects.len() {
            let item_height = project_list_item_height(&projects[next], wrap_width);
            if used + item_height > viewport_height {
                break;
            }
            used += item_height;
            next += 1;
        }

        if next == start {
            next = start.saturating_add(1);
        }
        start = next;
    }

    page_starts
}

fn dots_pager_spans(current_page: usize, page_count: usize, max_dots: usize) -> Vec<Span<'static>> {
    if page_count == 0 || max_dots == 0 {
        return Vec::new();
    }

    let current_page = current_page.min(page_count.saturating_sub(1));
    let window_len = page_count.min(max_dots).max(1);

    let mut window_start = 0usize;
    if page_count > window_len {
        let half = window_len / 2;
        window_start = current_page.saturating_sub(half);
        if window_start + window_len > page_count {
            window_start = page_count.saturating_sub(window_len);
        }
    }

    let mut spans = Vec::with_capacity(window_len);
    for idx in window_start..window_start.saturating_add(window_len) {
        spans.push(if idx == current_page {
            "▪".dim()
        } else {
            "▫".dim()
        });
    }
    spans
}

fn wrap_plain_lines(lines: Vec<Line<'static>>, wrap_width: usize) -> Vec<Line<'static>> {
    if wrap_width == 0 {
        return Vec::new();
    }

    crate::wrapping::word_wrap_lines(lines.iter(), wrap_width)
}

fn user_task_preview_lines(user_message: Option<&str>, wrap_width: usize) -> Vec<Line<'static>> {
    let user_message = user_message.unwrap_or_default();
    if user_message.trim().is_empty() {
        return vec![Line::from(vec![
            Span::from("(no user task recorded)").dim(),
        ])];
    }

    let lines = user_message
        .lines()
        .map(|line| Line::from(line.to_string()))
        .collect();
    let wrapped = wrap_plain_lines(lines, wrap_width);
    let preview_len = USER_TASK_PREVIEW_MAX_LINES.min(wrapped.len());
    let remaining = wrapped.len().saturating_sub(preview_len);
    let mut out: Vec<Line<'static>> = wrapped.into_iter().take(preview_len).collect();
    if remaining > 0 {
        out.push(Line::from(format!("... ({remaining} more lines)")));
    }

    out
}

fn render_project_list_item(
    project: &PotterProjectListEntry,
    wrap_width: usize,
    now: SystemTime,
    is_selected: bool,
) -> Vec<Line<'static>> {
    let (icon, status_style) = status_icon_and_style(&project.status);
    let round_label = if project.rounds == 1 {
        "round"
    } else {
        "rounds"
    };
    let rounds = format!("{icon} {} {round_label}", project.rounds);
    let age = project
        .started_at_unix_secs
        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)))
        .map(|ts| human_time_ago(ts, now))
        .unwrap_or_else(|| "unknown".to_string());

    let status_spans: Vec<Span<'static>> = vec![
        Span::from(rounds).set_style(status_style),
        " · ".dim(),
        Span::from(age).dim(),
    ];

    let mut lines: Vec<Line<'static>> = Vec::new();
    let highlight_bar_style = highlight_bar_style(&project.status);
    let mut first_spans: Vec<Span<'static>> = if is_selected {
        vec![Span::from("┃").set_style(highlight_bar_style), " ".into()]
    } else {
        vec!["  ".into()]
    };
    first_spans.extend(status_spans);
    lines.push(Line::from(first_spans));

    let desc_lines = description_lines(project, wrap_width);
    for desc in desc_lines {
        let mut spans: Vec<Span<'static>> = if is_selected {
            vec![Span::from("┃").set_style(highlight_bar_style), " ".into()]
        } else {
            vec!["  ".into()]
        };
        spans.push("  ".into());
        spans.push(Span::from(desc).set_style(project_description_style(&project.status)));
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines
}

fn description_lines(project: &PotterProjectListEntry, wrap_width: usize) -> Vec<String> {
    let description = project.description.trim();
    if description.is_empty() {
        return vec![String::new()];
    }

    let wrapped = textwrap::wrap(description, WrapOptions::new(wrap_width));
    let truncated = wrapped.len() > 2;
    let mut out: Vec<String> = wrapped
        .into_iter()
        .take(2)
        .map(|line| line.to_string())
        .collect();
    if out.is_empty() {
        return vec![String::new()];
    }

    if truncated && out.len() == 2 {
        out[1] = with_truncation_ellipsis(out[1].as_str(), wrap_width);
    }

    out
}

fn with_truncation_ellipsis(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    if UnicodeWidthStr::width(text) < max_width {
        return format!("{text}…");
    }

    let mut out = String::new();
    let mut used = 0usize;
    let content_width = max_width.saturating_sub(1);
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > content_width {
            break;
        }
        out.push_str(grapheme);
        used += width;
    }
    out.push('…');
    out
}

fn status_icon_and_style(status: &PotterProjectListStatus) -> (char, Style) {
    match status {
        PotterProjectListStatus::Succeeded => ('✓', Style::default().light_green()),
        PotterProjectListStatus::Cancelled => ('■', Style::default().dim()),
        PotterProjectListStatus::BudgetExhausted => ('■', Style::default().red()),
        PotterProjectListStatus::Interrupted => ('■', Style::default().fg(orange_color())),
        PotterProjectListStatus::Failed => ('■', Style::default().red()),
        PotterProjectListStatus::Incomplete => ('■', Style::default().fg(orange_color())),
    }
}

fn highlight_bar_style(status: &PotterProjectListStatus) -> Style {
    match status {
        PotterProjectListStatus::Succeeded => Style::default().light_green().bold(),
        PotterProjectListStatus::Cancelled => Style::default().dim(),
        PotterProjectListStatus::BudgetExhausted | PotterProjectListStatus::Failed => {
            Style::default().red().bold()
        }
        PotterProjectListStatus::Interrupted | PotterProjectListStatus::Incomplete => {
            Style::default().fg(orange_color()).bold()
        }
    }
}

fn project_description_style(status: &PotterProjectListStatus) -> Style {
    if matches!(status, PotterProjectListStatus::Cancelled) {
        Style::default().dim()
    } else {
        Style::default()
    }
}

fn append_round_details(
    out: &mut Vec<Line<'static>>,
    round: &PotterProjectRoundSummary,
    wrap_width: usize,
    now: SystemTime,
) {
    let took = if round.duration_secs > 0 {
        Some(crate::status_indicator_widget::fmt_elapsed_compact(
            round.duration_secs,
        ))
    } else {
        None
    };
    let when = round
        .final_message_unix_secs
        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)))
        .map(|ts| human_time_ago(ts, now));
    let header = match (took, when) {
        (Some(took), Some(when)) => {
            format!("ROUND {} (took {took}) @ {when}", round.round_current)
        }
        (Some(took), None) => format!("ROUND {} (took {took})", round.round_current),
        (None, _) => format!("ROUND {}", round.round_current),
    };
    out.extend(wrap_plain_lines(
        vec![Line::from(vec![Span::from(header).dim()]), Line::from("")],
        wrap_width,
    ));

    let Some(message) = round.final_message.as_deref() else {
        out.extend(wrap_plain_lines(
            vec![Line::from(
                Span::from("(no final agent message recorded)").dim(),
            )],
            wrap_width,
        ));
        return;
    };

    let rendered =
        crate::markdown_render::render_markdown_text_with_width(message, Some(wrap_width));
    out.extend(rendered.lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    fn render_overlay_to_terminal(
        overlay: &mut ProjectsOverlay,
        width: u16,
        height: u16,
        now: SystemTime,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());
                overlay.render(area, frame.buffer_mut(), now);
            })
            .expect("draw");
        terminal
    }

    #[test]
    fn projects_overlay_scrolls_by_item_when_selection_moves_past_viewport() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                    description: "Item 1".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Incomplete,
                },
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/2"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/2/MAIN.md"),
                    description: "Item 2".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Incomplete,
                },
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/3"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/3/MAIN.md"),
                    description: "Item 3".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Incomplete,
                },
            ],
            None,
        );

        let _terminal = render_overlay_to_terminal(&mut overlay, 60, 9, UNIX_EPOCH);

        assert_eq!(overlay.selected, 0);
        assert_eq!(overlay.scroll_top, 0);

        overlay.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(overlay.selected, 1);
        assert_eq!(overlay.scroll_top, 0);

        overlay.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(overlay.selected, 2);
        assert_eq!(overlay.scroll_top, 2);
    }

    #[test]
    fn projects_overlay_renders_list_and_details() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
                progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                description: "Add projects overlay".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 4,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        overlay.on_project_details(PotterProjectDetails {
            project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
            progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
            git_branch: Some("main".to_string()),
            user_message: Some(String::from(
                "Task line 1\nTask line 2\nTask line 3\nTask line 4\nTask line 5\nTask line 6\nTask line 7\nTask line 8\nTask line 9\nTask line 10\nTask line 11\nTask line 12\nTask line 13\nTask line 14\nTask line 15",
            )),
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 4,
                duration_secs: 1843,
                final_message_unix_secs: Some(1),
                final_message: Some(String::from("**Done**")),
            }],
            error: None,
        });

        let terminal =
            render_overlay_to_terminal(&mut overlay, 80, 18, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_renders_maximized_details_full_width() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
                progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                description: "Maximized projects overlay".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 4,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        overlay.on_project_details(PotterProjectDetails {
            project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
            progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
            git_branch: Some("main".to_string()),
            user_message: Some(String::from("Task line")),
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 4,
                duration_secs: 1843,
                final_message_unix_secs: Some(1),
                final_message: Some(String::from("Done")),
            }],
            error: None,
        });

        overlay.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        let terminal =
            render_overlay_to_terminal(&mut overlay, 80, 18, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_limits_details_width_when_not_maximized() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
                progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                description: "Details width cap".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        let long_line = (0..20)
            .map(|idx| format!("word{idx:02}"))
            .collect::<Vec<_>>()
            .join(" ");
        overlay.on_project_details(PotterProjectDetails {
            project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
            progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
            git_branch: Some("main".to_string()),
            user_message: Some(long_line.clone()),
            rounds: Vec::new(),
            error: None,
        });

        let lines = overlay.build_right_lines(Rect::new(0, 0, 200, 18), UNIX_EPOCH);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        insta::assert_snapshot!(rendered);

        overlay.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let maximized_lines = overlay.build_right_lines(Rect::new(0, 0, 200, 18), UNIX_EPOCH);
        let maximized_rendered: Vec<String> = maximized_lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(
            maximized_rendered
                .iter()
                .filter(|line| line.as_str() == long_line)
                .count(),
            1,
            "expected maximized details view to keep the long task line unwrapped: {maximized_rendered:?}"
        );
    }

    #[test]
    fn projects_overlay_inserts_blank_line_between_task_preview_and_first_round() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
                progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                description: "Task preview spacing".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        overlay.on_project_details(PotterProjectDetails {
            project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
            progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
            git_branch: Some("main".to_string()),
            user_message: Some("Task line".to_string()),
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some("Done".to_string()),
            }],
            error: None,
        });

        let lines = overlay.build_right_lines(
            Rect::new(0, 0, 80, 18),
            UNIX_EPOCH + Duration::from_secs(120),
        );
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        let task_idx = rendered
            .iter()
            .position(|line| line == "Task line")
            .unwrap_or_else(|| panic!("missing task preview line: {rendered:?}"));
        let round_idx = rendered
            .iter()
            .position(|line| line == "ROUND 1")
            .unwrap_or_else(|| panic!("missing round heading line: {rendered:?}"));

        assert_eq!(round_idx, task_idx + 2);
        assert_eq!(rendered[task_idx + 1], "");
    }

    #[test]
    fn projects_overlay_maximized_still_switches_selection() {
        let first_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let second_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/2");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![
                PotterProjectListEntry {
                    project_dir: first_project_dir,
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                    description: "First project".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: second_project_dir.clone(),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/2/MAIN.md"),
                    description: "Second project".to_string(),
                    started_at_unix_secs: Some(2),
                    rounds: 2,
                    status: PotterProjectListStatus::Interrupted,
                },
            ],
            None,
        );

        overlay.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        match overlay.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)) {
            Some(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, second_project_dir);
            }
            other => panic!("expected details request after moving selection, got {other:?}"),
        }
    }

    #[test]
    fn switching_back_to_cached_project_requests_fresh_details() {
        let first_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let second_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/2");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![
                PotterProjectListEntry {
                    project_dir: first_project_dir.clone(),
                    progress_file: first_project_dir.join("MAIN.md"),
                    description: "First project".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: second_project_dir.clone(),
                    progress_file: second_project_dir.join("MAIN.md"),
                    description: "Second project".to_string(),
                    started_at_unix_secs: Some(2),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
            ],
            None,
        );
        overlay.on_project_details(PotterProjectDetails {
            project_dir: first_project_dir.clone(),
            progress_file: first_project_dir.join("MAIN.md"),
            git_branch: None,
            user_message: None,
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some("old first details".to_string()),
            }],
            error: None,
        });
        overlay.on_project_details(PotterProjectDetails {
            project_dir: second_project_dir.clone(),
            progress_file: second_project_dir.join("MAIN.md"),
            git_branch: None,
            user_message: None,
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(2),
                final_message: Some("second details".to_string()),
            }],
            error: None,
        });

        match overlay.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)) {
            Some(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, second_project_dir);
            }
            other => panic!("expected refresh request for second project, got {other:?}"),
        }

        match overlay.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)) {
            Some(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, first_project_dir);
            }
            other => panic!("expected refresh request for cached project, got {other:?}"),
        }

        let rendered = overlay
            .build_right_lines(
                Rect::new(0, 0, 80, 18),
                UNIX_EPOCH + Duration::from_secs(120),
            )
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("old first details")),
            "expected cached details to remain visible while refresh is pending: {rendered:?}"
        );
    }

    #[test]
    fn projects_overlay_renders_loading_projects() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        let terminal =
            render_overlay_to_terminal(&mut overlay, 80, 8, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_renders_projects_list_error() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(Vec::new(), Some("permission denied".to_string()));

        let terminal =
            render_overlay_to_terminal(&mut overlay, 80, 8, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_renders_empty_projects_list() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(Vec::new(), None);

        let terminal =
            render_overlay_to_terminal(&mut overlay, 80, 8, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_renders_loading_project_details() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
                progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                description: "Loading details state".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        let terminal =
            render_overlay_to_terminal(&mut overlay, 80, 8, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_renders_project_details_error() {
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let progress_file = project_dir.join("MAIN.md");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file: progress_file.clone(),
                description: "Details error state".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );
        overlay.on_project_details(PotterProjectDetails {
            project_dir,
            progress_file,
            git_branch: None,
            user_message: None,
            rounds: Vec::new(),
            error: Some("malformed potter-rollout.jsonl".to_string()),
        });

        let terminal =
            render_overlay_to_terminal(&mut overlay, 80, 8, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_renders_left_list_pager() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                    description: "Paged project 1".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/2"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/2/MAIN.md"),
                    description: "Paged project 2".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/3"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/3/MAIN.md"),
                    description: "Paged project 3".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/4"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/4/MAIN.md"),
                    description: "Paged project 4".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/5"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/5/MAIN.md"),
                    description: "Paged project 5".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/6"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/6/MAIN.md"),
                    description: "Paged project 6".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/7"),
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/7/MAIN.md"),
                    description: "Paged project 7".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
            ],
            None,
        );

        let terminal =
            render_overlay_to_terminal(&mut overlay, 60, 12, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_renders_details_pager() {
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let progress_file = project_dir.join("MAIN.md");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file: progress_file.clone(),
                description: "Details pager state".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 4,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );
        overlay.on_project_details(PotterProjectDetails {
            project_dir,
            progress_file,
            git_branch: Some("main".to_string()),
            user_message: Some("Task line".to_string()),
            rounds: vec![
                PotterProjectRoundSummary {
                    round_current: 1,
                    round_total: 4,
                    duration_secs: 0,
                    final_message_unix_secs: Some(1),
                    final_message: Some(String::from("Done")),
                },
                PotterProjectRoundSummary {
                    round_current: 2,
                    round_total: 4,
                    duration_secs: 0,
                    final_message_unix_secs: Some(1),
                    final_message: Some(String::from("Done")),
                },
                PotterProjectRoundSummary {
                    round_current: 3,
                    round_total: 4,
                    duration_secs: 0,
                    final_message_unix_secs: Some(1),
                    final_message: Some(String::from("Done")),
                },
                PotterProjectRoundSummary {
                    round_current: 4,
                    round_total: 4,
                    duration_secs: 0,
                    final_message_unix_secs: Some(1),
                    final_message: Some(String::from("Done")),
                },
            ],
            error: None,
        });

        let terminal =
            render_overlay_to_terminal(&mut overlay, 80, 12, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn projects_overlay_missing_final_message_placeholder_is_dim_only() {
        let mut out = Vec::new();
        append_round_details(
            &mut out,
            &PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: None,
            },
            80,
            UNIX_EPOCH + Duration::from_secs(120),
        );

        let placeholder = out
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "(no final agent message recorded)")
            .unwrap_or_else(|| panic!("missing placeholder span: {out:?}"));

        assert!(
            placeholder.style.add_modifier.contains(Modifier::DIM),
            "placeholder should be dim: {placeholder:?}"
        );
        assert_eq!(
            placeholder.style.add_modifier,
            Modifier::DIM,
            "placeholder should not include any modifiers besides DIM: {placeholder:?}"
        );
    }

    #[test]
    fn projects_overlay_narrow_width_keeps_project_row_compact() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/123456"),
                progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/123456/MAIN.md"),
                description: "Add projects overlay compact narrow layout".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 12,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        overlay.on_project_details(PotterProjectDetails {
            project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/123456"),
            progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/123456/MAIN.md"),
            git_branch: Some("main".to_string()),
            user_message: Some("Task line".to_string()),
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 12,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some(String::from("Done")),
            }],
            error: None,
        });

        let terminal =
            render_overlay_to_terminal(&mut overlay, 40, 12, UNIX_EPOCH + Duration::from_secs(120));
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn incomplete_projects_render_with_orange_status_style() {
        let (icon, style) = status_icon_and_style(&PotterProjectListStatus::Incomplete);
        assert_eq!(icon, '■');
        assert_eq!(style, Style::default().fg(orange_color()));
    }

    #[test]
    fn terminal_project_statuses_use_requested_colors() {
        assert_eq!(
            status_icon_and_style(&PotterProjectListStatus::Cancelled),
            ('■', Style::default().dim())
        );
        assert_eq!(
            highlight_bar_style(&PotterProjectListStatus::Cancelled),
            Style::default().dim()
        );
        assert_eq!(
            status_icon_and_style(&PotterProjectListStatus::BudgetExhausted),
            ('■', Style::default().red())
        );
        assert_eq!(
            highlight_bar_style(&PotterProjectListStatus::BudgetExhausted),
            Style::default().red().bold()
        );
        assert_eq!(
            status_icon_and_style(&PotterProjectListStatus::Interrupted),
            ('■', Style::default().fg(orange_color()))
        );
        assert_eq!(
            highlight_bar_style(&PotterProjectListStatus::Interrupted),
            Style::default().fg(orange_color()).bold()
        );
    }

    #[test]
    fn cancelled_projects_dim_the_summary_text() {
        let project = PotterProjectListEntry {
            project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
            progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
            description: "Cancelled project".to_string(),
            started_at_unix_secs: Some(1),
            rounds: 1,
            status: PotterProjectListStatus::Cancelled,
        };

        let lines =
            render_project_list_item(&project, 32, UNIX_EPOCH + Duration::from_secs(120), true);
        let summary = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "Cancelled project")
            .unwrap_or_else(|| panic!("missing summary span: {lines:?}"));

        assert!(
            summary.style.add_modifier.contains(Modifier::DIM),
            "cancelled project summary should be dim: {summary:?}"
        );
    }

    #[test]
    fn dots_pager_spans_use_dim_squares() {
        assert_eq!(
            dots_pager_spans(0, 3, 3),
            vec!["▪".dim(), "▫".dim(), "▫".dim()]
        );
        assert_eq!(dots_pager_spans(1, 2, 2), vec!["▫".dim(), "▪".dim()]);
    }

    #[test]
    fn description_lines_keep_ellipsis_when_second_line_fills_width() {
        let project = PotterProjectListEntry {
            project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
            progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
            description: "abcde fghij klmno".to_string(),
            started_at_unix_secs: Some(1),
            rounds: 1,
            status: PotterProjectListStatus::Succeeded,
        };

        assert_eq!(
            description_lines(&project, 5),
            vec!["abcde".to_string(), "fghi…".to_string()]
        );
    }

    #[test]
    fn single_round_projects_render_singular_round_label() {
        let project = PotterProjectListEntry {
            project_dir: PathBuf::from(".codexpotter/projects/2026/04/16/1"),
            progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
            description: "Singular round label".to_string(),
            started_at_unix_secs: Some(1),
            rounds: 1,
            status: PotterProjectListStatus::Succeeded,
        };

        let lines =
            render_project_list_item(&project, 32, UNIX_EPOCH + Duration::from_secs(120), false);
        let first_line = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(first_line.trim_start(), "✓ 1 round · 1 minute ago");
    }

    #[test]
    fn refresh_preserves_cached_details_until_new_response_arrives() {
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let progress_file = project_dir.join("MAIN.md");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file: progress_file.clone(),
                description: "Refresh projects overlay".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );
        overlay.on_project_details(PotterProjectDetails {
            project_dir: project_dir.clone(),
            progress_file: progress_file.clone(),
            git_branch: None,
            user_message: None,
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some("old details".to_string()),
            }],
            error: None,
        });

        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file,
                description: "Refresh projects overlay".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        let cached = overlay
            .details_by_project
            .get(&project_dir)
            .expect("expected cached details to remain");
        assert_eq!(
            cached
                .rounds
                .first()
                .and_then(|round| round.final_message.as_deref()),
            Some("old details")
        );
    }

    #[test]
    fn refreshed_list_ignores_stale_details_reinserted_before_list_response() {
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let progress_file = project_dir.join("MAIN.md");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file: progress_file.clone(),
                description: "Refresh projects overlay".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );
        overlay.on_project_details(PotterProjectDetails {
            project_dir: project_dir.clone(),
            progress_file: progress_file.clone(),
            git_branch: None,
            user_message: None,
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some("old details".to_string()),
            }],
            error: None,
        });

        overlay.open_or_refresh();

        // Simulate a stale in-flight response from the previous refresh arriving before the new
        // projects list response is processed.
        overlay.on_project_details(PotterProjectDetails {
            project_dir: project_dir.clone(),
            progress_file: progress_file.clone(),
            git_branch: None,
            user_message: None,
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some("stale details".to_string()),
            }],
            error: None,
        });

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file,
                description: "Refresh projects overlay".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        assert_eq!(
            overlay
                .details_by_project
                .get(&project_dir)
                .expect("expected cached details to remain")
                .rounds
                .first()
                .and_then(|round| round.final_message.as_deref()),
            Some("old details"),
            "expected stale details response to be ignored while list refresh is in flight"
        );
    }

    #[test]
    fn refresh_preserves_right_pane_scroll_for_selected_project() {
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let progress_file = project_dir.join("MAIN.md");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file: progress_file.clone(),
                description: "Scroll preservation".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );
        overlay.on_project_details(PotterProjectDetails {
            project_dir: project_dir.clone(),
            progress_file: progress_file.clone(),
            git_branch: None,
            user_message: None,
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some("details".to_string()),
            }],
            error: None,
        });

        overlay.right_scroll = 7;
        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir,
                progress_file,
                description: "Scroll preservation".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );

        assert_eq!(overlay.right_scroll, 7);
    }

    #[test]
    fn refresh_preserves_list_scroll_top_anchor_when_possible() {
        let first_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let second_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/2");
        let third_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/3");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![
                PotterProjectListEntry {
                    project_dir: first_project_dir.clone(),
                    progress_file: first_project_dir.join("MAIN.md"),
                    description: "First".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: second_project_dir.clone(),
                    progress_file: second_project_dir.join("MAIN.md"),
                    description: "Second".to_string(),
                    started_at_unix_secs: Some(2),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: third_project_dir.clone(),
                    progress_file: third_project_dir.join("MAIN.md"),
                    description: "Third".to_string(),
                    started_at_unix_secs: Some(3),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
            ],
            None,
        );

        overlay.selected = 2;
        overlay.scroll_top = 1;
        assert_eq!(
            overlay.selected_project_dir(),
            Some(third_project_dir.clone())
        );

        overlay.open_or_refresh();
        overlay.on_projects_list(
            vec![
                PotterProjectListEntry {
                    project_dir: second_project_dir.clone(),
                    progress_file: second_project_dir.join("MAIN.md"),
                    description: "Second".to_string(),
                    started_at_unix_secs: Some(2),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: third_project_dir,
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/3/MAIN.md"),
                    description: "Third".to_string(),
                    started_at_unix_secs: Some(3),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: first_project_dir,
                    progress_file: PathBuf::from(".codexpotter/projects/2026/04/16/1/MAIN.md"),
                    description: "First".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
            ],
            None,
        );

        assert_eq!(overlay.scroll_top, 0);
        assert_eq!(overlay.selected, 1);
        assert_eq!(
            overlay.selected_project_dir(),
            Some(PathBuf::from(".codexpotter/projects/2026/04/16/3"))
        );
    }

    #[test]
    fn refresh_preserves_selected_project_context() {
        let first_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let second_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/2");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![
                PotterProjectListEntry {
                    project_dir: first_project_dir.clone(),
                    progress_file: first_project_dir.join("MAIN.md"),
                    description: "First project".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: second_project_dir.clone(),
                    progress_file: second_project_dir.join("MAIN.md"),
                    description: "Second project".to_string(),
                    started_at_unix_secs: Some(2),
                    rounds: 2,
                    status: PotterProjectListStatus::Interrupted,
                },
            ],
            None,
        );

        match overlay.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)) {
            Some(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, second_project_dir);
            }
            other => panic!("expected details request after moving selection, got {other:?}"),
        }

        assert_eq!(
            overlay.selected_project_dir(),
            Some(second_project_dir.clone())
        );

        assert!(matches!(
            overlay.open_or_refresh(),
            crate::ProjectsOverlayRequest::List
        ));

        match overlay.on_projects_list(
            vec![
                PotterProjectListEntry {
                    project_dir: second_project_dir.clone(),
                    progress_file: second_project_dir.join("MAIN.md"),
                    description: "Second project".to_string(),
                    started_at_unix_secs: Some(3),
                    rounds: 3,
                    status: PotterProjectListStatus::Succeeded,
                },
                PotterProjectListEntry {
                    project_dir: first_project_dir.clone(),
                    progress_file: first_project_dir.join("MAIN.md"),
                    description: "First project".to_string(),
                    started_at_unix_secs: Some(1),
                    rounds: 1,
                    status: PotterProjectListStatus::Succeeded,
                },
            ],
            None,
        ) {
            Some(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, second_project_dir);
            }
            other => panic!("expected refresh to request restored project details, got {other:?}"),
        }

        assert_eq!(overlay.selected_project_dir(), Some(second_project_dir));
    }

    #[test]
    fn shift_arrow_keys_scroll_details_three_lines_at_a_time() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.metrics.right_inner_height = 10;
        overlay.metrics.right_total_lines = 100;
        overlay.right_scroll = 0;

        assert!(
            overlay
                .handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT))
                .is_none()
        );
        assert_eq!(overlay.right_scroll, 3);

        assert!(
            overlay
                .handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT))
                .is_none()
        );
        assert_eq!(overlay.right_scroll, 0);
    }

    #[test]
    fn right_details_ctrl_u_d_scroll_one_third_screen() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.metrics.right_inner_height = 12;
        overlay.metrics.right_total_lines = 100;
        overlay.right_scroll = 0;

        assert!(
            overlay
                .handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(overlay.right_scroll, 4);

        assert!(
            overlay
                .handle_key_event(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(overlay.right_scroll, 0);
    }

    #[test]
    fn detail_pager_highlights_last_dot_at_max_scroll() {
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();
        overlay.metrics.right_inner_height = 10;
        overlay.metrics.right_total_lines = 21;
        overlay.right_scroll = overlay.max_right_scroll();

        let line = overlay.detail_pager_line(5);
        assert_eq!(line.spans, vec!["▫".dim(), "▫".dim(), "▪".dim()]);
    }

    #[test]
    fn narrow_width_right_paging_uses_wrapped_rendered_line_count() {
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/123456");
        let progress_file = project_dir.join("MAIN.md");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file: progress_file.clone(),
                description: "Narrow width scrolling".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );
        overlay.on_project_details(PotterProjectDetails {
            project_dir,
            progress_file,
            git_branch: None,
            user_message: None,
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some("Done".to_string()),
            }],
            error: None,
        });

        let mut terminal = Terminal::new(TestBackend::new(24, 8)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());
                overlay.render(
                    area,
                    frame.buffer_mut(),
                    UNIX_EPOCH + Duration::from_secs(120),
                );
            })
            .expect("draw");

        assert_eq!(overlay.right_scroll, 0);

        assert!(
            overlay
                .handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(
            overlay.right_scroll > 0,
            "expected wrapped progress path to make the right pane pageable"
        );
    }

    #[test]
    fn right_details_keep_markdown_code_block_lines_intact() {
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/123456");
        let progress_file = project_dir.join("MAIN.md");
        let mut overlay = ProjectsOverlay::default();
        overlay.open_or_refresh();

        overlay.on_projects_list(
            vec![PotterProjectListEntry {
                project_dir: project_dir.clone(),
                progress_file: progress_file.clone(),
                description: "Code block rendering".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            }],
            None,
        );
        overlay.on_project_details(PotterProjectDetails {
            project_dir,
            progress_file,
            git_branch: None,
            user_message: None,
            rounds: vec![PotterProjectRoundSummary {
                round_current: 1,
                round_total: 1,
                duration_secs: 0,
                final_message_unix_secs: Some(1),
                final_message: Some("```text\n12345678901234567890\n```".to_string()),
            }],
            error: None,
        });

        let lines = overlay.build_right_lines(
            Rect::new(0, 0, 12, 12),
            UNIX_EPOCH + Duration::from_secs(120),
        );
        let line_texts: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert!(
            line_texts.iter().any(|line| line == "12345678901234567890"),
            "expected code block line to stay intact after right-pane layout"
        );
    }
}
