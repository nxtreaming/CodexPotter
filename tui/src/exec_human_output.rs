//! Headless human-readable transcript rendering for `codex-potter exec`.
//!
//! # Divergence from upstream Codex / interactive TUI
//!
//! This renderer is intentionally append-only:
//! - it preserves the same broad visibility policy as codex-potter's interactive verbosity modes
//! - it does **not** use interactive folding/coalescing, because exec cannot rewrite prior output
//! - it emits a metadata header block (workdir/model/reasoning effort/project file) at the start of
//!   the transcript instead of the interactive `Project created:` hint
//! - it emits dim timestamped status hints when reasoning changes the live shimmer header, because
//!   exec has no mutable status bar; these hints omit the interactive round-prefix shimmer chrome
//!   and only surface `context left` when usage first crosses each 10% threshold
//! - it does not emit the interactive `─ Round finished in … ─` separator line
//! - it renders CodexPotter round / summary markers as plain text blocks instead of interactive
//!   transcript chrome

use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::models::MessagePhase;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::ServiceTier;
use codex_protocol::protocol::TokenUsage;
use crossterm::queue;
use crossterm::style::Color as CrosstermColor;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::Verbosity;
use crate::diff_render::create_compact_diff_summary;
use crate::diff_render::display_path_for;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::new_active_exec_command;
use crate::history_cell::HistoryCell;
use crate::history_cell::new_deprecation_notice;
use crate::history_cell::new_elicitation_request_event;
use crate::history_cell::new_error_event;
use crate::history_cell::new_guardian_assessment_event;
use crate::history_cell::new_patch_apply_failure;
use crate::history_cell::new_plan_update;
use crate::history_cell::new_request_permissions_event;
use crate::history_cell::new_request_user_input_event;
use crate::history_cell::new_warning_event;
use crate::history_cell_potter::PotterStreamRecoveryRetryCell;
use crate::history_cell_potter::PotterStreamRecoveryUnrecoverableCell;
use crate::markdown;
use crate::multi_agents;
use crate::potter_project_summary::build_potter_project_summary_detail_lines;
use crate::reasoning_status::ReasoningStatusTracker;
use crate::status_indicator_widget::fmt_elapsed_compact;
use crate::streaming::controller::PlanStreamController;
use crate::streaming::controller::StreamController;
use crate::ui_colors::secondary_color;

const DEFAULT_RENDER_WIDTH: u16 = 120;
const EXEC_REASONING_CONTEXT_STEP: i64 = 10;
const EXEC_REASONING_CONTEXT_MAX_LEVEL: i64 = 100;

#[derive(Debug)]
enum PendingProjectSummaryOutcome {
    Succeeded,
    BudgetExhausted,
}

#[derive(Debug)]
struct PendingProjectSummary {
    outcome: PendingProjectSummaryOutcome,
    rounds: u32,
    duration: Duration,
    user_prompt_file: PathBuf,
    git_commit_start: String,
    git_commit_end: String,
}

#[derive(Debug, Clone)]
struct PendingExecMetaInfo {
    workdir: PathBuf,
    user_prompt_file: PathBuf,
}

#[derive(Debug, Clone)]
struct SessionMetaInfo {
    model: String,
    reasoning_effort: Option<ReasoningEffortConfig>,
    service_tier: Option<ServiceTier>,
}

/// Append-only human-readable renderer used by `codex-potter exec` without `--json`.
pub struct ExecHumanRenderer {
    cwd: PathBuf,
    width: Option<u16>,
    color_enabled: bool,
    verbosity: Verbosity,
    stream: StreamController,
    saw_agent_delta: bool,
    plan_stream: Option<PlanStreamController>,
    pending_minimal_agent_message_lines: Option<Vec<Line<'static>>>,
    pending_minimal_agent_message_visible: bool,
    turn_has_non_commentary_agent_message: bool,
    pending_project_summary: Option<PendingProjectSummary>,
    pending_simple_final_message_separator: bool,
    separator_baseline: Option<Instant>,
    status_started_at: Option<Instant>,
    context_usage: TokenUsage,
    model_context_window: Option<i64>,
    context_output_level: i64,
    reasoning_status: ReasoningStatusTracker,
    last_status_hint_header: Option<String>,
    pending_exec_meta: Option<PendingExecMetaInfo>,
    session_meta: Option<SessionMetaInfo>,
    pending_round_marker: Option<(u32, u32)>,
    pending_initial_blocks: Vec<String>,
    emitted_exec_meta: bool,
}

impl ExecHumanRenderer {
    /// Create a renderer.
    pub fn new(verbosity: Verbosity, width: Option<u16>, color_enabled: bool) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            cwd: cwd.clone(),
            width,
            color_enabled,
            verbosity,
            stream: StreamController::new(width.map(usize::from), &cwd),
            saw_agent_delta: false,
            plan_stream: None,
            pending_minimal_agent_message_lines: None,
            pending_minimal_agent_message_visible: false,
            turn_has_non_commentary_agent_message: false,
            pending_project_summary: None,
            pending_simple_final_message_separator: false,
            separator_baseline: None,
            status_started_at: None,
            context_usage: TokenUsage::default(),
            model_context_window: None,
            context_output_level: EXEC_REASONING_CONTEXT_MAX_LEVEL,
            reasoning_status: ReasoningStatusTracker::new(),
            last_status_hint_header: None,
            pending_exec_meta: None,
            session_meta: None,
            pending_round_marker: None,
            pending_initial_blocks: Vec::new(),
            emitted_exec_meta: false,
        }
    }

    /// Provide the project start time for API parity with interactive renderers.
    pub fn set_project_started_at(&mut self, _started_at: Instant) {}

    /// Render a fatal error block.
    pub fn render_error_block(&self, message: String) -> io::Result<String> {
        self.render_cell_block(Box::new(new_error_event(message)))
    }

    /// Flush buffered transcript state before an abnormal exit.
    pub fn flush_for_exit(&mut self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        out.extend(self.flush_pending_exec_meta(true)?);
        out.extend(self.flush_agent_output(false)?);
        out.extend(self.flush_plan_stream()?);
        Ok(out)
    }

    /// Return whether minimal-mode output still has a hidden completed agent message waiting for
    /// an idle flush.
    pub fn needs_idle_agent_message_flush(&self) -> bool {
        self.verbosity == Verbosity::Minimal
            && self.pending_minimal_agent_message_lines.is_some()
            && !self.pending_minimal_agent_message_visible
    }

    /// Render the hidden minimal-mode agent message as a dim block so append-only exec output
    /// keeps pace with the live transcript even without a transient preview area.
    pub fn flush_idle_agent_message(&mut self) -> io::Result<Option<String>> {
        if !self.needs_idle_agent_message_flush() {
            return Ok(None);
        }

        let Some(lines) = self.pending_minimal_agent_message_lines.clone() else {
            return Ok(None);
        };
        self.pending_minimal_agent_message_visible = true;
        Ok(Some(self.render_agent_message_block(lines, true)?))
    }

    /// Render one protocol event into zero or more append-only output blocks.
    pub fn handle_event(
        &mut self,
        msg: &codex_protocol::protocol::EventMsg,
    ) -> io::Result<Vec<String>> {
        use codex_protocol::protocol::EventMsg;

        let mut out = Vec::new();
        match msg {
            EventMsg::SessionConfigured(cfg) => {
                self.cwd = cfg.cwd.clone();
                self.stream = StreamController::new(self.width.map(usize::from), &self.cwd);
                self.session_meta = Some(SessionMetaInfo {
                    model: cfg.model.clone(),
                    reasoning_effort: cfg.reasoning_effort,
                    service_tier: cfg.service_tier,
                });
                out.extend(self.flush_pending_exec_meta(false)?);
                self.maybe_emit_round_marker(&mut out)?;
            }
            EventMsg::TurnStarted(ev) => {
                self.pending_simple_final_message_separator = false;
                self.separator_baseline = Some(Instant::now());
                self.status_started_at = Some(Instant::now());
                self.model_context_window = ev.model_context_window;
                self.context_output_level = EXEC_REASONING_CONTEXT_MAX_LEVEL;
                self.reasoning_status.reset();
                self.last_status_hint_header = None;
                self.turn_has_non_commentary_agent_message = false;
            }
            EventMsg::PotterProjectStarted {
                working_dir,
                user_prompt_file,
                ..
            } => {
                self.pending_exec_meta = Some(PendingExecMetaInfo {
                    workdir: working_dir.clone(),
                    user_prompt_file: user_prompt_file.clone(),
                });
                out.extend(self.flush_pending_exec_meta(false)?);
            }
            EventMsg::PotterRoundStarted { current, total } => {
                self.pending_simple_final_message_separator = false;
                self.separator_baseline = Some(Instant::now());
                self.pending_round_marker = Some((*current, *total));
                self.maybe_emit_round_marker(&mut out)?;
            }
            EventMsg::TokenCount(ev) => {
                if let Some(info) = &ev.info {
                    self.context_usage = info.last_token_usage.clone();
                    self.model_context_window =
                        info.model_context_window.or(self.model_context_window);
                }
            }
            EventMsg::PotterProjectSucceeded {
                rounds,
                duration,
                user_prompt_file,
                git_commit_start,
                git_commit_end,
            } => {
                self.pending_project_summary = Some(PendingProjectSummary {
                    outcome: PendingProjectSummaryOutcome::Succeeded,
                    rounds: *rounds,
                    duration: *duration,
                    user_prompt_file: user_prompt_file.clone(),
                    git_commit_start: git_commit_start.clone(),
                    git_commit_end: git_commit_end.clone(),
                });
            }
            EventMsg::PotterProjectBudgetExhausted {
                rounds,
                duration,
                user_prompt_file,
                git_commit_start,
                git_commit_end,
            } => {
                self.pending_project_summary = Some(PendingProjectSummary {
                    outcome: PendingProjectSummaryOutcome::BudgetExhausted,
                    rounds: *rounds,
                    duration: *duration,
                    user_prompt_file: user_prompt_file.clone(),
                    git_commit_start: git_commit_start.clone(),
                    git_commit_end: git_commit_end.clone(),
                });
            }
            EventMsg::PotterRoundFinished { .. } => {
                out.extend(self.flush_agent_output(false)?);
                out.extend(self.flush_plan_stream()?);
                if let Some(summary) = self.pending_project_summary.take() {
                    out.push(self.render_project_summary(summary)?);
                }
                self.session_meta = None;
                self.pending_round_marker = None;
            }
            EventMsg::TurnComplete(ev) => {
                if let Some(message) = ev.last_agent_message.as_deref()
                    && !message.is_empty()
                    && !self.turn_has_non_commentary_agent_message
                {
                    self.pending_minimal_agent_message_lines = None;
                    self.pending_minimal_agent_message_visible = false;
                    if self.saw_agent_delta {
                        self.discard_streamed_agent_message_lines();
                    }
                    let lines = self.build_agent_message_lines(message);
                    if !lines.is_empty() {
                        out.push(self.render_agent_message_block(lines, false)?);
                    }
                    self.turn_has_non_commentary_agent_message = true;
                } else {
                    out.extend(self.flush_agent_output(true)?);
                }
                out.extend(self.flush_plan_stream()?);
                if let Some(summary) = self.pending_project_summary.take() {
                    out.push(self.render_project_summary(summary)?);
                }
            }
            EventMsg::TurnAborted(ev) => {
                out.extend(self.drop_incomplete_minimal_agent_stream_or_flush()?);
                out.extend(self.flush_plan_stream()?);
                if ev.reason == codex_protocol::protocol::TurnAbortReason::Interrupted {
                    out.push(
                        self.render_cell_block(Box::new(new_error_event(
                            "Conversation interrupted - tell the model what to do differently."
                                .to_string(),
                        )))?,
                    );
                }
            }
            EventMsg::AgentMessageDelta(ev) => {
                if self.verbosity == Verbosity::Minimal && !self.saw_agent_delta {
                    out.extend(self.flush_agent_output(false)?);
                }
                self.saw_agent_delta |= !ev.delta.is_empty();
                let _ = self.stream.push(&ev.delta);
            }
            EventMsg::AgentReasoningDelta(ev) => {
                if let Some(block) = self.maybe_render_reasoning_status_hint(&ev.delta)? {
                    out.push(block);
                }
            }
            EventMsg::AgentReasoningRawContentDelta(ev) => {
                if let Some(block) = self.maybe_render_reasoning_status_hint(&ev.delta)? {
                    out.push(block);
                }
            }
            EventMsg::AgentReasoningSectionBreak(_) => {
                self.reasoning_status.on_section_break();
            }
            EventMsg::AgentReasoning(ev) => {
                if let Some(block) = self.maybe_render_reasoning_status_hint(&ev.text)? {
                    out.push(block);
                }
                self.reasoning_status.on_final();
            }
            EventMsg::AgentReasoningRawContent(ev) => {
                if let Some(block) = self.maybe_render_reasoning_status_hint(&ev.text)? {
                    out.push(block);
                }
                self.reasoning_status.on_final();
            }
            EventMsg::AgentMessage(ev) => {
                if ev.phase != Some(MessagePhase::Commentary) {
                    self.turn_has_non_commentary_agent_message = true;
                }
                if self.verbosity == Verbosity::Minimal {
                    if ev.phase == Some(MessagePhase::Commentary) {
                        if self.saw_agent_delta {
                            self.discard_streamed_agent_message_lines();
                        }
                        if let Some(header) =
                            crate::commentary_status::status_header_from_commentary(&ev.message)
                            && let Some(block) =
                                self.maybe_render_reasoning_status_hint_block(header)?
                        {
                            out.push(block);
                        }
                        return Ok(out);
                    }

                    let lines = self.take_agent_message_lines(&ev.message);
                    self.store_pending_minimal_agent_message(lines, &mut out)?;
                } else {
                    let lines = self.take_agent_message_lines(&ev.message);
                    if !lines.is_empty() {
                        self.push_simple_final_message_separator(&mut out)?;
                        out.push(self.render_lines(lines)?);
                    }
                }
            }
            EventMsg::PlanDelta(ev) => {
                if self.verbosity == Verbosity::Minimal {
                    return Ok(out);
                }
                if self.plan_stream.is_none() {
                    self.plan_stream = Some(PlanStreamController::new(
                        self.width.map(usize::from),
                        &self.cwd,
                    ));
                }
                if let Some(controller) = self.plan_stream.as_mut() {
                    let _ = controller.push(&ev.delta);
                }
            }
            EventMsg::PlanUpdate(ev) => {
                out.extend(self.flush_agent_output(false)?);
                out.extend(self.flush_plan_stream()?);
                if self.verbosity != Verbosity::Minimal {
                    out.push(self.render_cell_block(Box::new(new_plan_update(ev.clone())))?);
                }
            }
            EventMsg::ContextCompacted(_) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_lines(vec![Line::from("Context compacted")])?);
            }
            EventMsg::Warning(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(new_warning_event(ev.message.clone())))?);
            }
            EventMsg::Error(ev) => {
                out.extend(self.drop_incomplete_minimal_agent_stream_or_flush()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(new_error_event(ev.message.clone())))?);
            }
            EventMsg::DeprecationNotice(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(new_deprecation_notice(
                    ev.summary.clone(),
                    ev.details.clone(),
                )))?);
            }
            EventMsg::RequestPermissions(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(
                    self.render_cell_block(Box::new(new_request_permissions_event(ev.clone())))?,
                );
                self.mark_work_activity();
            }
            EventMsg::RequestUserInput(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(
                    self.render_cell_block(Box::new(new_request_user_input_event(ev.clone())))?,
                );
            }
            EventMsg::ElicitationRequest(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(
                    self.render_cell_block(Box::new(new_elicitation_request_event(ev.clone())))?,
                );
            }
            EventMsg::GuardianAssessment(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(
                    self.render_cell_block(Box::new(new_guardian_assessment_event(ev.clone())))?,
                );
                self.mark_work_activity();
            }
            EventMsg::WebSearchEnd(ev) => {
                if self.verbosity != Verbosity::Minimal {
                    out.extend(self.flush_agent_output(false)?);
                    out.extend(self.flush_plan_stream()?);
                    let block = vec![
                        Line::from(vec!["Searched".bold()]),
                        Line::from(format!("  {}", ev.query)),
                    ];
                    out.push(self.render_lines(block)?);
                    self.mark_work_activity();
                }
            }
            EventMsg::ViewImageToolCall(ev) => {
                if self.verbosity == Verbosity::Simple {
                    out.extend(self.flush_agent_output(false)?);
                    out.extend(self.flush_plan_stream()?);
                    let path = display_path_for(&ev.path, &self.cwd);
                    let block = vec![
                        Line::from(vec!["Viewed Image".bold()]),
                        Line::from(vec![Span::from("  "), Span::from(path).dim()]),
                    ];
                    out.push(self.render_lines(block)?);
                    self.mark_work_activity();
                }
            }
            EventMsg::ExecCommandEnd(ev) => {
                if self.verbosity != Verbosity::Minimal {
                    out.extend(self.flush_agent_output(false)?);
                    out.extend(self.flush_plan_stream()?);
                    if let Some(block) = self.render_exec_command_end(ev)? {
                        out.push(block);
                        self.mark_work_activity();
                    }
                }
            }
            EventMsg::PatchApplyEnd(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                if ev.success {
                    let patch_blocks = self.render_patch_blocks(ev.changes.clone())?;
                    if !patch_blocks.is_empty() {
                        out.extend(patch_blocks);
                        self.mark_work_activity();
                    }
                } else {
                    out.push(
                        self.render_cell_block(Box::new(new_patch_apply_failure(
                            ev.stderr.clone(),
                        )))?,
                    );
                    self.mark_work_activity();
                }
            }
            EventMsg::HookStarted(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                let label = ev.run.event_name.as_kebab_case();
                let mut message = format!("Running {label} hook");
                if let Some(status_message) = &ev.run.status_message
                    && !status_message.is_empty()
                {
                    message.push_str(": ");
                    message.push_str(status_message);
                }
                out.push(self.render_lines(vec![Line::from(message)])?);
                self.mark_work_activity();
            }
            EventMsg::HookCompleted(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                let status = format!("{:?}", ev.run.status).to_lowercase();
                let header = format!("{} hook ({status})", ev.run.event_name.as_kebab_case());
                let mut lines: Vec<Line<'static>> = vec![Line::from(header)];
                for entry in &ev.run.entries {
                    let prefix = match entry.kind {
                        codex_protocol::protocol::HookOutputEntryKind::Warning => "warning: ",
                        codex_protocol::protocol::HookOutputEntryKind::Stop => "stop: ",
                        codex_protocol::protocol::HookOutputEntryKind::Feedback => "feedback: ",
                        codex_protocol::protocol::HookOutputEntryKind::Context => "hook context: ",
                        codex_protocol::protocol::HookOutputEntryKind::Error => "error: ",
                    };
                    lines.push(Line::from(format!("  {prefix}{}", entry.text)));
                }
                out.push(self.render_lines(lines)?);
                self.mark_work_activity();
            }
            EventMsg::CollabAgentSpawnEnd(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(multi_agents::spawn_end(ev.clone())))?);
                self.mark_work_activity();
            }
            EventMsg::CollabAgentInteractionEnd(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(
                    self.render_cell_block(Box::new(multi_agents::interaction_end(ev.clone())))?,
                );
                self.mark_work_activity();
            }
            EventMsg::CollabWaitingBegin(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(
                    self.render_cell_block(Box::new(multi_agents::waiting_begin(ev.clone())))?,
                );
                self.mark_work_activity();
            }
            EventMsg::CollabWaitingEnd(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(multi_agents::waiting_end(ev.clone())))?);
                self.mark_work_activity();
            }
            EventMsg::CollabCloseEnd(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(multi_agents::close_end(ev.clone())))?);
                self.mark_work_activity();
            }
            EventMsg::CollabResumeBegin(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(multi_agents::resume_begin(ev.clone())))?);
                self.mark_work_activity();
            }
            EventMsg::CollabResumeEnd(ev) => {
                out.extend(self.flush_barrier_agent_output()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(multi_agents::resume_end(ev.clone())))?);
                self.mark_work_activity();
            }
            EventMsg::PotterStreamRecoveryUpdate {
                attempt,
                max_attempts,
                error_message,
            } => {
                out.extend(self.drop_incomplete_minimal_agent_stream_or_flush()?);
                out.extend(self.flush_plan_stream()?);
                out.push(
                    self.render_cell_block(Box::new(PotterStreamRecoveryRetryCell {
                        attempt: *attempt,
                        max_attempts: *max_attempts,
                        error_message: error_message.clone(),
                    }))?,
                );
            }
            EventMsg::PotterStreamRecoveryRecovered => {}
            EventMsg::PotterStreamRecoveryGaveUp {
                error_message,
                max_attempts,
                ..
            } => {
                out.extend(self.drop_incomplete_minimal_agent_stream_or_flush()?);
                out.extend(self.flush_plan_stream()?);
                out.push(self.render_cell_block(Box::new(
                    PotterStreamRecoveryUnrecoverableCell {
                        max_attempts: *max_attempts,
                        error_message: error_message.clone(),
                    },
                ))?);
            }
            _ => {}
        }

        Ok(out)
    }

    fn maybe_emit_round_marker(&mut self, out: &mut Vec<String>) -> io::Result<()> {
        if self.pending_round_marker.is_none() || self.session_meta.is_none() {
            return Ok(());
        }

        let Some((current, total)) = self.pending_round_marker.take() else {
            return Ok(());
        };
        let Some(session_meta) = self.session_meta.as_ref() else {
            self.pending_round_marker = Some((current, total));
            return Ok(());
        };

        let label = crate::history_cell_potter::format_potter_round_session_label(
            &session_meta.model,
            session_meta.reasoning_effort,
            session_meta.service_tier,
        );
        let mut spans = vec![
            Span::styled(
                "CodexPotter: ",
                Style::default()
                    .fg(secondary_color())
                    .add_modifier(Modifier::BOLD),
            ),
            format!("iteration round {current}/{total}").into(),
        ];
        if !label.is_empty() {
            spans.push(format!(" ({label})").into());
        }
        self.push_block(out, self.render_lines(vec![Line::from(spans)])?);
        Ok(())
    }

    fn push_block(&mut self, out: &mut Vec<String>, block: String) {
        if self.emitted_exec_meta || self.pending_exec_meta.is_none() {
            out.push(block);
        } else {
            self.pending_initial_blocks.push(block);
        }
    }

    fn flush_pending_exec_meta(&mut self, allow_placeholders: bool) -> io::Result<Vec<String>> {
        if self.emitted_exec_meta {
            return Ok(Vec::new());
        }

        let Some(pending_meta) = self.pending_exec_meta.clone() else {
            return Ok(Vec::new());
        };

        let (model, reasoning_effort) = match &self.session_meta {
            Some(meta) => (meta.model.clone(), meta.reasoning_effort),
            None if allow_placeholders => ("<missing>".to_string(), None),
            None => return Ok(Vec::new()),
        };

        self.emitted_exec_meta = true;
        self.pending_exec_meta = None;

        let mut out = Vec::new();
        out.push(self.render_exec_meta_block(
            pending_meta.workdir.as_path(),
            &model,
            reasoning_effort,
            pending_meta.user_prompt_file.as_path(),
        )?);
        out.append(&mut self.pending_initial_blocks);
        Ok(out)
    }

    fn build_agent_message_lines(&self, message: &str) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        markdown::append_markdown(
            message,
            self.width.map(usize::from),
            Some(&self.cwd),
            &mut lines,
        );
        lines
    }

    fn take_streamed_agent_message_lines(&mut self) -> Vec<Line<'static>> {
        let lines = self.stream.take_finalized_lines();
        self.saw_agent_delta = false;
        lines
    }

    fn discard_streamed_agent_message_lines(&mut self) {
        let _ = self.take_streamed_agent_message_lines();
    }

    fn take_agent_message_lines(&mut self, message: &str) -> Vec<Line<'static>> {
        if self.saw_agent_delta {
            self.take_streamed_agent_message_lines()
        } else {
            self.build_agent_message_lines(message)
        }
    }

    fn render_pending_minimal_agent_message(
        &mut self,
        final_message: bool,
    ) -> io::Result<Option<String>> {
        let Some(lines) = self.pending_minimal_agent_message_lines.take() else {
            return Ok(None);
        };
        let was_visible = self.pending_minimal_agent_message_visible;
        self.pending_minimal_agent_message_visible = false;
        if was_visible {
            return Ok(None);
        }

        Ok(Some(
            self.render_agent_message_block(lines, !final_message)?,
        ))
    }

    fn flush_agent_output(&mut self, final_message: bool) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        if self.verbosity == Verbosity::Minimal {
            if self.saw_agent_delta {
                let lines = self.take_streamed_agent_message_lines();
                if !lines.is_empty() {
                    out.push(self.render_agent_message_block(lines, !final_message)?);
                }
            } else if let Some(block) = self.render_pending_minimal_agent_message(final_message)? {
                out.push(block);
            }
            return Ok(out);
        }

        if self.saw_agent_delta {
            let lines = self.take_streamed_agent_message_lines();
            if !lines.is_empty() {
                self.push_simple_final_message_separator(&mut out)?;
                out.push(self.render_agent_message_block(lines, false)?);
            }
        }

        Ok(out)
    }

    fn flush_barrier_agent_output(&mut self) -> io::Result<Vec<String>> {
        if self.verbosity == Verbosity::Minimal {
            let mut out = Vec::new();
            if let Some(block) = self.render_pending_minimal_agent_message(true)? {
                out.push(block);
            }
            return Ok(out);
        }

        self.flush_agent_output(false)
    }

    fn drop_incomplete_minimal_agent_stream_or_flush(&mut self) -> io::Result<Vec<String>> {
        if self.verbosity == Verbosity::Minimal && self.saw_agent_delta {
            self.discard_streamed_agent_message_lines();
            return Ok(Vec::new());
        }

        self.flush_agent_output(false)
    }

    fn store_pending_minimal_agent_message(
        &mut self,
        lines: Vec<Line<'static>>,
        out: &mut Vec<String>,
    ) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        if let Some(previous) = self.pending_minimal_agent_message_lines.replace(lines)
            && !self.pending_minimal_agent_message_visible
        {
            out.push(self.render_agent_message_block(previous, true)?);
        }
        self.pending_minimal_agent_message_visible = false;
        Ok(())
    }

    fn flush_plan_stream(&mut self) -> io::Result<Vec<String>> {
        if self.verbosity == Verbosity::Minimal {
            self.plan_stream = None;
            return Ok(Vec::new());
        }

        let Some(mut controller) = self.plan_stream.take() else {
            return Ok(Vec::new());
        };
        let Some(cell) = controller.finalize() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        self.push_simple_final_message_separator(&mut out)?;
        out.push(self.render_cell_block(cell)?);
        Ok(out)
    }

    fn render_exec_command_end(
        &self,
        ev: &codex_protocol::protocol::ExecCommandEndEvent,
    ) -> io::Result<Option<String>> {
        let aggregated_output = if !ev.aggregated_output.is_empty() {
            ev.aggregated_output.clone()
        } else {
            format!("{}{}", ev.stdout, ev.stderr)
        };

        let mut cell = new_active_exec_command(
            ev.call_id.clone(),
            ev.command.clone(),
            ev.parsed_cmd.clone(),
            ev.source,
            ev.interaction_input.clone(),
            false,
        );
        cell.complete_call(
            &ev.call_id,
            CommandOutput {
                exit_code: ev.exit_code,
                aggregated_output,
                formatted_output: ev.formatted_output.clone(),
            },
            ev.duration,
        );

        Ok(Some(self.render_cell_block(Box::new(cell))?))
    }

    fn maybe_render_reasoning_status_hint(&mut self, delta: &str) -> io::Result<Option<String>> {
        let Some(header) = self.reasoning_status.on_delta(delta) else {
            return Ok(None);
        };
        self.maybe_render_reasoning_status_hint_block(header)
    }

    fn maybe_render_reasoning_status_hint_block(
        &mut self,
        header: String,
    ) -> io::Result<Option<String>> {
        let elapsed = self
            .status_started_at
            .map(|started_at| started_at.elapsed())
            .unwrap_or_default();
        let context_window_percent = self.current_context_window_percent();
        let context_hint = self.take_reasoning_context_hint(context_window_percent);

        if self.last_status_hint_header.as_deref() == Some(header.as_str())
            && context_hint.is_none()
        {
            return Ok(None);
        }

        self.last_status_hint_header = Some(header.clone());
        let lines =
            self.build_reasoning_status_hint_lines_with_context_hint(header, elapsed, context_hint);
        self.render_lines(lines).map(Some)
    }

    fn build_reasoning_status_hint_lines_with_context_hint(
        &mut self,
        header: String,
        elapsed: Duration,
        context_hint: Option<i64>,
    ) -> Vec<Line<'static>> {
        let pretty_elapsed = fmt_elapsed_compact(elapsed.as_secs());
        let mut text = format!("@{pretty_elapsed}: {header}");
        if let Some(percent) = context_hint {
            text.push_str(&format!(" ({percent}% context left)"));
        }

        let mut lines = vec![Line::from(text)];
        crate::render::line_utils::dim_lines(&mut lines);
        lines
    }

    #[cfg(test)]
    fn build_reasoning_status_hint_lines_from_state(
        &mut self,
        header: String,
        elapsed: Duration,
        context_window_percent: Option<i64>,
    ) -> Vec<Line<'static>> {
        let context_hint = self.take_reasoning_context_hint(context_window_percent);
        self.build_reasoning_status_hint_lines_with_context_hint(header, elapsed, context_hint)
    }

    fn current_context_window_percent(&self) -> Option<i64> {
        let context_window = self
            .model_context_window
            .filter(|context_window| *context_window > 0)?;

        Some(
            self.context_usage
                .percent_of_context_window_remaining(context_window)
                .clamp(0, 100),
        )
    }

    fn take_reasoning_context_hint(&mut self, context_window_percent: Option<i64>) -> Option<i64> {
        let percent = context_window_percent?;
        let recovered_level = reasoning_context_output_level(percent);
        if recovered_level > self.context_output_level {
            self.context_output_level = recovered_level;
        }
        if percent >= self.context_output_level - EXEC_REASONING_CONTEXT_STEP {
            return None;
        }

        self.context_output_level = recovered_level;
        Some(percent)
    }

    fn render_patch_blocks(
        &self,
        changes: std::collections::HashMap<PathBuf, codex_protocol::protocol::FileChange>,
    ) -> io::Result<Vec<String>> {
        if self.verbosity == Verbosity::Simple {
            return Ok(vec![self.render_cell_block(Box::new(
                crate::history_cell::new_patch_event(changes, &self.cwd, self.verbosity),
            ))?]);
        }

        let lines =
            create_compact_diff_summary(&changes, &self.cwd, usize::from(self.cell_width()));
        if lines.is_empty() {
            return Ok(Vec::new());
        }

        if lines.len() == 1 {
            return Ok(vec![self.render_lines(normalize_general_lines(lines))?]);
        }

        let mut out = Vec::new();
        for line in lines.into_iter().skip(1) {
            out.push(self.render_lines(vec![normalize_standalone_line(line)])?);
        }
        Ok(out)
    }

    fn render_project_summary(&self, summary: PendingProjectSummary) -> io::Result<String> {
        let PendingProjectSummary {
            outcome,
            rounds,
            duration,
            user_prompt_file,
            git_commit_start,
            git_commit_end,
        } = summary;

        let mut header_spans = vec![
            Span::styled(
                "CodexPotter summary:",
                Style::default()
                    .fg(secondary_color())
                    .add_modifier(Modifier::BOLD),
            ),
            " ".into(),
            format!("{rounds} rounds").bold(),
            " in ".into(),
            fmt_elapsed_compact(duration.as_secs()).bold(),
        ];
        match outcome {
            PendingProjectSummaryOutcome::Succeeded => {}
            PendingProjectSummaryOutcome::BudgetExhausted => {
                header_spans.push(" ".into());
                header_spans.push("(Budget exhausted)".red());
            }
        }

        let mut lines = vec![Line::from(header_spans), Line::from("")];
        lines.extend(build_potter_project_summary_detail_lines(
            &user_prompt_file,
            &git_commit_start,
            &git_commit_end,
            None,
        ));

        self.render_lines(lines)
    }

    fn render_exec_meta_block(
        &self,
        workdir: &std::path::Path,
        model: &str,
        reasoning_effort: Option<ReasoningEffortConfig>,
        user_prompt_file: &std::path::Path,
    ) -> io::Result<String> {
        let reasoning_effort = reasoning_effort
            .map(|effort| effort.to_string())
            .unwrap_or_else(|| "<missing>".to_string());
        let lines = vec![
            Line::from("--------"),
            Line::from(vec![
                "workdir:".bold(),
                " ".into(),
                workdir.display().to_string().into(),
            ]),
            Line::from(vec!["model:".bold(), " ".into(), model.to_string().into()]),
            Line::from(vec![
                "reasoning effort:".bold(),
                " ".into(),
                reasoning_effort.into(),
            ]),
            Line::from(vec![
                "codexpotter project file:".bold(),
                " ".into(),
                user_prompt_file.to_string_lossy().to_string().into(),
            ]),
            Line::from("--------"),
        ];
        self.render_lines(lines)
    }

    fn mark_work_activity(&mut self) {
        if self.verbosity != Verbosity::Simple {
            return;
        }
        self.pending_simple_final_message_separator = true;
        if self.separator_baseline.is_none() {
            self.separator_baseline = Some(Instant::now());
        }
    }

    fn push_simple_final_message_separator(&mut self, out: &mut Vec<String>) -> io::Result<()> {
        if self.verbosity != Verbosity::Simple || !self.pending_simple_final_message_separator {
            return Ok(());
        }

        let elapsed_seconds = self
            .separator_baseline
            .map(|baseline| baseline.elapsed().as_secs());
        self.pending_simple_final_message_separator = false;
        self.separator_baseline = Some(Instant::now());
        out.push(self.render_cell_block(Box::new(
            crate::history_cell::FinalMessageSeparator::new(elapsed_seconds),
        ))?);
        Ok(())
    }

    fn render_cell_block(&self, cell: Box<dyn HistoryCell>) -> io::Result<String> {
        self.render_lines(normalize_general_lines(
            cell.display_lines(self.cell_width()),
        ))
    }

    fn render_agent_message_block(
        &self,
        mut lines: Vec<Line<'static>>,
        dim: bool,
    ) -> io::Result<String> {
        if dim {
            crate::render::line_utils::dim_lines(&mut lines);
        }
        self.render_lines(lines)
    }

    fn render_lines(&self, lines: Vec<Line<'static>>) -> io::Result<String> {
        let mut out = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            if idx > 0 {
                out.write_all(b"\n")?;
            }
            write_line(&mut out, line, self.color_enabled)?;
        }
        String::from_utf8(out)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
    }

    fn cell_width(&self) -> u16 {
        self.width.unwrap_or(DEFAULT_RENDER_WIDTH).max(1)
    }
}

fn normalize_general_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| {
            let line = replace_prefix(line, "• ", "");
            if idx == 0 {
                line
            } else {
                let line = replace_prefix(line, "  └ ", "  ");
                replace_prefix(line, "    ", "  ")
            }
        })
        .collect()
}

fn normalize_standalone_line(line: Line<'static>) -> Line<'static> {
    let line = replace_prefix(line, "• ", "");
    let line = replace_prefix(line, "  └ ", "");
    replace_prefix(line, "    ", "")
}

fn replace_prefix(mut line: Line<'static>, from: &str, to: &str) -> Line<'static> {
    let Some(first) = line.spans.first().cloned() else {
        return line;
    };
    let Some(rest) = first.content.as_ref().strip_prefix(from) else {
        return line;
    };

    let mut spans = Vec::with_capacity(line.spans.len() + usize::from(!to.is_empty()));
    if !to.is_empty() {
        spans.push(Span::styled(to.to_string(), first.style));
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), first.style));
    }
    spans.extend(line.spans.into_iter().skip(1));
    line.spans = spans;
    line
}

fn reasoning_context_output_level(percent: i64) -> i64 {
    if percent <= 0 {
        return EXEC_REASONING_CONTEXT_STEP;
    }

    ((percent + EXEC_REASONING_CONTEXT_STEP - 1) / EXEC_REASONING_CONTEXT_STEP
        * EXEC_REASONING_CONTEXT_STEP)
        .clamp(
            EXEC_REASONING_CONTEXT_STEP,
            EXEC_REASONING_CONTEXT_MAX_LEVEL,
        )
}

struct ModifierDiff {
    from: Modifier,
    to: Modifier,
}

impl ModifierDiff {
    fn queue<W: Write>(self, mut writer: W) -> io::Result<()> {
        use crossterm::style::Attribute;

        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(writer, SetAttribute(Attribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(writer, SetAttribute(Attribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(writer, SetAttribute(Attribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(writer, SetAttribute(Attribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(writer, SetAttribute(Attribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(writer, SetAttribute(Attribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(writer, SetAttribute(Attribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(writer, SetAttribute(Attribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(writer, SetAttribute(Attribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(Attribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(writer, SetAttribute(Attribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(writer, SetAttribute(Attribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(writer, SetAttribute(Attribute::RapidBlink))?;
        }

        Ok(())
    }
}

fn write_line(writer: &mut impl Write, line: &Line<'_>, color_enabled: bool) -> io::Result<()> {
    if !color_enabled {
        for span in &line.spans {
            writer.write_all(span.content.as_ref().as_bytes())?;
        }
        return Ok(());
    }

    let mut fg = ratatui::style::Color::Reset;
    let mut bg = ratatui::style::Color::Reset;
    let mut last_modifier = Modifier::empty();
    for span in &line.spans {
        let style = span.style.patch(line.style);

        let mut modifier = Modifier::empty();
        modifier.insert(style.add_modifier);
        modifier.remove(style.sub_modifier);
        if modifier != last_modifier {
            ModifierDiff {
                from: last_modifier,
                to: modifier,
            }
            .queue(&mut *writer)?;
            last_modifier = modifier;
        }

        let next_fg = style.fg.unwrap_or(ratatui::style::Color::Reset);
        let next_bg = style.bg.unwrap_or(ratatui::style::Color::Reset);
        if next_fg != fg || next_bg != bg {
            queue!(
                writer,
                SetColors(Colors::new(next_fg.into(), next_bg.into()))
            )?;
            fg = next_fg;
            bg = next_bg;
        }

        queue!(writer, Print(span.content.clone()))?;
    }

    queue!(
        writer,
        SetForegroundColor(CrosstermColor::Reset),
        SetBackgroundColor(CrosstermColor::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::AbsolutePathBuf;
    use codex_protocol::approvals::GuardianAssessmentEvent;
    use codex_protocol::approvals::GuardianAssessmentStatus;
    use codex_protocol::approvals::GuardianRiskLevel;
    use codex_protocol::models::FileSystemPermissions;
    use codex_protocol::models::NetworkPermissions;
    use codex_protocol::protocol::AgentMessageDeltaEvent;
    use codex_protocol::protocol::AgentReasoningDeltaEvent;
    use codex_protocol::protocol::AgentReasoningEvent;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ExecCommandEndEvent;
    use codex_protocol::protocol::ExecCommandSource;
    use codex_protocol::protocol::FileChange;
    use codex_protocol::protocol::PatchApplyEndEvent;
    use codex_protocol::protocol::TurnStartedEvent;
    use codex_protocol::protocol::ViewImageToolCallEvent;
    use codex_protocol::protocol::WebSearchEndEvent;
    use codex_protocol::request_permissions::RequestPermissionProfile;
    use codex_protocol::request_permissions::RequestPermissionsEvent;
    use pretty_assertions::assert_eq;
    use ratatui::style::Modifier;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn line_to_plain_string(line: &Line<'_>) -> String {
        let mut out = String::new();
        for span in &line.spans {
            out.push_str(span.content.as_ref());
        }
        out
    }

    fn assert_all_spans_dimmed(lines: &[Line<'_>]) {
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .all(|span| span.style.add_modifier.contains(Modifier::DIM)),
            "expected all spans to be dimmed: {:?}",
            lines.iter().map(line_to_plain_string).collect::<Vec<_>>()
        );
    }

    fn synthetic_absolute_path(components: &[&str]) -> PathBuf {
        #[cfg(windows)]
        {
            components
                .iter()
                .fold(PathBuf::from(r"C:\"), |path, component| {
                    path.join(component)
                })
        }

        #[cfg(not(windows))]
        {
            components
                .iter()
                .fold(PathBuf::from("/"), |path, component| path.join(component))
        }
    }

    fn synthetic_absolute_path_buf(components: &[&str]) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(synthetic_absolute_path(components))
            .expect("absolute path")
    }

    #[test]
    fn minimal_multi_file_patch_renders_each_file_without_changed_header() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let repo = synthetic_absolute_path(&["repo"]);
        let mut changes = HashMap::new();
        changes.insert(
            repo.join("a.txt"),
            FileChange::Update {
                unified_diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
                move_path: None,
            },
        );
        changes.insert(
            repo.join("b.txt"),
            FileChange::Add {
                content: "hello\n".to_string(),
            },
        );

        renderer.cwd = repo;
        let blocks = renderer
            .handle_event(&EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch".to_string(),
                turn_id: String::new(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }))
            .expect("render patch");

        assert_eq!(blocks.len(), 2);
        assert!(
            blocks
                .iter()
                .all(|block| !block.contains("Changed 2 files"))
        );
        assert!(
            blocks
                .iter()
                .any(|block| block.contains("Edited a.txt (+1 -1)"))
        );
        assert!(
            blocks
                .iter()
                .any(|block| block.contains("Added b.txt (+1 -0)"))
        );
    }

    #[test]
    fn simple_patch_keeps_full_diff_block_visible() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Simple, Some(120), false);
        let repo = synthetic_absolute_path(&["repo"]);
        let mut changes = HashMap::new();
        changes.insert(
            repo.join("a.txt"),
            FileChange::Update {
                unified_diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
                move_path: None,
            },
        );

        renderer.cwd = repo;
        let blocks = renderer
            .handle_event(&EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch".to_string(),
                turn_id: String::new(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }))
            .expect("render patch");

        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert!(block.contains("Edited a.txt (+1 -1)"));
        assert!(block.contains("-old"));
        assert!(block.contains("+new"));
    }

    #[test]
    fn summary_strips_interactive_loop_line_and_chrome() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let blocks = renderer
            .handle_event(&EventMsg::PotterProjectBudgetExhausted {
                rounds: 5,
                duration: Duration::from_secs(7328),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/03/26/3/MAIN.md"),
                git_commit_start: "96ca8c6abc".to_string(),
                git_commit_end: "0919e7bdef".to_string(),
            })
            .expect("store summary");
        assert!(blocks.is_empty());

        let blocks = renderer
            .handle_event(&EventMsg::PotterRoundFinished {
                outcome: codex_protocol::protocol::PotterRoundOutcome::Completed,
                duration_secs: 0,
            })
            .expect("emit summary");
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            concat!(
                "CodexPotter summary: 5 rounds in 2h 02m 08s (Budget exhausted)\n",
                "\n",
                "  View changes:      git diff 96ca8c6...0919e7b\n",
                "  Task history:      .codexpotter/projects/2026/03/26/3/MAIN.md",
            )
        );
    }

    #[test]
    fn summary_is_emitted_without_round_finished_separator_when_duration_is_available() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let blocks = renderer
            .handle_event(&EventMsg::PotterProjectBudgetExhausted {
                rounds: 5,
                duration: Duration::from_secs(7328),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/03/26/3/MAIN.md"),
                git_commit_start: "96ca8c6abc".to_string(),
                git_commit_end: "0919e7bdef".to_string(),
            })
            .expect("store summary");
        assert!(blocks.is_empty());

        let blocks = renderer
            .handle_event(&EventMsg::PotterRoundFinished {
                outcome: codex_protocol::protocol::PotterRoundOutcome::Completed,
                duration_secs: 733,
            })
            .expect("emit summary");
        assert_eq!(blocks.len(), 1);
        assert!(
            blocks[0].starts_with("CodexPotter summary: 5 rounds in 2h 02m 08s"),
            "unexpected summary block: {:?}",
            blocks[0]
        );
        assert!(!blocks[0].contains("Round finished in"));
    }

    #[test]
    fn summary_omits_view_changes_when_git_commits_are_missing() {
        let user_prompt_file = PathBuf::from(".codexpotter/projects/2026/03/26/3/MAIN.md");

        {
            let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
            let blocks = renderer
                .handle_event(&EventMsg::PotterProjectBudgetExhausted {
                    rounds: 1,
                    duration: Duration::from_secs(1),
                    user_prompt_file: user_prompt_file.clone(),
                    git_commit_start: String::new(),
                    git_commit_end: "0919e7bdef".to_string(),
                })
                .expect("store summary");
            assert!(blocks.is_empty());

            let blocks = renderer
                .handle_event(&EventMsg::PotterRoundFinished {
                    outcome: codex_protocol::protocol::PotterRoundOutcome::Completed,
                    duration_secs: 0,
                })
                .expect("emit summary");
            assert_eq!(blocks.len(), 1);
            let block = &blocks[0];
            assert!(!block.contains("View changes:"));
            assert!(block.contains("Task history:"));
        }

        {
            let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
            let blocks = renderer
                .handle_event(&EventMsg::PotterProjectBudgetExhausted {
                    rounds: 1,
                    duration: Duration::from_secs(1),
                    user_prompt_file,
                    git_commit_start: "96ca8c6abc".to_string(),
                    git_commit_end: String::new(),
                })
                .expect("store summary");
            assert!(blocks.is_empty());

            let blocks = renderer
                .handle_event(&EventMsg::PotterRoundFinished {
                    outcome: codex_protocol::protocol::PotterRoundOutcome::Completed,
                    duration_secs: 0,
                })
                .expect("emit summary");
            assert_eq!(blocks.len(), 1);
            let block = &blocks[0];
            assert!(!block.contains("View changes:"));
            assert!(block.contains("Task history:"));
        }
    }

    #[test]
    fn round_marker_emits_for_session_and_round_started_in_any_order() {
        let expected = vec!["CodexPotter: iteration round 1/10 (gpt-5.2 xhigh)".to_string()];

        {
            let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
            let blocks = renderer
                .handle_event(&EventMsg::PotterRoundStarted {
                    current: 1,
                    total: 10,
                })
                .expect("round marker");
            assert!(blocks.is_empty());

            let blocks = renderer
                .handle_event(&EventMsg::SessionConfigured(
                    codex_protocol::protocol::SessionConfiguredEvent {
                        session_id: codex_protocol::ThreadId::from_string(
                            "019ca423-63d9-7641-ae83-db060ad3c000",
                        )
                        .expect("thread id"),
                        forked_from_id: None,
                        model: "gpt-5.2".to_string(),
                        model_provider_id: "openai".to_string(),
                        service_tier: None,
                        cwd: PathBuf::from("/repo"),
                        reasoning_effort: Some(
                            codex_protocol::openai_models::ReasoningEffort::XHigh,
                        ),
                        history_log_id: 0,
                        history_entry_count: 0,
                        initial_messages: None,
                        rollout_path: PathBuf::from("/repo/rollout.jsonl"),
                    },
                ))
                .expect("session configured");
            assert_eq!(blocks, expected);
        }

        {
            let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
            let blocks = renderer
                .handle_event(&EventMsg::SessionConfigured(
                    codex_protocol::protocol::SessionConfiguredEvent {
                        session_id: codex_protocol::ThreadId::from_string(
                            "019ca423-63d9-7641-ae83-db060ad3c000",
                        )
                        .expect("thread id"),
                        forked_from_id: None,
                        model: "gpt-5.2".to_string(),
                        model_provider_id: "openai".to_string(),
                        service_tier: None,
                        cwd: PathBuf::from("/repo"),
                        reasoning_effort: Some(
                            codex_protocol::openai_models::ReasoningEffort::XHigh,
                        ),
                        history_log_id: 0,
                        history_entry_count: 0,
                        initial_messages: None,
                        rollout_path: PathBuf::from("/repo/rollout.jsonl"),
                    },
                ))
                .expect("session configured");
            assert!(blocks.is_empty());

            let blocks = renderer
                .handle_event(&EventMsg::PotterRoundStarted {
                    current: 1,
                    total: 10,
                })
                .expect("round marker");
            assert_eq!(blocks, expected);
        }
    }

    #[test]
    fn project_started_emits_exec_meta_block_first() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let blocks = renderer
            .handle_event(&EventMsg::PotterProjectStarted {
                user_message: Some("Fix the failing test".to_string()),
                working_dir: PathBuf::from("/repo"),
                project_dir: PathBuf::from(".codexpotter/projects/2026/03/27/1"),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/03/27/1/MAIN.md"),
            })
            .expect("project started");

        assert!(blocks.is_empty());

        let blocks = renderer
            .handle_event(&EventMsg::PotterRoundStarted {
                current: 1,
                total: 10,
            })
            .expect("round marker");
        assert!(blocks.is_empty());

        let blocks = renderer
            .handle_event(&EventMsg::SessionConfigured(
                codex_protocol::protocol::SessionConfiguredEvent {
                    session_id: codex_protocol::ThreadId::from_string(
                        "019ca423-63d9-7641-ae83-db060ad3c000",
                    )
                    .expect("thread id"),
                    forked_from_id: None,
                    model: "gpt-5.2".to_string(),
                    model_provider_id: "openai".to_string(),
                    service_tier: None,
                    cwd: PathBuf::from("/repo"),
                    reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::XHigh),
                    history_log_id: 0,
                    history_entry_count: 0,
                    initial_messages: None,
                    rollout_path: PathBuf::from("/repo/rollout.jsonl"),
                },
            ))
            .expect("session configured");

        assert_eq!(
            blocks,
            vec![
                "--------\n\
workdir: /repo\n\
model: gpt-5.2\n\
reasoning effort: xhigh\n\
codexpotter project file: .codexpotter/projects/2026/03/27/1/MAIN.md\n\
--------"
                    .to_string(),
                "CodexPotter: iteration round 1/10 (gpt-5.2 xhigh)".to_string(),
            ]
        );
    }

    #[test]
    fn minimal_agent_message_stays_pending_until_turn_complete() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let blocks = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "done".to_string(),
                    phase: None,
                },
            ))
            .expect("agent message");
        assert!(blocks.is_empty());

        let blocks = renderer
            .handle_event(&EventMsg::TurnComplete(
                codex_protocol::protocol::TurnCompleteEvent {
                    turn_id: "turn-1".to_string(),
                    last_agent_message: None,
                },
            ))
            .expect("turn complete");
        assert_eq!(blocks, vec!["done".to_string()]);
    }

    #[test]
    fn minimal_commentary_emits_status_hint_block_in_exec_mode() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));
        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(42));

        let blocks = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nWorking...".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("commentary agent message");

        assert_eq!(blocks, vec!["@42s: Inspecting".to_string()]);
    }

    #[test]
    fn minimal_commentary_stays_append_only_in_exec_mode() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));

        let mut blocks = Vec::new();

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(12));
        blocks.extend(
            renderer
                .handle_event(&EventMsg::AgentMessage(
                    codex_protocol::protocol::AgentMessageEvent {
                        message: "**Inspecting**\n\nWorking...".to_string(),
                        phase: Some(MessagePhase::Commentary),
                    },
                ))
                .expect("first commentary agent message"),
        );

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(18));
        blocks.extend(
            renderer
                .handle_event(&EventMsg::AgentMessage(
                    codex_protocol::protocol::AgentMessageEvent {
                        message: "**Patching**\n\nUpdating files...".to_string(),
                        phase: Some(MessagePhase::Commentary),
                    },
                ))
                .expect("second commentary agent message"),
        );

        blocks.extend(
            renderer
                .handle_event(&EventMsg::TurnComplete(
                    codex_protocol::protocol::TurnCompleteEvent {
                        turn_id: "turn-1".to_string(),
                        last_agent_message: Some("final".to_string()),
                    },
                ))
                .expect("turn complete"),
        );

        assert_eq!(
            blocks,
            vec![
                "@12s: Inspecting".to_string(),
                "@18s: Patching".to_string(),
                "final".to_string(),
            ]
        );
    }

    #[test]
    fn minimal_commentary_dedups_repeated_status_headers_in_exec_mode() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(12));
        let first = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nWorking...".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("first commentary agent message");
        assert_eq!(first, vec!["@12s: Inspecting".to_string()]);

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(18));
        let second = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nStill working...".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("second commentary agent message");
        assert!(second.is_empty());
    }

    #[test]
    fn reasoning_status_hint_dedups_repeated_headers_across_reasoning_items() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(12));
        let first = renderer
            .handle_event(&EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                delta: "**Inspecting**".to_string(),
            }))
            .expect("first reasoning delta");
        assert_eq!(first, vec!["@12s: Inspecting".to_string()]);

        let final_blocks = renderer
            .handle_event(&EventMsg::AgentReasoning(AgentReasoningEvent {
                text: "**Inspecting**\nDone.".to_string(),
            }))
            .expect("reasoning final");
        assert!(final_blocks.is_empty());

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(18));
        let second = renderer
            .handle_event(&EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                delta: "**Inspecting**".to_string(),
            }))
            .expect("second reasoning delta");
        assert!(second.is_empty());
    }

    #[test]
    fn status_hint_emits_context_hint_even_when_header_repeats() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: Some(12_001),
        }));

        renderer.context_usage.total_tokens = 0;
        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(12));
        let first = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nWorking...".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("first commentary agent message");
        assert_eq!(first, vec!["@12s: Inspecting".to_string()]);

        renderer.context_usage.total_tokens = 12_001;
        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(18));
        let second = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nWorking...".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("second commentary agent message");
        assert_eq!(
            second,
            vec!["@18s: Inspecting (0% context left)".to_string()]
        );
    }

    #[test]
    fn status_hint_dedups_repeated_headers_across_commentary_and_reasoning() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(12));
        let commentary = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nWorking...".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("commentary agent message");
        assert_eq!(commentary, vec!["@12s: Inspecting".to_string()]);

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(18));
        let reasoning = renderer
            .handle_event(&EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                delta: "**Inspecting**".to_string(),
            }))
            .expect("reasoning delta");
        assert!(reasoning.is_empty());
    }

    #[test]
    fn status_hint_header_resets_when_a_new_turn_starts() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(12));
        let first = renderer
            .handle_event(&EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                delta: "**Inspecting**".to_string(),
            }))
            .expect("first reasoning delta");
        assert_eq!(first, vec!["@12s: Inspecting".to_string()]);

        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-2".to_string(),
            model_context_window: None,
        }));

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(18));
        let second = renderer
            .handle_event(&EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                delta: "**Inspecting**".to_string(),
            }))
            .expect("second reasoning delta");
        assert_eq!(second, vec!["@18s: Inspecting".to_string()]);
    }

    #[test]
    fn minimal_patch_barrier_does_not_flush_inflight_commentary_delta() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let repo = synthetic_absolute_path(&["repo"]);
        renderer.cwd = repo.clone();
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));

        let delta_blocks = renderer
            .handle_event(&EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "Inspecting progress file".to_string(),
            }))
            .expect("agent delta");
        assert!(delta_blocks.is_empty());

        let mut changes = HashMap::new();
        changes.insert(
            repo.join("file.txt"),
            FileChange::Update {
                unified_diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
                move_path: None,
            },
        );
        let patch_blocks = renderer
            .handle_event(&EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-1".to_string(),
                turn_id: "turn-1".to_string(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }))
            .expect("patch apply end");
        assert!(
            patch_blocks
                .iter()
                .all(|block| !block.contains("Inspecting progress file")),
            "expected patch barrier not to flush in-flight commentary delta: {patch_blocks:?}"
        );
        assert!(
            patch_blocks
                .iter()
                .any(|block| block.contains("Edited file.txt (+1 -1)")),
            "expected patch block to stay visible: {patch_blocks:?}"
        );

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(15));
        let commentary_blocks = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nInspecting progress file".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("commentary agent message");
        assert_eq!(commentary_blocks, vec!["@15s: Inspecting".to_string()]);
    }

    #[test]
    fn minimal_stream_recovery_retry_discards_inflight_commentary_delta() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));

        let delta_blocks = renderer
            .handle_event(&EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "Inspecting progress file".to_string(),
            }))
            .expect("agent delta");
        assert!(delta_blocks.is_empty());

        let retry_blocks = renderer
            .handle_event(&EventMsg::PotterStreamRecoveryUpdate {
                attempt: 1,
                max_attempts: 10,
                error_message: "stream disconnected".to_string(),
            })
            .expect("stream recovery update");
        assert_eq!(retry_blocks.len(), 1);
        assert!(
            retry_blocks[0].contains("CodexPotter: retry 1/10"),
            "expected retry block to remain visible: {retry_blocks:?}"
        );
        assert!(
            !retry_blocks[0].contains("Inspecting progress file"),
            "expected retry barrier to discard in-flight commentary delta: {retry_blocks:?}"
        );

        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(21));
        let commentary_blocks = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nInspecting progress file".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("commentary agent message");
        assert_eq!(commentary_blocks, vec!["@21s: Inspecting".to_string()]);
    }

    #[test]
    fn minimal_commentary_turn_uses_turn_complete_last_agent_message_as_final() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));
        renderer.status_started_at = Some(Instant::now() - Duration::from_secs(15));

        let commentary_blocks = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "**Inspecting**\n\nWorking...".to_string(),
                    phase: Some(MessagePhase::Commentary),
                },
            ))
            .expect("commentary agent message");
        assert_eq!(commentary_blocks, vec!["@15s: Inspecting".to_string()]);

        let final_blocks = renderer
            .handle_event(&EventMsg::TurnComplete(
                codex_protocol::protocol::TurnCompleteEvent {
                    turn_id: "turn-1".to_string(),
                    last_agent_message: Some("final".to_string()),
                },
            ))
            .expect("turn complete");
        assert_eq!(final_blocks, vec!["final".to_string()]);
    }

    #[test]
    fn minimal_idle_flush_makes_pending_agent_message_visible_without_duplication() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let blocks = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "latest".to_string(),
                    phase: None,
                },
            ))
            .expect("agent message");
        assert!(blocks.is_empty());
        assert!(renderer.needs_idle_agent_message_flush());

        let idle_block = renderer
            .flush_idle_agent_message()
            .expect("idle flush")
            .expect("idle block");
        assert_eq!(idle_block, "latest");
        assert!(
            renderer
                .flush_idle_agent_message()
                .expect("second idle flush")
                .is_none()
        );

        let blocks = renderer
            .handle_event(&EventMsg::TurnComplete(
                codex_protocol::protocol::TurnCompleteEvent {
                    turn_id: "turn-1".to_string(),
                    last_agent_message: None,
                },
            ))
            .expect("turn complete");
        assert!(blocks.is_empty());
    }

    #[test]
    fn minimal_new_agent_stream_flushes_previous_pending_message() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let blocks = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "previous".to_string(),
                    phase: None,
                },
            ))
            .expect("agent message");
        assert!(blocks.is_empty());

        let blocks = renderer
            .handle_event(&EventMsg::AgentMessageDelta(
                codex_protocol::protocol::AgentMessageDeltaEvent {
                    delta: "next".to_string(),
                },
            ))
            .expect("agent delta");
        assert_eq!(blocks, vec!["previous".to_string()]);
    }

    #[test]
    fn search_and_image_events_visibility_depends_on_verbosity() {
        let repo = synthetic_absolute_path(&["repo"]);

        {
            let mut renderer = ExecHumanRenderer::new(Verbosity::Simple, Some(120), false);
            renderer.cwd = repo.clone();

            let search_blocks = renderer
                .handle_event(&EventMsg::WebSearchEnd(WebSearchEndEvent {
                    call_id: "search-1".to_string(),
                    query: "rust fmt".to_string(),
                }))
                .expect("search event");
            assert_eq!(search_blocks, vec!["Searched\n  rust fmt".to_string()]);

            let image_blocks = renderer
                .handle_event(&EventMsg::ViewImageToolCall(ViewImageToolCallEvent {
                    call_id: "image-1".to_string(),
                    path: repo.join("screenshot.png"),
                }))
                .expect("image event");
            assert_eq!(
                image_blocks,
                vec!["Viewed Image\n  screenshot.png".to_string()]
            );
        }

        {
            let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
            renderer.cwd = repo.clone();

            let search_blocks = renderer
                .handle_event(&EventMsg::WebSearchEnd(WebSearchEndEvent {
                    call_id: "search-1".to_string(),
                    query: "rust fmt".to_string(),
                }))
                .expect("search event");
            assert!(search_blocks.is_empty());

            let image_blocks = renderer
                .handle_event(&EventMsg::ViewImageToolCall(ViewImageToolCallEvent {
                    call_id: "image-1".to_string(),
                    path: repo.join("screenshot.png"),
                }))
                .expect("image event");
            assert!(image_blocks.is_empty());
        }
    }

    #[test]
    fn simple_mode_inserts_worked_separator_before_follow_up_agent_message() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Simple, Some(80), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
        }));

        let command_blocks = renderer
            .handle_event(&EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "cmd-1".to_string(),
                turn_id: "turn-1".to_string(),
                command: vec!["bash".to_string(), "-lc".to_string(), "true".to_string()],
                cwd: PathBuf::from("/repo"),
                aggregated_output: String::new(),
                parsed_cmd: Vec::new(),
                exit_code: 0,
                duration: Duration::from_secs(1),
                formatted_output: String::new(),
                stdout: String::new(),
                stderr: String::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                process_id: None,
            }))
            .expect("exec command");
        assert_eq!(command_blocks.len(), 1);

        let blocks = renderer
            .handle_event(&EventMsg::AgentMessage(
                codex_protocol::protocol::AgentMessageEvent {
                    message: "done".to_string(),
                    phase: None,
                },
            ))
            .expect("agent message");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("Worked for "));
        assert_eq!(blocks[1], "done");
    }

    #[test]
    fn simple_mode_inserts_worked_separator_after_non_agent_cells() {
        {
            let mut renderer = ExecHumanRenderer::new(Verbosity::Simple, Some(80), false);
            let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".to_string(),
                model_context_window: None,
            }));

            let write_root = synthetic_absolute_path_buf(&["Users", "me", "project"]);
            let request_blocks = renderer
                .handle_event(&EventMsg::RequestPermissions(RequestPermissionsEvent {
                    call_id: "call-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    reason: Some("Select a workspace root".to_string()),
                    cwd: None,
                    permissions: RequestPermissionProfile {
                        network: Some(NetworkPermissions {
                            enabled: Some(true),
                        }),
                        file_system: Some(FileSystemPermissions {
                            read: None,
                            write: Some(vec![write_root]),
                        }),
                    },
                }))
                .expect("request permissions");
            assert_eq!(request_blocks.len(), 1);

            let blocks = renderer
                .handle_event(&EventMsg::AgentMessage(
                    codex_protocol::protocol::AgentMessageEvent {
                        message: "done".to_string(),
                        phase: None,
                    },
                ))
                .expect("agent message");
            assert_eq!(blocks.len(), 2);
            assert!(blocks[0].contains("Worked for "));
            assert_eq!(blocks[1], "done");
        }

        {
            let mut renderer = ExecHumanRenderer::new(Verbosity::Simple, Some(80), false);
            let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".to_string(),
                model_context_window: None,
            }));

            let guardian_blocks = renderer
                .handle_event(&EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                    id: "assessment-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    status: GuardianAssessmentStatus::Approved,
                    risk_score: Some(15),
                    risk_level: Some(GuardianRiskLevel::Low),
                    rationale: Some("Looks safe.".to_string()),
                    action: None,
                }))
                .expect("guardian assessment");
            assert_eq!(guardian_blocks.len(), 1);

            let blocks = renderer
                .handle_event(&EventMsg::AgentMessage(
                    codex_protocol::protocol::AgentMessageEvent {
                        message: "done".to_string(),
                        phase: None,
                    },
                ))
                .expect("agent message");
            assert_eq!(blocks.len(), 2);
            assert!(blocks[0].contains("Worked for "));
            assert_eq!(blocks[1], "done");
        }
    }

    #[test]
    fn reasoning_status_hint_uses_exec_timestamp_format_and_dims_output() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);

        let lines = renderer.build_reasoning_status_hint_lines_from_state(
            "Updating progress file".to_string(),
            Duration::from_secs(2650),
            Some(12),
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(
            line_to_plain_string(&lines[0]),
            "@44m 10s: Updating progress file (12% context left)"
        );
        assert_all_spans_dimmed(&lines);
    }

    #[test]
    fn reasoning_status_hint_only_emits_context_when_crossing_new_thresholds() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);

        let first = renderer.build_reasoning_status_hint_lines_from_state(
            "Researching advisory lock mechanism".to_string(),
            Duration::from_secs(15),
            Some(89),
        );
        assert_eq!(
            line_to_plain_string(&first[0]),
            "@15s: Researching advisory lock mechanism (89% context left)"
        );

        let second = renderer.build_reasoning_status_hint_lines_from_state(
            "Planning inventory tasks".to_string(),
            Duration::from_secs(25),
            Some(80),
        );
        assert_eq!(
            line_to_plain_string(&second[0]),
            "@25s: Planning inventory tasks"
        );

        let third = renderer.build_reasoning_status_hint_lines_from_state(
            "Searching for advisory locks".to_string(),
            Duration::from_secs(40),
            Some(79),
        );
        assert_eq!(
            line_to_plain_string(&third[0]),
            "@40s: Searching for advisory locks (79% context left)"
        );

        let fourth = renderer.build_reasoning_status_hint_lines_from_state(
            "Examining code files".to_string(),
            Duration::from_secs(45),
            Some(75),
        );
        assert_eq!(
            line_to_plain_string(&fourth[0]),
            "@45s: Examining code files"
        );
    }

    #[test]
    fn reasoning_status_hint_context_thresholds_reset_after_recovery() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);

        let _ = renderer.build_reasoning_status_hint_lines_from_state(
            "Evaluating project locks".to_string(),
            Duration::from_secs(15),
            Some(89),
        );
        let _ = renderer.build_reasoning_status_hint_lines_from_state(
            "Evaluating project locks".to_string(),
            Duration::from_secs(25),
            Some(79),
        );

        let recovered = renderer.build_reasoning_status_hint_lines_from_state(
            "Context compacted".to_string(),
            Duration::from_secs(30),
            Some(95),
        );
        assert_eq!(
            line_to_plain_string(&recovered[0]),
            "@30s: Context compacted"
        );

        let after_recovery = renderer.build_reasoning_status_hint_lines_from_state(
            "Evaluating project locks".to_string(),
            Duration::from_secs(35),
            Some(89),
        );
        assert_eq!(
            line_to_plain_string(&after_recovery[0]),
            "@35s: Evaluating project locks (89% context left)"
        );
    }

    #[test]
    fn reasoning_status_hint_emits_once_until_header_changes() {
        let mut renderer = ExecHumanRenderer::new(Verbosity::Minimal, Some(120), false);
        let _ = renderer.handle_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: Some(128_000),
        }));

        let first = renderer
            .handle_event(&EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                delta: "**Updating progress file**".to_string(),
            }))
            .expect("first reasoning delta");
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("Updating progress file"));

        let second = renderer
            .handle_event(&EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                delta: "\nMore detail without a new title".to_string(),
            }))
            .expect("second reasoning delta");
        assert!(second.is_empty());

        let third = renderer
            .handle_event(&EventMsg::AgentReasoning(AgentReasoningEvent {
                text: "**Updating progress file**\nDone.".to_string(),
            }))
            .expect("reasoning final");
        assert!(third.is_empty());
    }
}
