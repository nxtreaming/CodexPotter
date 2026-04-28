use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use ratatui::prelude::Widget;
use ratatui::text::Line;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tokio_stream::StreamExt;

use crate::AppExitInfo;
use crate::ExitReason;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::bottom_pane::ChatComposerDraft;
use crate::bottom_pane::InputResult;
use crate::bottom_pane::PromptFooterContext;
use crate::bottom_pane::PromptFooterOverride;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::ExecCell;
use crate::exec_cell::new_active_exec_command;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::external_editor_integration;
use crate::file_search::FileSearchManager;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::history_cell_potter::PotterStreamRecoveryRetryCell;
use crate::history_cell_potter::PotterStreamRecoveryUnrecoverableCell;
use crate::reasoning_status::ReasoningStatusTracker;
use crate::render::line_utils::dim_lines;
use crate::render::renderable::Renderable;
use crate::slash_command::SlashCommand;
use crate::streaming::chunking::AdaptiveChunkingPolicy;
use crate::streaming::commit_tick::CommitTickScope;
use crate::streaming::commit_tick::run_commit_tick;
use crate::streaming::controller::PlanStreamController;
use crate::streaming::controller::StreamController;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use crate::verbosity::Verbosity;

/// Prompt inserted by the `/compact-kb` slash command.
const COMPACT_KB_PROMPT: &str = "Cleanup and compact the knowledge base in .codexpotter, remove outdated or duplicated contents, remove unnecessary detailed steps or records, reorganize into a few domain specific topics.";

/// Auto-refresh cadence for the projects overlay (`Ctrl+L` / `/list`) while it is open.
///
/// This keeps the overlay in sync with on-disk progress changes without requiring the user to
/// manually reopen it.
const PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

fn render_runner_viewport(
    area: ratatui::layout::Rect,
    buf: &mut ratatui::buffer::Buffer,
    bottom_pane: &BottomPane,
    transient_lines: Vec<Line<'static>>,
) {
    let width = area.width;
    let pane_height = bottom_pane.desired_height(width).max(1).min(area.height);
    let pane_area = ratatui::layout::Rect::new(
        area.x,
        area.bottom().saturating_sub(pane_height),
        area.width,
        pane_height,
    );

    let transient_area_height = area.height.saturating_sub(pane_height);
    if transient_area_height > 0 && !transient_lines.is_empty() {
        let transient_area =
            ratatui::layout::Rect::new(area.x, area.y, area.width, transient_area_height);
        let overflow = transient_lines
            .len()
            .saturating_sub(usize::from(transient_area_height));
        let scroll_y = u16::try_from(overflow).unwrap_or(u16::MAX);
        ratatui::widgets::Paragraph::new(ratatui::text::Text::from(transient_lines))
            .scroll((scroll_y, 0))
            .render(transient_area, buf);
    }

    bottom_pane.render(pane_area, buf);
}

/// Placeholder text shown in the bottom-pane composer.
///
/// # Divergence (codex-potter)
///
/// Upstream Codex uses `Ask Codex to do anything`; CodexPotter uses `Assign new task to
/// CodexPotter`.
const PROMPT_PLACEHOLDER_TEXT: &str = "Assign new task to CodexPotter";

fn new_default_bottom_pane(
    tui: &Tui,
    app_event_tx: AppEventSender,
    animations_enabled: bool,
) -> BottomPane {
    BottomPane::new(BottomPaneParams {
        frame_requester: tui.frame_requester(),
        enhanced_keys_supported: tui.enhanced_keys_supported(),
        app_event_tx,
        animations_enabled,
        placeholder_text: PROMPT_PLACEHOLDER_TEXT.to_string(),
        disable_paste_burst: false,
    })
}

/// Parameters for rendering the prompt screen before the first user submission.
pub struct PromptScreenOptions {
    pub show_startup_banner: bool,
    pub check_for_update_on_startup: bool,
    pub startup_warnings: Vec<String>,
    pub startup_codex_model_config: Option<crate::codex_config::ResolvedCodexModelConfig>,
    pub composer_draft: Option<ChatComposerDraft>,
}

fn format_startup_banner_model_label(
    codex_model: &crate::codex_config::ResolvedCodexModelConfig,
) -> String {
    let mut model_label = match codex_model.reasoning_effort {
        Some(effort) => format!("{} {effort}", codex_model.model),
        None => codex_model.model.clone(),
    };
    if codex_model.is_fast {
        model_label.push_str(" [fast]");
    }
    model_label
}

fn build_prompt_screen_startup_banner_lines(
    width: u16,
    working_dir: &Path,
    startup_codex_model_config: Option<crate::codex_config::ResolvedCodexModelConfig>,
) -> std::io::Result<Vec<Line<'static>>> {
    let codex_model = match startup_codex_model_config {
        Some(codex_model) => codex_model,
        None => crate::codex_config::resolve_codex_model_config(working_dir)?,
    };

    Ok(crate::startup_banner::build_startup_banner_lines(
        width,
        crate::CODEX_POTTER_VERSION,
        &format_startup_banner_model_label(&codex_model),
        working_dir,
    ))
}

/// Prompt the user for a new task using the bottom-pane composer.
///
/// Returns `Ok(Some(prompt))` when the user submits a prompt. Returns `Ok(None)` when the prompt
/// is cancelled (for example, <kbd>Ctrl</kbd>+<kbd>C</kbd> on an empty composer) or when the event
/// stream ends unexpectedly.
pub async fn prompt_user_with_tui(
    tui: &mut Tui,
    options: PromptScreenOptions,
    verbosity: &mut Verbosity,
    prompt_footer: PromptFooterContext,
    projects_overlay_state: &mut crate::projects_overlay::ProjectsOverlay,
    projects_overlay_provider: Option<crate::ProjectsOverlayProviderChannels>,
) -> anyhow::Result<Option<String>> {
    let PromptScreenOptions {
        show_startup_banner,
        check_for_update_on_startup,
        startup_warnings,
        startup_codex_model_config,
        composer_draft,
    } = options;

    let (app_event_tx_raw, mut app_event_rx) = unbounded_channel::<AppEvent>();
    let app_event_tx = AppEventSender::new(app_event_tx_raw);

    let file_search_dir = prompt_footer.working_dir.clone();
    let file_search = FileSearchManager::new(file_search_dir.clone(), app_event_tx.clone());
    let prompt_history = crate::prompt_history_store::PromptHistoryStore::new();

    let mut bottom_pane = new_default_bottom_pane(tui, app_event_tx.clone(), true);
    bottom_pane.set_prompt_footer_context(prompt_footer);
    if let Some(draft) = composer_draft {
        bottom_pane.composer_mut().restore_draft(draft);
    }
    let (history_log_id, history_entry_count) = prompt_history.metadata();
    bottom_pane
        .composer_mut()
        .set_history_metadata(history_log_id, history_entry_count);

    let mut should_pad_prompt_viewport = !show_startup_banner;
    if show_startup_banner {
        let width = tui.terminal.last_known_screen_size.width.max(1);
        let banner_lines = build_prompt_screen_startup_banner_lines(
            width,
            &file_search_dir,
            startup_codex_model_config,
        )?;
        should_pad_prompt_viewport = should_pad_prompt_after_history_insert(&banner_lines);
        tui.insert_history_lines(banner_lines);

        if check_for_update_on_startup
            && let Some(latest_version) = crate::updates::get_upgrade_version()
        {
            let width = tui.terminal.last_known_screen_size.width.max(1);
            let lines = crate::history_cell::UpdateAvailableHistoryCell::new(
                latest_version,
                crate::update_action::get_update_action(),
            )
            .display_lines(width);
            if !lines.is_empty() {
                should_pad_prompt_viewport =
                    should_pad_prompt_viewport || should_pad_prompt_after_history_insert(&lines);
                tui.insert_history_lines(lines);
            }
        }
    }

    if !startup_warnings.is_empty() {
        let width = tui.terminal.last_known_screen_size.width.max(1);
        for warning in startup_warnings {
            let lines = history_cell::new_warning_event(warning).display_lines(width);
            if !lines.is_empty() {
                should_pad_prompt_viewport =
                    should_pad_prompt_viewport || should_pad_prompt_after_history_insert(&lines);
                tui.insert_history_lines(lines);
            }
        }
    }

    let mut app = RenderAppState::new_prompt_screen(
        app_event_tx,
        bottom_pane,
        prompt_history,
        file_search,
        should_pad_prompt_viewport,
        *verbosity,
    );
    let mut overlay_response_rx = app.restore_projects_overlay(
        std::mem::take(projects_overlay_state),
        projects_overlay_provider,
    );

    let result = app
        .run(
            tui,
            &mut app_event_rx,
            None,
            None,
            overlay_response_rx.as_mut(),
        )
        .await;

    let prompt_action = app.prompt_action.take();
    *verbosity = app.processor.verbosity;
    *projects_overlay_state = app.projects_overlay;
    let _ = result?;

    Ok(match prompt_action {
        Some(PromptScreenAction::Submitted(text)) => Some(text),
        Some(PromptScreenAction::CancelledByUser) | None => None,
    })
}

/// Handle an `Op::GetHistoryEntryRequest` by serving prompt history from the local store.
///
/// # Divergence (codex-potter)
///
/// `codex-potter` persists prompt history under `~/.codexpotter/history.jsonl` and answers history
/// lookups directly in the TUI runner (rather than forwarding to an upstream core/session store).
/// See `tui/src/prompt_history_store.rs` and `tui/AGENTS.md`.
fn handle_prompt_history_entry_request(
    frame_requester: crate::tui::FrameRequester,
    bottom_pane: &mut BottomPane,
    prompt_history: &crate::prompt_history_store::PromptHistoryStore,
    log_id: u64,
    offset: usize,
) {
    let entry = prompt_history.lookup_text(log_id, offset);
    if bottom_pane
        .composer_mut()
        .on_history_entry_response(log_id, offset, entry)
    {
        frame_requester.schedule_frame();
    }
}

fn restore_runtime_theme_from_codex_config(cwd: &Path) {
    let codex_home = crate::codex_config::find_codex_home().ok();
    let configured = crate::codex_config::resolve_codex_tui_theme(cwd)
        .ok()
        .flatten();

    let fallback_name = crate::render::highlight::adaptive_default_theme_name();
    let theme = configured
        .as_deref()
        .and_then(|name| {
            crate::render::highlight::resolve_theme_by_name(name, codex_home.as_deref())
        })
        .or_else(|| {
            crate::render::highlight::resolve_theme_by_name(fallback_name, codex_home.as_deref())
        });
    if let Some(theme) = theme {
        crate::render::highlight::set_syntax_theme(theme);
    }
}

fn should_pad_prompt_after_history_insert(lines: &[Line<'_>]) -> bool {
    let Some(last) = lines.last() else {
        return false;
    };

    !last
        .spans
        .iter()
        .all(|span| span.content.as_ref().trim().is_empty())
}

fn maybe_insert_history_cell_separator(
    cell: &Arc<dyn HistoryCell>,
    has_emitted_history_lines: &mut bool,
    display: &mut Vec<Line<'static>>,
) {
    if display.is_empty() || cell.is_stream_continuation() {
        return;
    }

    if *has_emitted_history_lines {
        display.insert(0, Line::from(""));
    } else {
        *has_emitted_history_lines = true;
    }
}

/// Controls how a single Potter round is rendered into the terminal transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundRenderOptions {
    /// When true, renders the user prompt into the transcript before sending it to the backend.
    pub render_user_prompt: bool,

    /// Optional status header prefix shown while a task is running (e.g. `Round 2/10`).
    ///
    /// This is primarily used by `codex-potter` to keep the working banner consistent when
    /// continuing an unfinished round (where `EventMsg::PotterRoundStarted` may be suppressed to
    /// avoid inserting a duplicate transcript marker).
    pub status_header_prefix: Option<String>,

    /// When true, inserts a blank line before the first emitted history cell in this round.
    ///
    /// This is useful when multiple rounds are rendered into the same terminal transcript while
    /// suppressing per-round user prompt rendering.
    pub pad_before_first_cell: bool,
}

impl Default for RoundRenderOptions {
    fn default() -> Self {
        Self {
            render_user_prompt: true,
            status_header_prefix: None,
            pad_before_first_cell: false,
        }
    }
}

/// Channels required to drive a single Potter round render loop.
pub struct RoundBackendChannels {
    /// Sends ops from the UI to the backend.
    pub codex_op_tx: UnboundedSender<Op>,
    /// Receives events streamed from the backend.
    pub codex_event_rx: UnboundedReceiver<Event>,
    /// Receives fatal errors from the control plane that should abort the round immediately.
    pub fatal_exit_rx: UnboundedReceiver<String>,
    /// Provider channels for the projects list overlay.
    pub projects_overlay_provider: Option<crate::ProjectsOverlayProviderChannels>,
}

/// Mutable UI state that must persist across rounds.
pub struct RoundUiState<'a> {
    /// Prompts queued while a task is running (collected from the bottom composer).
    pub queued_user_messages: &'a mut VecDeque<String>,
    /// Draft composer contents to restore when returning to a prompt screen.
    pub composer_draft: &'a mut Option<crate::bottom_pane::ChatComposerDraft>,
    /// Current transcript verbosity preference.
    pub verbosity: &'a mut Verbosity,
    /// Projects overlay UI state shared across rounds.
    pub projects_overlay_state: &'a mut crate::projects_overlay::ProjectsOverlay,
}

/// Context that must persist across rounds within a CodexPotter project.
pub struct ProjectRenderContext {
    pub project_started_at: Instant,
    pub prompt_footer: PromptFooterContext,
    pub potter_resume_command_global_args: Vec<String>,
}

fn text_user_input_op(text: String) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text,
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
    }
}

/// Run a single Potter round render loop and collect any queued user messages typed mid-round.
///
/// This function consumes backend events, updates the transcript, and drives the bottom composer.
/// Any prompts queued while the round is running are appended to `queued_user_messages`. The
/// current composer draft is written back to `composer_draft` so it can be restored by subsequent
/// prompt screens.
pub async fn run_round_with_tui_options_and_queue(
    tui: &mut Tui,
    prompt: String,
    options: RoundRenderOptions,
    context: ProjectRenderContext,
    backend: RoundBackendChannels,
    startup_warnings: Vec<String>,
    state: RoundUiState<'_>,
) -> anyhow::Result<AppExitInfo> {
    let ProjectRenderContext {
        project_started_at,
        prompt_footer,
        potter_resume_command_global_args,
    } = context;
    let RoundBackendChannels {
        codex_op_tx,
        mut codex_event_rx,
        mut fatal_exit_rx,
        projects_overlay_provider,
    } = backend;

    let (app_event_tx_raw, mut app_event_rx) = unbounded_channel::<AppEvent>();
    let app_event_tx = AppEventSender::new(app_event_tx_raw);

    for warning in startup_warnings {
        app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
            history_cell::new_warning_event(warning),
        )));
    }

    let file_search_dir = prompt_footer.working_dir.clone();
    let file_search = FileSearchManager::new(file_search_dir, app_event_tx.clone());
    let prompt_history = crate::prompt_history_store::PromptHistoryStore::new();

    let mut driver = AppServerEventProcessor::new(app_event_tx.clone(), *state.verbosity);
    driver.potter_resume_command_global_args = potter_resume_command_global_args;
    if options.render_user_prompt {
        driver.emit_user_prompt(prompt.clone());
    }

    codex_op_tx
        .send(text_user_input_op(prompt))
        .map_err(|err| anyhow::Error::msg(err.to_string()))?;

    let mut bottom_pane = new_default_bottom_pane(tui, app_event_tx.clone(), true);
    bottom_pane.set_prompt_footer_context(prompt_footer);
    bottom_pane.set_project_started_at(Some(project_started_at));
    bottom_pane.set_status_header_prefix(options.status_header_prefix.clone());
    if let Some(draft) = state.composer_draft.take() {
        bottom_pane.composer_mut().restore_draft(draft);
    }
    let (history_log_id, history_entry_count) = prompt_history.metadata();
    bottom_pane
        .composer_mut()
        .set_history_metadata(history_log_id, history_entry_count);
    let queued_user_messages_state = std::mem::take(state.queued_user_messages);
    let projects_overlay_state = std::mem::take(state.projects_overlay_state);
    let mut app = RenderAppState::new(
        driver,
        app_event_tx.clone(),
        Some(codex_op_tx),
        bottom_pane,
        prompt_history,
        file_search,
        queued_user_messages_state,
    );
    let mut overlay_response_rx =
        app.restore_projects_overlay(projects_overlay_state, projects_overlay_provider);
    app.has_emitted_history_lines = options.pad_before_first_cell;
    app.refresh_queued_user_messages();

    let result = app
        .run(
            tui,
            &mut app_event_rx,
            Some(&mut codex_event_rx),
            Some(&mut fatal_exit_rx),
            overlay_response_rx.as_mut(),
        )
        .await;
    *state.queued_user_messages = app.queued_user_messages;
    *state.composer_draft = app.bottom_pane.composer_mut().take_draft();
    *state.verbosity = app.processor.verbosity;
    *state.projects_overlay_state = app.projects_overlay;
    result
}

struct AppServerEventProcessor {
    app_event_tx: AppEventSender,
    stream: StreamController,
    plan_stream: Option<PlanStreamController>,
    adaptive_chunking: AdaptiveChunkingPolicy,
    token_usage: TokenUsage,
    context_usage: TokenUsage,
    model_context_window: Option<i64>,
    thread_id: Option<codex_protocol::ThreadId>,
    cwd: PathBuf,
    verbosity: Verbosity,
    last_rendered_width: Option<u16>,
    saw_agent_delta: bool,
    saw_plan_delta: bool,
    needs_final_message_separator: bool,
    had_work_activity: bool,
    last_separator_elapsed_secs: Option<u64>,
    current_elapsed_secs: Option<u64>,
    pending_exploring_cell: Option<ExecCell>,
    /// Divergence (codex-potter): coalesce consecutive successful non-shell `Ran` items into one
    /// history cell.
    pending_success_ran_cell: Option<ExecCell>,
    /// Divergence (codex-potter): coalesce consecutive `Viewed Image` items into one history
    /// cell when `Verbosity::Simple`, while suppressing them entirely in `Verbosity::Minimal`.
    pending_view_image_paths: Vec<PathBuf>,
    /// Divergence (codex-potter): coalesce consecutive `Searched` items into one history cell
    /// when `Verbosity::Simple`, while suppressing them entirely in `Verbosity::Minimal`.
    pending_web_search_queries: Vec<String>,
    /// Divergence (codex-potter): coalesce consecutive successful patch applications into one
    /// compact `Edited ...` summary when `Verbosity::Minimal`.
    pending_compact_patch_changes: Vec<HashMap<PathBuf, codex_protocol::protocol::FileChange>>,
    pending_compact_patch_preview: Option<history_cell::PlainHistoryCell>,
    /// Divergence (codex-potter): render the latest `phase = commentary` agent message as a
    /// transient dim preview in `Verbosity::Minimal` instead of pushing it into shimmer/status or
    /// transcript history. Ordinary tool/history output does not clear this preview; it stays
    /// visible until replaced by newer commentary, superseded by non-commentary agent output, or
    /// the turn ends.
    pending_minimal_commentary_message_lines: Option<Vec<Line<'static>>>,
    /// Divergence (codex-potter): keep the latest completed non-commentary agent message pending
    /// in `Verbosity::Minimal` until a transcript barrier (tool output / `TurnComplete`).
    pending_minimal_agent_message_lines: Option<Vec<Line<'static>>>,
    /// Tracks whether the current turn produced a non-commentary `AgentMessage`.
    ///
    /// This is used to decide whether `TurnComplete.last_agent_message` should be rendered as the
    /// final answer (compatibility with legacy providers/replay logs that do not emit a final
    /// agent message item).
    turn_has_non_commentary_agent_message: bool,
    /// Divergence (codex-potter): include the current process's incoming global flags in the
    /// `Loop more rounds:` resume command rendered by project summary blocks.
    potter_resume_command_global_args: Vec<String>,
    pending_potter_project_summary: Option<PendingPotterProjectSummary>,
    pending_potter_round_marker: Option<(u32, u32)>,
    pending_potter_round_session: Option<PendingPotterRoundSession>,
}

#[derive(Debug, Clone)]
struct PendingPotterRoundSession {
    model: String,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    service_tier: Option<codex_protocol::protocol::ServiceTier>,
}

#[derive(Debug)]
enum PendingPotterProjectSummaryOutcome {
    Succeeded,
    BudgetExhausted,
}

#[derive(Debug)]
struct PendingPotterProjectSummary {
    outcome: PendingPotterProjectSummaryOutcome,
    rounds: u32,
    duration: Duration,
    user_prompt_file: PathBuf,
    git_commit_start: String,
    git_commit_end: String,
}

impl AppServerEventProcessor {
    fn new(app_event_tx: AppEventSender, verbosity: Verbosity) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            app_event_tx,
            stream: StreamController::new(None, &cwd),
            plan_stream: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            token_usage: TokenUsage::default(),
            context_usage: TokenUsage::default(),
            model_context_window: None,
            thread_id: None,
            cwd,
            verbosity,
            last_rendered_width: None,
            saw_agent_delta: false,
            saw_plan_delta: false,
            needs_final_message_separator: false,
            had_work_activity: false,
            last_separator_elapsed_secs: None,
            current_elapsed_secs: None,
            pending_exploring_cell: None,
            pending_success_ran_cell: None,
            pending_view_image_paths: Vec::new(),
            pending_web_search_queries: Vec::new(),
            pending_compact_patch_changes: Vec::new(),
            pending_compact_patch_preview: None,
            pending_minimal_commentary_message_lines: None,
            pending_minimal_agent_message_lines: None,
            turn_has_non_commentary_agent_message: false,
            potter_resume_command_global_args: Vec::new(),
            pending_potter_project_summary: None,
            pending_potter_round_marker: None,
            pending_potter_round_session: None,
        }
    }

    fn maybe_emit_potter_round_marker(&mut self) {
        if self.pending_potter_round_marker.is_none() || self.pending_potter_round_session.is_none()
        {
            return;
        }

        let Some((current, total)) = self.pending_potter_round_marker.take() else {
            return;
        };
        let Some(session) = self.pending_potter_round_session.take() else {
            self.pending_potter_round_marker = Some((current, total));
            return;
        };

        self.emit_history_cell(Box::new(
            crate::history_cell_potter::new_potter_round_marker(
                current,
                total,
                &session.model,
                session.reasoning_effort,
                session.service_tier,
            ),
        ));
    }

    fn maybe_emit_potter_project_summary(&mut self) {
        let Some(done) = self.pending_potter_project_summary.take() else {
            return;
        };

        let PendingPotterProjectSummary {
            outcome,
            rounds,
            duration,
            user_prompt_file,
            git_commit_start,
            git_commit_end,
        } = done;

        let cell = match outcome {
            PendingPotterProjectSummaryOutcome::Succeeded => {
                crate::history_cell_potter::new_potter_project_succeeded(
                    rounds,
                    duration,
                    user_prompt_file,
                    git_commit_start,
                    git_commit_end,
                )
            }
            PendingPotterProjectSummaryOutcome::BudgetExhausted => {
                crate::history_cell_potter::new_potter_project_budget_exhausted(
                    rounds,
                    duration,
                    user_prompt_file,
                    git_commit_start,
                    git_commit_end,
                )
            }
        }
        .with_potter_resume_command_global_args(self.potter_resume_command_global_args.clone());

        self.emit_history_cell(Box::new(cell));
    }

    fn handle_retryable_stream_error(&mut self) {
        self.flush_pending_live_activity_cells();
        self.clear_pending_minimal_commentary();
        self.flush_pending_compact_patch_changes();
        self.drop_incomplete_minimal_agent_stream_or_flush();
        self.flush_plan_stream();
        self.adaptive_chunking.reset();
        self.app_event_tx.send(AppEvent::StopCommitAnimation);
        self.saw_plan_delta = false;
        self.needs_final_message_separator = true;
    }

    fn emit_user_prompt(&mut self, prompt: String) {
        self.emit_history_cell(Box::new(history_cell::new_user_prompt(prompt)));
    }

    /// Emit a committed transcript cell.
    ///
    /// This is the insertion seam for history cells: before emitting anything visible, flush any
    /// pending `Verbosity::Minimal` agent message / compact patch preview so
    /// transcript-suppressed protocol events cannot break coalescing.
    fn emit_history_cell(&mut self, cell: Box<dyn HistoryCell>) {
        self.flush_pending_minimal_agent_message();
        self.flush_pending_compact_patch_changes();
        self.app_event_tx.send(AppEvent::InsertHistoryCell(cell));
    }

    fn build_agent_message_lines(&self, message: &str) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        crate::markdown::append_markdown(message, None, Some(&self.cwd), &mut lines);
        lines
    }

    fn insert_agent_message_lines_direct(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        self.clear_pending_minimal_commentary();
        self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
            history_cell::AgentMessageCell::new(lines, true),
        )));
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

    fn clear_pending_minimal_commentary(&mut self) {
        self.pending_minimal_commentary_message_lines = None;
    }

    fn store_pending_minimal_commentary(&mut self, lines: Vec<Line<'static>>) {
        self.pending_minimal_commentary_message_lines = (!lines.is_empty()).then_some(lines);
    }

    fn flush_pending_minimal_agent_message(&mut self) {
        let Some(lines) = self.pending_minimal_agent_message_lines.take() else {
            return;
        };
        self.insert_agent_message_lines_direct(lines);
    }

    fn store_pending_minimal_agent_message(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        self.clear_pending_minimal_commentary();
        self.flush_pending_minimal_agent_message();
        self.pending_minimal_agent_message_lines = Some(lines);
    }

    /// Flush only completed Minimal-mode agent output at tool/history barriers.
    ///
    /// In Minimal mode, active `AgentMessageDelta` text has unknown phase until the completed
    /// `AgentMessage` arrives. Tool results must not commit that in-flight stream into transcript
    /// history, otherwise commentary deltas leak as normal agent messages.
    fn flush_barrier_agent_output(&mut self) {
        if self.verbosity == Verbosity::Minimal {
            self.flush_pending_minimal_agent_message();
        } else {
            self.flush_agent_output();
        }
    }

    /// Drop incomplete Minimal-mode agent streams at abnormal boundaries.
    ///
    /// A live `AgentMessageDelta` has no phase metadata. If the turn aborts or the stream errors
    /// before the matching completed `AgentMessage` arrives, the partial text is not trustworthy
    /// enough to commit into transcript history.
    fn drop_incomplete_minimal_agent_stream_or_flush(&mut self) {
        if self.verbosity == Verbosity::Minimal && self.saw_agent_delta {
            self.discard_streamed_agent_message_lines();
        } else {
            self.flush_agent_output();
        }
    }

    fn flush_agent_output(&mut self) {
        if self.verbosity == Verbosity::Minimal {
            if self.saw_agent_delta {
                let lines = self.take_streamed_agent_message_lines();
                self.insert_agent_message_lines_direct(lines);
            } else {
                self.flush_pending_minimal_agent_message();
            }
            return;
        }

        if self.saw_agent_delta {
            let lines = self.take_streamed_agent_message_lines();
            self.insert_agent_message_lines_direct(lines);
        }
    }

    /// Preserve any buffered live transcript content before an explicit exit / shutdown path.
    ///
    /// In `Verbosity::Minimal`, the latest agent message stays pending until a later visible event
    /// confirms whether it is truly final. Exit paths still need to flush that buffered content so
    /// the user does not lose in-flight output; the flushed agent message is inserted as a normal
    /// history cell so the transcript stays free of gray agent messages.
    fn flush_live_transcript_buffers(&mut self) {
        self.flush_pending_live_activity_cells();
        self.clear_pending_minimal_commentary();
        self.flush_agent_output();
        self.flush_plan_stream();
        self.flush_pending_compact_patch_changes();
    }

    fn should_buffer_agent_stream_until_completion(&self) -> bool {
        self.verbosity == Verbosity::Minimal && self.saw_agent_delta
    }

    fn on_commit_tick(&mut self) {
        self.run_commit_tick_with_scope(CommitTickScope::AnyMode);
    }

    fn run_commit_tick_with_scope(&mut self, scope: CommitTickScope) {
        let outcome = if self.should_buffer_agent_stream_until_completion() {
            run_commit_tick(
                &mut self.adaptive_chunking,
                None,
                self.plan_stream.as_mut(),
                scope,
                Instant::now(),
            )
        } else {
            run_commit_tick(
                &mut self.adaptive_chunking,
                Some(&mut self.stream),
                self.plan_stream.as_mut(),
                scope,
                Instant::now(),
            )
        };
        for cell in outcome.cells {
            self.emit_history_cell(cell);
        }

        if outcome.has_controller && outcome.all_idle {
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
        }
    }

    fn worked_elapsed_from(&mut self, current_elapsed: u64) -> u64 {
        let baseline = match self.last_separator_elapsed_secs {
            Some(last) if current_elapsed < last => 0,
            Some(last) => last,
            None => 0,
        };
        let elapsed = current_elapsed.saturating_sub(baseline);
        self.last_separator_elapsed_secs = Some(current_elapsed);
        elapsed
    }

    fn maybe_emit_final_message_separator(&mut self) {
        // In `Verbosity::Minimal` successful patch applications are buffered and rendered as a
        // coalesced compact summary. Flush here so work activity is committed to the transcript
        // before deciding whether to insert a final-message separator.
        self.flush_pending_compact_patch_changes();

        if self.needs_final_message_separator && self.had_work_activity {
            let elapsed_seconds = self
                .current_elapsed_secs
                .map(|current| self.worked_elapsed_from(current));

            if self.verbosity == Verbosity::Simple {
                self.emit_history_cell(Box::new(history_cell::FinalMessageSeparator::new(
                    elapsed_seconds,
                )));
            }

            self.needs_final_message_separator = false;
            self.had_work_activity = false;
        } else if self.needs_final_message_separator {
            // Reset the flag even if we don't show separator (no work was done)
            self.needs_final_message_separator = false;
        }
    }

    fn handle_codex_event(&mut self, event: Event) {
        match event.msg {
            EventMsg::SessionConfigured(cfg) => {
                self.thread_id = Some(cfg.session_id);
                self.cwd = cfg.cwd;
                self.stream = StreamController::new(None, &self.cwd);
                self.pending_potter_round_session = Some(PendingPotterRoundSession {
                    model: cfg.model,
                    reasoning_effort: cfg.reasoning_effort,
                    service_tier: cfg.service_tier,
                });
                if !self.pending_compact_patch_changes.is_empty() {
                    self.pending_compact_patch_preview =
                        Some(history_cell::new_coalesced_compact_patch_event(
                            &self.pending_compact_patch_changes,
                            &self.cwd,
                        ));
                }
                self.maybe_emit_potter_round_marker();
            }
            EventMsg::PotterProjectStarted {
                user_message,
                user_prompt_file,
                ..
            } => {
                self.flush_pending_live_activity_cells();
                if let Some(message) = user_message.filter(|message| !message.is_empty()) {
                    self.emit_user_prompt(message);
                }
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(
                    crate::history_cell_potter::new_potter_project_hint(user_prompt_file),
                ));
            }
            EventMsg::PotterRoundStarted { current, total } => {
                self.flush_pending_live_activity_cells();
                self.needs_final_message_separator = true;
                self.pending_potter_round_marker = Some((current, total));
                self.maybe_emit_potter_round_marker();
            }
            EventMsg::PotterProjectSucceeded {
                rounds,
                duration,
                user_prompt_file,
                git_commit_start,
                git_commit_end,
            } => {
                self.flush_pending_live_activity_cells();
                self.pending_potter_project_summary = Some(PendingPotterProjectSummary {
                    outcome: PendingPotterProjectSummaryOutcome::Succeeded,
                    rounds,
                    duration,
                    user_prompt_file,
                    git_commit_start,
                    git_commit_end,
                });
            }
            EventMsg::PotterProjectBudgetExhausted {
                rounds,
                duration,
                user_prompt_file,
                git_commit_start,
                git_commit_end,
            } => {
                self.flush_pending_live_activity_cells();
                self.pending_potter_project_summary = Some(PendingPotterProjectSummary {
                    outcome: PendingPotterProjectSummaryOutcome::BudgetExhausted,
                    rounds,
                    duration,
                    user_prompt_file,
                    git_commit_start,
                    git_commit_end,
                });
            }
            EventMsg::PotterRoundFinished { duration_secs, .. } => {
                self.flush_pending_live_activity_cells();
                self.clear_pending_minimal_commentary();
                if duration_secs > 0 {
                    self.emit_history_cell(Box::new(
                        crate::history_cell_potter::PotterRoundFinishedSeparator::new(
                            duration_secs,
                        ),
                    ));
                }
                self.pending_potter_round_marker = None;
                self.pending_potter_round_session = None;
                self.maybe_emit_potter_project_summary();
                self.flush_pending_compact_patch_changes();
            }
            EventMsg::TokenCount(ev) => {
                if let Some(info) = ev.info {
                    self.token_usage = info.total_token_usage;
                    self.context_usage = info.last_token_usage;
                    self.model_context_window =
                        info.model_context_window.or(self.model_context_window);
                }
            }
            EventMsg::TurnStarted(TurnStartedEvent {
                model_context_window,
                ..
            }) => {
                self.model_context_window = model_context_window;
                self.adaptive_chunking.reset();
                self.plan_stream = None;
                self.saw_agent_delta = false;
                self.saw_plan_delta = false;
                self.turn_has_non_commentary_agent_message = false;
            }
            EventMsg::AgentMessageDelta(ev) => {
                self.flush_pending_live_activity_cells();
                if self.verbosity == Verbosity::Minimal && !self.saw_agent_delta {
                    self.flush_pending_minimal_agent_message();
                }
                if !self.saw_agent_delta && self.verbosity != Verbosity::Minimal {
                    self.maybe_emit_final_message_separator();
                }
                self.saw_agent_delta = true;
                if self.stream.push(&ev.delta)
                    && !self.should_buffer_agent_stream_until_completion()
                {
                    self.app_event_tx.send(AppEvent::StartCommitAnimation);
                    self.run_commit_tick_with_scope(CommitTickScope::CatchUpOnly);
                }
            }
            EventMsg::PlanDelta(ev) => {
                self.flush_pending_live_activity_cells();
                if self.verbosity == Verbosity::Minimal {
                    self.discard_plan_output();
                    return;
                }
                if !self.saw_agent_delta && !self.saw_plan_delta {
                    self.maybe_emit_final_message_separator();
                }
                self.saw_plan_delta = true;

                if self.plan_stream.is_none() {
                    let width = self
                        .last_rendered_width
                        .map(|width| usize::from(width.saturating_sub(4)));
                    self.plan_stream = Some(PlanStreamController::new(width, &self.cwd));
                }
                if let Some(controller) = self.plan_stream.as_mut()
                    && controller.push(&ev.delta)
                {
                    self.app_event_tx.send(AppEvent::StartCommitAnimation);
                    self.run_commit_tick_with_scope(CommitTickScope::CatchUpOnly);
                }
            }
            EventMsg::AgentMessage(ev) => {
                self.flush_pending_live_activity_cells();
                if ev.phase != Some(MessagePhase::Commentary) {
                    self.turn_has_non_commentary_agent_message = true;
                }
                if self.verbosity == Verbosity::Minimal {
                    if ev.phase == Some(MessagePhase::Commentary) {
                        if self.saw_agent_delta {
                            self.discard_streamed_agent_message_lines();
                        }
                        self.store_pending_minimal_commentary(
                            self.build_agent_message_lines(&ev.message),
                        );
                        return;
                    }
                    let lines = self.take_agent_message_lines(&ev.message);
                    // In Minimal mode the delta phase is unknown until the completed
                    // `AgentMessage` arrives. Only completed non-commentary messages should act
                    // like a separator boundary; commentary must not split the compact patch
                    // preview into transcript history.
                    self.maybe_emit_final_message_separator();
                    self.store_pending_minimal_agent_message(lines);
                    return;
                }

                if self.saw_agent_delta {
                    let lines = self.take_agent_message_lines(&ev.message);
                    self.insert_agent_message_lines_direct(lines);
                    return;
                }

                self.maybe_emit_final_message_separator();
                self.emit_agent_message(&ev.message, false);
            }
            EventMsg::TurnComplete(ev) => {
                self.flush_pending_live_activity_cells();
                self.clear_pending_minimal_commentary();
                if let Some(message) = ev.last_agent_message.as_deref()
                    && !message.is_empty()
                    && !self.turn_has_non_commentary_agent_message
                {
                    self.pending_minimal_agent_message_lines = None;
                    if self.saw_agent_delta {
                        self.discard_streamed_agent_message_lines();
                    }
                    self.insert_agent_message_lines_direct(self.build_agent_message_lines(message));
                    self.turn_has_non_commentary_agent_message = true;
                } else {
                    self.flush_agent_output();
                }
                self.flush_plan_stream();
                self.app_event_tx.send(AppEvent::StopCommitAnimation);
                self.maybe_emit_potter_project_summary();
                self.flush_pending_compact_patch_changes();
            }
            EventMsg::TurnAborted(ev) => {
                self.flush_pending_live_activity_cells();
                self.clear_pending_minimal_commentary();
                self.drop_incomplete_minimal_agent_stream_or_flush();
                self.flush_plan_stream();
                self.app_event_tx.send(AppEvent::StopCommitAnimation);

                if ev.reason == codex_protocol::protocol::TurnAbortReason::Interrupted {
                    self.emit_history_cell(Box::new(history_cell::new_error_event(String::from(
                        "Conversation interrupted - tell the model what to do differently.",
                    ))));
                }
                self.flush_pending_compact_patch_changes();
            }
            EventMsg::Warning(ev) => {
                self.flush_pending_live_activity_cells();
                self.flush_barrier_agent_output();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_warning_event(ev.message)));
            }
            EventMsg::ContextCompacted(_) => {
                self.flush_pending_live_activity_cells();
                self.flush_barrier_agent_output();
                self.clear_pending_minimal_commentary();
                self.emit_agent_message("Context compacted", false);
            }
            EventMsg::DeprecationNotice(ev) => {
                self.flush_pending_live_activity_cells();
                self.flush_barrier_agent_output();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_deprecation_notice(
                    ev.summary, ev.details,
                )));
            }
            EventMsg::RequestPermissions(ev) => {
                // Align with upstream behavior: flush any newline-gated agent output before
                // rendering the tool result so ordering matches "agent explains -> tool runs -> agent continues".
                self.flush_barrier_agent_output();
                self.flush_plan_stream();
                self.flush_pending_live_activity_cells();
                self.flush_pending_compact_patch_changes();

                self.needs_final_message_separator = true;
                self.had_work_activity = true;
                self.emit_history_cell(Box::new(history_cell::new_request_permissions_event(ev)));
            }
            EventMsg::RequestUserInput(ev) => {
                self.flush_barrier_agent_output();
                self.flush_plan_stream();
                self.flush_pending_live_activity_cells();
                self.flush_pending_compact_patch_changes();

                self.needs_final_message_separator = true;
                self.had_work_activity = true;
                self.emit_history_cell(Box::new(history_cell::new_request_user_input_event(ev)));
            }
            EventMsg::ElicitationRequest(ev) => {
                self.flush_barrier_agent_output();
                self.flush_plan_stream();
                self.flush_pending_live_activity_cells();
                self.flush_pending_compact_patch_changes();

                self.needs_final_message_separator = true;
                self.had_work_activity = true;
                self.emit_history_cell(Box::new(history_cell::new_elicitation_request_event(ev)));
            }
            EventMsg::GuardianAssessment(ev) => {
                self.flush_barrier_agent_output();
                self.flush_plan_stream();
                self.flush_pending_live_activity_cells();
                self.flush_pending_compact_patch_changes();

                self.needs_final_message_separator = true;
                self.had_work_activity = true;
                self.emit_history_cell(Box::new(history_cell::new_guardian_assessment_event(ev)));
            }
            EventMsg::PlanUpdate(ev) => {
                self.flush_pending_live_activity_cells();
                if self.verbosity == Verbosity::Minimal {
                    self.discard_plan_output();
                    return;
                }
                self.flush_agent_output();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_plan_update(ev)));
            }
            EventMsg::WebSearchEnd(ev) => {
                if self.verbosity == Verbosity::Minimal {
                    return;
                }

                // Align with upstream behavior: flush any newline-gated agent output before
                // rendering the tool result so ordering matches "agent explains -> tool runs -> agent continues".
                self.flush_agent_output();
                self.flush_plan_stream();

                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.flush_pending_view_image_tool_calls();
                self.flush_pending_compact_patch_changes();

                self.pending_web_search_queries.push(ev.query);
                self.needs_final_message_separator = true;
                self.had_work_activity = true;
            }
            EventMsg::ViewImageToolCall(ev) => {
                if self.verbosity == Verbosity::Simple {
                    self.flush_agent_output();
                    self.flush_plan_stream();
                    self.flush_pending_exploring_cell();
                    self.flush_pending_success_ran_cell();
                    self.flush_pending_web_search_calls();
                    self.flush_pending_compact_patch_changes();
                    self.pending_view_image_paths.push(ev.path);
                    self.had_work_activity = true;
                }
            }
            EventMsg::ExecCommandEnd(ev) => {
                // Align with upstream Codex TUI behavior: flush any newline-gated agent output
                // before rendering the tool result, so transcript ordering matches the semantic
                // "agent explains -> tool runs -> agent continues" flow.
                if self.verbosity != Verbosity::Minimal {
                    self.flush_agent_output();
                    self.flush_plan_stream();
                }

                let aggregated_output = if !ev.aggregated_output.is_empty() {
                    ev.aggregated_output
                } else {
                    format!("{}{}", ev.stdout, ev.stderr)
                };

                let mut cell = new_active_exec_command(
                    ev.call_id.clone(),
                    ev.command,
                    ev.parsed_cmd,
                    ev.source,
                    ev.interaction_input,
                    false,
                );
                cell.complete_call(
                    &ev.call_id,
                    CommandOutput {
                        exit_code: ev.exit_code,
                        aggregated_output,
                        formatted_output: ev.formatted_output,
                    },
                    ev.duration,
                );

                if cell.is_exploring_cell() {
                    self.flush_pending_success_ran_cell();
                    self.flush_pending_web_search_calls();
                    self.flush_pending_view_image_tool_calls();
                    if let Some(pending) = self.pending_exploring_cell.as_mut() {
                        pending.calls.extend(cell.calls);
                    } else {
                        self.pending_exploring_cell = Some(cell);
                    }
                } else if Self::can_coalesce_success_ran_cell(&cell) {
                    self.flush_pending_exploring_cell();
                    self.flush_pending_web_search_calls();
                    self.flush_pending_view_image_tool_calls();
                    if let Some(pending) = self.pending_success_ran_cell.as_mut() {
                        pending.calls.extend(cell.calls);
                    } else {
                        self.pending_success_ran_cell = Some(cell);
                    }
                } else {
                    self.flush_pending_live_activity_cells();
                    self.needs_final_message_separator = true;
                    if self.verbosity != Verbosity::Minimal {
                        self.emit_history_cell(Box::new(cell));
                    }
                }
                self.had_work_activity = true;
            }
            EventMsg::PatchApplyEnd(ev) => {
                self.flush_pending_live_activity_cells();
                self.flush_barrier_agent_output();
                if ev.success && self.verbosity == Verbosity::Minimal {
                    self.pending_compact_patch_changes.push(ev.changes);
                    self.pending_compact_patch_preview =
                        Some(history_cell::new_coalesced_compact_patch_event(
                            &self.pending_compact_patch_changes,
                            &self.cwd,
                        ));
                } else {
                    self.needs_final_message_separator = true;
                    if ev.success {
                        self.emit_history_cell(Box::new(history_cell::new_patch_event(
                            ev.changes,
                            &self.cwd,
                            self.verbosity,
                        )));
                    } else {
                        self.emit_history_cell(Box::new(history_cell::new_patch_apply_failure(
                            ev.stderr,
                        )));
                    }
                }
                self.had_work_activity = true;
            }
            EventMsg::HookStarted(ev) => {
                // Align with upstream behavior: flush any newline-gated agent output before
                // rendering the tool result so ordering matches "agent explains -> tool runs -> agent continues".
                self.flush_barrier_agent_output();
                self.flush_plan_stream();
                self.flush_pending_live_activity_cells();
                self.flush_pending_compact_patch_changes();

                let label = ev.run.event_name.as_kebab_case();
                let mut message = format!("Running {label} hook");
                if let Some(status_message) = ev.run.status_message
                    && !status_message.is_empty()
                {
                    message.push_str(": ");
                    message.push_str(&status_message);
                }

                self.needs_final_message_separator = true;
                self.had_work_activity = true;
                self.emit_history_cell(Box::new(history_cell::new_info_event(message, None)));
            }
            EventMsg::HookCompleted(ev) => {
                self.flush_barrier_agent_output();
                self.flush_plan_stream();
                self.flush_pending_live_activity_cells();
                self.flush_pending_compact_patch_changes();

                let status = format!("{:?}", ev.run.status).to_lowercase();
                let header = format!("{} hook ({status})", ev.run.event_name.as_kebab_case());
                let mut lines: Vec<Line<'static>> = vec![header.into()];
                for entry in ev.run.entries {
                    let prefix = match entry.kind {
                        codex_protocol::protocol::HookOutputEntryKind::Warning => "warning: ",
                        codex_protocol::protocol::HookOutputEntryKind::Stop => "stop: ",
                        codex_protocol::protocol::HookOutputEntryKind::Feedback => "feedback: ",
                        codex_protocol::protocol::HookOutputEntryKind::Context => "hook context: ",
                        codex_protocol::protocol::HookOutputEntryKind::Error => "error: ",
                    };
                    lines.push(format!("  {prefix}{}", entry.text).into());
                }

                self.needs_final_message_separator = true;
                self.had_work_activity = true;
                self.emit_history_cell(Box::new(history_cell::PlainHistoryCell::new(lines)));
            }
            EventMsg::CollabAgentSpawnBegin(_) => {}
            EventMsg::CollabAgentSpawnEnd(ev) => {
                self.on_collab_event(crate::multi_agents::spawn_end(ev))
            }
            EventMsg::CollabAgentInteractionBegin(_) => {}
            EventMsg::CollabAgentInteractionEnd(ev) => {
                self.on_collab_event(crate::multi_agents::interaction_end(ev));
            }
            EventMsg::CollabWaitingBegin(ev) => {
                self.on_collab_event(crate::multi_agents::waiting_begin(ev))
            }
            EventMsg::CollabWaitingEnd(ev) => {
                self.on_collab_event(crate::multi_agents::waiting_end(ev))
            }
            EventMsg::CollabCloseBegin(_) => {}
            EventMsg::CollabCloseEnd(ev) => {
                self.on_collab_event(crate::multi_agents::close_end(ev))
            }
            EventMsg::CollabResumeBegin(ev) => {
                self.on_collab_event(crate::multi_agents::resume_begin(ev))
            }
            EventMsg::CollabResumeEnd(ev) => {
                self.on_collab_event(crate::multi_agents::resume_end(ev))
            }
            EventMsg::Error(ev) => {
                self.flush_pending_live_activity_cells();
                self.drop_incomplete_minimal_agent_stream_or_flush();
                self.flush_plan_stream();
                self.app_event_tx.send(AppEvent::StopCommitAnimation);
                self.saw_plan_delta = false;
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_error_event(ev.message)));
            }
            _ => {}
        }
    }

    fn on_collab_event(&mut self, cell: history_cell::PlainHistoryCell) {
        // Align with upstream ordering: flush completed agent output before inserting the collab
        // transcript cell, but keep Minimal-mode in-flight deltas buffered until their completed
        // `AgentMessage` reveals the phase.
        self.flush_barrier_agent_output();
        self.flush_plan_stream();
        self.flush_pending_live_activity_cells();
        self.needs_final_message_separator = true;
        self.emit_history_cell(Box::new(cell));
        self.had_work_activity = true;
    }

    fn flush_pending_live_activity_cells(&mut self) {
        self.flush_pending_exploring_cell();
        self.flush_pending_success_ran_cell();
        self.flush_pending_web_search_calls();
        self.flush_pending_view_image_tool_calls();
    }

    fn flush_pending_exploring_cell(&mut self) {
        let Some(cell) = self.pending_exploring_cell.take() else {
            return;
        };
        self.needs_final_message_separator = true;
        if self.verbosity == Verbosity::Minimal {
            return;
        }
        self.emit_history_cell(Box::new(cell));
    }

    fn flush_pending_view_image_tool_calls(&mut self) {
        if self.pending_view_image_paths.is_empty() {
            return;
        }

        let paths = std::mem::take(&mut self.pending_view_image_paths);
        if self.verbosity == Verbosity::Minimal {
            return;
        }

        self.needs_final_message_separator = true;
        self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
            history_cell::new_view_image_tool_calls(&paths, &self.cwd),
        )));
    }

    fn flush_pending_web_search_calls(&mut self) {
        if self.pending_web_search_queries.is_empty() {
            return;
        }

        let queries = std::mem::take(&mut self.pending_web_search_queries);
        if self.verbosity == Verbosity::Minimal {
            return;
        }

        self.needs_final_message_separator = true;
        self.emit_history_cell(Box::new(history_cell::new_web_search_tool_calls(queries)));
    }

    fn flush_pending_compact_patch_changes(&mut self) {
        if self.pending_compact_patch_changes.is_empty() {
            return;
        }
        self.needs_final_message_separator = true;
        let cell = self
            .pending_compact_patch_preview
            .take()
            .unwrap_or_else(|| {
                history_cell::new_coalesced_compact_patch_event(
                    &self.pending_compact_patch_changes,
                    &self.cwd,
                )
            });
        self.pending_compact_patch_changes.clear();
        // Emit directly to avoid recursively flushing through `emit_history_cell`.
        self.app_event_tx
            .send(AppEvent::InsertHistoryCell(Box::new(cell)));
    }

    fn flush_pending_success_ran_cell(&mut self) {
        let Some(cell) = self.pending_success_ran_cell.take() else {
            return;
        };
        self.needs_final_message_separator = true;
        if self.verbosity == Verbosity::Minimal {
            return;
        }
        self.emit_history_cell(Box::new(cell));
    }

    /// Drop plan tool transcript state that `Verbosity::Minimal` intentionally suppresses.
    ///
    /// # Divergence (codex-potter)
    ///
    /// Upstream Codex keeps rendering plan tool output. CodexPotter hides both streamed
    /// `PlanDelta` (`Proposed Plan`) and committed `PlanUpdate` (`Updated Plan`) in
    /// `Verbosity::Minimal`.
    fn discard_plan_output(&mut self) {
        self.plan_stream = None;
        self.saw_plan_delta = false;
    }

    fn flush_plan_stream(&mut self) {
        if self.verbosity == Verbosity::Minimal {
            self.discard_plan_output();
            return;
        }

        let Some(mut controller) = self.plan_stream.take() else {
            return;
        };
        if let Some(cell) = controller.finalize() {
            self.emit_history_cell(cell);
        }
    }

    fn can_coalesce_success_ran_cell(cell: &ExecCell) -> bool {
        let [call] = cell.calls.as_slice() else {
            return false;
        };

        call.output
            .as_ref()
            .is_some_and(|output| output.exit_code == 0)
            && !call.is_user_shell_command()
            && !call.is_unified_exec_interaction()
    }

    fn emit_agent_message(&mut self, message: &str, dim: bool) {
        let mut lines = self.build_agent_message_lines(message);
        if lines.is_empty() {
            return;
        }
        if dim {
            dim_lines(&mut lines);
        }
        self.emit_history_cell(Box::new(history_cell::AgentMessageCell::new(lines, true)));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptScreenAction {
    Submitted(String),
    CancelledByUser,
}

fn is_control_char(key_event: &crossterm::event::KeyEvent, expected: char) -> bool {
    key_event
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && matches!(
            key_event.code,
            crossterm::event::KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected)
        )
}

struct RenderAppState {
    prompt_action: Option<PromptScreenAction>,
    should_pad_prompt_viewport: bool,
    processor: AppServerEventProcessor,
    app_event_tx: AppEventSender,
    codex_op_tx: Option<UnboundedSender<Op>>,
    projects_overlay_request_tx: Option<UnboundedSender<crate::ProjectsOverlayRequest>>,
    projects_overlay: crate::projects_overlay::ProjectsOverlay,
    projects_overlay_next_auto_refresh_at: Option<Instant>,
    /// True when the projects overlay has switched the TUI into alt-screen mode.
    ///
    /// This keeps the wheel from scrolling the terminal scrollback while the overlay is open,
    /// and ensures any transcript output is deferred until we return to the inline viewport.
    projects_overlay_alt_screen_active: bool,
    bottom_pane: BottomPane,
    prompt_history: crate::prompt_history_store::PromptHistoryStore,
    file_search: FileSearchManager,
    queued_user_messages: VecDeque<String>,
    reasoning_status: ReasoningStatusTracker,
    unified_exec_processes: Vec<UnifiedExecProcessSummary>,
    unified_exec_wait: Option<UnifiedExecWaitStatus>,
    stream_error_status_header: Option<String>,
    potter_stream_recovery_retry_cell: Option<PotterStreamRecoveryRetryCell>,
    commit_anim_running: Arc<AtomicBool>,
    has_emitted_history_lines: bool,
    exit_after_next_draw: bool,
    exit_requested_by_user: bool,
    exit_reason: ExitReason,
}

#[derive(Debug, Clone)]
struct UnifiedExecProcessSummary {
    key: String,
    call_id: String,
    command_display: String,
    recent_chunks: Vec<String>,
}

#[derive(Debug, Clone)]
struct UnifiedExecWaitStatus {
    process_id: String,
    previous_header: String,
}

impl RenderAppState {
    fn new(
        processor: AppServerEventProcessor,
        app_event_tx: AppEventSender,
        codex_op_tx: Option<UnboundedSender<Op>>,
        bottom_pane: BottomPane,
        prompt_history: crate::prompt_history_store::PromptHistoryStore,
        file_search: FileSearchManager,
        queued_user_messages: VecDeque<String>,
    ) -> Self {
        Self {
            prompt_action: None,
            should_pad_prompt_viewport: false,
            processor,
            app_event_tx,
            codex_op_tx,
            projects_overlay_request_tx: None,
            projects_overlay: crate::projects_overlay::ProjectsOverlay::default(),
            projects_overlay_next_auto_refresh_at: None,
            projects_overlay_alt_screen_active: false,
            bottom_pane,
            prompt_history,
            file_search,
            queued_user_messages,
            reasoning_status: ReasoningStatusTracker::new(),
            unified_exec_processes: Vec::new(),
            unified_exec_wait: None,
            stream_error_status_header: None,
            potter_stream_recovery_retry_cell: None,
            commit_anim_running: Arc::new(AtomicBool::new(false)),
            has_emitted_history_lines: false,
            exit_after_next_draw: false,
            exit_requested_by_user: false,
            exit_reason: ExitReason::UserRequested,
        }
    }

    fn new_prompt_screen(
        app_event_tx: AppEventSender,
        bottom_pane: BottomPane,
        prompt_history: crate::prompt_history_store::PromptHistoryStore,
        file_search: FileSearchManager,
        should_pad_prompt_viewport: bool,
        verbosity: Verbosity,
    ) -> Self {
        let processor = AppServerEventProcessor::new(app_event_tx.clone(), verbosity);
        let mut app = Self::new(
            processor,
            app_event_tx,
            None,
            bottom_pane,
            prompt_history,
            file_search,
            VecDeque::new(),
        );
        app.should_pad_prompt_viewport = should_pad_prompt_viewport;
        app
    }

    fn build_transient_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.codex_op_tx.is_none() {
            return if self.should_pad_prompt_viewport {
                vec![Line::from("")]
            } else {
                Vec::new()
            };
        }

        let mut transient_lines: Vec<Line<'static>> = Vec::new();

        if self.processor.verbosity == Verbosity::Simple {
            if let Some(cell) = self.processor.pending_success_ran_cell.as_ref() {
                transient_lines.push(Line::from(""));
                transient_lines.extend(cell.display_lines(width));
            }

            if let Some(cell) = self.processor.pending_exploring_cell.as_ref() {
                // Keep a blank line between the transcript (which may include a background-colored
                // user prompt cell) and the live explored block.
                transient_lines.push(Line::from(""));
                transient_lines.extend(cell.display_lines(width));
            }
        }

        if self.processor.verbosity == Verbosity::Simple
            && !self.processor.pending_web_search_queries.is_empty()
        {
            transient_lines.push(Line::from(""));
            transient_lines.extend(
                history_cell::new_web_search_tool_calls(
                    self.processor.pending_web_search_queries.clone(),
                )
                .display_lines(width),
            );
        }

        if self.processor.verbosity == Verbosity::Simple
            && !self.processor.pending_view_image_paths.is_empty()
        {
            transient_lines.push(Line::from(""));
            transient_lines.extend(
                history_cell::new_view_image_tool_calls(
                    &self.processor.pending_view_image_paths,
                    &self.processor.cwd,
                )
                .display_lines(width),
            );
        }

        if self.processor.verbosity == Verbosity::Minimal
            && let Some(cell) = self.processor.pending_compact_patch_preview.as_ref()
        {
            transient_lines.push(Line::from(""));
            transient_lines.extend(cell.display_lines(width));
        }

        if self.processor.verbosity == Verbosity::Minimal {
            let commentary_lines = self
                .processor
                .pending_minimal_commentary_message_lines
                .as_ref()
                .cloned();

            if let Some(mut commentary_lines) = commentary_lines {
                dim_lines(&mut commentary_lines);
                transient_lines.push(Line::from(""));
                transient_lines.extend(
                    history_cell::AgentMessageCell::new(commentary_lines, true)
                        .display_lines(width),
                );
            }

            let preview_lines = self
                .processor
                .pending_minimal_agent_message_lines
                .as_ref()
                .cloned();

            if let Some(mut preview_lines) = preview_lines {
                dim_lines(&mut preview_lines);
                transient_lines.push(Line::from(""));
                transient_lines.extend(
                    history_cell::AgentMessageCell::new(preview_lines, true).display_lines(width),
                );
            }
        }

        if let Some(cell) = self.potter_stream_recovery_retry_cell.as_ref() {
            transient_lines.push(Line::from(""));
            transient_lines.extend(cell.display_lines(width));
        }

        // When the bottom pane shrinks (e.g., after a turn completes and the status indicator is
        // removed), the prompt background can end up directly adjacent to the last transcript
        // line. Keep a blank line between the transcript and the bottom pane for readability.
        //
        // While a task is running, the status indicator already renders with padding that
        // separates it from the transcript; avoid adding redundant whitespace in that case.
        if transient_lines.is_empty()
            && self.has_emitted_history_lines
            && self.bottom_pane.status_widget().is_none()
        {
            transient_lines.push(Line::from(""));
        }

        transient_lines
    }

    async fn run(
        &mut self,
        tui: &mut Tui,
        app_event_rx: &mut UnboundedReceiver<AppEvent>,
        codex_event_rx: Option<&mut UnboundedReceiver<Event>>,
        fatal_exit_rx: Option<&mut UnboundedReceiver<String>>,
        projects_overlay_response_rx: Option<
            &mut UnboundedReceiver<crate::ProjectsOverlayResponse>,
        >,
    ) -> anyhow::Result<AppExitInfo> {
        let has_backend = self.codex_op_tx.is_some();
        anyhow::ensure!(
            has_backend == codex_event_rx.is_some() && has_backend == fatal_exit_rx.is_some(),
            "internal error: backend channels must be either all present or all absent",
        );
        anyhow::ensure!(
            self.projects_overlay_request_tx.is_some() == projects_overlay_response_rx.is_some(),
            "internal error: projects overlay backend channels must be either both present or both absent",
        );

        let mut tui_events = tui.event_stream();
        self.bottom_pane.set_task_running(has_backend);
        tui.frame_requester().schedule_frame();

        let mut codex_event_rx = codex_event_rx;
        let mut fatal_exit_rx = fatal_exit_rx;
        let mut projects_overlay_response_rx = projects_overlay_response_rx;

        loop {
            tokio::select! {
                maybe_event = tui_events.next() => {
                    let Some(event) = maybe_event else {
                        break;
                    };
                        match event {
                        TuiEvent::Draw => {
                            if self.bottom_pane.composer_mut().flush_paste_burst_if_due() {
                                // A paste just flushed; request an immediate redraw and skip this frame.
                                tui.frame_requester().schedule_frame();
                                continue;
                            }
                            if self.bottom_pane.composer().is_in_paste_burst() {
                                // While capturing a burst, schedule a follow-up tick and skip this frame
                                // to avoid redundant renders.
                                tui.frame_requester().schedule_frame_in(crate::bottom_pane::ChatComposer::recommended_paste_flush_delay());
                                continue;
                            }

                            self.maybe_send_projects_overlay_auto_refresh(
                                tui.frame_requester(),
                                Instant::now(),
                            );
                            // Drain any queued events before drawing so the rendered frame reflects
                            // the latest history inserts. This also avoids a race where a scheduled
                            // Draw event wins the select! before the final InsertHistoryCell events
                            // are processed, which would otherwise cause this runner to exit with
                            // missing output.
                            while let Ok(app_event) = app_event_rx.try_recv() {
                                self.handle_app_event(tui, app_event)?;
                            }
                            if let Some(rx) = codex_event_rx.as_mut() {
                                while let Ok(event) = rx.try_recv() {
                                    self.handle_app_event(tui, AppEvent::CodexEvent(event))?;
                                }
                            }
                            if let Some(rx) = projects_overlay_response_rx.as_mut() {
                                while let Ok(response) = rx.try_recv() {
                                    self.handle_projects_overlay_response(
                                        tui.frame_requester(),
                                        response,
                                    );
                                }
                            }
                            if let Some(rx) = fatal_exit_rx.as_mut() {
                                while let Ok(message) = rx.try_recv() {
                                    self.handle_app_event(
                                        tui,
                                        AppEvent::FatalExitRequest(message),
                                    )?;
                                }
                            }

                            // Drain any new app events produced by the codex events we just
                            // processed above before rendering the next frame.
                            while let Ok(app_event) = app_event_rx.try_recv() {
                                self.handle_app_event(tui, app_event)?;
                            }
                            self.draw(tui)?;
                            if self.exit_after_next_draw {
                                break;
                            }
                        }
                            TuiEvent::Key(key_event) => {
                                if external_editor_integration::is_ctrl_g(&key_event) {
                                    if key_event.kind == crossterm::event::KeyEventKind::Press {
                                        self.handle_external_editor(tui).await?;
                                    }
                                    continue;
                                }
                                let width = tui.terminal.last_known_screen_size.width.max(1);
                                self.handle_key_event(key_event, tui.frame_requester(), width);
                                if self.prompt_action.is_some() {
                                    break;
                                }
                            }
                        TuiEvent::Paste(pasted) => {
                            // Many terminals convert newlines to \r when pasting (e.g., iTerm2),
                            // but tui-textarea expects \n. Normalize CR to LF.
                            let pasted = pasted.replace("\r", "\n");
                            if self.bottom_pane.composer_mut().handle_paste(pasted) {
                                tui.frame_requester().schedule_frame();
                            }
                        }
                    }
                }
                maybe_app_event = app_event_rx.recv() => {
                    let Some(app_event) = maybe_app_event else {
                        break;
                    };
                    self.handle_app_event(tui, app_event)?;
                }
                maybe_codex_event = async {
                    if let Some(rx) = codex_event_rx.as_mut() {
                        rx.recv().await
                    } else {
                        None
                    }
                }, if has_backend => {
                    match maybe_codex_event {
                        Some(event) => {
                            self.handle_app_event(tui, AppEvent::CodexEvent(event))?;
                        }
                        None => {
                            if !self.exit_after_next_draw {
                                self.processor.flush_live_transcript_buffers();
                                self.exit_reason =
                                    ExitReason::Fatal("Backend disconnected".to_string());
                                self.exit_after_next_draw = true;
                                tui.frame_requester().schedule_frame();
                            }
                        }
                    }
                }
                maybe_fatal = async {
                    if let Some(rx) = fatal_exit_rx.as_mut() {
                        rx.recv().await
                    } else {
                        None
                    }
                }, if has_backend => {
                    let Some(message) = maybe_fatal else {
                        continue;
                    };
                    self.handle_app_event(tui, AppEvent::FatalExitRequest(message))?;
                }
                maybe_overlay_response = async {
                    if let Some(rx) = projects_overlay_response_rx.as_mut() {
                        rx.recv().await
                    } else {
                        None
                    }
                }, if projects_overlay_response_rx.is_some() => {
                    match maybe_overlay_response {
                        Some(response) => {
                            self.handle_projects_overlay_response(tui.frame_requester(), response);
                        }
                        None => {
                            self.handle_projects_overlay_provider_disconnected(
                                tui.frame_requester(),
                            );
                            projects_overlay_response_rx = None;
                        }
                    }
                }
            }
        }

        self.commit_anim_running.store(false, Ordering::Release);
        Ok(AppExitInfo {
            token_usage: self.processor.token_usage.clone(),
            thread_id: self.processor.thread_id,
            exit_reason: self.exit_reason.clone(),
        })
    }

    fn send_projects_overlay_request(&self, request: crate::ProjectsOverlayRequest) {
        let Some(tx) = self.projects_overlay_request_tx.as_ref() else {
            return;
        };
        let _ = tx.send(request);
    }

    fn restore_projects_overlay(
        &mut self,
        projects_overlay_state: crate::projects_overlay::ProjectsOverlay,
        projects_overlay_provider: Option<crate::ProjectsOverlayProviderChannels>,
    ) -> Option<UnboundedReceiver<crate::ProjectsOverlayResponse>> {
        self.projects_overlay = projects_overlay_state;
        let (overlay_request_tx, overlay_response_rx) = match projects_overlay_provider {
            Some(provider) => (Some(provider.request_tx), Some(provider.response_rx)),
            None => (None, None),
        };
        self.projects_overlay_request_tx = overlay_request_tx;
        if self.projects_overlay.is_open() && self.projects_overlay_request_tx.is_some() {
            let request = self.projects_overlay.open_or_refresh();
            self.send_projects_overlay_request(request);
        }
        overlay_response_rx
    }

    fn maybe_send_projects_overlay_auto_refresh(
        &mut self,
        frame_requester: crate::tui::FrameRequester,
        now: Instant,
    ) {
        if !self.projects_overlay.is_open() || self.projects_overlay_request_tx.is_none() {
            self.projects_overlay_next_auto_refresh_at = None;
            return;
        }

        let next_deadline = match self.projects_overlay_next_auto_refresh_at {
            Some(deadline) => deadline,
            None => {
                self.projects_overlay_next_auto_refresh_at =
                    Some(now + PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL);
                frame_requester.schedule_frame_in(PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL);
                return;
            }
        };

        if now < next_deadline {
            return;
        }

        let request = self.projects_overlay.open_or_refresh();
        self.send_projects_overlay_request(request);

        self.projects_overlay_next_auto_refresh_at =
            Some(now + PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL);
        frame_requester.schedule_frame_in(PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL);
    }

    fn handle_projects_overlay_response(
        &mut self,
        frame_requester: crate::tui::FrameRequester,
        response: crate::ProjectsOverlayResponse,
    ) {
        match response {
            crate::ProjectsOverlayResponse::List { projects, error } => {
                if let Some(request) = self.projects_overlay.on_projects_list(projects, error) {
                    self.send_projects_overlay_request(request);
                }
                frame_requester.schedule_frame();
            }
            crate::ProjectsOverlayResponse::Details { details } => {
                if self.projects_overlay.is_open() {
                    self.projects_overlay.on_project_details(details);
                    frame_requester.schedule_frame();
                }
            }
        }
    }

    fn handle_projects_overlay_provider_disconnected(
        &mut self,
        frame_requester: crate::tui::FrameRequester,
    ) {
        self.projects_overlay.close();
        self.projects_overlay_next_auto_refresh_at = None;
        self.processor
            .emit_history_cell(Box::new(history_cell::new_error_event(
                "Projects overlay provider disconnected".to_string(),
            )));
        self.projects_overlay_request_tx = None;
        frame_requester.schedule_frame();
    }

    async fn handle_external_editor(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        self.bottom_pane
            .set_prompt_footer_override(Some(PromptFooterOverride::ExternalEditorHint));
        self.draw(tui)?;

        match external_editor_integration::run_external_editor(tui, self.bottom_pane.composer())
            .await
        {
            Ok(Some(new_text)) => {
                self.bottom_pane
                    .composer_mut()
                    .apply_external_edit(new_text);
            }
            Ok(None) => {
                self.processor
                    .emit_history_cell(Box::new(history_cell::new_error_event(
                        external_editor_integration::MISSING_EDITOR_ERROR.to_string(),
                    )));
            }
            Err(err) => {
                self.processor
                    .emit_history_cell(Box::new(history_cell::new_error_event(format!(
                        "Failed to open editor: {err}",
                    ))));
            }
        }

        self.bottom_pane.set_prompt_footer_override(None);
        tui.frame_requester().schedule_frame();
        Ok(())
    }

    fn handle_key_event(
        &mut self,
        key_event: crossterm::event::KeyEvent,
        frame_requester: crate::tui::FrameRequester,
        terminal_width: u16,
    ) {
        if key_event.kind == crossterm::event::KeyEventKind::Release {
            return;
        }

        let is_press = key_event.kind == crossterm::event::KeyEventKind::Press;

        if self.projects_overlay.is_open() {
            if let Some(request) = self.projects_overlay.handle_key_event(key_event) {
                self.send_projects_overlay_request(request);
            }
            if !self.projects_overlay.is_open() {
                self.projects_overlay_next_auto_refresh_at = None;
            }
            frame_requester.schedule_frame();
            return;
        }

        if is_control_char(&key_event, 'l') && !self.bottom_pane.composer().popup_active() {
            if !is_press {
                return;
            }
            if self.projects_overlay_request_tx.is_none() {
                self.processor
                    .emit_history_cell(Box::new(history_cell::new_error_event(
                        "ctrl+l projects list is unavailable in this mode.".to_string(),
                    )));
                frame_requester.schedule_frame();
                return;
            }

            let request = self.projects_overlay.open_or_refresh();
            self.send_projects_overlay_request(request);
            self.projects_overlay_next_auto_refresh_at =
                Some(Instant::now() + PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL);
            frame_requester.schedule_frame_in(PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL);
            frame_requester.schedule_frame();
            return;
        }

        // Restore the last queued message into the composer for quick edits.
        if key_event.modifiers == crossterm::event::KeyModifiers::ALT
            && matches!(key_event.code, crossterm::event::KeyCode::Up)
            && !self.queued_user_messages.is_empty()
        {
            if !is_press {
                return;
            }
            if let Some(message) = self.queued_user_messages.pop_back() {
                self.bottom_pane.composer_mut().set_text_content(message);
                self.refresh_queued_user_messages();
                frame_requester.schedule_frame();
            }
            return;
        }

        if is_control_char(&key_event, 'c') {
            if !is_press {
                return;
            }
            if self.bottom_pane.composer().selection_popup_visible() {
                let (_result, needs_redraw) =
                    self.bottom_pane.composer_mut().handle_key_event(key_event);
                if needs_redraw {
                    frame_requester.schedule_frame();
                }
                return;
            }

            if !self.bottom_pane.composer().is_empty() {
                self.bottom_pane.composer_mut().clear_for_ctrl_c();
                frame_requester.schedule_frame();
                return;
            }

            if self.codex_op_tx.is_some() && self.bottom_pane.is_task_running() {
                if self.bottom_pane.composer().popup_active() {
                    let (_result, needs_redraw) = self.bottom_pane.composer_mut().handle_key_event(
                        crossterm::event::KeyEvent::new(
                            crossterm::event::KeyCode::Esc,
                            crossterm::event::KeyModifiers::NONE,
                        ),
                    );
                    if needs_redraw {
                        frame_requester.schedule_frame();
                    }
                } else {
                    self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));
                    frame_requester.schedule_frame();
                }
                return;
            }

            if self.codex_op_tx.is_none() {
                self.prompt_action = Some(PromptScreenAction::CancelledByUser);
            } else {
                // Preserve any live output in the transcript before clearing the inline
                // viewport on exit.
                self.processor.flush_live_transcript_buffers();

                self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));

                // Treat Ctrl+C as an explicit user cancellation, even if the turn just
                // finished, so callers can stop multi-round loops reliably.
                if !matches!(self.exit_reason, ExitReason::Fatal(_)) {
                    self.exit_reason = ExitReason::UserRequested;
                    self.exit_requested_by_user = true;
                }
                self.exit_after_next_draw = true;
            }
            frame_requester.schedule_frame();
            return;
        }

        if is_control_char(&key_event, 'd')
            && self.bottom_pane.composer().is_empty()
            && !self.bottom_pane.composer().popup_active()
        {
            if !is_press {
                return;
            }
            if self.codex_op_tx.is_none() {
                self.prompt_action = Some(PromptScreenAction::CancelledByUser);
            } else {
                // Preserve any live output in the transcript before clearing the inline
                // viewport on exit.
                self.processor.flush_live_transcript_buffers();

                self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));

                // Treat Ctrl+D as an explicit user cancellation, even if the turn just
                // finished, so callers can stop multi-round loops reliably.
                if !matches!(self.exit_reason, ExitReason::Fatal(_)) {
                    self.exit_reason = ExitReason::UserRequested;
                    self.exit_requested_by_user = true;
                }
                self.exit_after_next_draw = true;
            }
            frame_requester.schedule_frame();
            return;
        }

        if matches!(key_event.code, crossterm::event::KeyCode::Esc)
            && key_event.modifiers == crossterm::event::KeyModifiers::NONE
            && self.codex_op_tx.is_some()
            && self.bottom_pane.is_task_running()
            && !self.bottom_pane.composer().popup_active()
        {
            if !is_press {
                return;
            }

            self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));
            frame_requester.schedule_frame();
            return;
        }

        let (result, needs_redraw) = self.bottom_pane.composer_mut().handle_key_event(key_event);
        if needs_redraw {
            frame_requester.schedule_frame();
        }
        if self.bottom_pane.composer().is_in_paste_burst() {
            frame_requester.schedule_frame_in(
                crate::bottom_pane::ChatComposer::recommended_paste_flush_delay(),
            );
        }

        match result {
            InputResult::Submitted(text) | InputResult::Queued(text) => {
                let history_text = self
                    .bottom_pane
                    .composer()
                    .encode_prompt_history_text(&text);
                self.prompt_history.record_submission(&history_text);
                if self.codex_op_tx.is_none() {
                    self.prompt_action = Some(PromptScreenAction::Submitted(text));
                } else {
                    self.queued_user_messages.push_back(text);
                    self.refresh_queued_user_messages();
                    frame_requester.schedule_frame();
                }
            }
            InputResult::Command(cmd) => match cmd {
                SlashCommand::Mention => {
                    self.bottom_pane.composer_mut().insert_str("@");
                    frame_requester.schedule_frame();
                }
                SlashCommand::List => {
                    if self.projects_overlay_request_tx.is_none() {
                        self.processor
                            .emit_history_cell(Box::new(history_cell::new_error_event(
                                "'/list' is unavailable in this mode.".to_string(),
                            )));
                        frame_requester.schedule_frame();
                        return;
                    }

                    let request = self.projects_overlay.open_or_refresh();
                    self.send_projects_overlay_request(request);
                    self.projects_overlay_next_auto_refresh_at =
                        Some(Instant::now() + PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL);
                    frame_requester.schedule_frame_in(PROJECTS_OVERLAY_AUTO_REFRESH_INTERVAL);
                    frame_requester.schedule_frame();
                }
                SlashCommand::CompactKb => {
                    self.bottom_pane
                        .composer_mut()
                        .insert_str(COMPACT_KB_PROMPT);
                    frame_requester.schedule_frame();
                }
                SlashCommand::Yolo => {
                    let current_enabled = match crate::potter_config::load_potter_yolo_enabled() {
                        Ok(enabled) => enabled,
                        Err(err) => {
                            self.processor.emit_history_cell(Box::new(
                                history_cell::new_error_event(format!(
                                    "Failed to load YOLO default: {err}"
                                )),
                            ));
                            false
                        }
                    };
                    let params = crate::yolo_picker::build_yolo_picker_params(current_enabled);
                    self.bottom_pane.composer_mut().show_selection_view(params);
                    frame_requester.schedule_frame();
                }
                SlashCommand::PotterXModel => {
                    self.bottom_pane
                        .composer_mut()
                        .insert_str("/potter:xmodel ");
                    frame_requester.schedule_frame();
                }
                SlashCommand::Exit => {
                    if self.codex_op_tx.is_none() {
                        self.prompt_action = Some(PromptScreenAction::CancelledByUser);
                    } else {
                        // Preserve any live output in the transcript before clearing the inline
                        // viewport on exit.
                        self.processor.flush_live_transcript_buffers();

                        self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));

                        // Treat /exit as an explicit user cancellation, even if the turn just
                        // finished, so callers can stop multi-round loops reliably.
                        if !matches!(self.exit_reason, ExitReason::Fatal(_)) {
                            self.exit_reason = ExitReason::UserRequested;
                            self.exit_requested_by_user = true;
                        }
                        self.exit_after_next_draw = true;
                        frame_requester.schedule_frame();
                    }
                }
                SlashCommand::Theme => {
                    if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
                        let message = format!(
                            "'/{}' is disabled while a task is in progress.",
                            cmd.command()
                        );
                        self.processor
                            .emit_history_cell(Box::new(history_cell::new_error_event(message)));
                        frame_requester.schedule_frame();
                        return;
                    }

                    let codex_home = crate::codex_config::find_codex_home().ok();
                    let cwd = self.bottom_pane.prompt_working_dir();
                    let current_name = crate::codex_config::resolve_codex_tui_theme(cwd)
                        .ok()
                        .flatten();
                    let params = crate::theme_picker::build_theme_picker_params(
                        current_name.as_deref(),
                        codex_home.as_deref(),
                        Some(terminal_width),
                    );
                    self.bottom_pane.composer_mut().show_selection_view(params);
                    frame_requester.schedule_frame();
                }
                SlashCommand::Verbosity => {
                    let params = crate::verbosity_picker::build_verbosity_picker_params(
                        self.processor.verbosity,
                    );
                    self.bottom_pane.composer_mut().show_selection_view(params);
                    frame_requester.schedule_frame();
                }
                SlashCommand::Ps => {
                    let processes = self
                        .unified_exec_processes
                        .iter()
                        .map(|process| history_cell::UnifiedExecProcessDetails {
                            command_display: process.command_display.clone(),
                            recent_chunks: process.recent_chunks.clone(),
                        })
                        .collect();
                    self.processor.emit_history_cell(Box::new(
                        history_cell::new_unified_exec_processes_output(processes),
                    ));
                    frame_requester.schedule_frame();
                }
                SlashCommand::Stop => {
                    if self.codex_op_tx.is_some() {
                        self.app_event_tx
                            .send(AppEvent::CodexOp(Op::CleanBackgroundTerminals));
                    }

                    self.unified_exec_processes.clear();
                    self.sync_unified_exec_footer();
                    self.processor
                        .emit_history_cell(Box::new(history_cell::new_info_event(
                            "Stopping all background terminals.".to_string(),
                            /*hint*/ None,
                        )));
                    frame_requester.schedule_frame();
                }
            },
            InputResult::None => {}
        }
    }

    fn refresh_queued_user_messages(&mut self) {
        let messages: Vec<String> = self.queued_user_messages.iter().cloned().collect();
        self.bottom_pane.set_queued_user_messages(messages);
    }

    fn draw(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        if self.projects_overlay.is_open() {
            if !tui.is_alt_screen_active() {
                tui.enter_alt_screen()?;
            }
            self.projects_overlay_alt_screen_active = tui.is_alt_screen_active();
            tui.draw(u16::MAX, |frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());
                self.projects_overlay
                    .render(area, frame.buffer_mut(), SystemTime::now());
                // No input in the projects overlay; keep the terminal cursor hidden.
            })?;
            return Ok(());
        }

        if self.projects_overlay_alt_screen_active {
            tui.leave_alt_screen()?;
            self.projects_overlay_alt_screen_active = false;
        }

        let width = tui.terminal.last_known_screen_size.width;
        self.processor.last_rendered_width = Some(width);
        let pane_height = self.bottom_pane.desired_height(width).max(1);
        let transient_lines = self.build_transient_lines(width);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        tui.draw(viewport_height, |frame| {
            let area = frame.area();
            ratatui::widgets::Clear.render(area, frame.buffer_mut());
            render_runner_viewport(area, frame.buffer_mut(), &self.bottom_pane, transient_lines);

            let pane_height = self
                .bottom_pane
                .desired_height(area.width)
                .max(1)
                .min(area.height);
            let pane_area = ratatui::layout::Rect::new(
                area.x,
                area.bottom().saturating_sub(pane_height),
                area.width,
                pane_height,
            );
            let cursor = self
                .bottom_pane
                .cursor_pos(pane_area)
                .unwrap_or((area.x, area.bottom().saturating_sub(1)));
            frame.set_cursor_position(cursor);
        })?;
        Ok(())
    }

    fn apply_persisted_yolo_to_prompt_footer(&mut self, enabled: bool) {
        if self.codex_op_tx.is_some() {
            return;
        }

        let prompt_footer = self
            .bottom_pane
            .prompt_footer_context()
            .clone()
            .with_persisted_yolo_enabled(enabled);
        self.bottom_pane.set_prompt_footer_context(prompt_footer);
    }

    /// Build the one-shot transcript notice shown immediately after `/yolo` persists a new
    /// default. This intentionally does not surface on later startups; the prompt footer's
    /// `▲YOLO` indicator is the ongoing state signal for subsequent sessions.
    fn yolo_default_notice(enabled: bool) -> Box<dyn HistoryCell> {
        if enabled {
            Box::new(history_cell::new_warning_event(String::from(
                "YOLO is now persisted in config and will apply to all subsequent sessions.",
            )))
        } else {
            Box::new(history_cell::new_info_event(
                String::from("YOLO is disabled by default."),
                None,
            ))
        }
    }

    fn handle_app_event(&mut self, tui: &mut Tui, app_event: AppEvent) -> anyhow::Result<()> {
        match app_event {
            AppEvent::EmitHistoryCell(cell) => {
                self.processor.emit_history_cell(cell);
            }
            AppEvent::InsertHistoryCell(cell) => {
                let cell: Arc<dyn HistoryCell> = cell.into();
                let width = tui.terminal.last_known_screen_size.width;
                let mut display = cell.display_lines(width);
                if display.is_empty() {
                    return Ok(());
                }

                if self.codex_op_tx.is_none() {
                    self.should_pad_prompt_viewport = self.should_pad_prompt_viewport
                        || should_pad_prompt_after_history_insert(&display);
                }

                maybe_insert_history_cell_separator(
                    &cell,
                    &mut self.has_emitted_history_lines,
                    &mut display,
                );
                tui.insert_history_lines(display);
            }
            AppEvent::SyntaxThemeSelected { name } => {
                let cwd = self.bottom_pane.prompt_working_dir();
                match crate::codex_config::find_codex_home() {
                    Ok(codex_home) => {
                        match crate::codex_config::persist_codex_tui_theme(&codex_home, &name) {
                            Ok(()) => {
                                if let Some(theme) = crate::render::highlight::resolve_theme_by_name(
                                    &name,
                                    Some(&codex_home),
                                ) {
                                    crate::render::highlight::set_syntax_theme(theme);
                                }
                            }
                            Err(err) => {
                                restore_runtime_theme_from_codex_config(cwd);
                                self.processor.emit_history_cell(Box::new(
                                    history_cell::new_error_event(format!(
                                        "Failed to save theme: {err}"
                                    )),
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        restore_runtime_theme_from_codex_config(cwd);
                        self.processor
                            .emit_history_cell(Box::new(history_cell::new_error_event(format!(
                                "Failed to find CODEX_HOME: {err}"
                            ))));
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::VerbositySelected { verbosity } => {
                match crate::potter_config::persist_potter_tui_verbosity(verbosity) {
                    Ok(()) => {
                        self.processor.verbosity = verbosity;
                        if verbosity == Verbosity::Minimal {
                            self.processor.discard_plan_output();
                        }
                    }
                    Err(err) => {
                        self.processor
                            .emit_history_cell(Box::new(history_cell::new_error_event(format!(
                                "Failed to save verbosity: {err}"
                            ))));
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::YoloSelected { enabled } => {
                match crate::potter_config::persist_potter_yolo_enabled(enabled) {
                    Ok(()) => {
                        self.apply_persisted_yolo_to_prompt_footer(enabled);
                        self.processor
                            .emit_history_cell(Self::yolo_default_notice(enabled));
                    }
                    Err(err) => {
                        self.processor
                            .emit_history_cell(Box::new(history_cell::new_error_event(format!(
                                "Failed to save YOLO default: {err}"
                            ))));
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::StartCommitAnimation => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: StartCommitAnimation requires backend channels",
                );
                if self
                    .commit_anim_running
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let tx = self.app_event_tx.clone();
                    let running = self.commit_anim_running.clone();
                    thread::spawn(move || {
                        while running.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                            tx.send(AppEvent::CommitTick);
                        }
                    });
                }
            }
            AppEvent::StopCommitAnimation => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: StopCommitAnimation requires backend channels",
                );
                self.commit_anim_running.store(false, Ordering::Release);
            }
            AppEvent::CommitTick => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: CommitTick requires backend channels",
                );
                self.processor.on_commit_tick();
            }
            AppEvent::CodexEvent(event) => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: CodexEvent requires backend channels",
                );
                self.handle_codex_event(tui.frame_requester(), event)?;
            }
            AppEvent::CodexOp(op) => match op {
                Op::GetHistoryEntryRequest { offset, log_id } => {
                    handle_prompt_history_entry_request(
                        tui.frame_requester(),
                        &mut self.bottom_pane,
                        &self.prompt_history,
                        log_id,
                        offset,
                    );
                }
                _ => {
                    let Some(tx) = self.codex_op_tx.as_ref() else {
                        anyhow::bail!("internal error: unexpected {op:?} without backend channels");
                    };
                    let _ = tx.send(op);
                }
            },
            AppEvent::StartFileSearch(query) => {
                self.file_search.on_user_query(query);
            }
            AppEvent::FileSearchResult { query, matches } => {
                self.bottom_pane
                    .composer_mut()
                    .on_file_search_result(query, matches);
                tui.frame_requester().schedule_frame();
            }
            AppEvent::FatalExitRequest(message) => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: FatalExitRequest requires backend channels",
                );
                self.processor.flush_live_transcript_buffers();
                self.exit_reason = ExitReason::Fatal(message);
                self.bottom_pane.set_task_running(false);
                self.exit_after_next_draw = true;
                tui.frame_requester().schedule_frame();
            }
        }

        Ok(())
    }

    fn handle_codex_event(
        &mut self,
        frame_requester: crate::tui::FrameRequester,
        event: Event,
    ) -> anyhow::Result<()> {
        let Event { id, msg } = event;
        let event = Event { id, msg };

        match &event.msg {
            EventMsg::PotterStreamRecoveryUpdate {
                attempt,
                max_attempts,
                error_message,
            } => {
                self.potter_stream_recovery_retry_cell = Some(PotterStreamRecoveryRetryCell {
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    error_message: error_message.clone(),
                });

                self.processor.current_elapsed_secs = self
                    .bottom_pane
                    .status_widget()
                    .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
                self.processor.handle_retryable_stream_error();
                frame_requester.schedule_frame();
                return Ok(());
            }
            EventMsg::PotterStreamRecoveryRecovered => {
                self.potter_stream_recovery_retry_cell = None;
                frame_requester.schedule_frame();
                return Ok(());
            }
            EventMsg::PotterStreamRecoveryGaveUp {
                error_message,
                max_attempts,
                ..
            } => {
                self.potter_stream_recovery_retry_cell = None;
                self.processor.current_elapsed_secs = self
                    .bottom_pane
                    .status_widget()
                    .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
                self.processor.handle_retryable_stream_error();
                self.processor
                    .emit_history_cell(Box::new(PotterStreamRecoveryUnrecoverableCell {
                        max_attempts: *max_attempts,
                        error_message: error_message.clone(),
                    }));

                frame_requester.schedule_frame();
                return Ok(());
            }
            _ => {}
        }

        if let EventMsg::StreamError(ev) = &event.msg {
            if self.stream_error_status_header.is_none() {
                self.stream_error_status_header =
                    Some(self.bottom_pane.status_header().to_string());
            }
            self.bottom_pane.update_status_header_with_details(
                ev.message.clone(),
                ev.additional_details.clone(),
            );
            return Ok(());
        }
        if let Some(header) = self.stream_error_status_header.take() {
            self.bottom_pane.update_status_header(header);
        }

        match &event.msg {
            EventMsg::ExecCommandBegin(ev) => self.record_exec_command(ev),
            EventMsg::ExecCommandOutputDelta(ev) => {
                self.track_unified_exec_output_chunk(&ev.call_id, &ev.chunk);
            }
            EventMsg::TerminalInteraction(ev) => {
                self.show_unified_exec_wait(ev);
                return Ok(());
            }
            EventMsg::ExecCommandEnd(ev) => self.handle_exec_command_end_status(ev),
            EventMsg::BackgroundEvent(ev) => {
                self.restore_status_after_unified_exec_wait();
                self.bottom_pane.update_status_header(ev.message.clone());
                return Ok(());
            }
            EventMsg::TurnComplete(_)
            | EventMsg::TurnAborted(_)
            | EventMsg::PotterRoundFinished { .. } => {
                self.restore_status_after_unified_exec_wait();
            }
            _ => {}
        }

        match &event.msg {
            EventMsg::PotterRoundStarted { current, total } => {
                self.bottom_pane
                    .set_status_header_prefix(Some(format!("Round {current}/{total}")));
            }
            EventMsg::TurnStarted(_) => {
                self.reasoning_status.reset();
                self.unified_exec_wait = None;
                self.bottom_pane
                    .update_status_header(String::from("Working"));
            }
            EventMsg::AgentReasoningDelta(ev) => {
                if let Some(header) = self.reasoning_status.on_delta(&ev.delta)
                    && self.unified_exec_wait.is_none()
                {
                    self.bottom_pane.update_status_header(header);
                }
                return Ok(());
            }
            EventMsg::AgentReasoningRawContentDelta(ev) => {
                if let Some(header) = self.reasoning_status.on_delta(&ev.delta)
                    && self.unified_exec_wait.is_none()
                {
                    self.bottom_pane.update_status_header(header);
                }
                return Ok(());
            }
            EventMsg::AgentReasoningSectionBreak(_) => {
                self.reasoning_status.on_section_break();
                return Ok(());
            }
            EventMsg::AgentReasoning(ev) => {
                if let Some(header) = self.reasoning_status.on_delta(&ev.text)
                    && self.unified_exec_wait.is_none()
                {
                    self.bottom_pane.update_status_header(header);
                }
                self.reasoning_status.on_final();
                return Ok(());
            }
            EventMsg::AgentReasoningRawContent(ev) => {
                if let Some(header) = self.reasoning_status.on_delta(&ev.text)
                    && self.unified_exec_wait.is_none()
                {
                    self.bottom_pane.update_status_header(header);
                }
                self.reasoning_status.on_final();
                return Ok(());
            }
            _ => {}
        }

        if should_filter_thinking_event(&event.msg) {
            return Ok(());
        }

        let should_exit_on_round_end = matches!(&event.msg, EventMsg::PotterRoundFinished { .. });
        let should_stop_footer = match &event.msg {
            EventMsg::PotterRoundFinished { .. } => should_exit_on_round_end,
            _ => false,
        };
        let should_update_context = matches!(
            &event.msg,
            EventMsg::TokenCount(_) | EventMsg::TurnStarted(_)
        );
        let should_redraw_after_event = matches!(
            &event.msg,
            EventMsg::ExecCommandEnd(_) | EventMsg::PatchApplyEnd(_)
        );

        match &event.msg {
            EventMsg::PotterRoundFinished { outcome, .. } if should_exit_on_round_end => {
                let exit_reason = match outcome {
                    codex_protocol::protocol::PotterRoundOutcome::Completed => {
                        ExitReason::Completed
                    }
                    codex_protocol::protocol::PotterRoundOutcome::Interrupted => {
                        ExitReason::Interrupted
                    }
                    codex_protocol::protocol::PotterRoundOutcome::UserRequested => {
                        ExitReason::UserRequested
                    }
                    codex_protocol::protocol::PotterRoundOutcome::TaskFailed { message } => {
                        ExitReason::TaskFailed(message.clone())
                    }
                    codex_protocol::protocol::PotterRoundOutcome::Fatal { message } => {
                        ExitReason::Fatal(message.clone())
                    }
                };
                if !matches!(self.exit_reason, ExitReason::Fatal(_)) {
                    self.exit_reason = if self.exit_requested_by_user
                        && !matches!(exit_reason, ExitReason::Fatal(_))
                    {
                        ExitReason::UserRequested
                    } else {
                        exit_reason
                    };
                }
                self.exit_after_next_draw = true;
                frame_requester.schedule_frame();
            }
            _ => {}
        }

        self.processor.current_elapsed_secs = self
            .bottom_pane
            .status_widget()
            .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
        self.processor.handle_codex_event(event);
        if should_update_context {
            self.update_bottom_pane_context_window();
        }
        if should_redraw_after_event {
            frame_requester.schedule_frame();
        }
        if should_stop_footer {
            self.bottom_pane.set_task_running(false);
        }

        Ok(())
    }

    fn record_exec_command(&mut self, ev: &codex_protocol::protocol::ExecCommandBeginEvent) {
        if ev.source != codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup {
            return;
        }
        let key = ev.process_id.clone().unwrap_or_else(|| ev.call_id.clone());
        let command_display = strip_bash_lc_and_escape(&ev.command);
        if let Some(existing) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.key == key)
        {
            existing.call_id = ev.call_id.clone();
            existing.command_display = command_display.clone();
            existing.recent_chunks.clear();
        } else {
            self.unified_exec_processes.push(UnifiedExecProcessSummary {
                key: key.clone(),
                call_id: ev.call_id.clone(),
                command_display: command_display.clone(),
                recent_chunks: Vec::new(),
            });
        }
        self.sync_unified_exec_footer();

        if self
            .unified_exec_wait
            .as_ref()
            .is_some_and(|wait| wait.process_id == key)
        {
            self.bottom_pane.update_status_header_with_details_options(
                String::from("Waiting for background terminal"),
                Some(command_display),
                super::status_indicator_widget::StatusDetailsCapitalization::Preserve,
                /*max_lines*/ 1,
            );
        }
    }

    fn show_unified_exec_wait(&mut self, ev: &codex_protocol::protocol::TerminalInteractionEvent) {
        if !ev.stdin.is_empty() {
            return;
        }

        let previous_header = self
            .unified_exec_wait
            .as_ref()
            .map(|wait| wait.previous_header.clone())
            .unwrap_or_else(|| self.bottom_pane.status_header().to_string());
        self.unified_exec_wait = Some(UnifiedExecWaitStatus {
            process_id: ev.process_id.clone(),
            previous_header,
        });
        let command_display = self
            .unified_exec_processes
            .iter()
            .find(|process| process.key == ev.process_id)
            .map(|process| process.command_display.clone());
        self.bottom_pane.update_status_header_with_details_options(
            String::from("Waiting for background terminal"),
            command_display,
            super::status_indicator_widget::StatusDetailsCapitalization::Preserve,
            /*max_lines*/ 1,
        );
    }

    fn restore_status_after_unified_exec_wait(&mut self) {
        let Some(wait) = self.unified_exec_wait.take() else {
            return;
        };
        let header = self
            .reasoning_status
            .current_header()
            .unwrap_or(wait.previous_header);
        let header = if header.is_empty() {
            String::from("Working")
        } else {
            header
        };
        self.bottom_pane.update_status_header(header);
    }

    fn handle_exec_command_end_status(
        &mut self,
        ev: &codex_protocol::protocol::ExecCommandEndEvent,
    ) {
        if ev.source != codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup {
            return;
        }

        let key = ev.process_id.clone().unwrap_or_else(|| ev.call_id.clone());
        if self
            .unified_exec_wait
            .as_ref()
            .is_some_and(|wait| wait.process_id == key)
        {
            self.restore_status_after_unified_exec_wait();
        }
        let before = self.unified_exec_processes.len();
        self.unified_exec_processes
            .retain(|process| process.key != key);
        if before != self.unified_exec_processes.len() {
            self.sync_unified_exec_footer();
        }
    }

    fn track_unified_exec_output_chunk(&mut self, call_id: &str, chunk: &[u8]) {
        let Some(process) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.call_id == call_id)
        else {
            return;
        };

        let text = String::from_utf8_lossy(chunk);
        for line in text
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
        {
            process.recent_chunks.push(line.to_string());
        }

        const MAX_RECENT_CHUNKS: usize = 3;
        if process.recent_chunks.len() > MAX_RECENT_CHUNKS {
            let drop_count = process.recent_chunks.len() - MAX_RECENT_CHUNKS;
            process.recent_chunks.drain(0..drop_count);
        }
    }

    fn sync_unified_exec_footer(&mut self) {
        self.bottom_pane
            .set_unified_exec_process_count(self.unified_exec_processes.len());
    }

    fn update_bottom_pane_context_window(&mut self) {
        let Some(context_window) = self
            .processor
            .model_context_window
            .filter(|context_window| *context_window > 0)
        else {
            let used_tokens = self.processor.token_usage.total_tokens;
            self.bottom_pane
                .set_context_window(None, (used_tokens > 0).then_some(used_tokens));
            return;
        };

        let percent_left = self
            .processor
            .context_usage
            .percent_of_context_window_remaining(context_window);
        self.bottom_pane
            .set_context_window(Some(percent_left), None);
    }
}

/// Returns true when `msg` is a reasoning/thinking stream event that should not be rendered as a
/// transcript/history item.
///
/// # Divergence (codex-potter)
///
/// Reasoning messages are never rendered in the transcript; they are only used to update the live
/// status header.
fn should_filter_thinking_event(msg: &EventMsg) -> bool {
    matches!(
        msg,
        EventMsg::AgentReasoning(_)
            | EventMsg::AgentReasoningDelta(_)
            | EventMsg::AgentReasoningRawContent(_)
            | EventMsg::AgentReasoningRawContentDelta(_)
            | EventMsg::AgentReasoningSectionBreak(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insert_history::insert_history_lines;
    use crate::test_backend::VT100Backend;
    use codex_protocol::AbsolutePathBuf;
    use codex_protocol::ThreadId;
    use codex_protocol::approvals::ElicitationRequest;
    use codex_protocol::approvals::ElicitationRequestEvent;
    use codex_protocol::approvals::GuardianAssessmentEvent;
    use codex_protocol::approvals::GuardianAssessmentStatus;
    use codex_protocol::approvals::GuardianRiskLevel;
    use codex_protocol::mcp::RequestId as McpRequestId;
    use codex_protocol::models::FileSystemPermissions;
    use codex_protocol::models::NetworkPermissions;
    use codex_protocol::parse_command::ParsedCommand;
    use codex_protocol::plan_tool::PlanItemArg;
    use codex_protocol::plan_tool::StepStatus;
    use codex_protocol::plan_tool::UpdatePlanArgs;
    use codex_protocol::protocol::AgentMessageDeltaEvent;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::AgentReasoningDeltaEvent;
    use codex_protocol::protocol::AgentReasoningEvent;
    use codex_protocol::protocol::BackgroundEventEvent;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::CollabAgentRef;
    use codex_protocol::protocol::CollabWaitingBeginEvent;
    use codex_protocol::protocol::ContextCompactedEvent;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::ExecCommandBeginEvent;
    use codex_protocol::protocol::ExecCommandEndEvent;
    use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
    use codex_protocol::protocol::ExecCommandSource;
    use codex_protocol::protocol::ExecOutputStream;
    use codex_protocol::protocol::HookCompletedEvent;
    use codex_protocol::protocol::HookEventName;
    use codex_protocol::protocol::HookExecutionMode;
    use codex_protocol::protocol::HookHandlerType;
    use codex_protocol::protocol::HookOutputEntry;
    use codex_protocol::protocol::HookOutputEntryKind;
    use codex_protocol::protocol::HookRunStatus;
    use codex_protocol::protocol::HookRunSummary;
    use codex_protocol::protocol::HookScope;
    use codex_protocol::protocol::HookStartedEvent;
    use codex_protocol::protocol::PatchApplyBeginEvent;
    use codex_protocol::protocol::PatchApplyEndEvent;
    use codex_protocol::protocol::PlanDeltaEvent;
    use codex_protocol::protocol::PotterProjectListEntry;
    use codex_protocol::protocol::PotterProjectListStatus;
    use codex_protocol::protocol::SessionConfiguredEvent;
    use codex_protocol::protocol::StreamErrorEvent;
    use codex_protocol::protocol::TerminalInteractionEvent;
    use codex_protocol::protocol::TokenCountEvent;
    use codex_protocol::protocol::TokenUsageInfo;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::protocol::TurnStartedEvent;
    use codex_protocol::protocol::ViewImageToolCallEvent;
    use codex_protocol::protocol::WebSearchEndEvent;
    use codex_protocol::request_permissions::RequestPermissionProfile;
    use codex_protocol::request_permissions::RequestPermissionsEvent;
    use codex_protocol::request_user_input::RequestUserInputEvent;
    use codex_protocol::request_user_input::RequestUserInputQuestion;
    use codex_protocol::request_user_input::RequestUserInputQuestionOption;
    use insta::assert_snapshot;
    use ratatui::backend::Backend;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::sync::mpsc::unbounded_channel;

    fn line_to_plain_string(line: &ratatui::text::Line<'_>) -> String {
        let mut out = String::new();
        for span in &line.spans {
            out.push_str(span.content.as_ref());
        }
        out
    }

    fn lines_to_plain_strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines.iter().map(line_to_plain_string).collect()
    }

    fn lines_to_plain_text(lines: &[ratatui::text::Line<'_>]) -> String {
        let mut out = lines_to_plain_strings(lines).join("\n");
        out.push('\n');
        out
    }

    fn synthetic_absolute_path_buf(components: &[&str]) -> AbsolutePathBuf {
        #[cfg(windows)]
        let path = components
            .iter()
            .fold(PathBuf::from(r"C:\"), |path, component| {
                path.join(component)
            });

        #[cfg(not(windows))]
        let path = components
            .iter()
            .fold(PathBuf::from("/"), |path, component| path.join(component));

        AbsolutePathBuf::from_absolute_path(path).expect("absolute path")
    }

    fn assert_line_with_text_dimmed(
        lines: &[ratatui::text::Line<'_>],
        needle: &str,
        expected_dim: bool,
    ) {
        let Some(content) = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref().contains(needle))
        else {
            panic!("expected span containing {needle:?}");
        };

        assert_eq!(
            content.style.add_modifier.contains(Modifier::DIM),
            expected_dim,
            "unexpected dim state for {needle:?}"
        );
    }

    fn drain_history_cell_strings(
        rx: &mut UnboundedReceiver<AppEvent>,
        width: u16,
    ) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            let AppEvent::InsertHistoryCell(cell) = ev else {
                continue;
            };
            out.push(lines_to_plain_strings(&cell.display_lines(width)));
        }
        out
    }

    fn recv_inserted_history_cell(rx: &mut UnboundedReceiver<AppEvent>) -> Box<dyn HistoryCell> {
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = ev {
                return cell;
            }
        }
        panic!("expected an inserted history cell");
    }

    fn drain_render_history_events(
        rx: &mut UnboundedReceiver<AppEvent>,
        terminal: &mut crate::custom_terminal::Terminal<VT100Backend>,
        width: u16,
        has_emitted_history_lines: &mut bool,
    ) {
        while let Ok(ev) = rx.try_recv() {
            let AppEvent::InsertHistoryCell(cell) = ev else {
                continue;
            };

            let cell: Arc<dyn HistoryCell> = cell.into();
            let mut display = cell.display_lines(width);
            if display.is_empty() {
                continue;
            }

            if !cell.is_stream_continuation() {
                if *has_emitted_history_lines {
                    display.insert(0, Line::from(""));
                } else {
                    *has_emitted_history_lines = true;
                }
            }

            insert_history_lines(terminal, display).expect("insert history");
        }
    }

    fn drive_stream_to_idle(
        proc: &mut AppServerEventProcessor,
        rx: &mut UnboundedReceiver<AppEvent>,
        terminal: &mut crate::custom_terminal::Terminal<VT100Backend>,
        width: u16,
        has_emitted_history_lines: &mut bool,
    ) {
        for _ in 0..100 {
            proc.on_commit_tick();
            drain_render_history_events(rx, terminal, width, has_emitted_history_lines);
        }
    }

    fn draw_inline_runner_frame(
        terminal: &mut crate::custom_terminal::Terminal<VT100Backend>,
        app: &mut RenderAppState,
    ) {
        let screen = terminal.size().expect("terminal size");
        let width = screen.width.max(1);
        app.processor.last_rendered_width = Some(width);

        let pane_height = app.bottom_pane.desired_height(width).max(1);
        let transient_lines = app.build_transient_lines(width);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height
            .saturating_add(transient_height)
            .min(screen.height);

        let mut area = terminal.viewport_area;
        area.height = viewport_height;
        area.width = screen.width;
        if area.bottom() > screen.height {
            terminal
                .backend_mut()
                .scroll_region_up(0..area.top(), area.bottom() - screen.height)
                .expect("scroll viewport");
            area.y = screen.height - area.height;
        }
        if area != terminal.viewport_area {
            terminal.clear().expect("clear viewport");
            terminal.set_viewport_area(area);
        }

        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());
                render_runner_viewport(area, frame.buffer_mut(), &app.bottom_pane, transient_lines);
                let pane_height = app
                    .bottom_pane
                    .desired_height(area.width)
                    .max(1)
                    .min(area.height);
                let pane_area = ratatui::layout::Rect::new(
                    area.x,
                    area.bottom().saturating_sub(pane_height),
                    area.width,
                    pane_height,
                );
                let cursor = app
                    .bottom_pane
                    .cursor_pos(pane_area)
                    .unwrap_or((area.x, area.bottom().saturating_sub(1)));
                frame.set_cursor_position(cursor);
            })
            .expect("draw inline runner frame");
    }

    fn make_round_renderer_processor(
        prompt: &str,
    ) -> (AppServerEventProcessor, UnboundedReceiver<AppEvent>) {
        let (tx_raw, rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());
        proc.emit_user_prompt(prompt.to_string());
        (proc, rx)
    }

    fn make_round_renderer_processor_without_prompt()
    -> (AppServerEventProcessor, UnboundedReceiver<AppEvent>) {
        let (tx_raw, rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        (
            AppServerEventProcessor::new(app_event_tx, Verbosity::default()),
            rx,
        )
    }

    fn make_round_renderer_app(
        verbosity: Verbosity,
    ) -> (RenderAppState, UnboundedReceiver<AppEvent>) {
        let (tx_raw, rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let processor = AppServerEventProcessor::new(app_event_tx.clone(), verbosity);
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        (
            RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            ),
            rx,
        )
    }

    #[tokio::test]
    async fn should_filter_thinking_events() {
        assert!(should_filter_thinking_event(
            &EventMsg::AgentReasoningDelta(codex_protocol::protocol::AgentReasoningDeltaEvent {
                delta: "thinking".to_string(),
            })
        ));
        assert!(!should_filter_thinking_event(&EventMsg::AgentMessageDelta(
            codex_protocol::protocol::AgentMessageDeltaEvent {
                delta: "output".to_string(),
            }
        )));
    }

    #[test]
    fn reasoning_delta_updates_status_header_from_first_bold() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);

        let mut tracker = ReasoningStatusTracker::new();
        assert!(
            tracker.on_delta("**Inspecting").is_none(),
            "incomplete header"
        );

        let Some(header) = tracker.on_delta(" for code duplication**") else {
            panic!("expected a header after receiving closing **");
        };
        bottom_pane.update_status_header(header);

        let status = bottom_pane.status_widget().expect("status indicator");
        assert_eq!(status.header(), "Inspecting for code duplication");
    }

    #[test]
    fn reasoning_final_updates_status_header_without_prior_deltas() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            None,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "reasoning-1".into(),
                msg: EventMsg::AgentReasoning(AgentReasoningEvent {
                    text: "**Inspecting for code duplication**\n\nSearch duplicated helpers."
                        .into(),
                }),
            },
        )
        .expect("handle reasoning final");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Inspecting for code duplication");
    }

    #[test]
    fn potter_stream_recovery_retry_block_persists_and_clears_on_recovered_event() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        bottom_pane.update_status_header("Inspecting for code duplication".to_string());
        if let Some(status) = bottom_pane.status_indicator_mut() {
            status.pause_timer_at(Instant::now());
        }
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err".into(),
                msg: EventMsg::PotterStreamRecoveryUpdate {
                    attempt: 1,
                    max_attempts: 10,
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                },
            },
        )
        .expect("handle codex event");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Inspecting for code duplication");
        pretty_assertions::assert_eq!(status.details(), None);

        let transient_lines = app.build_transient_lines(width);
        let transient_blob = lines_to_plain_strings(&transient_lines).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 1/10"),
            "missing retry header: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains(
                "└ Stream disconnected before completion: error sending request for url (...)"
            ),
            "missing retry error details: {transient_blob:?}"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );

        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op for stream recovery update"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "turn-started".into(),
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: None,
                }),
            },
        )
        .expect("handle retry turn start");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Working");
        pretty_assertions::assert_eq!(status.details(), None);

        let transient_lines = app.build_transient_lines(width);
        let transient_blob = lines_to_plain_strings(&transient_lines).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 1/10"),
            "expected retry block to persist until recovered: {transient_blob:?}"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "recovered".into(),
                msg: EventMsg::PotterStreamRecoveryRecovered,
            },
        )
        .expect("handle recovered event");

        let transient_lines = app.build_transient_lines(width);
        let transient_blob = lines_to_plain_strings(&transient_lines).join("\n");
        assert!(
            !transient_blob.contains("CodexPotter: retry"),
            "expected retry block to be cleared: {transient_blob:?}"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );
    }

    #[test]
    fn potter_stream_recovery_update_replaces_existing_retry_block_in_place() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);

        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err-1".into(),
                msg: EventMsg::PotterStreamRecoveryUpdate {
                    attempt: 1,
                    max_attempts: 10,
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                },
            },
        )
        .expect("handle first update");

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 1/10"),
            "missing retry 1/10: {transient_blob:?}"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err-3".into(),
                msg: EventMsg::PotterStreamRecoveryUpdate {
                    attempt: 3,
                    max_attempts: 10,
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                },
            },
        )
        .expect("handle second update");

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 3/10"),
            "missing retry 3/10: {transient_blob:?}"
        );
        assert!(
            !transient_blob.contains("• CodexPotter: retry 1/10"),
            "expected retry block to update in place: {transient_blob:?}"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );
        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op for stream recovery update"
        );
    }

    #[test]
    fn background_event_updates_status_header_without_history_cells() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "bg-1".into(),
                msg: EventMsg::BackgroundEvent(BackgroundEventEvent {
                    message: "Waiting for `vim`".to_string(),
                }),
            },
        )
        .expect("handle background event");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Waiting for `vim`");
        pretty_assertions::assert_eq!(status.details(), None);

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );
    }

    #[test]
    fn terminal_interaction_wait_updates_status_and_restores_reasoning_header_on_exec_end() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "turn-1".into(),
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: None,
                }),
            },
        )
        .expect("handle turn start");
        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "reasoning-1".into(),
                msg: EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                    delta: "**Inspecting".to_string(),
                }),
            },
        )
        .expect("handle reasoning delta start");
        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "reasoning-2".into(),
                msg: EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                    delta: " for background shell**".to_string(),
                }),
            },
        )
        .expect("handle reasoning delta end");
        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "exec-begin".into(),
                msg: EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                    call_id: "call-1".to_string(),
                    process_id: Some("proc-1".to_string()),
                    turn_id: "turn-1".to_string(),
                    command: vec!["bash".to_string(), "-lc".to_string(), "sleep 5".to_string()],
                    cwd: PathBuf::from("project"),
                    parsed_cmd: Vec::new(),
                    source: ExecCommandSource::UnifiedExecStartup,
                    interaction_input: None,
                }),
            },
        )
        .expect("handle exec begin");
        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "terminal-1".into(),
                msg: EventMsg::TerminalInteraction(TerminalInteractionEvent {
                    call_id: "call-1".to_string(),
                    process_id: "proc-1".to_string(),
                    stdin: String::new(),
                }),
            },
        )
        .expect("handle terminal interaction");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Waiting for background terminal");
        pretty_assertions::assert_eq!(status.details(), Some("sleep 5"));
        pretty_assertions::assert_eq!(
            status.inline_message(),
            Some("1 background terminal running · /ps to view · /stop to close")
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "exec-end".into(),
                msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                    call_id: "call-1".to_string(),
                    process_id: Some("proc-1".to_string()),
                    turn_id: "turn-1".to_string(),
                    command: vec!["bash".to_string(), "-lc".to_string(), "sleep 5".to_string()],
                    cwd: PathBuf::from("project"),
                    parsed_cmd: Vec::new(),
                    source: ExecCommandSource::UnifiedExecStartup,
                    interaction_input: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    aggregated_output: String::new(),
                    exit_code: 0,
                    duration: Duration::from_secs(1),
                    formatted_output: String::new(),
                }),
            },
        )
        .expect("handle exec end");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Inspecting for background shell");
        pretty_assertions::assert_eq!(status.details(), None);
        pretty_assertions::assert_eq!(status.inline_message(), None);

        let _cells = drain_history_cell_strings(&mut rx_app, width);
    }

    #[test]
    fn background_terminal_survives_turn_complete_and_ps_shows_recent_chunks() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "exec-begin".into(),
                msg: EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                    call_id: "call-1".to_string(),
                    process_id: Some("proc-1".to_string()),
                    turn_id: "turn-1".to_string(),
                    command: vec!["bash".to_string(), "-lc".to_string(), "sleep 5".to_string()],
                    cwd: PathBuf::from("project"),
                    parsed_cmd: Vec::new(),
                    source: ExecCommandSource::UnifiedExecStartup,
                    interaction_input: None,
                }),
            },
        )
        .expect("handle exec begin");
        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "output-1".into(),
                msg: EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
                    call_id: "call-1".to_string(),
                    stream: ExecOutputStream::Stdout,
                    chunk: b"first\nsecond\n".to_vec(),
                }),
            },
        )
        .expect("handle first output delta");
        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "output-2".into(),
                msg: EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
                    call_id: "call-1".to_string(),
                    stream: ExecOutputStream::Stdout,
                    chunk: b"third\nfourth\n".to_vec(),
                }),
            },
        )
        .expect("handle second output delta");
        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "turn-complete".into(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "turn-1".to_string(),
                    last_agent_message: None,
                }),
            },
        )
        .expect("handle turn complete");

        pretty_assertions::assert_eq!(app.unified_exec_processes.len(), 1);
        let process = &app.unified_exec_processes[0];
        pretty_assertions::assert_eq!(
            process.recent_chunks,
            vec![
                "second".to_string(),
                "third".to_string(),
                "fourth".to_string()
            ]
        );

        let cell = history_cell::new_unified_exec_processes_output(vec![
            history_cell::UnifiedExecProcessDetails {
                command_display: process.command_display.clone(),
                recent_chunks: process.recent_chunks.clone(),
            },
        ]);
        let rendered = lines_to_plain_text(&cell.display_lines(width));
        assert!(rendered.contains("sleep 5"), "rendered={rendered:?}");
        assert!(rendered.contains("second"), "rendered={rendered:?}");
        assert!(rendered.contains("fourth"), "rendered={rendered:?}");
        assert!(!rendered.contains("first"), "rendered={rendered:?}");

        let _cells = drain_history_cell_strings(&mut rx_app, width);
    }

    #[test]
    fn stream_error_status_updates_and_restores_on_next_event() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        bottom_pane.update_status_header("Inspecting for code duplication".to_string());
        if let Some(status) = bottom_pane.status_indicator_mut() {
            status.pause_timer_at(Instant::now());
        }
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "stream-error-1".into(),
                msg: EventMsg::StreamError(StreamErrorEvent {
                    message: "Reconnecting... 1/5".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ResponseStreamDisconnected {
                        http_status_code: None,
                    }),
                    additional_details: Some(
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                    ),
                }),
            },
        )
        .expect("handle stream error event");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Reconnecting... 1/5");
        pretty_assertions::assert_eq!(
            status.details(),
            Some("Stream disconnected before completion: error sending request for url (...)")
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );
        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op for stream error"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "stream-error-2".into(),
                msg: EventMsg::StreamError(StreamErrorEvent {
                    message: "Reconnecting... 2/5".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ResponseStreamDisconnected {
                        http_status_code: None,
                    }),
                    additional_details: Some(
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                    ),
                }),
            },
        )
        .expect("handle stream error event");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Reconnecting... 2/5");

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "delta".into(),
                msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    delta: "hello".to_string(),
                }),
            },
        )
        .expect("handle delta");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Inspecting for code duplication");
        pretty_assertions::assert_eq!(status.details(), None);
    }

    #[test]
    fn non_retryable_error_inserts_error_history_cell() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err".into(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "unauthorized".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Unauthorized),
                }),
            },
        )
        .expect("handle codex event");

        let cells = drain_history_cell_strings(&mut rx_app, width);
        pretty_assertions::assert_eq!(cells.len(), 1);
        let blob = cells[0].join("\n");
        assert!(
            blob.contains("■ unauthorized"),
            "unexpected error cell: {blob:?}"
        );

        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op for non-retryable errors"
        );
    }

    #[test]
    fn stream_recovery_gave_up_inserts_unrecoverable_cell_and_exits_on_round_finished_task_failed()
    {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err".into(),
                msg: EventMsg::PotterStreamRecoveryGaveUp {
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                    attempts: 10,
                    max_attempts: 10,
                },
            },
        )
        .expect("handle codex event");

        assert!(
            !app.exit_after_next_draw,
            "expected app to wait for PotterRoundFinished"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        pretty_assertions::assert_eq!(cells.len(), 1);
        let blob = cells[0].join("\n");
        assert!(
            blob.contains("■ CodexPotter: unrecoverable error after 10 retries"),
            "unexpected unrecoverable cell: {blob:?}"
        );
        assert!(
            blob.contains(
                "Stream disconnected before completion: error sending request for url (...)"
            ),
            "missing underlying error message: {blob:?}"
        );

        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op after giving up"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "round-finished".into(),
                msg: EventMsg::PotterRoundFinished {
                    outcome: codex_protocol::protocol::PotterRoundOutcome::TaskFailed {
                        message: "stream recovery gave up".to_string(),
                    },
                    duration_secs: 0,
                },
            },
        )
        .expect("handle round finished event");

        assert!(
            matches!(&app.exit_reason, ExitReason::TaskFailed(_)),
            "expected TaskFailed exit reason; got: {:?}",
            &app.exit_reason
        );
        assert!(app.exit_after_next_draw, "expected app to request exit");
    }

    #[test]
    fn round_renderer_context_window_percent_and_fallbacks() {
        {
            let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let processor =
                AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
            let (op_tx, _op_rx) = unbounded_channel::<Op>();
            let mut bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
                PathBuf::from("project"),
                Some("main".to_string()),
            ));
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            );

            app.processor.handle_codex_event(Event {
                id: "token-count".into(),
                msg: EventMsg::TokenCount(TokenCountEvent {
                    info: Some(TokenUsageInfo {
                        // Simulate cumulative billing usage (should not drive the context window percent).
                        total_token_usage: TokenUsage {
                            total_tokens: 100_000,
                            ..TokenUsage::default()
                        },
                        // Simulate Codex's estimated tokens currently in the context window.
                        last_token_usage: TokenUsage {
                            total_tokens: 20_000,
                            ..TokenUsage::default()
                        },
                        model_context_window: Some(128_000),
                    }),
                    rate_limits: None,
                }),
            });

            app.update_bottom_pane_context_window();

            assert_eq!(app.bottom_pane.context_window_percent(), Some(93));
            assert_eq!(app.bottom_pane.context_window_used_tokens(), None);
            assert_eq!(app.processor.token_usage.total_tokens, 100_000);
        }

        {
            let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let processor =
                AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
            let (op_tx, _op_rx) = unbounded_channel::<Op>();
            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            );

            app.processor.token_usage = TokenUsage {
                total_tokens: 123_456,
                ..TokenUsage::default()
            };
            app.processor.model_context_window = None;

            app.update_bottom_pane_context_window();

            assert_eq!(app.bottom_pane.context_window_percent(), None);
            assert_eq!(app.bottom_pane.context_window_used_tokens(), Some(123_456));
        }

        {
            let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let processor =
                AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
            let (op_tx, _op_rx) = unbounded_channel::<Op>();
            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            );

            app.processor.token_usage = TokenUsage::default();
            app.processor.model_context_window = None;

            app.update_bottom_pane_context_window();

            assert_eq!(app.bottom_pane.context_window_percent(), None);
            assert_eq!(app.bottom_pane.context_window_used_tokens(), None);
        }
    }

    #[test]
    fn round_renderer_composer_processes_repeat_key_events() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyEventKind;
        use crossterm::event::KeyModifiers;

        {
            let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let processor =
                AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
            let (op_tx, _op_rx) = unbounded_channel::<Op>();
            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            );

            app.bottom_pane
                .composer_mut()
                .set_text_content("hello".to_string());
            let area = Rect::new(0, 0, 80, 10);
            let before =
                crate::render::renderable::Renderable::cursor_pos(&app.bottom_pane, area).unwrap();

            let mut right_repeat = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
            right_repeat.kind = KeyEventKind::Repeat;
            app.handle_key_event(right_repeat, crate::tui::FrameRequester::test_dummy(), 80);

            let after =
                crate::render::renderable::Renderable::cursor_pos(&app.bottom_pane, area).unwrap();
            assert!(
                after.0 > before.0,
                "expected cursor to move right on Repeat (before={before:?}, after={after:?})",
            );
        }

        {
            let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let processor =
                AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
            let (op_tx, _op_rx) = unbounded_channel::<Op>();
            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            );

            app.bottom_pane.composer_mut().set_disable_paste_burst(true);
            for ch in "hello world".chars() {
                app.handle_key_event(
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                    crate::tui::FrameRequester::test_dummy(),
                    80,
                );
            }
            assert_eq!(app.bottom_pane.composer().current_text(), "hello world");

            let mut ctrl_w_repeat = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
            ctrl_w_repeat.kind = KeyEventKind::Repeat;
            app.handle_key_event(ctrl_w_repeat, crate::tui::FrameRequester::test_dummy(), 80);

            assert_eq!(app.bottom_pane.composer().current_text(), "hello ");
        }
    }

    #[test]
    fn round_renderer_slash_mention_inserts_at_and_starts_file_search() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/mention".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.bottom_pane.composer().current_text(), "@");

        let mut saw_file_search = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::StartFileSearch(query) = ev {
                assert_eq!(query, "");
                saw_file_search = true;
                break;
            }
        }
        assert!(
            saw_file_search,
            "expected StartFileSearch after inserting '@'"
        );
    }

    #[test]
    fn round_renderer_ctrl_l_opens_projects_overlay_and_restores_interrupt_after_close() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            app.projects_overlay.is_open(),
            "expected Ctrl+L to open overlay"
        );
        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected list request, got {other:?}"),
        }

        let first_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let second_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/2");
        app.handle_projects_overlay_response(
            crate::tui::FrameRequester::test_dummy(),
            crate::ProjectsOverlayResponse::List {
                projects: vec![
                    PotterProjectListEntry {
                        project_dir: first_project_dir.clone(),
                        progress_file: first_project_dir.join("MAIN.md"),
                        description: "First overlay project".to_string(),
                        started_at_unix_secs: Some(1),
                        rounds: 1,
                        status: PotterProjectListStatus::Succeeded,
                    },
                    PotterProjectListEntry {
                        project_dir: second_project_dir.clone(),
                        progress_file: second_project_dir.join("MAIN.md"),
                        description: "Second overlay project".to_string(),
                        started_at_unix_secs: Some(2),
                        rounds: 2,
                        status: PotterProjectListStatus::Interrupted,
                    },
                ],
                error: None,
            },
        );

        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, first_project_dir);
            }
            other => panic!("expected initial details request, got {other:?}"),
        }

        app.handle_key_event(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, second_project_dir);
            }
            other => panic!("expected selection details request, got {other:?}"),
        }

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            app.projects_overlay.is_open(),
            "expected Ctrl+D to stay inside overlay while open"
        );
        assert!(
            rx_app.try_recv().is_err(),
            "overlay paging should not request interrupt"
        );

        app.handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            !app.projects_overlay.is_open(),
            "expected Esc to close overlay first"
        );
        assert!(
            rx_app.try_recv().is_err(),
            "closing overlay should not emit backend ops"
        );

        app.handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        match rx_app.try_recv() {
            Ok(AppEvent::CodexOp(Op::Interrupt)) => {}
            other => panic!("expected Esc after close to interrupt round, got {other:?}"),
        }
    }

    #[test]
    fn projects_overlay_auto_refresh_sends_list_request() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );
        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected initial list request, got {other:?}"),
        }

        app.projects_overlay_next_auto_refresh_at = Some(Instant::now() - Duration::from_secs(1));
        app.maybe_send_projects_overlay_auto_refresh(
            crate::tui::FrameRequester::test_dummy(),
            Instant::now(),
        );

        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected auto-refresh list request, got {other:?}"),
        }
    }

    #[test]
    fn projects_overlay_ctrl_l_closes_overlay_when_already_open() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            app.projects_overlay.is_open(),
            "expected Ctrl+L to open overlay"
        );
        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected list request, got {other:?}"),
        }

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            !app.projects_overlay.is_open(),
            "expected Ctrl+L to close overlay when already open"
        );
        assert!(
            overlay_request_rx.try_recv().is_err(),
            "closing overlay should not request another refresh"
        );
    }

    #[test]
    fn projects_overlay_ctrl_c_closes_overlay_without_clearing_composer() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.bottom_pane
            .composer_mut()
            .set_text_content("do not clear".to_string());

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected list request, got {other:?}"),
        }

        assert!(app.projects_overlay.is_open());
        assert_eq!(app.bottom_pane.composer().current_text(), "do not clear");

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(!app.projects_overlay.is_open());
        assert_eq!(app.bottom_pane.composer().current_text(), "do not clear");
        assert!(
            overlay_request_rx.try_recv().is_err(),
            "closing overlay should not request another refresh"
        );
        assert!(
            rx_app.try_recv().is_err(),
            "closing overlay should not emit backend ops"
        );
    }

    #[test]
    fn projects_overlay_arrow_keys_schedule_redraws() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.projects_overlay.open_or_refresh();
        assert!(
            app.projects_overlay.is_open(),
            "expected overlay to be open for redraw test"
        );

        let (frame_requester, mut schedule_rx) = crate::tui::FrameRequester::test_recorder();
        app.handle_key_event(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            frame_requester,
            80,
        );

        assert!(
            schedule_rx.try_recv().is_ok(),
            "expected overlay navigation key to schedule a redraw"
        );
    }

    #[test]
    fn round_renderer_slash_compact_kb_inserts_prompt_text() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        {
            let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let processor =
                AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
            let (op_tx, _op_rx) = unbounded_channel::<Op>();
            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            );

            app.bottom_pane.composer_mut().set_disable_paste_burst(true);
            for ch in "/compact-kb".chars() {
                app.handle_key_event(
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                    crate::tui::FrameRequester::test_dummy(),
                    80,
                );
            }
            app.handle_key_event(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );

            assert_eq!(app.bottom_pane.composer().current_text(), COMPACT_KB_PROMPT);
        }
    }

    #[test]
    fn round_renderer_ctrl_c_closes_projects_overlay_without_exiting() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );
        assert!(
            app.projects_overlay.is_open(),
            "expected Ctrl+L to open overlay"
        );
        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected list request, got {other:?}"),
        }

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            !app.exit_after_next_draw,
            "expected Ctrl+C to close overlay instead of requesting exit"
        );
        assert!(
            !app.projects_overlay.is_open(),
            "expected Ctrl+C to close overlay"
        );
        assert!(
            rx_app.try_recv().is_err(),
            "closing overlay should not request interrupt"
        );
    }

    #[test]
    fn round_renderer_slash_list_opens_projects_overlay() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/list".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            app.projects_overlay.is_open(),
            "expected /list to open overlay"
        );
        assert_eq!(app.bottom_pane.composer().current_text(), "");
        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected list request, got {other:?}"),
        }
    }

    #[test]
    fn round_renderer_slash_popup_compact_kb_inserts_prompt_text() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/c".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.bottom_pane.composer().current_text(), COMPACT_KB_PROMPT);
    }

    #[test]
    fn round_renderer_slash_exit_requests_interrupt_and_exit() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/exit".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(app.exit_after_next_draw, "expected /exit to request exit");

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(saw_interrupt, "expected /exit to request Op::Interrupt");
    }

    #[test]
    fn round_renderer_ctrl_d_requests_interrupt_and_exit() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        {
            let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let processor =
                AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
            let (op_tx, _op_rx) = unbounded_channel::<Op>();
            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            );

            app.handle_key_event(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );

            assert!(app.exit_after_next_draw, "expected Ctrl+D to request exit");

            let mut saw_interrupt = false;
            while let Ok(ev) = rx_app.try_recv() {
                if let AppEvent::CodexOp(Op::Interrupt) = ev {
                    saw_interrupt = true;
                    break;
                }
            }
            assert!(saw_interrupt, "expected Ctrl+D to request Op::Interrupt");
        }

        {
            let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let processor =
                AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
            let (op_tx, _op_rx) = unbounded_channel::<Op>();
            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new(
                processor,
                app_event_tx,
                Some(op_tx),
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                VecDeque::new(),
            );

            app.handle_key_event(
                KeyEvent::new(
                    KeyCode::Char('D'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );

            assert!(
                app.exit_after_next_draw,
                "expected uppercase Ctrl+D to request exit"
            );

            let mut saw_interrupt = false;
            while let Ok(ev) = rx_app.try_recv() {
                if let AppEvent::CodexOp(Op::Interrupt) = ev {
                    saw_interrupt = true;
                    break;
                }
            }
            assert!(
                saw_interrupt,
                "expected uppercase Ctrl+D to request Op::Interrupt"
            );
        }
    }

    #[test]
    fn round_renderer_ctrl_d_exit_reason_survives_round_finished_interrupted() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            matches!(&app.exit_reason, ExitReason::UserRequested),
            "expected Ctrl+D to set UserRequested exit reason"
        );
        assert!(
            app.exit_requested_by_user,
            "expected Ctrl+D to set exit_requested_by_user"
        );

        // The backend will typically surface an Interrupted round outcome after handling the
        // interrupt op. Preserve UserRequested so the CLI can print queued prompts on exit.
        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "round-finished".into(),
                msg: EventMsg::PotterRoundFinished {
                    outcome: codex_protocol::protocol::PotterRoundOutcome::Interrupted,
                    duration_secs: 0,
                },
            },
        )
        .expect("handle round finished event");

        assert!(
            matches!(&app.exit_reason, ExitReason::UserRequested),
            "expected Ctrl+D to keep UserRequested exit reason; got: {:?}",
            &app.exit_reason
        );
    }

    #[test]
    fn round_renderer_ctrl_d_does_not_exit_when_composer_not_empty() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane
            .composer_mut()
            .set_text_content("abc".to_string());
        assert_eq!(app.bottom_pane.composer().current_text(), "abc");

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            !app.exit_after_next_draw,
            "did not expect Ctrl+D to request exit"
        );
        assert_eq!(app.bottom_pane.composer().current_text(), "bc");

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Ctrl+D to request Op::Interrupt"
        );
    }

    #[test]
    fn round_renderer_ctrl_d_does_not_exit_when_selection_popup_is_visible() {
        use crate::bottom_pane::SelectionItem;
        use crate::bottom_pane::SelectionViewParams;
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane
            .composer_mut()
            .show_selection_view(SelectionViewParams {
                items: vec![SelectionItem {
                    name: "Option".to_string(),
                    ..SelectionItem::default()
                }],
                ..SelectionViewParams::default()
            });

        assert!(app.bottom_pane.composer().selection_popup_visible());
        assert!(app.bottom_pane.composer().is_empty());

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            !app.exit_after_next_draw,
            "did not expect Ctrl+D to request exit while popup is visible"
        );
        assert!(app.bottom_pane.composer().selection_popup_visible());

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Ctrl+D to request Op::Interrupt while popup is visible"
        );
    }

    #[test]
    fn round_renderer_ctrl_d_flushes_pending_minimal_patch_summary_before_exit() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let width: u16 = 80;
        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut processor =
            AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        processor.verbosity = Verbosity::Minimal;

        let patch = diffy::create_patch("a\n", "b\n").to_string();
        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: patch,
                move_path: None,
            },
        );

        processor.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.set_task_running(true);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            width,
        );

        assert!(app.exit_after_next_draw, "expected Ctrl+D to request exit");

        let events = drain_history_cell_strings(&mut rx_app, width);
        pretty_assertions::assert_eq!(events, vec![vec!["• Edited file.txt (+1 -1)".to_string()]]);
    }

    #[test]
    fn round_renderer_ctrl_d_flushes_buffered_minimal_agent_stream_before_exit() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let width: u16 = 80;
        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::Minimal);
        processor.handle_codex_event(Event {
            id: "agent-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "hello\n".to_string(),
            }),
        });
        processor.on_commit_tick();

        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.set_task_running(true);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            width,
        );

        assert!(app.exit_after_next_draw, "expected Ctrl+D to request exit");

        let events = drain_history_cell_strings(&mut rx_app, width);
        pretty_assertions::assert_eq!(events, vec![vec!["• hello".to_string()]]);
    }

    #[test]
    fn round_renderer_esc_requests_interrupt_when_task_is_running() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.set_task_running(true);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(saw_interrupt, "expected Esc to request Op::Interrupt");
    }

    #[test]
    fn round_renderer_ctrl_c_interrupts_instead_of_exiting_when_task_is_running() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::default());

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            !app.exit_after_next_draw,
            "did not expect Ctrl+C to request exit while a task is running"
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            saw_interrupt,
            "expected Ctrl+C to request Op::Interrupt while a task is running"
        );
    }

    #[test]
    fn round_renderer_ctrl_c_clears_draft_instead_of_interrupting_task() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::default());

        app.bottom_pane
            .composer_mut()
            .set_text_content("queued draft".to_string());
        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            app.bottom_pane.composer().is_empty(),
            "expected Ctrl+C to clear the draft when non-empty"
        );

        assert!(
            !app.exit_after_next_draw,
            "did not expect Ctrl+C to exit when clearing draft"
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Ctrl+C to request Op::Interrupt when clearing draft"
        );
    }

    #[test]
    fn round_renderer_ctrl_c_with_popup_does_not_exit_or_interrupt_when_task_is_running() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::default());

        app.bottom_pane
            .composer_mut()
            .set_text_content("/".to_string());
        assert!(
            app.bottom_pane.composer().popup_active(),
            "expected slash popup to be active"
        );

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            !app.exit_after_next_draw,
            "did not expect Ctrl+C to request exit while a popup is visible"
        );
        assert!(
            app.bottom_pane.composer().is_empty(),
            "expected Ctrl+C to clear the draft and dismiss the popup"
        );
        assert!(
            !app.bottom_pane.composer().popup_active(),
            "expected Ctrl+C to dismiss the popup when clearing draft"
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Ctrl+C to request Op::Interrupt while clearing a popup draft"
        );
    }

    #[test]
    fn prompt_slash_exit_cancels_without_interrupt_or_history() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );

        app.bottom_pane.set_task_running(false);
        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/exit".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.prompt_action, Some(PromptScreenAction::CancelledByUser));

        let (_log_id, entry_count) = app.prompt_history.metadata();
        assert_eq!(
            entry_count, 0,
            "expected prompt screen /exit not to be recorded in history"
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Op::Interrupt in prompt screen"
        );
    }

    #[test]
    fn prompt_slash_stop_succeeds_without_backend_or_interrupt() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );

        app.bottom_pane.set_task_running(false);
        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/stop".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.prompt_action, None);

        let (_log_id, entry_count) = app.prompt_history.metadata();
        assert_eq!(
            entry_count, 0,
            "expected prompt screen /stop not to be recorded in history"
        );

        let mut history_cells: Vec<Vec<String>> = Vec::new();
        let mut saw_codex_op = false;
        while let Ok(ev) = rx_app.try_recv() {
            match ev {
                AppEvent::InsertHistoryCell(cell) => {
                    history_cells.push(lines_to_plain_strings(&cell.display_lines(80)));
                }
                AppEvent::CodexOp(_) => {
                    saw_codex_op = true;
                }
                _ => {}
            }
        }
        assert!(
            history_cells
                .iter()
                .flatten()
                .any(|line| line.contains("Stopping all background terminals.")),
            "expected /stop to emit an info history cell"
        );

        assert!(
            !saw_codex_op,
            "did not expect any CodexOp when /stop runs without a backend"
        );
    }

    #[test]
    fn prompt_screen_inserts_blank_line_between_ps_and_stop_history_cells() {
        let width: u16 = 80;
        let mut has_emitted_history_lines = false;

        let ps_cell: Arc<dyn HistoryCell> = Arc::from(Box::new(
            history_cell::new_unified_exec_processes_output(Vec::new()),
        ) as Box<dyn HistoryCell>);
        let mut ps_display = ps_cell.display_lines(width);
        maybe_insert_history_cell_separator(
            &ps_cell,
            &mut has_emitted_history_lines,
            &mut ps_display,
        );
        assert_eq!(
            lines_to_plain_strings(&ps_display)
                .first()
                .map(String::as_str),
            Some("/ps"),
            "expected first inserted cell to omit a leading separator"
        );

        let stop_cell: Arc<dyn HistoryCell> = Arc::from(Box::new(history_cell::new_info_event(
            "Stopping all background terminals.".to_string(),
            /*hint*/ None,
        )) as Box<dyn HistoryCell>);
        let mut stop_display = stop_cell.display_lines(width);
        maybe_insert_history_cell_separator(
            &stop_cell,
            &mut has_emitted_history_lines,
            &mut stop_display,
        );

        let stop_lines = lines_to_plain_strings(&stop_display);
        assert_eq!(
            stop_lines.first().map(String::as_str),
            Some(""),
            "expected subsequent history cells to be separated by a blank line"
        );
        assert!(
            stop_lines
                .iter()
                .any(|line| line.contains("Stopping all background terminals.")),
            "expected stop message to remain visible"
        );
    }

    #[test]
    fn prompt_screen_ctrl_l_opens_projects_overlay() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.bottom_pane.set_task_running(false);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            app.projects_overlay.is_open(),
            "expected Ctrl+L to open overlay on prompt screen"
        );
        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected list request, got {other:?}"),
        }
    }

    #[test]
    fn restored_projects_overlay_keeps_open_state_and_refreshes_selected_project() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let first_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let second_project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/2");
        let projects = vec![
            PotterProjectListEntry {
                project_dir: first_project_dir.clone(),
                progress_file: first_project_dir.join("MAIN.md"),
                description: "First overlay project".to_string(),
                started_at_unix_secs: Some(1),
                rounds: 1,
                status: PotterProjectListStatus::Succeeded,
            },
            PotterProjectListEntry {
                project_dir: second_project_dir.clone(),
                progress_file: second_project_dir.join("MAIN.md"),
                description: "Second overlay project".to_string(),
                started_at_unix_secs: Some(2),
                rounds: 2,
                status: PotterProjectListStatus::Interrupted,
            },
        ];

        let (mut first_app, _rx) = make_round_renderer_app(Verbosity::default());
        let (first_request_tx, mut first_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        first_app.projects_overlay_request_tx = Some(first_request_tx);
        first_app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );
        match first_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected initial list request, got {other:?}"),
        }
        first_app.handle_projects_overlay_response(
            crate::tui::FrameRequester::test_dummy(),
            crate::ProjectsOverlayResponse::List {
                projects: projects.clone(),
                error: None,
            },
        );
        match first_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, first_project_dir);
            }
            other => panic!("expected initial details request, got {other:?}"),
        }

        first_app.handle_key_event(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );
        match first_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, second_project_dir);
            }
            other => panic!("expected second-project details request, got {other:?}"),
        }

        let saved_overlay = first_app.projects_overlay;

        let (mut second_app, _rx) = make_round_renderer_app(Verbosity::default());
        let (second_request_tx, mut second_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        let (_second_response_tx, second_response_rx) =
            unbounded_channel::<crate::ProjectsOverlayResponse>();
        let _overlay_response_rx = second_app.restore_projects_overlay(
            saved_overlay,
            Some(crate::ProjectsOverlayProviderChannels {
                request_tx: second_request_tx,
                response_rx: second_response_rx,
            }),
        );

        assert!(
            second_app.projects_overlay.is_open(),
            "expected restored overlay to remain open"
        );
        match second_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected restore-time list refresh, got {other:?}"),
        }

        second_app.handle_projects_overlay_response(
            crate::tui::FrameRequester::test_dummy(),
            crate::ProjectsOverlayResponse::List {
                projects,
                error: None,
            },
        );
        match second_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::Details { project_dir }) => {
                assert_eq!(project_dir, second_project_dir);
            }
            other => panic!("expected restored selection details request, got {other:?}"),
        }
    }

    #[test]
    fn prompt_screen_slash_list_opens_projects_overlay() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.bottom_pane.set_task_running(false);
        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/list".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(
            app.projects_overlay.is_open(),
            "expected /list to open overlay on prompt screen"
        );
        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected list request, got {other:?}"),
        }
    }

    #[test]
    fn projects_overlay_provider_disconnect_closes_overlay_and_disables_reopen() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );
        let (overlay_request_tx, mut overlay_request_rx) =
            unbounded_channel::<crate::ProjectsOverlayRequest>();
        app.projects_overlay_request_tx = Some(overlay_request_tx);

        app.bottom_pane.set_task_running(false);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );
        assert!(
            app.projects_overlay.is_open(),
            "expected Ctrl+L to open overlay before disconnect"
        );
        match overlay_request_rx.try_recv() {
            Ok(crate::ProjectsOverlayRequest::List) => {}
            other => panic!("expected list request, got {other:?}"),
        }

        app.handle_projects_overlay_provider_disconnected(crate::tui::FrameRequester::test_dummy());
        assert!(
            !app.projects_overlay.is_open(),
            "expected disconnect to close overlay"
        );
        assert!(
            app.projects_overlay_request_tx.is_none(),
            "expected disconnect to clear overlay provider"
        );

        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );
        assert!(
            !app.projects_overlay.is_open(),
            "expected Ctrl+L to stay unavailable after disconnect"
        );
        assert!(
            overlay_request_rx.try_recv().is_err(),
            "expected no further overlay requests after disconnect"
        );
    }

    #[test]
    fn prompt_slash_compact_kb_inserts_prompt_without_submission() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );

        app.bottom_pane.set_task_running(false);
        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/compact-kb".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.prompt_action, None);
        assert_eq!(app.bottom_pane.composer().current_text(), COMPACT_KB_PROMPT);

        let (_log_id, entry_count) = app.prompt_history.metadata();
        assert_eq!(entry_count, 0);

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Op::Interrupt in prompt screen"
        );
    }

    #[test]
    fn prompt_ctrl_c_empty_cancels_without_interrupt() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        {
            let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new_prompt_screen(
                app_event_tx,
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                true,
                Verbosity::default(),
            );

            app.bottom_pane.set_task_running(false);
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );

            assert_eq!(app.prompt_action, Some(PromptScreenAction::CancelledByUser));

            let mut saw_interrupt = false;
            while let Ok(ev) = rx_app.try_recv() {
                if let AppEvent::CodexOp(Op::Interrupt) = ev {
                    saw_interrupt = true;
                    break;
                }
            }
            assert!(
                !saw_interrupt,
                "did not expect Op::Interrupt in prompt screen"
            );
        }

        {
            let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new_prompt_screen(
                app_event_tx,
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                true,
                Verbosity::default(),
            );

            app.bottom_pane.set_task_running(false);
            app.handle_key_event(
                KeyEvent::new(
                    KeyCode::Char('C'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );

            assert_eq!(app.prompt_action, Some(PromptScreenAction::CancelledByUser));

            let mut saw_interrupt = false;
            while let Ok(ev) = rx_app.try_recv() {
                if let AppEvent::CodexOp(Op::Interrupt) = ev {
                    saw_interrupt = true;
                    break;
                }
            }
            assert!(
                !saw_interrupt,
                "did not expect Op::Interrupt in prompt screen"
            );
        }
    }

    #[test]
    fn prompt_ctrl_d_empty_cancels_without_interrupt() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );

        app.bottom_pane.set_task_running(false);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.prompt_action, Some(PromptScreenAction::CancelledByUser));

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Op::Interrupt in prompt screen"
        );
    }

    #[test]
    fn prompt_screen_yolo_selection_updates_footer_indicator() {
        {
            let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let mut bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
                PathBuf::from("project"),
                Some("main".to_string()),
            ));
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new_prompt_screen(
                app_event_tx,
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                true,
                Verbosity::default(),
            );

            assert!(!app.bottom_pane.prompt_footer_context().yolo_active);

            app.apply_persisted_yolo_to_prompt_footer(true);
            assert!(app.bottom_pane.prompt_footer_context().yolo_active);

            app.apply_persisted_yolo_to_prompt_footer(false);
            assert!(!app.bottom_pane.prompt_footer_context().yolo_active);
        }

        {
            let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
            let app_event_tx = AppEventSender::new(tx_raw);

            let mut bottom_pane = BottomPane::new(BottomPaneParams {
                frame_requester: crate::tui::FrameRequester::test_dummy(),
                enhanced_keys_supported: false,
                app_event_tx: app_event_tx.clone(),
                animations_enabled: false,
                placeholder_text: "Assign new task to CodexPotter".to_string(),
                disable_paste_burst: false,
            });
            bottom_pane.set_prompt_footer_context(
                PromptFooterContext::new(PathBuf::from("project"), Some("main".to_string()))
                    .with_yolo_cli_override(true)
                    .with_yolo_active(true),
            );
            let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
            let mut app = RenderAppState::new_prompt_screen(
                app_event_tx,
                bottom_pane,
                crate::prompt_history_store::PromptHistoryStore::new(),
                file_search,
                true,
                Verbosity::default(),
            );

            app.apply_persisted_yolo_to_prompt_footer(false);

            assert!(app.bottom_pane.prompt_footer_context().yolo_active);
        }
    }

    #[test]
    fn yolo_default_notice_messages() {
        let enabled = RenderAppState::yolo_default_notice(true);
        assert_eq!(
            lines_to_plain_text(&enabled.display_lines(u16::MAX)),
            "⚠ YOLO is now persisted in config and will apply to all subsequent sessions.\n"
        );

        let disabled = RenderAppState::yolo_default_notice(false);
        assert_eq!(
            lines_to_plain_text(&disabled.display_lines(u16::MAX)),
            "• YOLO is disabled by default.\n"
        );
    }

    #[test]
    fn prompt_enter_submits_prompt_and_records_history() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );

        app.bottom_pane.set_task_running(false);
        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "hello".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(
            app.prompt_action,
            Some(PromptScreenAction::Submitted("hello".to_string()))
        );
        assert!(
            app.queued_user_messages.is_empty(),
            "prompt screen should not queue prompts"
        );

        let (log_id, entry_count) = app.prompt_history.metadata();
        assert_eq!(entry_count, 1);
        assert_eq!(
            app.prompt_history.lookup_text(log_id, 0),
            Some("hello".to_string())
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Op::Interrupt in prompt screen"
        );
    }

    #[test]
    fn round_renderer_idle_prompt_is_separated_from_transcript_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;

        let transient_lines = app.build_transient_lines(width);

        let pane_height = app.bottom_pane.desired_height(width).max(1);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        let history_lines = vec![
            Line::from(
                "─ Worked for 4m 59s ──────────────────────────────────────────────────────",
            ),
            Line::from(""),
            Line::from("• ok"),
        ];
        let history_height = u16::try_from(history_lines.len()).unwrap_or(u16::MAX);
        let height = history_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let history_height = history_height.min(area.height);
                let history_area = Rect::new(area.x, area.y, area.width, history_height);
                let viewport_area = Rect::new(
                    area.x,
                    area.y + history_height,
                    area.width,
                    area.height.saturating_sub(history_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(history_lines))
                    .render(history_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &app.bottom_pane,
                    transient_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_idle_prompt_is_separated_from_transcript_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_round_banner_does_not_add_extra_padding_before_status_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);
        bottom_pane.set_project_started_at(Some(Instant::now()));
        bottom_pane.set_status_header_prefix(Some("Round 1/10".to_string()));
        if let Some(status) = bottom_pane.status_indicator_mut() {
            // Ensure the elapsed timer stays at 0s for a stable snapshot.
            status.pause_timer_at(Instant::now());
        }
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;

        let transient_lines = app.build_transient_lines(width);

        let pane_height = app.bottom_pane.desired_height(width).max(1);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        let history_lines = crate::history_cell_potter::new_potter_round_marker(
            1,
            10,
            "gpt-5.2",
            Some(codex_protocol::openai_models::ReasoningEffort::XHigh),
            None,
        )
        .display_lines(width);
        let history_height = u16::try_from(history_lines.len()).unwrap_or(u16::MAX);
        let height = history_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let history_height = history_height.min(area.height);
                let history_area = Rect::new(area.x, area.y, area.width, history_height);
                let viewport_area = Rect::new(
                    area.x,
                    area.y + history_height,
                    area.width,
                    area.height.saturating_sub(history_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(history_lines))
                    .render(history_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &app.bottom_pane,
                    transient_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_round_banner_padding_before_status_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_round_banner_reconnecting_status_renders_details_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);
        bottom_pane.set_project_started_at(Some(Instant::now()));
        bottom_pane.set_status_header_prefix(Some("Round 1/10".to_string()));
        bottom_pane.update_status_header_with_details(
            "Reconnecting... 1/5".to_string(),
            Some(
                "stream disconnected before completion: error sending request for url (https://free.xxsxx.fun/v1/responses)"
                    .to_string(),
            ),
        );
        if let Some(status) = bottom_pane.status_indicator_mut() {
            // Ensure the elapsed timer stays at 0s for a stable snapshot.
            status.pause_timer_at(Instant::now());
        }
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;
        app.potter_stream_recovery_retry_cell =
            Some(crate::history_cell_potter::PotterStreamRecoveryRetryCell {
                attempt: 3,
                max_attempts: 10,
                error_message: "stream disconnected before completion: error sending request for url (https://free.xxsxx.fun/v1/responses)".to_string(),
            });

        let transient_lines = app.build_transient_lines(width);

        let pane_height = app.bottom_pane.desired_height(width).max(1);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        let history_lines = crate::history_cell_potter::new_potter_round_marker(
            1,
            10,
            "gpt-5.2",
            Some(codex_protocol::openai_models::ReasoningEffort::XHigh),
            None,
        )
        .display_lines(width);
        let history_height = u16::try_from(history_lines.len()).unwrap_or(u16::MAX);
        let height = history_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let history_height = history_height.min(area.height);
                let history_area = Rect::new(area.x, area.y, area.width, history_height);
                let viewport_area = Rect::new(
                    area.x,
                    area.y + history_height,
                    area.width,
                    area.height.saturating_sub(history_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(history_lines))
                    .render(history_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &app.bottom_pane,
                    transient_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_round_banner_reconnecting_status_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn prompt_idle_prompt_is_separated_from_transcript_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        let transient_lines = vec![Line::from("")];

        let pane_height = bottom_pane.desired_height(width).max(1);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        let history_lines = vec![Line::from("• ok")];
        let history_height = u16::try_from(history_lines.len()).unwrap_or(u16::MAX);
        let height = history_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let history_height = history_height.min(area.height);
                let history_area = Rect::new(area.x, area.y, area.width, history_height);
                let viewport_area = Rect::new(
                    area.x,
                    area.y + history_height,
                    area.width,
                    area.height.saturating_sub(history_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(history_lines))
                    .render(history_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &bottom_pane,
                    transient_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "prompt_idle_prompt_is_separated_from_transcript_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_renders_context_compacted_event() {
        let width: u16 = 80;
        let height: u16 = 12;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("Explain this: `1 + 1`.");

        let configured = SessionConfiguredEvent {
            session_id: ThreadId::new(),
            forked_from_id: None,
            model: "test-model".to_string(),
            model_provider_id: "test-provider".to_string(),
            service_tier: None,
            cwd: PathBuf::from("project"),
            reasoning_effort: None,
            history_log_id: 0,
            history_entry_count: 0,
            initial_messages: None,
            rollout_path: PathBuf::from("rollout.jsonl"),
        };

        proc.handle_codex_event(Event {
            id: "session".into(),
            msg: EventMsg::SessionConfigured(configured),
        });

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "context-compacted".into(),
            msg: EventMsg::ContextCompacted(ContextCompactedEvent),
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_context_compacted_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_does_not_duplicate_agent_message_on_turn_complete_last_agent_message() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "hello".to_string(),
                phase: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("hello".to_string()),
            }),
        });

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(cells, vec![vec!["• hello".to_string()]]);
    }

    #[test]
    fn round_renderer_renders_turn_complete_last_agent_message_when_no_agent_message_emitted() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("hello".to_string()),
            }),
        });

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(cells, vec![vec!["• hello".to_string()]]);
    }

    #[test]
    fn round_renderer_minimal_commentary_turn_uses_turn_complete_last_agent_message_as_final() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "commentary".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx, width).is_empty(),
            "expected commentary agent message to be suppressed in minimal mode"
        );

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("final".to_string()),
            }),
        });

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(cells, vec![vec!["• final".to_string()]]);
    }

    #[test]
    fn round_renderer_renders_history_cells() {
        let width: u16 = 80;

        {
            let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

            let write_root = synthetic_absolute_path_buf(&["Users", "me", "project"]);
            proc.handle_codex_event(Event {
                id: "request-permissions".into(),
                msg: EventMsg::RequestPermissions(RequestPermissionsEvent {
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
                            write: Some(vec![write_root.clone()]),
                        }),
                    },
                }),
            });

            let cell = recv_inserted_history_cell(&mut rx);
            assert_eq!(
                lines_to_plain_text(&cell.display_lines(width)),
                format!(
                    "• Requested permissions\n  └ Reason: Select a workspace root\n    Network: enabled\n    FileSystem write: {}\n",
                    write_root.as_path().display()
                )
            );
        }

        {
            let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

            proc.handle_codex_event(Event {
                id: "hook-started".into(),
                msg: EventMsg::HookStarted(HookStartedEvent {
                    turn_id: Some("turn-1".to_string()),
                    run: HookRunSummary {
                        id: "hook-run-1".to_string(),
                        event_name: HookEventName::SessionStart,
                        handler_type: HookHandlerType::Command,
                        execution_mode: HookExecutionMode::Sync,
                        scope: HookScope::Thread,
                        source_path: PathBuf::from("hooks/session_start.sh"),
                        display_order: 0,
                        status: HookRunStatus::Running,
                        status_message: Some("Setting up environment".to_string()),
                        started_at: 0,
                        completed_at: None,
                        duration_ms: None,
                        entries: Vec::new(),
                    },
                }),
            });

            let cell = recv_inserted_history_cell(&mut rx);
            assert_snapshot!(
                "round_renderer_hook_started",
                lines_to_plain_text(&cell.display_lines(width))
            );
        }

        {
            let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

            proc.handle_codex_event(Event {
                id: "hook-completed".into(),
                msg: EventMsg::HookCompleted(HookCompletedEvent {
                    turn_id: None,
                    run: HookRunSummary {
                        id: "hook-run-1".to_string(),
                        event_name: HookEventName::SessionStart,
                        handler_type: HookHandlerType::Command,
                        execution_mode: HookExecutionMode::Sync,
                        scope: HookScope::Thread,
                        source_path: PathBuf::from("hooks/session_start.sh"),
                        display_order: 0,
                        status: HookRunStatus::Completed,
                        status_message: None,
                        started_at: 0,
                        completed_at: Some(5),
                        duration_ms: Some(5),
                        entries: vec![
                            HookOutputEntry {
                                kind: HookOutputEntryKind::Warning,
                                text: "fallback value used".to_string(),
                            },
                            HookOutputEntry {
                                kind: HookOutputEntryKind::Feedback,
                                text: "consider adding more logging".to_string(),
                            },
                            HookOutputEntry {
                                kind: HookOutputEntryKind::Context,
                                text: "exported CODEX_HOME".to_string(),
                            },
                            HookOutputEntry {
                                kind: HookOutputEntryKind::Error,
                                text: "failed to warm cache".to_string(),
                            },
                            HookOutputEntry {
                                kind: HookOutputEntryKind::Stop,
                                text: "hook requested stop".to_string(),
                            },
                        ],
                    },
                }),
            });

            let cell = recv_inserted_history_cell(&mut rx);
            assert_snapshot!(
                "round_renderer_hook_completed",
                lines_to_plain_text(&cell.display_lines(width))
            );
        }

        {
            let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

            proc.handle_codex_event(Event {
                id: "request-user-input".into(),
                msg: EventMsg::RequestUserInput(RequestUserInputEvent {
                    call_id: "call-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    questions: vec![RequestUserInputQuestion {
                        id: "confirm_path".to_string(),
                        header: "Confirm".to_string(),
                        question: "Proceed with the plan?".to_string(),
                        is_other: true,
                        is_secret: false,
                        options: Some(vec![
                            RequestUserInputQuestionOption {
                                label: "Yes (Recommended)".to_string(),
                                description: "Continue the current plan.".to_string(),
                            },
                            RequestUserInputQuestionOption {
                                label: "No".to_string(),
                                description: "Stop and revisit the approach.".to_string(),
                            },
                        ]),
                    }],
                }),
            });

            let cell = recv_inserted_history_cell(&mut rx);
            assert_snapshot!(
                "round_renderer_request_user_input",
                lines_to_plain_text(&cell.display_lines(width))
            );
        }

        {
            let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

            proc.handle_codex_event(Event {
                id: "elicitation".into(),
                msg: EventMsg::ElicitationRequest(ElicitationRequestEvent {
                    turn_id: Some("turn-1".to_string()),
                    server_name: "mcp-server".to_string(),
                    id: McpRequestId::String("req-1".to_string()),
                    request: Some(ElicitationRequest::Url {
                        meta: None,
                        message: "Please log in.".to_string(),
                        url: "https://example.com/auth".to_string(),
                        elicitation_id: "elicitation-1".to_string(),
                    }),
                    message: None,
                }),
            });

            let cell = recv_inserted_history_cell(&mut rx);
            assert_snapshot!(
                "round_renderer_elicitation_request",
                lines_to_plain_text(&cell.display_lines(width))
            );
        }

        {
            let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

            proc.handle_codex_event(Event {
                id: "guardian".into(),
                msg: EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                    id: "assessment-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    status: GuardianAssessmentStatus::Approved,
                    risk_score: Some(15),
                    risk_level: Some(GuardianRiskLevel::Low),
                    rationale: Some("Looks safe.".to_string()),
                    action: None,
                }),
            });

            let cell = recv_inserted_history_cell(&mut rx);
            assert_snapshot!(
                "round_renderer_guardian_assessment",
                lines_to_plain_text(&cell.display_lines(width))
            );
        }
    }

    #[test]
    fn round_renderer_minimal_keeps_completed_agent_message_pending_in_transient_lines() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "hello".to_string(),
                phase: None,
            }),
        });

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected completed message to stay pending in minimal mode; got: {cells:?}"
        );

        let transient_lines = app.build_transient_lines(width);
        assert_line_with_text_dimmed(&transient_lines, "hello", true);
    }

    #[test]
    fn round_renderer_minimal_renders_commentary_agent_message_in_transient_lines() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "turn-started".into(),
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: None,
                }),
            },
        )
        .expect("handle turn started");

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "commentary-delta".into(),
                msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    delta: "**Inspecting**".to_string(),
                }),
            },
        )
        .expect("handle commentary delta");

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            !transient_blob.contains("Inspecting"),
            "expected minimal mode to keep streamed commentary out of transient transcript preview: {transient_blob:?}"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "commentary".into(),
                msg: EventMsg::AgentMessage(AgentMessageEvent {
                    message: "**Inspecting**\n\nWorking...".to_string(),
                    phase: Some(MessagePhase::Commentary),
                }),
            },
        )
        .expect("handle commentary agent message");

        assert_eq!(app.bottom_pane.status_header(), "Working");
        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected minimal mode to suppress commentary agent message history cells"
        );
        assert_line_with_text_dimmed(&app.build_transient_lines(width), "Inspecting", true);
    }

    #[test]
    fn round_renderer_simple_keeps_commentary_in_transcript_history() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Simple);

        app.processor.handle_codex_event(Event {
            id: "commentary-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "**Inspecting**".to_string(),
            }),
        });
        app.processor.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Inspecting**\n\nWorking...".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells
                .iter()
                .flatten()
                .any(|line| line.contains("Inspecting")),
            "expected Simple mode commentary in transcript history: {cells:?}"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            !transient_blob.contains("Inspecting"),
            "expected Simple mode commentary to keep the original non-transient rendering: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_commentary_preview_replaces_previous_commentary() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        for (id, message) in [
            ("commentary-1", "**Inspecting**\n\nFirst pass"),
            ("commentary-2", "**Patching**\n\nSecond pass"),
        ] {
            app.processor.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::AgentMessage(AgentMessageEvent {
                    message: message.to_string(),
                    phase: Some(MessagePhase::Commentary),
                }),
            });
        }

        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected commentary preview to stay transient-only"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("Patching"),
            "expected latest commentary preview to be visible: {transient_blob:?}"
        );
        assert!(
            !transient_blob.contains("Inspecting"),
            "expected older commentary preview to be replaced: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_commentary_does_not_leave_stale_lines_in_inline_viewport() {
        let width: u16 = 80;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "commentary-1".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Inspecting**\n\nFirst commentary body".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });
        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected commentary preview to stay transient-only"
        );
        draw_inline_runner_frame(&mut terminal, &mut app);

        app.processor.handle_codex_event(Event {
            id: "commentary-2".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Patching**\n\nSecond commentary body with more lines\n\nLine three"
                    .to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });
        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected replacement commentary preview to stay transient-only"
        );
        draw_inline_runner_frame(&mut terminal, &mut app);

        let screen = terminal.backend().vt100().screen().contents();
        assert!(
            screen.contains("Second commentary body with more lines"),
            "expected latest commentary body in viewport: {screen:?}"
        );
        assert!(
            !screen.contains("Inspecting"),
            "expected replaced commentary to be cleared instead of lingering in transcript: {screen:?}"
        );
        assert!(
            !screen.contains("First commentary body"),
            "expected replaced commentary body to be cleared instead of lingering in transcript: {screen:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_compact_patch_preview_does_not_leave_stale_blocks_in_inline_viewport()
    {
        let width: u16 = 80;
        let height: u16 = 12;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        let mut changes_a: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_a.insert(
            PathBuf::from("a.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                move_path: None,
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-a-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-a".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_a,
            }),
        });
        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected first compact patch preview to stay transient-only"
        );
        draw_inline_runner_frame(&mut terminal, &mut app);

        let mut changes_b: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_b.insert(
            PathBuf::from("b.txt"),
            codex_protocol::protocol::FileChange::Add {
                content: "new\n".to_string(),
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-b-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-b".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_b,
            }),
        });
        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected coalesced compact patch preview to stay transient-only"
        );
        draw_inline_runner_frame(&mut terminal, &mut app);

        let screen = terminal.backend().vt100().screen().contents();
        assert!(
            screen.contains("• Changed 2 files (+2 -1)"),
            "expected current coalesced patch preview in viewport: {screen:?}"
        );
        assert!(
            !screen.contains("• Edited a.txt (+1 -1)"),
            "expected stale one-file patch preview to be cleared instead of lingering in transcript: {screen:?}"
        );
        assert!(
            screen.contains("└ Edited a.txt (+1 -1)"),
            "expected current coalesced preview to retain file list ordering: {screen:?}"
        );
        assert!(
            screen.contains("Added b.txt (+1 -0)"),
            "expected current coalesced preview to include latest file: {screen:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_final_agent_message_clears_commentary_preview() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Inspecting**\n\nWorking...".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        app.processor.handle_codex_event(Event {
            id: "final".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "done".to_string(),
                phase: None,
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected final agent message to remain pending in minimal mode"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("done"),
            "expected pending final agent message to be visible: {transient_blob:?}"
        );
        assert!(
            !transient_blob.contains("Inspecting"),
            "expected commentary preview to clear once final agent message arrives: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_context_compacted_clears_commentary_preview() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Inspecting**\n\nReviewing previous turns".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        app.processor.handle_codex_event(Event {
            id: "context-compacted".into(),
            msg: EventMsg::ContextCompacted(ContextCompactedEvent),
        });

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells
                .iter()
                .flatten()
                .any(|line| line.contains("Context compacted")),
            "expected context compacted message in transcript history: {cells:?}"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            !transient_blob.contains("Inspecting"),
            "expected commentary preview to clear once context compacted enters transcript: {transient_blob:?}"
        );
        assert!(
            !transient_blob.contains("Reviewing previous turns"),
            "expected commentary body to clear once context compacted enters transcript: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_turn_aborted_clears_commentary_preview() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Inspecting**\n\nWorking...".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        app.processor.handle_codex_event(Event {
            id: "turn-aborted".into(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                reason: TurnAbortReason::Interrupted,
            }),
        });

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert_eq!(cells.len(), 1, "expected exactly one interrupt error cell");
        assert!(
            cells[0].iter().any(|line| line
                .contains("Conversation interrupted - tell the model what to do differently.")),
            "expected interrupt error cell content; got: {cells:?}"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            !transient_blob.contains("Inspecting"),
            "expected interrupted rounds to clear commentary preview: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_turn_aborted_discards_incomplete_streamed_agent_message() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "agent-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                // Phase is unknown until the completed AgentMessage arrives, so interrupted
                // commentary streams must not leak into transcript history.
                delta: "inspect workspace".to_string(),
            }),
        });

        app.processor.handle_codex_event(Event {
            id: "turn-aborted".into(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                reason: TurnAbortReason::Interrupted,
            }),
        });

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert_eq!(cells.len(), 1, "expected exactly one interrupt error cell");
        assert!(
            cells[0].iter().any(|line| line
                .contains("Conversation interrupted - tell the model what to do differently.")),
            "expected interrupt error cell content; got: {cells:?}"
        );
        assert!(
            cells
                .iter()
                .flatten()
                .all(|line| !line.contains("inspect workspace")),
            "expected interrupted streamed agent text to be discarded: {cells:?}"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            !transient_blob.contains("inspect workspace"),
            "expected interrupted streamed agent text to be cleared from transient preview: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_commits_previous_agent_message_without_dimming_on_turn_complete() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "first-agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "first".to_string(),
                phase: None,
            }),
        });
        assert!(
            rx.try_recv().is_err(),
            "expected first message to stay pending"
        );

        proc.handle_codex_event(Event {
            id: "second-agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "second".to_string(),
                phase: None,
            }),
        });

        let first = recv_inserted_history_cell(&mut rx);
        assert_line_with_text_dimmed(&first.display_lines(width), "first", false);
        assert!(
            rx.try_recv().is_err(),
            "expected second message to remain pending until turn complete"
        );

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let second = recv_inserted_history_cell(&mut rx);
        assert_line_with_text_dimmed(&second.display_lines(width), "second", false);
    }

    #[test]
    fn round_renderer_minimal_flushes_pending_agent_message_before_compact_patch_preview() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "inspect".to_string(),
                phase: None,
            }),
        });

        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                move_path: None,
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        let agent_message = recv_inserted_history_cell(&mut rx_app);
        assert_line_with_text_dimmed(&agent_message.display_lines(width), "inspect", false);

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• Edited file.txt (+1 -1)"),
            "missing compact patch preview: {transient_blob:?}"
        );
        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected compact patch preview to remain transient-only"
        );
    }

    #[test]
    fn round_renderer_minimal_patch_preview_keeps_commentary_visible() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Inspecting**\n\nReviewing current progress".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                move_path: None,
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected commentary + patch preview to stay transient-only"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("Inspecting"),
            "expected commentary preview to remain visible across patch barrier: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains("Reviewing current progress"),
            "expected commentary body to remain visible across patch barrier: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains("Edited file.txt (+1 -1)"),
            "expected compact patch preview to stay visible: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_commentary_does_not_split_compact_patch_preview() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        let mut changes_a: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_a.insert(
            PathBuf::from("a.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                move_path: None,
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-a-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-a".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_a,
            }),
        });

        let mut changes_b: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_b.insert(
            PathBuf::from("b.txt"),
            codex_protocol::protocol::FileChange::Add {
                content: "new\n".to_string(),
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-b-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-b".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_b,
            }),
        });

        app.processor.handle_codex_event(Event {
            id: "commentary-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "Reviewing progress file".to_string(),
            }),
        });
        app.processor.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Reviewing**\n\nReviewing progress file".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        let mut changes_c: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_c.insert(
            PathBuf::from(".codexpotter/projects/2026/04/22/1/MAIN.md"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("older\n", "newer\n").to_string(),
                move_path: None,
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-c-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-c".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_c,
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected commentary not to flush compact patch preview into transcript history"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• Changed 3 files (+3 -2)"),
            "expected all patch changes to remain coalesced: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains("└ Edited a.txt (+1 -1)"),
            "missing first file entry: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains("Added b.txt (+1 -0)"),
            "missing second file entry: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains("Edited .codexpotter/projects/2026/04/22/1/MAIN.md (+1 -1)"),
            "missing later patch entry: {transient_blob:?}"
        );
        let last_patch_index = transient_blob
            .find("Edited .codexpotter/projects/2026/04/22/1/MAIN.md (+1 -1)")
            .expect("find later patch entry");
        let commentary_index = transient_blob
            .find("Reviewing progress file")
            .expect("find commentary line");
        assert!(
            last_patch_index < commentary_index,
            "expected commentary preview below compact patch preview: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_patch_barrier_does_not_commit_inflight_commentary_delta() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "commentary-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "Inspecting progress file".to_string(),
            }),
        });

        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from(".codexpotter/projects/2026/04/21/13/MAIN.md"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                move_path: None,
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        app.processor.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Inspecting**\n\nInspecting progress file".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells
                .iter()
                .flatten()
                .all(|line| !line.contains("Inspecting progress file")),
            "expected patch barrier not to commit in-flight commentary delta into transcript history: {cells:?}"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("Inspecting progress file"),
            "expected completed commentary preview to remain visible transiently: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains("Edited .codexpotter/projects/2026/04/21/13/MAIN.md"),
            "expected compact patch preview to remain visible transiently: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_collab_barrier_does_not_commit_inflight_commentary_delta() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);
        let sender_thread_id = ThreadId::new();
        let receiver_thread_id = ThreadId::new();

        app.processor.handle_codex_event(Event {
            id: "commentary-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "Inspecting collaborator state".to_string(),
            }),
        });

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "collab-wait".into(),
                msg: EventMsg::CollabWaitingBegin(CollabWaitingBeginEvent {
                    sender_thread_id,
                    receiver_thread_ids: vec![receiver_thread_id],
                    receiver_agents: vec![CollabAgentRef {
                        thread_id: receiver_thread_id,
                        agent_nickname: Some("Robie".to_string()),
                        agent_role: Some("explorer".to_string()),
                    }],
                    call_id: "call-wait".to_string(),
                }),
            },
        )
        .expect("handle collab waiting begin");

        app.processor.handle_codex_event(Event {
            id: "commentary".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "**Inspecting**\n\nInspecting collaborator state".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells
                .iter()
                .flatten()
                .all(|line| !line.contains("Inspecting collaborator state")),
            "expected collab barrier not to commit in-flight commentary delta into transcript history: {cells:?}"
        );
        assert!(
            cells
                .iter()
                .flatten()
                .any(|line| line.contains("Waiting for")),
            "expected collab transcript cell to remain visible: {cells:?}"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("Inspecting collaborator state"),
            "expected completed commentary preview to remain transient after collab barrier: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_flushes_pending_agent_message_before_stream_recovery_retry_block() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "inspect".to_string(),
                phase: None,
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected completed message to stay pending before stream recovery"
        );
        assert_line_with_text_dimmed(&app.build_transient_lines(width), "inspect", true);

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "recovery".into(),
                msg: EventMsg::PotterStreamRecoveryUpdate {
                    attempt: 1,
                    max_attempts: 10,
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                },
            },
        )
        .expect("handle stream recovery update");

        let agent_message = recv_inserted_history_cell(&mut rx_app);
        assert_line_with_text_dimmed(&agent_message.display_lines(width), "inspect", false);

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 1/10"),
            "missing stream recovery retry block: {transient_blob:?}"
        );
        assert!(
            !transient_blob.contains("inspect"),
            "expected pending agent preview to be cleared after recovery barrier: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_stream_recovery_retry_discards_incomplete_streamed_agent_message() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "agent-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                // Phase is unknown until the completed AgentMessage arrives, so retryable
                // recovery boundaries must not commit this partial text into transcript history.
                delta: "inspect workspace".to_string(),
            }),
        });

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "recovery".into(),
                msg: EventMsg::PotterStreamRecoveryUpdate {
                    attempt: 1,
                    max_attempts: 10,
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                },
            },
        )
        .expect("handle stream recovery update");

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells
                .iter()
                .flatten()
                .all(|line| !line.contains("inspect workspace")),
            "expected recovery barrier not to commit streamed agent text into transcript history: {cells:?}"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 1/10"),
            "missing stream recovery retry block: {transient_blob:?}"
        );
        assert!(
            !transient_blob.contains("inspect workspace"),
            "expected recovery barrier to clear in-flight agent text from transient preview: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_hides_live_streamed_agent_message_until_completion() {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "agent-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "inspect workspace".to_string(),
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected active streamed message to stay out of history before completion"
        );

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            !transient_blob.contains("inspect workspace"),
            "expected minimal mode to hide streamed agent text before completion: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_buffers_streamed_agent_message_until_completion_and_keeps_it_pending()
    {
        let width: u16 = 80;

        let (mut app, mut rx_app) = make_round_renderer_app(Verbosity::Minimal);

        app.processor.handle_codex_event(Event {
            id: "agent-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "done\n".to_string(),
            }),
        });

        app.processor.on_commit_tick();
        assert!(
            rx_app.try_recv().is_err(),
            "expected minimal mode to wait for completion before committing agent stream"
        );

        app.processor.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "done\n".to_string(),
                phase: None,
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx_app, width).is_empty(),
            "expected completed streamed message to remain pending"
        );

        let transient_lines = app.build_transient_lines(width);
        assert_line_with_text_dimmed(&transient_lines, "done", true);
    }

    #[test]
    fn round_renderer_minimal_turn_complete_promotes_streamed_agent_message_to_normal_history() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "agent-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "done\n".to_string(),
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "done\n".to_string(),
                phase: None,
            }),
        });

        assert!(
            rx.try_recv().is_err(),
            "expected streamed message to remain pending before turn complete"
        );

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let final_message = recv_inserted_history_cell(&mut rx);
        assert_line_with_text_dimmed(&final_message.display_lines(width), "done", false);
    }

    #[test]
    fn round_renderer_streamed_agent_message_completion_resets_stream_header() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        for (id, message) in [("first", "first\n"), ("second", "second\n")] {
            proc.handle_codex_event(Event {
                id: format!("{id}-delta"),
                msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    delta: message.to_string(),
                }),
            });

            proc.handle_codex_event(Event {
                id: format!("{id}-message"),
                msg: EventMsg::AgentMessage(AgentMessageEvent {
                    message: message.to_string(),
                    phase: None,
                }),
            });
        }

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(
            cells,
            vec![vec!["• first".to_string()], vec!["• second".to_string()],]
        );
    }

    #[test]
    fn round_renderer_streaming_plan_delta_renders_proposed_plan_block() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();
        proc.verbosity = Verbosity::Simple;

        proc.handle_codex_event(Event {
            id: "plan-1".into(),
            msg: EventMsg::PlanDelta(PlanDeltaEvent {
                delta: "- first\n".to_string(),
            }),
        });
        proc.on_commit_tick();

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(
            cells,
            vec![vec![
                "• Proposed Plan".to_string(),
                " ".to_string(),
                "   ".to_string(),
                "  - first".to_string(),
            ]]
        );

        proc.handle_codex_event(Event {
            id: "plan-2".into(),
            msg: EventMsg::PlanDelta(PlanDeltaEvent {
                delta: "- second\n".to_string(),
            }),
        });
        proc.on_commit_tick();

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(cells, vec![vec!["  - second".to_string()]]);

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(cells, vec![vec!["   ".to_string()]]);
    }

    #[test]
    fn round_renderer_minimal_suppresses_streaming_plan_delta() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();
        proc.verbosity = Verbosity::Minimal;

        proc.handle_codex_event(Event {
            id: "plan-1".into(),
            msg: EventMsg::PlanDelta(PlanDeltaEvent {
                delta: "- first\n".to_string(),
            }),
        });
        proc.on_commit_tick();

        assert!(
            drain_history_cell_strings(&mut rx, width).is_empty(),
            "expected minimal mode to suppress streamed plan deltas"
        );

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx, width).is_empty(),
            "expected minimal mode to suppress finalized proposed plan output"
        );
    }

    #[test]
    fn round_renderer_minimal_suppresses_plan_update() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();
        proc.verbosity = Verbosity::Minimal;

        proc.handle_codex_event(Event {
            id: "plan-update".into(),
            msg: EventMsg::PlanUpdate(UpdatePlanArgs {
                explanation: Some("inspect then patch".to_string()),
                plan: vec![
                    PlanItemArg {
                        step: "Inspect docs".to_string(),
                        status: StepStatus::Completed,
                    },
                    PlanItemArg {
                        step: "Patch renderer".to_string(),
                        status: StepStatus::InProgress,
                    },
                ],
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx, width).is_empty(),
            "expected minimal mode to suppress updated plan cells"
        );
    }

    #[test]
    fn round_renderer_minimal_discards_pending_plan_stream_on_turn_complete() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "plan-1".into(),
            msg: EventMsg::PlanDelta(PlanDeltaEvent {
                delta: "- first\n".to_string(),
            }),
        });

        proc.verbosity = Verbosity::Minimal;
        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        assert!(
            drain_history_cell_strings(&mut rx, width).is_empty(),
            "expected minimal mode to drop buffered plan stream on turn completion"
        );
    }

    #[tokio::test]
    async fn round_renderer_vt100_snapshots() {
        let width: u16 = 80;
        let height: u16 = 28;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("Explain this: `1 + 1`.");

        let configured = SessionConfiguredEvent {
            session_id: ThreadId::new(),
            forked_from_id: None,
            model: "test-model".to_string(),
            model_provider_id: "test-provider".to_string(),
            service_tier: None,
            cwd: PathBuf::from("project"),
            reasoning_effort: None,
            history_log_id: 0,
            history_entry_count: 0,
            initial_messages: None,
            rollout_path: PathBuf::from("rollout.jsonl"),
        };

        proc.handle_codex_event(Event {
            id: "session".into(),
            msg: EventMsg::SessionConfigured(configured),
        });

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Stream markdown output in a few chunks to exercise incremental rendering.
        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "## Result\n".into(),
            }),
        });
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "delta-2".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "- **Answer**: `2`\n".into(),
            }),
        });
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_streaming_partial_vt100",
            terminal.backend().vt100().screen().contents()
        );

        proc.handle_codex_event(Event {
            id: "delta-3".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "\n```sh\nprintf 'hello\\n'\n```\n".into(),
            }),
        });
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Finalize stream without a final AgentMessage, matching the streaming-only code path.
        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Exec output should render with truncation.
        let command = vec!["bash".into(), "-lc".into(), "printf 'line\\n'".into()];
        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command,
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: (1..=30).map(|i| format!("line {i}\n")).collect::<String>(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Patch apply should render a diff summary.
        let patch = diffy::create_patch("old\n", "new\n").to_string();
        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("example.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: patch,
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        // In `Verbosity::Minimal` successful patch applications are buffered and rendered as a
        // coalesced compact summary. Flush explicitly so the snapshot includes the patch cell.
        proc.flush_pending_compact_patch_changes();
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_end_to_end_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_esc_interrupt_flow_vt100() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let width: u16 = 80;
        let height: u16 = 12;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);

        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.processor.emit_user_prompt("test prompt".to_string());
        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        app.handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            width,
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(saw_interrupt, "expected Esc to request Op::Interrupt");

        app.processor.handle_codex_event(Event {
            id: "turn-aborted".into(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                reason: TurnAbortReason::Interrupted,
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_esc_interrupt_flow_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_inserts_worked_for_separator_before_agent_message_vt100() {
        let width: u16 = 80;
        let height: u16 = 16;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        proc.verbosity = Verbosity::Simple;

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Simulate a successful command: it should coalesce into the "Ran" cell.
        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });

        // No cells should be emitted yet; the Ran cell is buffered until the next non-exec output.
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Start agent output; this should flush the buffered Ran cell and insert the separator.
        proc.current_elapsed_secs = Some(0);
        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "ok\n".into(),
            }),
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_worked_for_separator_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_minimal_suppresses_worked_for_separator_before_agent_message_vt100() {
        let width: u16 = 80;
        let height: u16 = 16;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        proc.verbosity = Verbosity::Minimal;

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.current_elapsed_secs = Some(0);
        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "ok\n".into(),
            }),
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_minimal_suppresses_worked_for_separator_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_minimal_replay_fixture_suppresses_worked_for_separator_vt100() {
        let width: u16 = 80;
        let height: u16 = 24;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        proc.verbosity = Verbosity::Minimal;
        proc.current_elapsed_secs = Some(0);

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        let fixture = include_str!("../tests/fixtures/minimal-worked-for-suppressed.jsonl");
        for (idx, line) in fixture.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("invalid fixture event at line {}: {error}", idx + 1)
            });
            proc.handle_codex_event(event);
            drain_render_history_events(
                &mut rx,
                &mut terminal,
                width,
                &mut has_emitted_history_lines,
            );
        }

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        let contents = terminal.backend().vt100().screen().contents();
        assert!(
            !contents.contains("Worked for"),
            "expected minimal replay to suppress worked-for separators"
        );

        assert_snapshot!(
            "round_renderer_minimal_replay_fixture_suppresses_worked_for_separator_vt100",
            contents
        );
    }

    #[tokio::test]
    async fn round_renderer_flushes_agent_stream_before_ran_vt100() {
        let width: u16 = 80;
        let height: u16 = 16;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        proc.verbosity = Verbosity::Simple;
        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "first message.".into(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "echo tool".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "delta-2".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "second message.".into(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_flushes_agent_stream_before_ran_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_renders_potter_project_succeeded_block_vt100() {
        let width: u16 = 80;
        let height: u16 = 24;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        proc.potter_resume_command_global_args =
            vec!["--sandbox".to_string(), "read-only".to_string()];
        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.current_elapsed_secs = Some(0);
        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "- Finished the project.\n".into(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });
        proc.handle_codex_event(Event {
            id: "potter-succeeded".into(),
            msg: EventMsg::PotterProjectSucceeded {
                rounds: 4,
                duration: Duration::from_secs(24 * 60 + 34),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/02/01/11/MAIN.md"),
                git_commit_start: String::from("fb827a203635875b58d7e6792da84f22d723d41b"),
                git_commit_end: String::from("662d232cafebabedeadbeefdeadbeefdeadbeef"),
            },
        });
        proc.handle_codex_event(Event {
            id: "round-finished".into(),
            msg: EventMsg::PotterRoundFinished {
                outcome: codex_protocol::protocol::PotterRoundOutcome::Completed,
                duration_secs: 733,
            },
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_potter_project_succeeded_block_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_renders_potter_project_budget_exhausted_block_vt100() {
        let width: u16 = 80;
        let height: u16 = 24;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        proc.potter_resume_command_global_args =
            vec!["--sandbox".to_string(), "read-only".to_string()];
        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.current_elapsed_secs = Some(0);
        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "- Finished the project.\n".into(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });
        proc.handle_codex_event(Event {
            id: "potter-budget-exhausted".into(),
            msg: EventMsg::PotterProjectBudgetExhausted {
                rounds: 10,
                duration: Duration::from_secs(24 * 60 + 34),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/02/01/11/MAIN.md"),
                git_commit_start: String::from("fb827a203635875b58d7e6792da84f22d723d41b"),
                git_commit_end: String::from("662d232cafebabedeadbeefdeadbeefdeadbeef"),
            },
        });
        proc.handle_codex_event(Event {
            id: "round-finished".into(),
            msg: EventMsg::PotterRoundFinished {
                outcome: codex_protocol::protocol::PotterRoundOutcome::Completed,
                duration_secs: 0,
            },
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_potter_project_budget_exhausted_block_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_live_explored_renders_in_viewport_and_merges_calls_vt100() {
        let width: u16 = 80;

        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        // Simulate a burst of "exploring" exec results arriving over time.
        for (id, parsed_cmd) in [
            (
                "explore-1",
                vec![ParsedCommand::ListFiles {
                    cmd: "ls".into(),
                    path: None,
                }],
            ),
            (
                "explore-2",
                vec![ParsedCommand::ListFiles {
                    cmd: "ls -la".into(),
                    path: Some(".codexpotter".into()),
                }],
            ),
            (
                "explore-3",
                vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "README.md".into(),
                    path: PathBuf::from("README.md"),
                }],
            ),
            (
                "explore-4",
                vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "MAIN.md".into(),
                    path: PathBuf::from("MAIN.md"),
                }],
            ),
            (
                "explore-5",
                vec![ParsedCommand::Search {
                    cmd: "rg -n \"KeyCode::Tab\"".into(),
                    query: Some("KeyCode::Tab|\\\\bTab\\\\b".into()),
                    path: Some("cli".into()),
                }],
            ),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                    call_id: id.into(),
                    parsed_cmd,
                    ..base.clone()
                }),
            });
        }

        // No history cell events should have been emitted yet; explored output should render
        // live in the viewport instead.
        assert!(rx.try_recv().is_err());

        let Some(explored) = proc.pending_exploring_cell.as_ref() else {
            panic!("expected a pending explored cell");
        };
        let mut exploring_lines = Vec::new();
        exploring_lines.push(Line::from(""));
        exploring_lines.extend(explored.display_lines(width));

        let prompt_lines =
            history_cell::new_user_prompt("test prompt".to_string()).display_lines(width);

        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);
        if let Some(status) = bottom_pane.status_indicator_mut() {
            // Ensure the elapsed timer stays at 0s for a stable snapshot.
            status.pause_timer_at(Instant::now());
        }

        let pane_height = bottom_pane.desired_height(width).max(1);
        let exploring_height = u16::try_from(exploring_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(exploring_height);
        let prompt_height = u16::try_from(prompt_lines.len()).unwrap_or(u16::MAX);
        let height = prompt_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let prompt_height = prompt_height.min(area.height);
                let prompt_area =
                    ratatui::layout::Rect::new(area.x, area.y, area.width, prompt_height);
                let viewport_area = ratatui::layout::Rect::new(
                    area.x,
                    area.y + prompt_height,
                    area.width,
                    area.height.saturating_sub(prompt_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(prompt_lines))
                    .render(prompt_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &bottom_pane,
                    exploring_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_live_explored_in_viewport_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_live_explored_coalesces_reads_across_mixed_calls_vt100() {
        let width: u16 = 80;

        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        // Simulate a "mixed" exploring call that ends in a Read, followed by another
        // Read of the same file. The renderer should coalesce consecutive Read lines
        // even when they come from different calls.
        for (id, parsed_cmd) in [
            (
                "explore-1",
                vec![
                    ParsedCommand::ListFiles {
                        cmd: "ls -la".into(),
                        path: Some(".codexpotter".into()),
                    },
                    ParsedCommand::Read {
                        cmd: "sed -n '1,240p'".into(),
                        name: "resume_design.md".into(),
                        path: PathBuf::from(".codexpotter/resume_design.md"),
                    },
                ],
            ),
            (
                "explore-2",
                vec![ParsedCommand::Read {
                    cmd: "sed -n '240,520p'".into(),
                    name: "resume_design.md".into(),
                    path: PathBuf::from(".codexpotter/resume_design.md"),
                }],
            ),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                    call_id: id.into(),
                    parsed_cmd,
                    ..base.clone()
                }),
            });
        }

        assert!(rx.try_recv().is_err());

        let Some(explored) = proc.pending_exploring_cell.as_ref() else {
            panic!("expected a pending explored cell");
        };
        let mut exploring_lines = Vec::new();
        exploring_lines.push(Line::from(""));
        exploring_lines.extend(explored.display_lines(width));

        let prompt_lines =
            history_cell::new_user_prompt("test prompt".to_string()).display_lines(width);

        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);
        if let Some(status) = bottom_pane.status_indicator_mut() {
            status.pause_timer_at(Instant::now());
        }

        let pane_height = bottom_pane.desired_height(width).max(1);
        let exploring_height = u16::try_from(exploring_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(exploring_height);
        let prompt_height = u16::try_from(prompt_lines.len()).unwrap_or(u16::MAX);
        let height = prompt_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let prompt_height = prompt_height.min(area.height);
                let prompt_area =
                    ratatui::layout::Rect::new(area.x, area.y, area.width, prompt_height);
                let viewport_area = ratatui::layout::Rect::new(
                    area.x,
                    area.y + prompt_height,
                    area.width,
                    area.height.saturating_sub(prompt_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(prompt_lines))
                    .render(prompt_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &bottom_pane,
                    exploring_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_live_explored_coalesces_mixed_call_reads_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_ctrl_c_preserves_pending_explored_output_vt100() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyEventKind;
        use crossterm::event::KeyEventState;
        use crossterm::event::KeyModifiers;

        let width: u16 = 80;
        let height: u16 = 18;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 6, width, 6));

        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::Simple);
        proc.handle_codex_event(Event {
            id: "session-start".into(),
            msg: EventMsg::PotterProjectStarted {
                user_message: None,
                working_dir: PathBuf::from("project"),
                project_dir: PathBuf::from(".codexpotter/projects/2026/01/29/18"),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/01/29/18/MAIN.md"),
            },
        });
        proc.handle_codex_event(Event {
            id: "round-start".into(),
            msg: EventMsg::PotterRoundStarted {
                current: 1,
                total: 10,
            },
        });
        proc.handle_codex_event(Event {
            id: "session-configured".into(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: ThreadId::new(),
                forked_from_id: None,
                model: "gpt-5.2".to_string(),
                model_provider_id: "test-provider".to_string(),
                service_tier: None,
                cwd: PathBuf::from("project"),
                reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::XHigh),
                history_log_id: 0,
                history_entry_count: 0,
                initial_messages: None,
                rollout_path: PathBuf::from("rollout.jsonl"),
            }),
        });

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        proc.handle_codex_event(Event {
            id: "explore-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-1".into(),
                command: vec!["bash".into(), "-lc".into(), "ls -la".into()],
                parsed_cmd: vec![ParsedCommand::ListFiles {
                    cmd: "ls -la".into(),
                    path: None,
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-2".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-2".into(),
                command: vec![
                    "bash".into(),
                    "-lc".into(),
                    "rg -n README.md .codexpotter".into(),
                ],
                parsed_cmd: vec![ParsedCommand::Search {
                    cmd: "rg -n README.md .codexpotter".into(),
                    query: Some("README.md".into()),
                    path: Some(".codexpotter".into()),
                }],
                ..base
            }),
        });

        assert!(rx.try_recv().is_err());
        assert!(proc.pending_exploring_cell.is_some());

        let (codex_op_tx, _codex_op_rx) = unbounded_channel::<Op>();
        let file_search_dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let file_search = FileSearchManager::new(file_search_dir, app_event_tx.clone());
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let mut app = RenderAppState::new(
            proc,
            app_event_tx,
            Some(codex_op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_key_event(
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
            crate::tui::FrameRequester::test_dummy(),
            width,
        );

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        crate::terminal_cleanup::clear_inline_viewport_for_exit(&mut terminal)
            .expect("clear viewport");

        assert_snapshot!(
            "round_renderer_ctrl_c_preserves_pending_explored_output_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_coalesces_success_ran_cells_snapshot() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        for (id, inner) in [
            ("ran-1", "git status --porcelain=v1"),
            ("ran-2", "git --no-pager log -5 --oneline"),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                    call_id: id.into(),
                    command: vec!["bash".into(), "-lc".into(), inner.into()],
                    ..base.clone()
                }),
            });
        }

        // Coalesced Ran output should render live (not emitted as transcript history yet).
        assert!(rx.try_recv().is_err());

        let Some(cell) = proc.pending_success_ran_cell.as_ref() else {
            panic!("expected a pending Ran cell");
        };
        let lines = cell.display_lines(width);
        assert_snapshot!(
            "round_renderer_coalesced_success_ran_cells",
            lines_to_plain_strings(&lines).join("\n")
        );
    }

    #[test]
    fn round_renderer_minimal_hides_ran_cells() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        proc.handle_codex_event(Event {
            id: "ran-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "ran-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1),
                formatted_output: String::new(),
            }),
        });

        proc.handle_codex_event(Event {
            id: "ran-failed".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "ran-failed".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "false".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: "nope".into(),
                aggregated_output: String::new(),
                exit_code: 1,
                duration: std::time::Duration::from_millis(1),
                formatted_output: String::new(),
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [agent_message] = events.as_slice() else {
            panic!("expected agent message");
        };
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_coalesces_consecutive_patch_cells() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        for (id, patch) in [
            ("patch-1", diffy::create_patch("a\n", "b\n").to_string()),
            ("patch-2", diffy::create_patch("b\n", "c\n").to_string()),
        ] {
            let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> =
                HashMap::new();
            changes.insert(
                PathBuf::from("file.txt"),
                codex_protocol::protocol::FileChange::Update {
                    unified_diff: patch,
                    move_path: None,
                },
            );

            proc.handle_codex_event(Event {
                id: format!("{id}-begin"),
                msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                    call_id: id.into(),
                    turn_id: "turn-1".into(),
                    auto_approved: true,
                    changes: changes.clone(),
                }),
            });
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                    call_id: id.into(),
                    turn_id: "turn-1".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    success: true,
                    changes,
                }),
            });
        }

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [patch, agent_message] = events.as_slice() else {
            panic!("expected patch, then agent message");
        };
        pretty_assertions::assert_eq!(patch, &vec!["• Edited file.txt (+2 -2)".to_string()]);
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_coalesces_patch_cells_across_suppressed_events() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let mut changes_1: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_1.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("a\n", "b\n").to_string(),
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-1-begin".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                auto_approved: true,
                changes: changes_1.clone(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "patch-1-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_1,
            }),
        });

        proc.handle_codex_event(Event {
            id: "token-count".into(),
            msg: EventMsg::TokenCount(TokenCountEvent {
                info: None,
                rate_limits: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "thread-rolled-back".into(),
            msg: EventMsg::ThreadRolledBack(codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
            }),
        });

        proc.handle_codex_event(Event {
            id: "ran-failed".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "ran-failed".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "false".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: "nope".into(),
                aggregated_output: String::new(),
                exit_code: 1,
                duration: std::time::Duration::from_millis(1),
                formatted_output: String::new(),
            }),
        });

        let mut changes_2: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_2.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("b\n", "c\n").to_string(),
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-2-begin".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-2".into(),
                turn_id: "turn-1".into(),
                auto_approved: true,
                changes: changes_2.clone(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "patch-2-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-2".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_2,
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [patch, agent_message] = events.as_slice() else {
            panic!("expected patch, then agent message");
        };
        pretty_assertions::assert_eq!(patch, &vec!["• Edited file.txt (+2 -2)".to_string()]);
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_renders_pending_patch_summary_in_transient_lines() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::Minimal);
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        let mut changes_a: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_a.insert(
            PathBuf::from("a.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                move_path: None,
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-a-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-a".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_a,
            }),
        });

        let mut changes_b: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_b.insert(
            PathBuf::from("b.txt"),
            codex_protocol::protocol::FileChange::Add {
                content: "new\n".to_string(),
            },
        );
        app.processor.handle_codex_event(Event {
            id: "patch-b-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-b".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_b,
            }),
        });

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• Changed 2 files (+2 -1)"),
            "missing compact patch summary: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains("└ Edited a.txt (+1 -1)"),
            "missing file entry: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains("Added b.txt (+1 -0)"),
            "missing file entry: {transient_blob:?}"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected pending patch summary to be transient-only; got: {cells:?}"
        );
    }

    #[test]
    fn round_renderer_minimal_flushes_pending_patch_summary_on_turn_complete() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::Minimal);

        let patch = diffy::create_patch("a\n", "b\n").to_string();
        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: patch,
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(events, vec![vec!["• Edited file.txt (+1 -1)".to_string()]]);
    }

    #[test]
    fn round_renderer_minimal_coalesces_patch_cells_into_changed_file_list() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        // File list should preserve patch event ordering (not alphabetical ordering).
        let mut changes_b: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_b.insert(
            PathBuf::from("b.txt"),
            codex_protocol::protocol::FileChange::Add {
                content: "new\n".to_string(),
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-b-begin".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-b".into(),
                turn_id: "turn-1".into(),
                auto_approved: true,
                changes: changes_b.clone(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "patch-b-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-b".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_b,
            }),
        });

        let patch_a = diffy::create_patch("old\n", "new\n").to_string();
        let mut changes_a: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_a.insert(
            PathBuf::from("a.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: patch_a,
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-a-begin".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-a".into(),
                turn_id: "turn-1".into(),
                auto_approved: true,
                changes: changes_a.clone(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "patch-a-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-a".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_a,
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [patch, agent_message] = events.as_slice() else {
            panic!("expected patch, then agent message");
        };
        pretty_assertions::assert_eq!(
            patch,
            &vec![
                "• Changed 2 files (+2 -1)".to_string(),
                "  └ Added b.txt (+1 -0)".to_string(),
                "    Edited a.txt (+1 -1)".to_string(),
            ]
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[tokio::test]
    async fn round_renderer_coalesces_explored_cells() {
        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let _ = drain_history_cell_strings(&mut rx, 80);
        proc.verbosity = Verbosity::Simple;

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        proc.handle_codex_event(Event {
            id: "explore-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-1".into(),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "AGENTS.override.md".into(),
                    path: PathBuf::from("AGENTS.override.md"),
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-2".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-2".into(),
                parsed_cmd: vec![ParsedCommand::ListFiles {
                    cmd: "ls -la".into(),
                    path: Some(".codexpotter".into()),
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-3".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-3".into(),
                parsed_cmd: vec![ParsedCommand::ListFiles {
                    cmd: "ls -la".into(),
                    path: Some(".codexpotter".into()),
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-4".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-4".into(),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "MAIN.md".into(),
                    path: PathBuf::from("MAIN.md"),
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-5".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-5".into(),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "developer_prompt.md".into(),
                    path: PathBuf::from("developer_prompt.md"),
                }],
                ..base
            }),
        });

        // Any non-exec output should flush the buffered exploring cell.
        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, 80);
        let [explored, _separator, _agent_message] = events.as_slice() else {
            panic!("expected explored cell, separator, then agent message");
        };
        let rendered = explored.join("\n") + "\n";
        assert_snapshot!("round_renderer_coalesces_explored_cells", rendered);
    }

    #[tokio::test]
    async fn round_renderer_minimal_hides_explored_cells() {
        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let _ = drain_history_cell_strings(&mut rx, 80);

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        proc.handle_codex_event(Event {
            id: "explore-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-1".into(),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "AGENTS.override.md".into(),
                    path: PathBuf::from("AGENTS.override.md"),
                }],
                ..base
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, 80);
        let [agent_message] = events.as_slice() else {
            panic!("expected agent message");
        };
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[tokio::test]
    async fn round_renderer_flushes_explored_cells_on_turn_complete() {
        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let _ = drain_history_cell_strings(&mut rx, u16::MAX);
        proc.verbosity = Verbosity::Simple;

        proc.handle_codex_event(Event {
            id: "explore-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "AGENTS.override.md".into(),
                    path: PathBuf::from("AGENTS.override.md"),
                }],
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1),
                formatted_output: String::new(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        let [explored] = events.as_slice() else {
            panic!("expected exactly one explored cell");
        };
        let rendered = explored.join("\n") + "\n";
        assert_snapshot!(
            "round_renderer_flushes_explored_cells_on_turn_complete",
            rendered
        );
    }

    #[test]
    fn round_renderer_simple_renders_live_web_searches_in_transient_lines() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut proc = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::Simple);
        proc.emit_user_prompt("test prompt".to_string());
        let _ = drain_history_cell_strings(&mut rx, width);

        for (id, query) in [
            (
                "search-1",
                "'--label=' in\nhttps://docs.podman.io/en/latest/markdown/podman-create.1.html",
            ),
            (
                "search-2",
                "site:docs.podman.io/en/stable/markdown podman-logs official docs",
            ),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::WebSearchEnd(WebSearchEndEvent {
                    call_id: id.into(),
                    query: query.to_string(),
                }),
            });
        }

        assert!(rx.try_recv().is_err());

        let (codex_op_tx, _codex_op_rx) = unbounded_channel::<Op>();
        let file_search_dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let file_search = FileSearchManager::new(file_search_dir, app_event_tx.clone());
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);

        let mut app = RenderAppState::new(
            proc,
            app_event_tx,
            Some(codex_op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;

        pretty_assertions::assert_eq!(
            lines_to_plain_strings(&app.build_transient_lines(width)),
            vec![
                "".to_string(),
                "• Searched".to_string(),
                "  └ '--label=' in".to_string(),
                "    https://docs.podman.io/en/latest/markdown/podman-create.1.html".to_string(),
                "    site:docs.podman.io/en/stable/markdown podman-logs official docs".to_string(),
            ]
        );

        app.processor.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [searches, _separator, agent_message] = events.as_slice() else {
            panic!("expected web search cell, separator, then agent message");
        };
        pretty_assertions::assert_eq!(
            searches,
            &vec![
                "• Searched".to_string(),
                "  └ '--label=' in".to_string(),
                "    https://docs.podman.io/en/latest/markdown/podman-create.1.html".to_string(),
                "    site:docs.podman.io/en/stable/markdown podman-logs official docs".to_string(),
            ]
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_suppresses_web_search_cells() {
        let width: u16 = 80;
        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let _ = drain_history_cell_strings(&mut rx, width);
        proc.verbosity = Verbosity::Minimal;

        proc.handle_codex_event(Event {
            id: "search-1".into(),
            msg: EventMsg::WebSearchEnd(WebSearchEndEvent {
                call_id: "search-1".into(),
                query: "podman logs --follow".to_string(),
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [agent_message] = events.as_slice() else {
            panic!("expected only agent message in minimal mode");
        };
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_hides_viewed_images_from_transient_lines() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut proc = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::Minimal);
        proc.emit_user_prompt("test prompt".to_string());
        let _ = drain_history_cell_strings(&mut rx, width);

        for (id, path) in [
            ("view-image-1", "/tmp/slock_ui_05_invite_human_modal.png"),
            ("view-image-2", "/tmp/slock_ui_06_edit_channel_modal.png"),
            ("view-image-3", "/tmp/slock_ui_07_create_channel_modal.png"),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ViewImageToolCall(ViewImageToolCallEvent {
                    call_id: id.into(),
                    path: PathBuf::from(path),
                }),
            });
        }

        assert!(rx.try_recv().is_err());

        let (codex_op_tx, _codex_op_rx) = unbounded_channel::<Op>();
        let file_search_dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let file_search = FileSearchManager::new(file_search_dir, app_event_tx.clone());
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);

        let mut app = RenderAppState::new(
            proc,
            app_event_tx,
            Some(codex_op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;

        let transient_blob = lines_to_plain_text(&app.build_transient_lines(width));
        assert!(
            !transient_blob.contains("Viewed Image"),
            "expected minimal mode to hide viewed images; got: {transient_blob:?}"
        );
    }

    #[test]
    fn round_renderer_simple_renders_live_viewed_images_and_separator() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut proc = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::Simple);
        proc.emit_user_prompt("test prompt".to_string());
        let _ = drain_history_cell_strings(&mut rx, width);

        for (id, path) in [
            ("view-image-1", "/tmp/slock_ui_05_invite_human_modal.png"),
            ("view-image-2", "/tmp/slock_ui_06_edit_channel_modal.png"),
            ("view-image-3", "/tmp/slock_ui_07_create_channel_modal.png"),
            ("view-image-4", "/tmp/slock_ui_08_create_agent_modal.png"),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ViewImageToolCall(ViewImageToolCallEvent {
                    call_id: id.into(),
                    path: PathBuf::from(path),
                }),
            });
        }

        assert!(rx.try_recv().is_err());

        let (codex_op_tx, _codex_op_rx) = unbounded_channel::<Op>();
        let file_search_dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let file_search = FileSearchManager::new(file_search_dir, app_event_tx.clone());
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);

        let mut app = RenderAppState::new(
            proc,
            app_event_tx,
            Some(codex_op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;

        pretty_assertions::assert_eq!(
            lines_to_plain_strings(&app.build_transient_lines(width)),
            vec![
                "".to_string(),
                "• Viewed Image".to_string(),
                "  └ /tmp/slock_ui_05_invite_human_modal.png".to_string(),
                "    /tmp/slock_ui_06_edit_channel_modal.png".to_string(),
                "    /tmp/slock_ui_07_create_channel_modal.png".to_string(),
                "    /tmp/slock_ui_08_create_agent_modal.png".to_string(),
            ]
        );

        app.processor.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [viewed_images, _separator, agent_message] = events.as_slice() else {
            panic!("expected viewed image cell, separator, then agent message");
        };
        pretty_assertions::assert_eq!(
            viewed_images,
            &vec![
                "• Viewed Image".to_string(),
                "  └ /tmp/slock_ui_05_invite_human_modal.png".to_string(),
                "    /tmp/slock_ui_06_edit_channel_modal.png".to_string(),
                "    /tmp/slock_ui_07_create_channel_modal.png".to_string(),
                "    /tmp/slock_ui_08_create_agent_modal.png".to_string(),
            ]
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_suppresses_viewed_image_cells_on_turn_complete() {
        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let _ = drain_history_cell_strings(&mut rx, u16::MAX);
        proc.verbosity = Verbosity::Minimal;

        for (id, path) in [
            ("view-image-1", "/tmp/slock_ui_05_invite_human_modal.png"),
            ("view-image-2", "/tmp/slock_ui_06_edit_channel_modal.png"),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ViewImageToolCall(ViewImageToolCallEvent {
                    call_id: id.into(),
                    path: PathBuf::from(path),
                }),
            });
        }

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        assert!(
            events.is_empty(),
            "expected minimal mode to suppress viewed images; got: {events:?}"
        );
    }

    #[test]
    fn round_renderer_viewed_image_does_not_flush_agent_stream_in_minimal() {
        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();
        proc.verbosity = Verbosity::Minimal;

        proc.handle_codex_event(Event {
            id: "agent-delta".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "first message.".into(),
            }),
        });
        assert!(
            rx.try_recv().is_err(),
            "expected minimal mode to keep agent deltas buffered"
        );

        proc.handle_codex_event(Event {
            id: "view-image-1".into(),
            msg: EventMsg::ViewImageToolCall(ViewImageToolCallEvent {
                call_id: "view-image-1".into(),
                path: PathBuf::from("/tmp/slock_ui_05_invite_human_modal.png"),
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        let [agent_message] = events.as_slice() else {
            panic!("expected agent stream flush on turn complete only");
        };
        pretty_assertions::assert_eq!(agent_message, &vec!["• first message.".to_string()]);
    }

    #[test]
    fn round_renderer_potter_project_started_emits_user_prompt() {
        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "potter-project-started".into(),
            msg: EventMsg::PotterProjectStarted {
                user_message: Some("test prompt".to_string()),
                working_dir: PathBuf::from("/workdir"),
                project_dir: PathBuf::from(".codexpotter/projects/2026/01/29/11"),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/01/29/11/MAIN.md"),
            },
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        let [prompt, project_hint] = events.as_slice() else {
            panic!("expected prompt cell followed by project hint cell");
        };

        let prompt_rendered = prompt.join("\n") + "\n";
        assert_snapshot!("round_renderer_potter_project_started", prompt_rendered);

        let hint_rendered = project_hint.join("\n") + "\n";
        assert_snapshot!(
            "round_renderer_potter_project_started_project_hint",
            hint_rendered
        );
    }

    #[test]
    fn round_renderer_potter_round_started_emits_iteration_marker() {
        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "potter-round-started".into(),
            msg: EventMsg::PotterRoundStarted {
                current: 1,
                total: 15,
            },
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        assert!(
            events.is_empty(),
            "expected no marker before SessionConfigured; got: {events:?}"
        );

        proc.handle_codex_event(Event {
            id: "session-configured".into(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: ThreadId::new(),
                forked_from_id: None,
                model: "gpt-5.2".to_string(),
                model_provider_id: "test-provider".to_string(),
                service_tier: Some(codex_protocol::protocol::ServiceTier::Fast),
                cwd: PathBuf::from("project"),
                reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::XHigh),
                history_log_id: 0,
                history_entry_count: 0,
                initial_messages: None,
                rollout_path: PathBuf::from("rollout.jsonl"),
            }),
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        let [marker] = events.as_slice() else {
            panic!("expected exactly one marker cell; got: {events:?}");
        };
        let rendered = marker.join("\n") + "\n";
        assert_snapshot!("round_renderer_potter_round_started", rendered);
    }

    #[test]
    fn round_renderer_session_configured_before_potter_round_started_emits_iteration_marker() {
        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "session-configured".into(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: ThreadId::new(),
                forked_from_id: None,
                model: "gpt-5.2".to_string(),
                model_provider_id: "test-provider".to_string(),
                service_tier: Some(codex_protocol::protocol::ServiceTier::Fast),
                cwd: PathBuf::from("project"),
                reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::XHigh),
                history_log_id: 0,
                history_entry_count: 0,
                initial_messages: None,
                rollout_path: PathBuf::from("rollout.jsonl"),
            }),
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        assert!(
            events.is_empty(),
            "expected no marker without PotterRoundStarted; got: {events:?}"
        );

        proc.handle_codex_event(Event {
            id: "potter-round-started".into(),
            msg: EventMsg::PotterRoundStarted {
                current: 1,
                total: 15,
            },
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        let [marker] = events.as_slice() else {
            panic!("expected exactly one marker cell; got: {events:?}");
        };
        pretty_assertions::assert_eq!(
            marker,
            &vec!["• CodexPotter: iteration round 1/15 (gpt-5.2 xhigh [fast])".to_string()]
        );
    }

    #[test]
    fn prompt_selection_view_hides_composer_and_prompt_footer_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));

        bottom_pane
            .composer_mut()
            .show_selection_view(crate::bottom_pane::SelectionViewParams {
                title: Some("Select Syntax Theme".to_string()),
                subtitle: Some("Move up/down to live preview themes".to_string()),
                footer_hint: Some(crate::bottom_pane::popup_consts::standard_popup_hint_line()),
                items: vec![
                    crate::bottom_pane::SelectionItem {
                        name: "base16".to_string(),
                        is_current: true,
                        dismiss_on_select: true,
                        ..Default::default()
                    },
                    crate::bottom_pane::SelectionItem {
                        name: "catppuccin-latte".to_string(),
                        dismiss_on_select: true,
                        ..Default::default()
                    },
                ],
                is_searchable: true,
                search_placeholder: Some("Type to filter themes...".to_string()),
                ..Default::default()
            });

        let height = bottom_pane.desired_height(width).max(1);
        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());
                render_runner_viewport(area, frame.buffer_mut(), &bottom_pane, Vec::new());
            })
            .expect("draw");

        assert_snapshot!(
            "prompt_selection_view_hides_composer_and_prompt_footer_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    fn render_prompt_footer_line(
        override_mode: Option<PromptFooterOverride>,
        git_branch: Option<&str>,
        yolo_active: bool,
    ) -> String {
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        crate::bottom_pane::render_prompt_footer_for_test(
            area,
            &mut buf,
            override_mode,
            std::path::Path::new("project"),
            git_branch,
            yolo_active,
        );

        let mut out = String::new();
        for x in 0..area.width {
            out.push_str(buf[(x, 0)].symbol());
        }
        out.trim_end().to_string()
    }

    fn render_prompt_footer_line_for_branch(git_branch: Option<&str>) -> String {
        render_prompt_footer_line(None, git_branch, false)
    }

    #[test]
    fn prompt_footer_snapshots() {
        assert_snapshot!(
            "prompt_footer_includes_branch_and_working_dir",
            render_prompt_footer_line_for_branch(Some("main"))
        );
        assert_snapshot!(
            "prompt_footer_omits_branch_separator_when_branch_unknown",
            render_prompt_footer_line_for_branch(None)
        );
        assert_snapshot!(
            "prompt_footer_includes_yolo_indicator",
            render_prompt_footer_line(None, Some("main"), true)
        );
        assert_snapshot!(
            "prompt_footer_external_editor_override",
            render_prompt_footer_line(
                Some(PromptFooterOverride::ExternalEditorHint),
                Some("main"),
                false,
            )
        );
    }

    #[test]
    fn prompt_screen_startup_banner_uses_pre_resolved_model_config() {
        let lines = build_prompt_screen_startup_banner_lines(
            120,
            std::path::Path::new("/Users/example/repo"),
            Some(crate::codex_config::ResolvedCodexModelConfig {
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::High),
                is_fast: true,
            }),
        )
        .expect("build");

        assert_snapshot!(
            "prompt_screen_startup_banner_uses_pre_resolved_model_config",
            lines_to_plain_text(&lines)
        );
    }
}
