//! Projects list overlay detail extraction.
//!
//! This module parses a project's `potter-rollout.jsonl` and referenced upstream rollout JSONL
//! files to build the right-pane content for the projects list overlay.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use codex_protocol::protocol::PotterProjectDetails;
use codex_protocol::protocol::PotterProjectRoundSummary;
use codex_protocol::protocol::PotterRoundOutcome;

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
    let (user_message, round_entries) = overlay_round_entries_from_potter_rollout(&potter_lines)
        .with_context(|| format!("parse {}", potter_rollout_path.display()))?;

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

    let mut rounds = Vec::new();
    for round in round_entries {
        let (final_message_unix_secs, final_message) = round
            .rollout_path
            .as_ref()
            .map(|path| read_round_message(path))
            .unwrap_or((None, None));
        rounds.push(PotterProjectRoundSummary {
            round_current: round.round_current,
            round_total: round.round_total,
            duration_secs: round.duration_secs,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct OverlayRoundEntry {
    round_current: u32,
    round_total: u32,
    duration_secs: u64,
    rollout_path: Option<PathBuf>,
}

fn overlay_round_entries_from_potter_rollout(
    lines: &[crate::workflow::rollout::PotterRolloutLine],
) -> anyhow::Result<(Option<String>, Vec<OverlayRoundEntry>)> {
    struct RoundBuilder {
        round_current: u32,
        round_total: u32,
        rollout_path: Option<PathBuf>,
        project_succeeded: bool,
    }

    let mut project_started_seen = false;
    let mut project_started_user_message: Option<String> = None;
    let mut rounds: Vec<OverlayRoundEntry> = Vec::new();
    let mut current: Option<RoundBuilder> = None;

    for line in lines {
        match line {
            crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message, ..
            } => {
                if project_started_seen || !rounds.is_empty() || current.is_some() {
                    anyhow::bail!("potter-rollout: project_started must appear once at the top");
                }
                project_started_seen = true;
                project_started_user_message = user_message.clone();
            }
            crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: round_current,
                total: round_total,
            } => {
                if !project_started_seen {
                    anyhow::bail!("potter-rollout: missing project_started before first round");
                }
                if current.is_some() {
                    anyhow::bail!("potter-rollout: round_started before previous round_finished");
                }
                current = Some(RoundBuilder {
                    round_current: *round_current,
                    round_total: *round_total,
                    rollout_path: None,
                    project_succeeded: false,
                });
            }
            crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                rollout_path, ..
            } => {
                let Some(builder) = current.as_mut() else {
                    anyhow::bail!("potter-rollout: round_configured before round_started");
                };
                if builder.rollout_path.is_some() {
                    anyhow::bail!("potter-rollout: duplicate round_configured in a single round");
                }
                builder.rollout_path = Some(rollout_path.clone());
            }
            crate::workflow::rollout::PotterRolloutLine::ProjectSucceeded { .. } => {
                let Some(builder) = current.as_mut() else {
                    anyhow::bail!("potter-rollout: project_succeeded outside a round");
                };
                if builder.project_succeeded {
                    anyhow::bail!("potter-rollout: duplicate project_succeeded in a single round");
                }
                builder.project_succeeded = true;
            }
            crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome,
                duration_secs,
            } => {
                let Some(builder) = current.take() else {
                    anyhow::bail!("potter-rollout: round_finished without round_started");
                };
                if builder.project_succeeded && !matches!(outcome, PotterRoundOutcome::Completed) {
                    anyhow::bail!(
                        "potter-rollout: project_succeeded recorded but round_finished outcome is {outcome:?}"
                    );
                }
                if matches!(outcome, PotterRoundOutcome::Completed)
                    && builder.rollout_path.is_none()
                {
                    anyhow::bail!(
                        "potter-rollout: completed round_finished without round_configured"
                    );
                }

                rounds.push(OverlayRoundEntry {
                    round_current: builder.round_current,
                    round_total: builder.round_total,
                    duration_secs: *duration_secs,
                    rollout_path: builder.rollout_path,
                });
            }
        }
    }

    if let Some(builder) = current.take() {
        rounds.push(OverlayRoundEntry {
            round_current: builder.round_current,
            round_total: builder.round_total,
            duration_secs: 0,
            rollout_path: builder.rollout_path,
        });
    }

    if !project_started_seen {
        anyhow::bail!("potter-rollout: missing project_started before first round");
    }

    Ok((project_started_user_message, rounds))
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

    #[test]
    fn overlay_details_tolerate_unfinished_round_without_round_configured_at_eof() {
        let workdir = tempfile::tempdir().expect("tempdir");
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/24/1");
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
                total: 3,
            },
        )
        .expect("append round_started");

        let details = build_project_details_for_overlay(workdir.path(), &project_dir);
        assert_eq!(details.error, None);
        assert_eq!(details.project_dir, project_dir);
        assert_eq!(details.git_branch.as_deref(), Some("main"));
        assert_eq!(details.user_message.as_deref(), Some("hello task"));
        assert_eq!(details.rounds.len(), 1);

        let round = details.rounds.first().expect("round");
        assert_eq!(round.round_current, 1);
        assert_eq!(round.round_total, 3);
        assert_eq!(round.duration_secs, 0);
        assert_eq!(round.final_message_unix_secs, None);
        assert_eq!(round.final_message, None);
    }

    #[test]
    fn overlay_details_tolerate_project_started_without_rounds() {
        let workdir = tempfile::tempdir().expect("tempdir");
        let project_dir = PathBuf::from(".codexpotter/projects/2026/04/24/2");
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

        let details = build_project_details_for_overlay(workdir.path(), &project_dir);
        assert_eq!(details.error, None);
        assert_eq!(details.project_dir, project_dir);
        assert_eq!(details.git_branch.as_deref(), Some("main"));
        assert_eq!(details.user_message.as_deref(), Some("hello task"));
        assert_eq!(details.rounds, Vec::new());
    }
}
