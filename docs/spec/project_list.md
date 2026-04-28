# Project List

## Overview

The project list is the canonical browsing surface for CodexPotter projects in the current
workspace. It lets users inspect prior and in-progress projects, understand their outcomes, review
round summaries, and choose a project to resume.

This document is the source of truth for product behavior and technical direction. It intentionally
does not specify low-level parsing, rendering, or event-loop details; those belong in code and
focused implementation notes.

## Goals

- Provide a fast, read-only overview of CodexPotter projects in the current workspace.
- Make project outcomes understandable without opening progress files by hand.
- Surface enough detail to decide whether a project should be resumed or inspected further.
- Reuse one interaction model for the live project list and the `codex-potter resume` picker.
- Keep project discovery and persistence concerns out of the TUI rendering layer.

## Non-goals

- Editing, deleting, renaming, archiving, or reordering projects.
- Searching or filtering projects.
- Rendering full transcripts.
- Reconstructing complete upstream session state from the project list UI.
- Treating the project list as the authoritative project storage format.

## User-facing Behavior

The project list is available from both the prompt screen and an active round.

- `Ctrl+L` opens the project list.
- `/list` opens the same project list through the slash command picker.
- When opened during a running round, the current task continues running; the list is an overlay for
  inspection, not an interrupt or pause mechanism.
- Closing the overlay returns the user to the same interactive context.

The normal project list is read-only:

- `Esc`, `Ctrl+L`, or `Ctrl+C` closes the overlay.
- Up and Down change the selected project.
- Detail scrolling is available for long project details.
- `Tab` toggles a maximized details view.

The resume picker reuses the project list surface with resume-specific actions:

- `Enter` resumes the selected project.
- `Esc` starts a fresh project instead of resuming.
- `Ctrl+C` exits the picker.

## List Contents

The normal project list shows all discoverable CodexPotter projects under the current workspace's
project storage area. Projects are ordered newest first when a reliable project start time is known.
When start time is unavailable, ordering remains deterministic.

Each project row should communicate:

- Project status.
- Round count.
- Approximate start age when available.
- A short human-facing description.

The description should prefer an explicit short title when one exists. If no short title exists, it
should fall back to the original user task or the first useful goal text recorded for the project.
An empty description is allowed only when no meaningful project text is available.

The list must have clear empty and error states. Missing, malformed, or partial data should be
surfaced as visible state instead of silently disappearing from the normal list whenever the project
can still be identified as a project.

## Details Pane

The details pane shows contextual information for the selected project.

It should include:

- The progress file path.
- The recorded git branch when available.
- A preview of the original user task when available.
- Per-round summaries in project order.

Each round summary should show:

- The round number.
- Duration when available.
- Approximate final-message time when available.
- The final assistant message when available.

If a round has no final assistant message, the details pane should make that absence explicit. If
details cannot be loaded, the details pane should show a clear error for the selected project.

The details pane is a summary surface. It should stay concise and scannable; users who need full
history should inspect the persisted project artifacts directly.

In the normal split-pane view, details text should wrap to at most 100 columns even if the right pane
is wider. Maximized details view may use the full available width.

## Status Semantics

Project status is user-facing and should remain stable:

- `Succeeded`: the project recorded a successful completion marker.
- `Cancelled`: the project stopped before any round completed successfully.
- `Budget exhausted`: all configured rounds were used without a successful completion marker.
- `Interrupted`: the project stopped after at least one round completed successfully.
- `Failed`: a round ended with a task failure or fatal error.
- `Incomplete`: the project is still running, malformed, missing a terminal outcome, or otherwise
  cannot be classified as a completed terminal state.

Prefer showing `Incomplete` over guessing a terminal outcome when the persisted evidence is not
strong enough. This keeps ambiguous project state visible without inventing false certainty.

## Refresh and Continuity

The project list should remain useful while work is actively changing on disk.

- Opening the overlay requests the latest list.
- While the overlay stays open, it should refresh every minute.
- Refreshes should preserve the selected project and scroll position when possible.
- Changing selection should refresh the selected project's details.
- If a project disappears or moves during refresh, the UI should keep a valid selection and continue
  to render a coherent state.

The overlay may remain open across round boundaries. This is intentional: users can watch project
state change without repeatedly reopening the list.

## Resume Picker Policy

The resume picker should show only projects that are expected to be resumable from persisted
artifacts. A project that is visible in the normal project list may be hidden from the resume picker
if required resume artifacts are missing.

This distinction is deliberate:

- The normal list is for inspection and should be permissive.
- The resume picker is for action and should avoid choices that are known to fail immediately.

## Technical Direction

The project list has three ownership boundaries:

- The workflow layer owns project discovery, artifact reading, status classification, and detail
  summary construction.
- The TUI layer owns overlay state, keyboard interaction, scrolling, and rendering.
- Shared protocol types define the data contract between those layers.

The TUI must not read project files directly or duplicate project classification rules. This keeps
business logic out of `tui/` and preserves the existing direction that `tui/` remains pure UI.

The project list is a read-only view over persisted project artifacts. It should not introduce a
second project database or cache that becomes another source of truth. Caching inside the overlay is
acceptable only as transient UI state.

New behavior should preserve the shared overlay model between `/list`, `Ctrl+L`, and the resume
picker unless there is a clear product reason to split them.

## Testing Direction

Project list changes should cover the behavior that users depend on:

- Project discovery and status classification.
- Description selection and ordering.
- Resumable-project filtering.
- Details-pane content for normal, missing, and partial project data.
- Overlay keyboard behavior and rendered states through snapshot tests.
- Prompt-screen, active-round, and resume-picker entry points.

Tests should stay focused on behavior. Avoid tests that lock in incidental rendering dimensions or
private implementation structure unless the dimension or structure is itself a user-facing contract.

## Related Documents

- `docs/wiki/progress-files-and-kb.md`
- `docs/wiki/resume.md`
- `docs/wiki/tui-design.md`
- `docs/wiki/interactive-testing.md`
