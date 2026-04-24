//! Projects list overlay detail extraction.
//!
//! This module parses a project's `potter-rollout.jsonl` and referenced upstream rollout JSONL
//! files to build the right-pane content for the projects list overlay.

use std::path::Path;

use anyhow::Context;
use codex_protocol::protocol::PotterProjectDetails;
use codex_protocol::protocol::PotterProjectRoundSummary;

pub fn build_project_details_for_overlay(
    workdir: &Path,
    project_dir: &Path,
) -> PotterProjectDetails {
    match build_project_details_for_overlay_inner(workdir, project_dir) {
        Ok(details) => details,
        Err(err) => PotterProjectDetails {
            project_dir: project_dir.to_path_buf(),
            progress_file: project_dir.join("MAIN.md"),
            git_branch: None,
            user_message: None,
            rounds: Vec::new(),
            error: Some(format!("{err:#}")),
        },
    }
}

fn build_project_details_for_overlay_inner(
    workdir: &Path,
    project_dir: &Path,
) -> anyhow::Result<PotterProjectDetails> {
    let project_dir_abs = if project_dir.is_absolute() {
        project_dir.to_path_buf()
    } else {
        workdir.join(project_dir)
    };
    let progress_file = project_dir.join("MAIN.md");
    let progress_file_abs = project_dir_abs.join("MAIN.md");
    anyhow::ensure!(
        progress_file_abs.is_file(),
        "progress file missing: {}",
        progress_file_abs.display()
    );
    let git_branch = crate::workflow::project::progress_file_git_branch(&progress_file_abs)
        .context("read git_branch from progress file")?;

    let potter_rollout_path = crate::workflow::rollout::potter_rollout_path(&project_dir_abs);
    let potter_lines = crate::workflow::rollout::read_project_rollout_lines(&potter_rollout_path)?;
    let index = crate::workflow::rollout_resume_index::build_resume_index(&potter_lines)
        .with_context(|| format!("parse {}", potter_rollout_path.display()))?;

    let user_message = index.project_started.user_message.clone();
    let mut rounds = Vec::new();

    let read_round_message = |rollout_path: &Path| -> (Option<u64>, Option<String>) {
        let abs = crate::workflow::replay_session_config::resolve_rollout_path_for_replay(
            workdir,
            rollout_path,
        );
        match crate::workflow::rollout_final_message::read_final_agent_message_from_rollout(&abs) {
            Ok(message) => message,
            Err(err) => {
                tracing::warn!(
                    "failed to read final assistant message from rollout {}: {err:#}",
                    abs.display()
                );
                (None, None)
            }
        }
    };

    for round in index.completed_rounds {
        let (final_message_unix_secs, final_message) = match round.configured.as_ref() {
            Some(cfg) => read_round_message(&cfg.rollout_path),
            None => (None, None),
        };

        rounds.push(PotterProjectRoundSummary {
            round_current: round.round_current,
            round_total: round.round_total,
            duration_secs: round.duration_secs,
            final_message_unix_secs,
            final_message,
        });
    }

    if let Some(unfinished) = index.unfinished_round {
        let (final_message_unix_secs, final_message) = read_round_message(&unfinished.rollout_path);
        rounds.push(PotterProjectRoundSummary {
            round_current: unfinished.round_current,
            round_total: unfinished.round_total,
            duration_secs: 0,
            final_message_unix_secs,
            final_message,
        });
    }

    Ok(PotterProjectDetails {
        project_dir: project_dir.to_path_buf(),
        progress_file,
        git_branch,
        user_message,
        rounds,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::workflow::rollout::PotterRolloutLine;
    use codex_protocol::protocol::PotterRoundOutcome;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn overlay_details_include_user_task_message_from_potter_rollout() {
        let workdir = tempfile::tempdir().expect("tempdir");
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/1");
        let project_dir_abs = workdir.path().join(&project_dir);
        std::fs::create_dir_all(&project_dir_abs).expect("create project dir");

        let progress_file_abs = project_dir_abs.join("MAIN.md");
        std::fs::write(&progress_file_abs, "---\ngit_branch: \"main\"\n---\n").expect("write MAIN");

        let potter_rollout_path = crate::workflow::rollout::potter_rollout_path(&project_dir_abs);
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &PotterRolloutLine::ProjectStarted {
                user_message: Some("hello task".to_string()),
                user_prompt_file: project_dir.join("MAIN.md"),
            },
        )
        .expect("append project_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &PotterRolloutLine::RoundStarted {
                current: 1,
                total: 1,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Interrupted,
                duration_secs: 0,
            },
        )
        .expect("append round_finished");

        let details = build_project_details_for_overlay(workdir.path(), &project_dir);
        assert_eq!(details.error, None);
        assert_eq!(details.project_dir, project_dir);
        assert_eq!(
            details.user_message.as_deref(),
            Some("hello task"),
            "expected details to surface the original user task message"
        );
        assert_eq!(details.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn overlay_details_reports_empty_potter_rollout_with_shared_contract() {
        let workdir = tempfile::tempdir().expect("tempdir");
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/16/2");
        let project_dir_abs = workdir.path().join(&project_dir);
        std::fs::create_dir_all(&project_dir_abs).expect("create project dir");

        let progress_file_abs = project_dir_abs.join("MAIN.md");
        std::fs::write(&progress_file_abs, "---\ngit_branch: \"main\"\n---\n").expect("write MAIN");

        let potter_rollout_path = crate::workflow::rollout::potter_rollout_path(&project_dir_abs);
        std::fs::write(&potter_rollout_path, "").expect("write empty rollout");

        let details = build_project_details_for_overlay(workdir.path(), &project_dir);
        let error = details.error.expect("expected overlay error");
        assert!(
            error.contains("potter-rollout is empty"),
            "unexpected error: {error}"
        );
    }
}
