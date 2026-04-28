# Verbosity

## Overview

Verbosity controls how much interim transcript detail CodexPotter shows while a task runs. It is a
presentation policy only: it must not change model behavior, project state, persisted artifacts, or
the control-plane outcome of a round.

CodexPotter currently supports two user-facing verbosity modes:

- `minimal`: show results only.
- `simple`: show key progress updates.

This document is the source of truth for verbosity product behavior and technical direction. It
intentionally avoids low-level rendering, buffering, and event-loop implementation details.

## Goals

- Keep the transcript readable during long-running work.
- Preserve final answers, errors, and project outcomes across all modes.
- Let users choose between a compact result-focused view and a progress-oriented view.
- Keep interactive TUI output and `codex-potter exec` human output aligned in visibility policy.
- Make new output categories choose an explicit behavior for each verbosity mode.

## Non-goals

- Changing the model's reasoning effort or prompts.
- Changing machine-readable `--json` output.
- Hiding failures, interruptions, warnings that require user attention, or final project summaries.
- Providing a full debug log of every upstream event.
- Reconstructing transcript history that was intentionally suppressed by the chosen mode.

## User Selection

Interactive sessions use the configured default verbosity. If no default has been configured yet,
the TUI may prompt the user to choose one during startup.

Users can change the default from the TUI with `/verbosity`. Non-interactive human output can be
overridden for a single command with `codex-potter exec --verbosity <minimal|simple>`.

`minimal` is the compact mode. `simple` is the more informative mode. Older config names that map to
`simple` are compatibility aliases, not distinct product modes.

## Shared Visibility Rules

All modes should keep these visible:

- User-visible errors and fatal failures.
- Interruptions and cancellation outcomes.
- Final assistant answers.
- Project and round outcome summaries.
- Information needed to understand whether the task completed, failed, or needs follow-up.

All modes should avoid showing raw reasoning messages as transcript content. Reasoning may update
status text, but it should not become durable transcript history by itself.

Verbosity should hide or summarize low-signal progress without losing the ability to diagnose real
failures. A failed command, malformed project state, or unrecoverable error should not disappear just
because the user selected `minimal`.

## Simple Mode

Simple mode is for users who want key progress updates without a noisy raw event stream.

Expected behavior:

- Reasoning messages are not rendered into the transcript.
- Successful non-user-shell command runs are summarized without output previews.
- Adjacent successful command runs may be collapsed into one progress item.
- Exploratory file/list/search activity may be collapsed into compact `Explored` summaries.
- Search activity is shown as compact `Searched` summaries.
- Image-view activity is shown as compact `Viewed Image` summaries.
- Plans, meaningful tool output, changes, warnings, errors, and final answers remain visible.

Simple mode may use live transient progress blocks in the interactive TUI while work is still
running. Once ordering matters for the transcript, those blocks should settle into coherent history
items rather than leaving stale live output behind.

## Minimal Mode

Minimal mode is for users who primarily want the final result and essential status.

Minimal mode includes the Simple-mode suppressions and additionally:

- `Ran` and `Explored` items are hidden.
- `Searched` and `Viewed Image` items are hidden.
- Plan output is hidden.
- Generic duration separators such as `Worked for ...` are hidden.
- File changes are coalesced into a compact file-list summary without diff bodies.
- Coalesced file-change summaries preserve event order rather than sorting paths alphabetically.
- Non-commentary assistant messages render without dimming.
- Streamed assistant text should become transcript history only after it is complete.

Commentary-phase assistant messages are treated as status, not transcript content:

- In the interactive TUI, the latest commentary is shown as a transient dim status block.
- A newer commentary message replaces the previous one.
- Commentary clears when superseded by non-commentary or final assistant output, or when the turn
  ends.
- Commentary must not split compact file-change previews or leak partial streamed text into durable
  transcript history.

For replay and compatibility, when no non-commentary assistant message was emitted, a final message
available from turn-completion metadata may be rendered as the final answer.

## Interactive TUI vs Exec Human Output

Interactive TUI output may update live transient regions in place. `codex-potter exec` without
`--json` is append-only, so it cannot revise earlier output in place.

Despite that mechanical difference, `exec` human output should follow the same broad visibility
policy:

- `minimal` stays result-focused and suppresses low-signal progress.
- `simple` shows key progress updates.
- Commentary in `minimal` appears as compact status hints instead of transient in-place blocks.
- Shimmer/status text may be emitted when it changes because append-only output has no persistent
  status area.
- Interactive-only round-finished separator lines are not emitted by `exec` human output.

Any future difference between interactive output and `exec` human output should be justified by the
append-only nature of `exec`, not by divergent product semantics.

## Technical Direction

Verbosity is a rendering policy layered over the same underlying event stream. It should not become
a second workflow state machine.

New output categories must define their behavior in both modes:

- Show in `simple` when the item helps users understand meaningful progress.
- Hide or compact in `minimal` unless the item affects final outcome, user action, or failure
  diagnosis.
- Keep errors and unrecoverable conditions visible in every mode.

Prefer semantic categories over command-string-specific rules. If a category is hard to explain to a
user, it is probably too low-level for `simple` and should be hidden or summarized in `minimal`.

When adding compact summaries, prioritize stable ordering and scannability over showing every
detail. Full details should remain available through persisted artifacts or explicit command output
when they are important.

## Testing Direction

Verbosity changes should test user-visible behavior, not private buffering structure.

Important coverage:

- Mode labels, config values, and one-shot overrides.
- Simple-mode summaries for successful commands, exploration, search, image viewing, and changes.
- Minimal-mode suppression of low-signal progress.
- Minimal-mode compact change summaries and ordering.
- Commentary status behavior in interactive output.
- Append-only commentary/status behavior in `exec` human output.
- Final answers, failures, interrupts, and project outcomes remaining visible in every mode.

Snapshot tests are appropriate for rendered TUI output. Unit tests are appropriate for mode parsing
and compact-summary policy.

## Related Documents

- `docs/wiki/cli.md`
- `docs/wiki/config-and-conventions.md`
- `docs/wiki/tui-design.md`
- `docs/wiki/interactive-testing.md`
