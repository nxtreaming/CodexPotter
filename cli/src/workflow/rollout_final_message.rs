//! Read the final assistant message from upstream `rollout.jsonl` logs.
//!
//! CodexPotter extracts the "final answer" message from upstream rollouts in a few places (for
//! example the projects overlay details pane and `Potter.ProjectStop` hook payload assembly). Keep
//! the selection rules centralized so these surfaces stay consistent.

use std::io::BufRead as _;
use std::path::Path;

use anyhow::Context;
use chrono::DateTime;

/// Read the most recent "final answer" agent message from an upstream rollout JSONL file.
///
/// Returns `(unix_secs, message)` when found, where `unix_secs` is derived from the entry's RFC
/// 3339 `timestamp`.
///
/// Selection rules:
/// - Prefer the last `agent_message` with `phase = "final_answer"`.
/// - Otherwise, fall back to the last `agent_message` without a phase (phase unknown; treated as
///   final answer for compatibility).
/// - If no suitable `agent_message` exists, fall back to the last `turn_complete.last_agent_message`
///   when present (legacy senders that attach the final answer to TurnComplete instead of an
///   agent message item).
pub fn read_final_agent_message_from_rollout(
    rollout_path: &Path,
) -> anyhow::Result<(Option<u64>, Option<String>)> {
    let file = std::fs::File::open(rollout_path)
        .with_context(|| format!("open rollout {}", rollout_path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut last_without_phase: Option<(u64, String)> = None;
    let mut last_final: Option<(u64, String)> = None;
    let mut last_turn_complete: Option<(u64, String)> = None;

    for (idx, line) in reader.lines().enumerate() {
        let line_number = idx + 1;
        let line = line.with_context(|| format!("read rollout line {line_number}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("parse rollout json line {line_number}: {trimmed}"))?;

        let item_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if item_type != "event_msg" {
            continue;
        }

        let Some(payload) = value.get("payload") else {
            continue;
        };
        let payload_type = payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if payload_type != "agent_message" && payload_type != "turn_complete" {
            continue;
        }

        let Some(ts) = value.get("timestamp").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let parsed = DateTime::parse_from_rfc3339(ts)
            .with_context(|| format!("parse rollout timestamp {ts:?}"))?;
        let unix_secs = u64::try_from(parsed.timestamp())
            .context("convert rollout timestamp to unix seconds")?;

        match payload_type {
            "agent_message" => {
                let Some(message) = payload.get("message").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let phase = payload.get("phase").and_then(serde_json::Value::as_str);
                let message = message.to_string();

                match phase {
                    Some("final_answer") => {
                        last_final = Some((unix_secs, message));
                    }
                    Some(_) => {}
                    None => {
                        last_without_phase = Some((unix_secs, message));
                    }
                }
            }
            "turn_complete" => {
                let Some(message) = payload
                    .get("last_agent_message")
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let message = message.to_string();
                if !message.is_empty() {
                    last_turn_complete = Some((unix_secs, message));
                }
            }
            _ => {}
        }
    }

    // The overlay/hook payload should show each round's conclusion, not mid-turn commentary.
    // Prefer explicit `final_answer` phases when present. Phase-less messages are treated as final
    // answers for compatibility, even if other messages include phase metadata.
    let selected = last_final.or(last_without_phase).or(last_turn_complete);

    let Some((secs, message)) = selected else {
        return Ok((None, None));
    };
    let secs = if secs == 0 { None } else { Some(secs) };
    let message = if message.is_empty() {
        None
    } else {
        Some(message)
    };
    Ok((secs, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn final_agent_message_prefers_final_answer_phase() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-03-01T00:00:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"commentary","phase":"commentary"}}
{"timestamp":"2026-03-01T00:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"final","phase":"final_answer"}}
"#,
        )
        .expect("write rollout");

        let (secs, message) = read_final_agent_message_from_rollout(&rollout_path).expect("read");
        assert_eq!(secs, Some(1_772_323_201));
        assert_eq!(message.as_deref(), Some("final"));
    }

    #[test]
    fn final_agent_message_falls_back_to_last_message_when_phase_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-03-01T00:00:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"first"}}
{"timestamp":"2026-03-01T00:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"second"}}
"#,
        )
        .expect("write rollout");

        let (secs, message) = read_final_agent_message_from_rollout(&rollout_path).expect("read");
        assert_eq!(secs, Some(1_772_323_201));
        assert_eq!(message.as_deref(), Some("second"));
    }

    #[test]
    fn final_agent_message_uses_phase_missing_message_even_when_commentary_phase_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-03-01T00:00:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"commentary","phase":"commentary"}}
{"timestamp":"2026-03-01T00:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"final without phase"}}
"#,
        )
        .expect("write rollout");

        let (secs, message) = read_final_agent_message_from_rollout(&rollout_path).expect("read");
        assert_eq!(secs, Some(1_772_323_201));
        assert_eq!(message.as_deref(), Some("final without phase"));
    }

    #[test]
    fn final_agent_message_falls_back_to_turn_complete_last_agent_message_when_needed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-03-01T00:00:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"commentary","phase":"commentary"}}
{"timestamp":"2026-03-01T00:00:01.000Z","type":"event_msg","payload":{"type":"turn_complete","turn_id":"turn-1","last_agent_message":"final"}}
"#,
        )
        .expect("write rollout");

        let (secs, message) = read_final_agent_message_from_rollout(&rollout_path).expect("read");
        assert_eq!(secs, Some(1_772_323_201));
        assert_eq!(message.as_deref(), Some("final"));
    }

    #[test]
    fn final_agent_message_handles_paths_resolved_for_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path();
        let rollout_rel = Path::new("nested/rollout.jsonl");
        let rollout_path = workdir.join(rollout_rel);
        std::fs::create_dir_all(rollout_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-03-01T00:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"final","phase":"final_answer"}}
"#,
        )
        .expect("write rollout");

        let rollout_path = crate::workflow::replay_session_config::resolve_rollout_path_for_replay(
            workdir,
            rollout_rel,
        );
        let (secs, message) = read_final_agent_message_from_rollout(&rollout_path).expect("read");
        assert_eq!(secs, Some(1_772_323_201));
        assert_eq!(message.as_deref(), Some("final"));
    }

    #[test]
    fn final_agent_message_does_not_fall_back_to_commentary_phase() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-03-01T00:00:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"commentary","phase":"commentary"}}
"#,
        )
        .expect("write rollout");

        let (secs, message) = read_final_agent_message_from_rollout(&rollout_path).expect("read");
        assert_eq!(secs, None);
        assert_eq!(message, None);
    }

    #[test]
    fn final_agent_message_ignores_invalid_timestamps_on_non_agent_events() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"not-a-date","type":"event_msg","payload":{"type":"token_count"}}
{"timestamp":"2026-03-01T00:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"final","phase":"final_answer"}}
"#,
        )
        .expect("write rollout");

        let (secs, message) = read_final_agent_message_from_rollout(&rollout_path).expect("read");
        assert_eq!(secs, Some(1_772_323_201));
        assert_eq!(message.as_deref(), Some("final"));
    }
}
