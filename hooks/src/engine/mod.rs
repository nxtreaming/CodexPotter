use std::path::Path;
use std::path::PathBuf;

use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;

use crate::events::project_stop::ProjectStopOutcome;
use crate::events::project_stop::ProjectStopRequest;
use crate::schema::PotterProjectStopCommandInput;

mod command_runner;
mod common;
mod config;
mod discovery;
mod dispatcher;
mod schema_loader;

#[derive(Debug, Clone)]
struct CommandShell {
    program: Option<String>,
    args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredHandler {
    pub event_name: HookEventName,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout_sec: u64,
    pub status_message: Option<String>,
    pub source_path: PathBuf,
    pub display_order: i64,
}

impl ConfiguredHandler {
    fn run_id(&self) -> String {
        format!(
            "{}:{}:{}",
            self.event_name.as_kebab_case(),
            self.display_order,
            self.source_path.display()
        )
    }
}

#[derive(Clone)]
pub(super) struct HooksEngine {
    handlers: Vec<ConfiguredHandler>,
    warnings: Vec<String>,
    shell: CommandShell,
}

impl HooksEngine {
    pub(super) fn new(
        cwd: Option<&Path>,
        codex_home_dir: Option<&Path>,
        shell_program: Option<String>,
        shell_args: Vec<String>,
    ) -> Self {
        let shell = CommandShell {
            program: shell_program.filter(|program| !program.is_empty()),
            args: shell_args,
        };
        let Some(cwd) = cwd else {
            return Self {
                handlers: Vec::new(),
                warnings: Vec::new(),
                shell,
            };
        };

        if cfg!(windows) {
            return Self {
                handlers: Vec::new(),
                warnings: vec![
                    "Disabled hooks because hooks.json lifecycle hooks are not supported on Windows yet."
                        .to_string(),
                ],
                shell,
            };
        }

        schema_loader::validate_generated_hook_schemas_loaded();
        let discovered = discovery::discover_handlers(cwd, codex_home_dir);
        Self {
            handlers: discovered.handlers,
            warnings: discovered.warnings,
            shell,
        }
    }

    pub(super) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(super) fn preview_project_stop(
        &self,
        _request: &ProjectStopRequest,
    ) -> Vec<HookRunSummary> {
        dispatcher::select_handlers(
            &self.handlers,
            HookEventName::PotterProjectStop,
            /*matcher_input*/ None,
        )
        .into_iter()
        .map(|handler| dispatcher::running_summary(&handler))
        .collect()
    }

    pub(super) async fn run_project_stop(&self, request: ProjectStopRequest) -> ProjectStopOutcome {
        let matched = dispatcher::select_handlers(
            &self.handlers,
            HookEventName::PotterProjectStop,
            /*matcher_input*/ None,
        );
        if matched.is_empty() {
            return ProjectStopOutcome {
                hook_events: Vec::new(),
            };
        }

        let input_json = match serde_json::to_string(&PotterProjectStopCommandInput {
            project_dir: request.project_dir.display().to_string(),
            project_file_path: request.project_file_path.display().to_string(),
            cwd: request.cwd.display().to_string(),
            hook_event_name: "Potter.ProjectStop".to_string(),
            user_prompt: request.user_prompt,
            all_session_ids: request.all_session_ids,
            new_session_ids: request.new_session_ids,
            all_assistant_messages: request.all_assistant_messages,
            new_assistant_messages: request.new_assistant_messages,
            stop_reason_code: request.stop_reason_code,
        }) {
            Ok(input_json) => input_json,
            Err(error) => {
                return ProjectStopOutcome {
                    hook_events: common::serialization_failure_hook_events(
                        matched,
                        None,
                        format!("failed to serialize project stop hook input: {error}"),
                    ),
                };
            }
        };

        let hook_events = dispatcher::execute_handlers(
            &self.shell,
            matched,
            input_json,
            request.cwd.as_path(),
            None,
            parse_project_stop_completed,
        )
        .await;

        ProjectStopOutcome { hook_events }
    }
}

fn hook_failure_entries(
    primary_message: String,
    run_result: &command_runner::CommandRunResult,
) -> Vec<HookOutputEntry> {
    let mut entries = vec![HookOutputEntry {
        kind: HookOutputEntryKind::Error,
        text: primary_message,
    }];
    if let Some(stderr) = common::trimmed_non_empty(&run_result.stderr) {
        entries.push(HookOutputEntry {
            kind: HookOutputEntryKind::Error,
            text: stderr,
        });
    }
    if let Some(stdout) = common::trimmed_non_empty(&run_result.stdout) {
        entries.push(HookOutputEntry {
            kind: HookOutputEntryKind::Error,
            text: format!("stdout: {stdout}"),
        });
    }
    entries
}

fn parse_project_stop_completed(
    handler: &ConfiguredHandler,
    run_result: command_runner::CommandRunResult,
    turn_id: Option<String>,
) -> HookCompletedEvent {
    let (status, entries) = match run_result.error.as_deref() {
        Some(error) => (
            HookRunStatus::Failed,
            hook_failure_entries(error.to_string(), &run_result),
        ),
        None => match run_result.exit_code {
            Some(0) => (HookRunStatus::Completed, Vec::new()),
            Some(code) => (
                HookRunStatus::Failed,
                hook_failure_entries(format!("hook exited with code {code}"), &run_result),
            ),
            None => (
                HookRunStatus::Failed,
                hook_failure_entries("hook exited without an exit code".to_string(), &run_result),
            ),
        },
    };

    HookCompletedEvent {
        turn_id,
        run: dispatcher::completed_summary(handler, &run_result, status, entries),
    }
}
