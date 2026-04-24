//! Potter.ProjectStop hook integration.
//!
//! This module lives in the workflow layer because it needs access to `potter-rollout.jsonl`
//! parsing and to upstream rollout JSONL files for extracting round summaries.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use codex_hooks::Hooks;
use codex_hooks::HooksConfig;
use codex_hooks::ProjectStopRequest;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookStartedEvent;
use codex_protocol::protocol::WarningEvent;

struct PreparedProjectStopHookRequest {
    request: ProjectStopRequest,
    warnings: Vec<String>,
}

fn read_final_assistant_message_for_hook(
    workdir: &Path,
    rollout_path: &Path,
    round_label: &str,
    round_current: u32,
    warnings: &mut Vec<String>,
) -> String {
    let abs = crate::workflow::replay_session_config::resolve_rollout_path_for_replay(
        workdir,
        rollout_path,
    );
    match crate::workflow::rollout_final_message::read_final_agent_message_from_rollout(&abs) {
        Ok((_, message)) => message.unwrap_or_default(),
        Err(err) => {
            warnings.push(format!(
                "Potter.ProjectStop hooks: failed to read final assistant message for {round_label} {round_current} from rollout {}: {err:#}",
                abs.display()
            ));
            String::new()
        }
    }
}

/// Stable stop-reason categories surfaced to `Potter.ProjectStop` hooks.
///
/// This is intentionally narrower than `PotterProjectOutcome`: hook payloads need only the stable
/// reason code, while project outcomes may carry extra data such as failure messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PotterProjectStopReason {
    Succeeded,
    Interrupted,
    BudgetExhausted,
    TaskFailed,
    Fatal,
}

impl PotterProjectStopReason {
    /// Return the stable wire code written into `Potter.ProjectStop` hook payloads.
    pub fn code(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Interrupted => "interrupted",
            Self::BudgetExhausted => "budget_exhausted",
            Self::TaskFailed => "task_failed",
            Self::Fatal => "fatal",
        }
    }
}

fn prepare_project_stop_hook_request(
    workdir: &Path,
    progress_file_path: PathBuf,
    project_dir: PathBuf,
    potter_rollout_path: &Path,
    baseline_round_count: usize,
    stop_reason: PotterProjectStopReason,
) -> anyhow::Result<PreparedProjectStopHookRequest> {
    let potter_lines = crate::workflow::rollout::read_project_rollout_lines(potter_rollout_path)?;
    let index = crate::workflow::rollout_resume_index::build_resume_index(&potter_lines)
        .with_context(|| format!("parse {}", potter_rollout_path.display()))?;

    let mut completed_session_ids = Vec::new();
    let mut completed_assistant_messages = Vec::new();
    let mut warnings = Vec::new();

    for round in &index.completed_rounds {
        let (thread_id, rollout_path) = match &round.configured {
            Some(cfg) => (Some(cfg.thread_id), Some(&cfg.rollout_path)),
            None => {
                warnings.push(format!(
                    "Potter.ProjectStop hooks: missing RoundConfigured entry for round {}",
                    round.round_current
                ));
                (None, None)
            }
        };

        completed_session_ids.push(thread_id.map(|id| id.to_string()).unwrap_or_default());

        let message = match rollout_path {
            Some(rollout_path) => read_final_assistant_message_for_hook(
                workdir,
                rollout_path,
                "round",
                round.round_current,
                &mut warnings,
            ),
            None => String::new(),
        };
        completed_assistant_messages.push(message);
    }

    let baseline_round_count = if baseline_round_count > completed_session_ids.len() {
        let recorded_rounds = completed_session_ids.len();
        warnings.push(format!(
            "Potter.ProjectStop hooks: baseline round count {baseline_round_count} exceeds recorded completed rounds {recorded_rounds}; treating as empty new_* window",
        ));
        completed_session_ids.len()
    } else {
        baseline_round_count
    };

    let new_session_ids = completed_session_ids[baseline_round_count..].to_vec();
    let new_assistant_messages = completed_assistant_messages[baseline_round_count..].to_vec();

    let mut all_session_ids = completed_session_ids;
    let mut all_assistant_messages = completed_assistant_messages;
    if let Some(unfinished) = &index.unfinished_round {
        all_session_ids.push(unfinished.thread_id.to_string());
        let message = read_final_assistant_message_for_hook(
            workdir,
            &unfinished.rollout_path,
            "unfinished round",
            unfinished.round_current,
            &mut warnings,
        );
        all_assistant_messages.push(message);
    }

    Ok(PreparedProjectStopHookRequest {
        request: ProjectStopRequest {
            project_dir,
            project_file_path: progress_file_path,
            cwd: workdir.to_path_buf(),
            user_prompt: index.project_started.user_message.unwrap_or_default(),
            all_session_ids,
            new_session_ids,
            all_assistant_messages,
            new_assistant_messages,
            stop_reason_code: stop_reason.code().to_string(),
        },
        warnings,
    })
}

/// Build synthetic `EventMsg` items that represent a full `Potter.ProjectStop` hook execution.
///
/// Returns:
/// - startup warnings from `hooks.json` discovery
/// - zero or more `HookStarted` events (for previewed runs)
/// - zero or more `HookCompleted` events after execution
///
/// This is best-effort by design: failures preparing the request payload become `Warning` events
/// so the project can still stop cleanly.
pub async fn build_project_stop_hook_events(
    workdir: &Path,
    progress_file_rel: &Path,
    potter_rollout_path: &Path,
    baseline_round_count: usize,
    stop_reason: PotterProjectStopReason,
    codex_home_dir: Option<&Path>,
) -> Vec<Event> {
    let hooks = Hooks::new(HooksConfig {
        cwd: Some(workdir.to_path_buf()),
        codex_home_dir: codex_home_dir.map(|dir| dir.to_path_buf()),
        ..HooksConfig::default()
    });

    let mut events = Vec::new();
    let event = |msg| Event {
        id: String::new(),
        msg,
    };

    for warning in hooks.startup_warnings() {
        events.push(event(EventMsg::Warning(WarningEvent {
            message: warning.clone(),
        })));
    }

    let progress_file_path = workdir.join(progress_file_rel);
    let project_dir = match progress_file_path.parent() {
        Some(parent) => parent.to_path_buf(),
        None => {
            events.push(event(EventMsg::Warning(WarningEvent {
                message: format!(
                    "Failed to derive project directory from progress file path: {}",
                    progress_file_path.display()
                ),
            })));
            return events;
        }
    };

    // ProjectStop does not support matchers, so we can check whether any handlers exist without
    // scanning `potter-rollout.jsonl` first.
    let stub_request = ProjectStopRequest {
        project_dir: project_dir.clone(),
        project_file_path: progress_file_path.clone(),
        cwd: workdir.to_path_buf(),
        user_prompt: String::new(),
        all_session_ids: Vec::new(),
        new_session_ids: Vec::new(),
        all_assistant_messages: Vec::new(),
        new_assistant_messages: Vec::new(),
        stop_reason_code: stop_reason.code().to_string(),
    };

    let preview_runs = hooks.preview_project_stop(&stub_request);
    if preview_runs.is_empty() {
        return events;
    }

    let prepared = match prepare_project_stop_hook_request(
        workdir,
        progress_file_path,
        project_dir,
        potter_rollout_path,
        baseline_round_count,
        stop_reason,
    ) {
        Ok(prepared) => prepared,
        Err(err) => {
            events.push(event(EventMsg::Warning(WarningEvent {
                message: format!("Failed to prepare Potter.ProjectStop hooks: {err:#}"),
            })));
            return events;
        }
    };

    for warning in prepared.warnings {
        events.push(event(EventMsg::Warning(WarningEvent { message: warning })));
    }

    for run in preview_runs {
        events.push(event(EventMsg::HookStarted(HookStartedEvent {
            turn_id: None,
            run,
        })));
    }

    let hook_outcome = hooks.run_project_stop(prepared.request).await;
    for completed in hook_outcome.hook_events {
        events.push(event(EventMsg::HookCompleted(completed)));
    }

    events
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use codex_protocol::ThreadId;
    use codex_protocol::protocol::PotterRoundOutcome;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn potter_project_stop_reason_matches_contract() {
        assert_eq!(PotterProjectStopReason::Succeeded.code(), "succeeded");
        assert_eq!(PotterProjectStopReason::Interrupted.code(), "interrupted");
        assert_eq!(
            PotterProjectStopReason::BudgetExhausted.code(),
            "budget_exhausted"
        );
        assert_eq!(PotterProjectStopReason::TaskFailed.code(), "task_failed");
        assert_eq!(PotterProjectStopReason::Fatal.code(), "fatal");
    }

    #[test]
    fn prepare_project_stop_hook_request_rejects_empty_rollout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();
        let progress_file_rel = PathBuf::from(".codexpotter/projects/2026/04/21/1/MAIN.md");
        let progress_file = workdir.join(&progress_file_rel);
        std::fs::create_dir_all(progress_file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&progress_file, "---\nstatus: open\n---\n").expect("write progress file");

        let potter_rollout_path = workdir.join("potter-rollout.jsonl");
        std::fs::write(&potter_rollout_path, "").expect("write empty rollout");

        let err = prepare_project_stop_hook_request(
            workdir,
            progress_file.clone(),
            progress_file.parent().expect("parent").to_path_buf(),
            &potter_rollout_path,
            0,
            PotterProjectStopReason::Succeeded,
        )
        .err()
        .expect("expected empty rollout error");

        assert!(
            format!("{err:#}").contains("potter-rollout is empty"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn prepare_project_stop_hook_request_excludes_unfinished_round_from_new_slices() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();
        let progress_file_rel = PathBuf::from(".codexpotter/projects/2026/04/21/1/MAIN.md");
        let progress_file = workdir.join(&progress_file_rel);
        std::fs::create_dir_all(progress_file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&progress_file, "---\nstatus: open\n---\n").expect("write progress file");

        let upstream_1 = workdir.join("upstream-1.jsonl");
        let upstream_2 = workdir.join("upstream-2.jsonl");
        std::fs::write(
            &upstream_1,
            r#"{"timestamp":"2026-03-01T00:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"round 1 final","phase":"final_answer"}}
"#,
        )
        .expect("write upstream 1");
        std::fs::write(
            &upstream_2,
            r#"{"timestamp":"2026-03-01T00:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"round 2 partial","phase":"final_answer"}}
"#,
        )
        .expect("write upstream 2");

        let upstream_1 = upstream_1.canonicalize().expect("canonical upstream 1");
        let upstream_2 = upstream_2.canonicalize().expect("canonical upstream 2");

        let potter_rollout_path = workdir.join("potter-rollout.jsonl");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message: Some("hello".to_string()),
                user_prompt_file: progress_file_rel.clone(),
            },
        )
        .expect("append project_started");

        let thread_id_1 =
            ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c111").expect("thread id 1");
        let thread_id_2 =
            ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c222").expect("thread id 2");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 2,
            },
        )
        .expect("append round 1 started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id: thread_id_1,
                rollout_path: upstream_1,
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round 1 configured");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
                duration_secs: 0,
            },
        )
        .expect("append round 1 finished");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 2,
                total: 2,
            },
        )
        .expect("append round 2 started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id: thread_id_2,
                rollout_path: upstream_2,
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round 2 configured");

        let prepared = prepare_project_stop_hook_request(
            workdir,
            progress_file.clone(),
            progress_file.parent().expect("parent").to_path_buf(),
            &potter_rollout_path,
            /*baseline_round_count*/ 1,
            PotterProjectStopReason::Succeeded,
        )
        .expect("prepare hook request");

        assert_eq!(
            prepared.request.all_session_ids,
            vec![thread_id_1.to_string(), thread_id_2.to_string()]
        );
        assert_eq!(prepared.request.new_session_ids, Vec::<String>::new());
        assert_eq!(
            prepared.request.all_assistant_messages,
            vec!["round 1 final".to_string(), "round 2 partial".to_string()]
        );
        assert_eq!(
            prepared.request.new_assistant_messages,
            Vec::<String>::new()
        );
    }
}
