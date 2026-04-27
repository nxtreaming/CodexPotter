//! Projects list overlay discovery.
//!
//! This module scans `.codexpotter/projects/**/MAIN.md` under a workdir and builds
//! [`codex_protocol::protocol::PotterProjectListEntry`] items for the interactive projects list
//! overlay.

use std::io::BufRead as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use chrono::DateTime;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::PotterProjectListEntry;
use codex_protocol::protocol::PotterProjectListStatus;
use codex_protocol::protocol::PotterRoundOutcome;
use codex_protocol::protocol::TurnAbortReason;

use crate::workflow::rollout::PotterRolloutLine;
use crate::workflow::rollout_resume_index::PotterRolloutResumeIndex;

#[derive(Debug)]
struct DiscoveredOverlayProject {
    row: PotterProjectListEntry,
    resume_index: Option<PotterRolloutResumeIndex>,
}

/// Discover CodexPotter project progress files under `workdir` and build overlay list entries.
///
/// Discovery is best-effort: malformed or incomplete projects remain renderable. Only explicit
/// terminal outcomes before any completed round are classified as `Cancelled`.
pub fn discover_projects_for_overlay(
    workdir: &Path,
) -> anyhow::Result<Vec<PotterProjectListEntry>> {
    let mut rows: Vec<PotterProjectListEntry> = discover_projects_for_overlay_internal(workdir)
        .into_iter()
        .map(|project| project.row)
        .collect();

    sort_rows(&mut rows);
    Ok(rows)
}

/// Discover resumable CodexPotter projects under `workdir`.
///
/// Resumable projects are those whose progress and rollout logs reference only rollout files that
/// still exist on disk. This powers `codex-potter resume` so the picker avoids projects that
/// would error immediately.
pub fn discover_resumable_projects_for_overlay(
    workdir: &Path,
) -> anyhow::Result<Vec<PotterProjectListEntry>> {
    let mut rows = Vec::new();
    for project in discover_projects_for_overlay_internal(workdir) {
        let Some(index) = project.resume_index.as_ref() else {
            continue;
        };
        if !all_referenced_rollouts_exist(workdir, index) {
            continue;
        }
        rows.push(project.row);
    }

    sort_rows(&mut rows);
    Ok(rows)
}

fn discover_projects_for_overlay_internal(workdir: &Path) -> Vec<DiscoveredOverlayProject> {
    let mut projects = Vec::new();

    for progress_file in super::project_progress_files::discover_project_progress_files(workdir) {
        if let Ok(project) = project_entry_for_progress_file(workdir, &progress_file) {
            projects.push(project);
        }
    }

    projects
}

fn project_entry_for_progress_file(
    workdir: &Path,
    progress_file_abs: &Path,
) -> anyhow::Result<DiscoveredOverlayProject> {
    let project_dir_abs = progress_file_abs
        .parent()
        .context("derive project_dir from progress file path")?;

    let progress_file = relativize_path(workdir, progress_file_abs);
    let project_dir = relativize_path(workdir, project_dir_abs);

    let potter_rollout_path = crate::workflow::rollout::potter_rollout_path(project_dir_abs);
    let rollout_lines = crate::workflow::rollout::read_lines(&potter_rollout_path).ok();
    let resume_index = rollout_lines
        .as_deref()
        .filter(|lines| !lines.is_empty())
        .and_then(|lines| crate::workflow::rollout_resume_index::build_resume_index(lines).ok());

    let Some(index) = resume_index else {
        let (user_message, rounds, started_at_unix_secs, status) = rollout_lines
            .as_deref()
            .map(|lines| {
                (
                    best_effort_project_user_message(lines),
                    best_effort_project_list_rounds(lines),
                    best_effort_project_started_at_unix_secs(workdir, lines),
                    best_effort_project_list_status(lines),
                )
            })
            .unwrap_or((None, 0, None, PotterProjectListStatus::Incomplete));
        let description =
            read_project_description(progress_file_abs, user_message).unwrap_or_default();
        return Ok(DiscoveredOverlayProject {
            row: PotterProjectListEntry {
                project_dir,
                progress_file,
                description,
                started_at_unix_secs,
                rounds,
                status,
            },
            resume_index: None,
        });
    };

    let status = project_list_status(workdir, &index);
    let description = read_project_description(
        progress_file_abs,
        index.project_started.user_message.as_deref(),
    )
    .unwrap_or_default();
    let rounds = project_list_rounds(&index);
    let started_at_unix_secs = project_started_at_unix_secs(workdir, &index);

    Ok(DiscoveredOverlayProject {
        row: PotterProjectListEntry {
            project_dir,
            progress_file,
            description,
            started_at_unix_secs,
            rounds,
            status,
        },
        resume_index: Some(index),
    })
}

fn relativize_path(workdir: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(workdir).unwrap_or(path).to_path_buf()
}

fn project_list_status(
    workdir: &Path,
    index: &PotterRolloutResumeIndex,
) -> PotterProjectListStatus {
    if index
        .completed_rounds
        .iter()
        .any(|round| round.project_succeeded.is_some())
    {
        return PotterProjectListStatus::Succeeded;
    }

    let has_completed_round = index
        .completed_rounds
        .iter()
        .any(|round| matches!(round.outcome, PotterRoundOutcome::Completed));

    if let Some(unfinished_round) = index.unfinished_round.as_ref() {
        if !has_completed_round
            && rollout_has_interrupted_turn(workdir, &unfinished_round.rollout_path)
        {
            return PotterProjectListStatus::Cancelled;
        }
        return PotterProjectListStatus::Incomplete;
    }

    let Some(last_round) = index.completed_rounds.last() else {
        return PotterProjectListStatus::Incomplete;
    };

    match &last_round.outcome {
        PotterRoundOutcome::Completed => {
            if last_round.round_current == last_round.round_total {
                PotterProjectListStatus::BudgetExhausted
            } else {
                PotterProjectListStatus::Incomplete
            }
        }
        PotterRoundOutcome::Interrupted | PotterRoundOutcome::UserRequested => {
            if has_completed_round {
                PotterProjectListStatus::Interrupted
            } else {
                PotterProjectListStatus::Cancelled
            }
        }
        PotterRoundOutcome::TaskFailed { .. } | PotterRoundOutcome::Fatal { .. } => {
            PotterProjectListStatus::Failed
        }
    }
}

fn best_effort_project_list_status(lines: &[PotterRolloutLine]) -> PotterProjectListStatus {
    if lines
        .iter()
        .any(|line| matches!(line, PotterRolloutLine::ProjectSucceeded { .. }))
    {
        return PotterProjectListStatus::Succeeded;
    }

    let has_completed_round = lines.iter().any(|line| {
        matches!(
            line,
            PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
                ..
            }
        )
    });
    if !has_completed_round
        && lines.iter().any(|line| {
            matches!(
                line,
                PotterRolloutLine::RoundFinished {
                    outcome: PotterRoundOutcome::Interrupted | PotterRoundOutcome::UserRequested,
                    ..
                }
            )
        })
    {
        return PotterProjectListStatus::Cancelled;
    }

    PotterProjectListStatus::Incomplete
}

fn rollout_has_interrupted_turn(workdir: &Path, rollout_path: &Path) -> bool {
    let rollout_path = crate::workflow::replay_session_config::resolve_rollout_path_for_replay(
        workdir,
        rollout_path,
    );
    read_rollout_has_interrupted_turn(&rollout_path).unwrap_or(false)
}

fn read_rollout_has_interrupted_turn(rollout_path: &Path) -> anyhow::Result<bool> {
    let file = std::fs::File::open(rollout_path)
        .with_context(|| format!("open rollout {}", rollout_path.display()))?;
    let reader = std::io::BufReader::new(file);

    for (idx, line) in reader.lines().enumerate() {
        let line_number = idx + 1;
        let line = line.with_context(|| format!("read rollout line {line_number}"))?;
        if line.trim().is_empty() {
            continue;
        }

        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("parse rollout json line {line_number}: {line}"))?;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Ok(EventMsg::TurnAborted(ev)) = serde_json::from_value::<EventMsg>(payload.clone())
        else {
            continue;
        };
        if matches!(ev.reason, TurnAbortReason::Interrupted) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn best_effort_project_user_message(lines: &[PotterRolloutLine]) -> Option<&str> {
    lines.iter().find_map(|line| match line {
        PotterRolloutLine::ProjectStarted { user_message, .. } => user_message.as_deref(),
        _ => None,
    })
}

fn best_effort_project_list_rounds(lines: &[PotterRolloutLine]) -> u32 {
    let mut started_rounds: u32 = 0;
    let mut max_started_round = 0;
    let mut finished_rounds: u32 = 0;

    for line in lines {
        match line {
            PotterRolloutLine::RoundStarted { current, .. } => {
                started_rounds = started_rounds.saturating_add(1);
                max_started_round = max_started_round.max(*current);
            }
            PotterRolloutLine::RoundFinished { .. } => {
                finished_rounds = finished_rounds.saturating_add(1);
            }
            _ => {}
        }
    }

    if started_rounds > 0 {
        started_rounds.max(max_started_round)
    } else {
        finished_rounds
    }
}

fn best_effort_project_started_at_unix_secs(
    workdir: &Path,
    lines: &[PotterRolloutLine],
) -> Option<u64> {
    let rollout_path = lines.iter().find_map(|line| match line {
        PotterRolloutLine::RoundConfigured { rollout_path, .. } => Some(rollout_path.clone()),
        _ => None,
    })?;

    let resolved = crate::workflow::replay_session_config::resolve_rollout_path_for_replay(
        workdir,
        &rollout_path,
    );
    read_first_rollout_timestamp_unix_secs(&resolved).ok()
}

fn project_list_rounds(index: &PotterRolloutResumeIndex) -> u32 {
    // Count total rounds across the entire project lifecycle (including prior `resume` windows).
    //
    // Older/partially-corrupted logs might contain only the latest round marker. Preserve the
    // previous best-effort behavior by ensuring we never report fewer rounds than the last
    // recorded round index.
    let completed_rounds: u32 = index.completed_rounds.len().try_into().unwrap_or(u32::MAX);
    let counted = completed_rounds.saturating_add(u32::from(index.unfinished_round.is_some()));

    let last_round_current = index
        .unfinished_round
        .as_ref()
        .map(|round| round.round_current)
        .or_else(|| {
            index
                .completed_rounds
                .last()
                .map(|round| round.round_current)
        })
        .unwrap_or_default();

    counted.max(last_round_current)
}

fn project_started_at_unix_secs(workdir: &Path, index: &PotterRolloutResumeIndex) -> Option<u64> {
    let rollout_path = index
        .completed_rounds
        .iter()
        .find_map(|round| {
            round
                .configured
                .as_ref()
                .map(|configured| configured.rollout_path.clone())
        })
        .or_else(|| {
            index
                .unfinished_round
                .as_ref()
                .map(|round| round.rollout_path.clone())
        })?;

    let rollout_path = crate::workflow::replay_session_config::resolve_rollout_path_for_replay(
        workdir,
        &rollout_path,
    );
    read_first_rollout_timestamp_unix_secs(&rollout_path).ok()
}

fn all_referenced_rollouts_exist(workdir: &Path, index: &PotterRolloutResumeIndex) -> bool {
    let unfinished = index
        .unfinished_round
        .as_ref()
        .map(|round| round.rollout_path.as_path());

    index
        .completed_rounds
        .iter()
        .filter_map(|round| {
            round
                .configured
                .as_ref()
                .map(|configured| configured.rollout_path.as_path())
        })
        .chain(unfinished)
        .all(|rollout_path| {
            let resolved = crate::workflow::replay_session_config::resolve_rollout_path_for_replay(
                workdir,
                rollout_path,
            );
            resolved.is_file()
        })
}

fn read_first_rollout_timestamp_unix_secs(rollout_path: &Path) -> anyhow::Result<u64> {
    let file = std::fs::File::open(rollout_path)
        .with_context(|| format!("open rollout {}", rollout_path.display()))?;
    let reader = std::io::BufReader::new(file);

    for (idx, line) in reader.lines().enumerate() {
        let line_number = idx + 1;
        let line = line.with_context(|| format!("read rollout line {line_number}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("parse rollout json line {line_number}: {trimmed}"))?;
        let Some(ts) = value.get("timestamp").and_then(serde_json::Value::as_str) else {
            continue;
        };

        let parsed = DateTime::parse_from_rfc3339(ts)
            .with_context(|| format!("parse rollout timestamp {ts:?}"))?;
        return u64::try_from(parsed.timestamp())
            .context("convert rollout timestamp to unix seconds");
    }

    anyhow::bail!("missing timestamp in rollout {}", rollout_path.display());
}

fn read_project_description(
    progress_file_abs: &Path,
    user_message: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(short_title) =
        crate::workflow::project::progress_file_short_title(progress_file_abs)
            .context("read short_title")?
    {
        return Ok(short_title);
    }

    if let Some(user_message) = user_message {
        let trimmed = user_message.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let contents = std::fs::read_to_string(progress_file_abs)
        .with_context(|| format!("read {}", progress_file_abs.display()))?;

    let mut in_overall_goal = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if !in_overall_goal {
            if trimmed == "# Overall Goal" {
                in_overall_goal = true;
            }
            continue;
        }

        if trimmed.starts_with('#') {
            break;
        }

        if trimmed.is_empty() {
            continue;
        }

        return Ok(trimmed.to_string());
    }

    Ok(String::new())
}

fn sort_rows(rows: &mut [PotterProjectListEntry]) {
    rows.sort_by(|a, b| {
        b.started_at_unix_secs
            .cmp(&a.started_at_unix_secs)
            .then_with(|| a.project_dir.cmp(&b.project_dir))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    fn write_main(workdir: &Path, rel_dir: &str, short_title: Option<&str>) -> PathBuf {
        let path = workdir.join(rel_dir).join("MAIN.md");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");

        let short_title = short_title.unwrap_or("");
        std::fs::write(
            &path,
            format!(
                r#"---
status: open
short_title: "{short_title}"
git_branch: ""
---

# Overall Goal
original goal line

## Todo
"#
            ),
        )
        .expect("write MAIN.md");

        path
    }

    fn write_rollout_with_timestamp(path: &Path, timestamp: &str) {
        std::fs::write(
            path,
            format!(
                r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"agent_message","message":"hello"}}}}
"#
            ),
        )
        .expect("write rollout");
    }

    fn write_rollout_with_interrupted_turn(path: &Path, timestamp: &str) {
        std::fs::write(
            path,
            format!(
                r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}}}}
"#
            ),
        )
        .expect("write rollout");
    }

    fn write_potter_rollout(
        project_dir: &Path,
        user_prompt_file: &Path,
        rounds: u32,
        round_total: u32,
        rollout_path: &Path,
        outcome: PotterRoundOutcome,
        succeeded_rounds: Option<u32>,
    ) {
        let potter_rollout_path =
            project_dir.join(crate::workflow::rollout::POTTER_ROLLOUT_FILENAME);
        let thread_id =
            codex_protocol::ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000")
                .expect("thread id");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message: Some("original prompt".to_string()),
                user_prompt_file: user_prompt_file.to_path_buf(),
            },
        )
        .expect("append project_started");

        for round_current in 1..=rounds {
            crate::workflow::rollout::append_line(
                &potter_rollout_path,
                &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                    current: round_current,
                    total: round_total,
                },
            )
            .expect("append round_started");

            crate::workflow::rollout::append_line(
                &potter_rollout_path,
                &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                    thread_id,
                    rollout_path: rollout_path.to_path_buf(),
                    service_tier: None,
                    rollout_path_raw: None,
                    rollout_base_dir: None,
                },
            )
            .expect("append round_configured");

            if round_current == rounds
                && let Some(project_succeeded_rounds) = succeeded_rounds
            {
                crate::workflow::rollout::append_line(
                    &potter_rollout_path,
                    &crate::workflow::rollout::PotterRolloutLine::ProjectSucceeded {
                        rounds: project_succeeded_rounds,
                        duration_secs: 1,
                        user_prompt_file: user_prompt_file.to_path_buf(),
                        git_commit_start: "".to_string(),
                        git_commit_end: "".to_string(),
                    },
                )
                .expect("append project_succeeded");
            }

            let outcome = if round_current == rounds {
                outcome.clone()
            } else {
                PotterRoundOutcome::Completed
            };
            crate::workflow::rollout::append_line(
                &potter_rollout_path,
                &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                    outcome,
                    duration_secs: 0,
                },
            )
            .expect("append round_finished");
        }
    }

    #[test]
    fn discover_projects_sorts_by_started_at_and_prefers_short_title() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main_a = write_main(
            workdir,
            ".codexpotter/projects/2026/02/28/1",
            Some("Short A"),
        );
        let main_b = write_main(workdir, ".codexpotter/projects/2026/02/28/2", None);

        let rollout_a = workdir.join("a.jsonl");
        let rollout_b = workdir.join("b.jsonl");
        write_rollout_with_timestamp(&rollout_a, "2026-02-28T00:00:00.000Z");
        write_rollout_with_timestamp(&rollout_b, "2026-02-28T00:10:00.000Z");

        write_potter_rollout(
            main_a.parent().expect("project dir"),
            main_a.strip_prefix(workdir).expect("rel"),
            1,
            10,
            Path::new("a.jsonl"),
            PotterRoundOutcome::Interrupted,
            None,
        );
        write_potter_rollout(
            main_b.parent().expect("project dir"),
            main_b.strip_prefix(workdir).expect("rel"),
            10,
            10,
            Path::new("b.jsonl"),
            PotterRoundOutcome::Completed,
            None,
        );

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 2);

        // Newest (rollout_b) first.
        assert_eq!(
            rows[0].progress_file,
            PathBuf::from(".codexpotter/projects/2026/02/28/2/MAIN.md")
        );
        assert_eq!(rows[0].status, PotterProjectListStatus::BudgetExhausted);
        assert_eq!(rows[0].rounds, 10);
        assert_eq!(rows[0].description, "original prompt");

        assert_eq!(
            rows[1].progress_file,
            PathBuf::from(".codexpotter/projects/2026/02/28/1/MAIN.md")
        );
        assert_eq!(rows[1].status, PotterProjectListStatus::Cancelled);
        assert_eq!(rows[1].rounds, 1);
        assert_eq!(rows[1].description, "Short A");
    }

    #[test]
    fn discover_projects_marks_interrupted_after_completed_round() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main = write_main(workdir, ".codexpotter/projects/2026/03/05/1", None);
        let rollout = workdir.join("interrupted.jsonl");
        write_rollout_with_timestamp(&rollout, "2026-03-05T00:00:00.000Z");

        write_potter_rollout(
            main.parent().expect("project dir"),
            main.strip_prefix(workdir).expect("rel"),
            2,
            10,
            Path::new("interrupted.jsonl"),
            PotterRoundOutcome::Interrupted,
            None,
        );

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PotterProjectListStatus::Interrupted);
        assert_eq!(rows[0].rounds, 2);
    }

    #[test]
    fn discover_projects_marks_failed_and_succeeded_statuses() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main_failed = write_main(workdir, ".codexpotter/projects/2026/03/01/1", None);
        let main_ok = write_main(workdir, ".codexpotter/projects/2026/03/01/2", None);

        let rollout_failed = workdir.join("failed.jsonl");
        let rollout_ok = workdir.join("ok.jsonl");
        write_rollout_with_timestamp(&rollout_failed, "2026-03-01T00:00:00.000Z");
        write_rollout_with_timestamp(&rollout_ok, "2026-03-01T00:01:00.000Z");

        write_potter_rollout(
            main_failed.parent().expect("project dir"),
            main_failed.strip_prefix(workdir).expect("rel"),
            2,
            10,
            Path::new("failed.jsonl"),
            PotterRoundOutcome::Fatal {
                message: "boom".to_string(),
            },
            None,
        );

        write_potter_rollout(
            main_ok.parent().expect("project dir"),
            main_ok.strip_prefix(workdir).expect("rel"),
            4,
            10,
            Path::new("ok.jsonl"),
            PotterRoundOutcome::Completed,
            Some(4),
        );

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 2);

        let ok = rows
            .iter()
            .find(|row| row.project_dir == Path::new(".codexpotter/projects/2026/03/01/2"))
            .expect("ok row");
        assert_eq!(ok.status, PotterProjectListStatus::Succeeded);
        assert_eq!(ok.rounds, 4);

        let failed = rows
            .iter()
            .find(|row| row.project_dir == Path::new(".codexpotter/projects/2026/03/01/1"))
            .expect("failed row");
        assert_eq!(failed.status, PotterProjectListStatus::Failed);
        assert_eq!(failed.rounds, 2);
    }

    #[test]
    fn discover_projects_marks_unfinished_round_as_incomplete() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main = write_main(workdir, ".codexpotter/projects/2026/03/02/1", None);
        let rollout = workdir.join("live.jsonl");
        write_rollout_with_timestamp(&rollout, "2026-03-02T00:00:00.000Z");

        let potter_rollout_path = main
            .parent()
            .expect("project dir")
            .join(crate::workflow::rollout::POTTER_ROLLOUT_FILENAME);
        let user_prompt_file = main.strip_prefix(workdir).expect("rel").to_path_buf();
        let thread_id =
            codex_protocol::ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000")
                .expect("thread id");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message: Some("live prompt".to_string()),
                user_prompt_file,
            },
        )
        .expect("append project_started");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: Path::new("live.jsonl").to_path_buf(),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
                duration_secs: 0,
            },
        )
        .expect("append round_finished");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 2,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: Path::new("live.jsonl").to_path_buf(),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PotterProjectListStatus::Incomplete);
        assert_eq!(rows[0].rounds, 2);
    }

    #[test]
    fn discover_projects_keeps_unfinished_first_round_incomplete() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main = write_main(workdir, ".codexpotter/projects/2026/03/02/2", None);
        let rollout = workdir.join("cancelled.jsonl");
        write_rollout_with_timestamp(&rollout, "2026-03-02T00:00:00.000Z");

        let potter_rollout_path = main
            .parent()
            .expect("project dir")
            .join(crate::workflow::rollout::POTTER_ROLLOUT_FILENAME);
        let user_prompt_file = main.strip_prefix(workdir).expect("rel").to_path_buf();
        let thread_id =
            codex_protocol::ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000")
                .expect("thread id");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message: Some("cancelled prompt".to_string()),
                user_prompt_file,
            },
        )
        .expect("append project_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: Path::new("cancelled.jsonl").to_path_buf(),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PotterProjectListStatus::Incomplete);
        assert_eq!(rows[0].rounds, 1);
    }

    #[test]
    fn discover_projects_marks_unfinished_first_round_with_interrupted_turn_as_cancelled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main = write_main(workdir, ".codexpotter/projects/2026/03/02/6", None);
        let rollout = workdir.join("interrupted.jsonl");
        write_rollout_with_interrupted_turn(&rollout, "2026-03-02T00:00:00.000Z");

        let potter_rollout_path = main
            .parent()
            .expect("project dir")
            .join(crate::workflow::rollout::POTTER_ROLLOUT_FILENAME);
        let user_prompt_file = main.strip_prefix(workdir).expect("rel").to_path_buf();
        let thread_id =
            codex_protocol::ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000")
                .expect("thread id");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message: Some("interrupted prompt".to_string()),
                user_prompt_file,
            },
        )
        .expect("append project_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: Path::new("interrupted.jsonl").to_path_buf(),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PotterProjectListStatus::Cancelled);
        assert_eq!(rows[0].rounds, 1);
        assert_eq!(rows[0].description, "interrupted prompt");
        assert_eq!(rows[0].started_at_unix_secs, Some(1_772_409_600));
    }

    #[test]
    fn discover_projects_keeps_malformed_unfinished_first_round_incomplete() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main = write_main(workdir, ".codexpotter/projects/2026/03/02/3", None);
        let potter_rollout_path = main
            .parent()
            .expect("project dir")
            .join(crate::workflow::rollout::POTTER_ROLLOUT_FILENAME);
        let user_prompt_file = main.strip_prefix(workdir).expect("rel").to_path_buf();

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message: Some("cancelled prompt".to_string()),
                user_prompt_file,
            },
        )
        .expect("append project_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
        )
        .expect("append round_started");

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PotterProjectListStatus::Incomplete);
        assert_eq!(rows[0].rounds, 1);
        assert_eq!(rows[0].description, "cancelled prompt");
        assert_eq!(rows[0].started_at_unix_secs, None);
    }

    #[test]
    fn discover_projects_best_effort_rounds_and_started_at_when_resume_index_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main = write_main(workdir, ".codexpotter/projects/2026/03/02/4", None);
        let rollout = workdir.join("live.jsonl");
        write_rollout_with_timestamp(&rollout, "2026-03-02T00:00:00.000Z");

        let potter_rollout_path = main
            .parent()
            .expect("project dir")
            .join(crate::workflow::rollout::POTTER_ROLLOUT_FILENAME);
        let user_prompt_file = main.strip_prefix(workdir).expect("rel").to_path_buf();
        let thread_id =
            codex_protocol::ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000")
                .expect("thread id");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message: Some("live prompt".to_string()),
                user_prompt_file,
            },
        )
        .expect("append project_started");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: Path::new("live.jsonl").to_path_buf(),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
                duration_secs: 0,
            },
        )
        .expect("append round_finished");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 2,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: Path::new("live.jsonl").to_path_buf(),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
                duration_secs: 0,
            },
        )
        .expect("append round_finished");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 3,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: Path::new("live.jsonl").to_path_buf(),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
                duration_secs: 0,
            },
        )
        .expect("append round_finished");

        // Resumed window: round indices reset, but the total rounds should keep growing.
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
        )
        .expect("append round_started");

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PotterProjectListStatus::Incomplete);
        assert_eq!(rows[0].rounds, 4);
        assert_eq!(rows[0].description, "live prompt");
        assert!(rows[0].started_at_unix_secs.is_some());
    }

    #[test]
    fn discover_projects_marks_project_succeeded_prefix_as_succeeded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main = write_main(workdir, ".codexpotter/projects/2026/03/02/5", None);
        let rollout = workdir.join("succeeded.jsonl");
        write_rollout_with_timestamp(&rollout, "2026-03-02T00:00:00.000Z");

        let project_dir = main.parent().expect("project dir");
        let potter_rollout_path =
            project_dir.join(crate::workflow::rollout::POTTER_ROLLOUT_FILENAME);
        let user_prompt_file = main.strip_prefix(workdir).expect("rel").to_path_buf();
        let thread_id =
            codex_protocol::ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000")
                .expect("thread id");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectStarted {
                user_message: Some("live success".to_string()),
                user_prompt_file: user_prompt_file.clone(),
            },
        )
        .expect("append project_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 3,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: PathBuf::from("succeeded.jsonl"),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectSucceeded {
                rounds: 1,
                duration_secs: 1,
                user_prompt_file,
                git_commit_start: "".to_string(),
                git_commit_end: "".to_string(),
            },
        )
        .expect("append project_succeeded");

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PotterProjectListStatus::Succeeded);
        assert_eq!(rows[0].rounds, 1);
        assert_eq!(rows[0].description, "live success");
        assert_eq!(rows[0].started_at_unix_secs, Some(1_772_409_600));
    }

    #[test]
    fn discover_projects_rounds_count_across_resume_windows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let main = write_main(workdir, ".codexpotter/projects/2026/03/04/1", None);
        let rollout = workdir.join("rollout.jsonl");
        write_rollout_with_timestamp(&rollout, "2026-03-04T00:00:00.000Z");

        let project_dir = main.parent().expect("project dir");
        let user_prompt_file = main.strip_prefix(workdir).expect("rel");

        // First iteration window: 3 rounds, then stop.
        write_potter_rollout(
            project_dir,
            user_prompt_file,
            3,
            10,
            Path::new("rollout.jsonl"),
            PotterRoundOutcome::Interrupted,
            None,
        );

        // Resumed window: rounds reset to 1, succeeds in 2 rounds.
        let potter_rollout_path =
            project_dir.join(crate::workflow::rollout::POTTER_ROLLOUT_FILENAME);
        let thread_id =
            codex_protocol::ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000")
                .expect("thread id");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: PathBuf::from("rollout.jsonl"),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
                duration_secs: 0,
            },
        )
        .expect("append round_finished");

        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundStarted {
                current: 2,
                total: 10,
            },
        )
        .expect("append round_started");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path: PathBuf::from("rollout.jsonl"),
                service_tier: None,
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        )
        .expect("append round_configured");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::ProjectSucceeded {
                rounds: 2,
                duration_secs: 1,
                user_prompt_file: user_prompt_file.to_path_buf(),
                git_commit_start: "".to_string(),
                git_commit_end: "".to_string(),
            },
        )
        .expect("append project_succeeded");
        crate::workflow::rollout::append_line(
            &potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
                duration_secs: 0,
            },
        )
        .expect("append round_finished");

        let rows = discover_projects_for_overlay(workdir).expect("discover");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PotterProjectListStatus::Succeeded);
        assert_eq!(rows[0].rounds, 5);
    }

    #[test]
    fn discover_resumable_projects_filters_missing_rollout_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();

        let ok_main = write_main(workdir, ".codexpotter/projects/2026/03/03/1", None);
        let missing_main = write_main(workdir, ".codexpotter/projects/2026/03/03/2", None);

        let ok_rollout = workdir.join("ok.jsonl");
        write_rollout_with_timestamp(&ok_rollout, "2026-03-03T00:00:00.000Z");

        write_potter_rollout(
            ok_main.parent().expect("project dir"),
            ok_main.strip_prefix(workdir).expect("rel"),
            1,
            10,
            Path::new("ok.jsonl"),
            PotterRoundOutcome::Interrupted,
            None,
        );
        write_potter_rollout(
            missing_main.parent().expect("project dir"),
            missing_main.strip_prefix(workdir).expect("rel"),
            1,
            10,
            Path::new("missing.jsonl"),
            PotterRoundOutcome::Interrupted,
            None,
        );

        let rows = discover_resumable_projects_for_overlay(workdir).expect("discover resumable");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].project_dir,
            PathBuf::from(".codexpotter/projects/2026/03/03/1")
        );
    }
}
