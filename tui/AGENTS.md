# codex-potter TUI

## Overview

This `tui/` crate is expected to match upstream Codex CLI TUI behavior and styles as closely as possible,
so that users switching between codex and codex-potter have a consistent experience.

Unless explicitly documented below, changes should preserve parity.

## Explicit Divergences

Content below lists explicit divergences in codex-potter's TUI compared to upstream codex's TUI.

When introducing new changes, first identify whether it is a divergence from upstream or it makes the code more aligned with upstream.
Divergences must be documented in places below to avoid regression when syncing changes from upstream:

- Record divergences in this file, keep words concise but clear, be specific about the new behavior.
- Record divergences in doc comments.
- Cover divergences via proper tests (unit / end to end).

### Text Box

- Supports `$` skills picker, the same as upstream.
- Slash command picker exists but only supports `/mention`, `/list`, `/theme`, `/verbosity`, `/yolo`, `/compact-kb` (inserts a canned KB cleanup prompt), `/exit`, `/potter:xmodel` (inserts a literal marker only).
- No `?` shortcuts overlay (treat `?` as a literal character).
- `Tab` inserts a literal tab character (`\t`) into the composer.
- Composer placeholder text is customized.
- No Esc-driven rewind/backtrack UX; `Esc` interrupts running project and otherwise dismisses popups.
- No steer mode (always queue).
- Hardens non-bracketed paste bursts against delayed trailing `Enter` key events: after a burst flush, keep Enter suppression alive briefly so they insert a newline instead of submit/queue.
- Uses a slightly longer non-Windows paste-burst idle timeout (16ms) to avoid splitting large pastes under scheduler jitter.
- No image pasting support.
- Bottom pane prompt footer shows the ctrl+g editor hint first, then the optional git branch + working dir (`<branch> ❯ <dir>`). When YOLO is active, it prefixes the footer with a red bold `▲YOLO`.
- Better word jump by using ICU4X word segmentation, plus grouping consecutive identical ASCII separators as a single segment (e.g. `====` one jump; `+-` splits).
- Prompt history is persisted under `~/.codexpotter/history.jsonl`.

### Message Items

- /verbosity provides finer-grained control over what content is printed:
  Simple mode:
  - Reasoning messages are never rendered.
  - Successful `Ran` items suppress output preview and adjacent ones are collapsed into one.
  - `Explored` items are more aggressively collapsed.
  Minimal mode:
  - With all the above Simple-mode suppressions, plus:
  - `phase = commentary` agent messages render as a single dim transient transcript block that updates in place, never enters transcript history, stays visible across ordinary tool/history output, and clears only when replaced by newer commentary, superseded by non-commentary/final agent output, or the turn ends (append-only exec still prints them as status hints). Commentary deltas must not split the compact Change preview, and the live Change preview renders above the commentary block so the commentary reads like a status area rather than transcript history.
  - Non-commentary agent messages are rendered without dimming (no gray agent messages in the transcript).
  - Streamed agent text is committed only after completion; the latest completed non-commentary agent message may stay pending until a transcript barrier or `TurnComplete`.
  - Tool/result barriers flush only completed Minimal-mode agent messages; in-flight `AgentMessageDelta` text waits for the completed `AgentMessage` (or is dropped on abnormal termination) so commentary deltas cannot leak into transcript history.
  - `TurnComplete.last_agent_message` is rendered as the final answer when no non-commentary agent message was emitted (legacy/replay compatibility).
  - Plan tool output is hidden
  - All `Ran` and `Explored` items are hidden
  - `Worked for ...` separators are hidden
  - Consecutive Change (Edited, Created, Deleted) items are coalesced into one, and provide file list only, no diff body.
  - The coalesced Change file list preserves patch event order instead of sorting paths alphabetically.
- Consecutive `Viewed Image` items are coalesced into one block in Simple mode, preserve event order, and render live as new paths arrive; Minimal mode hides them.
- Consecutive `Searched` items are coalesced into one block in Simple mode and render live as new queries arrive; Minimal mode hides them.
- Additional codex-potter items (e.g. project creation hints, stream recovery retries, project-finished summary on success / budget exhaustion).
- In interactive mode, after each CodexPotter round finishes, emits a dim `─ Round finished in … ─` separator line in the transcript (before any CodexPotter summary blocks).
- `codex-potter exec` without `--json`:
  - renders content similar to interactive mode, respect verbosity, but in append-only way — never folds/coalesces prior output.
  - additionally emits the text of the shimmer when it changes.
  - does not emit the dim `─ Round finished in … ─` separator line.
- Hook status output uses protocol kebab-case event labels (for example `session-start`) to match
  exec output and hook run identifiers.

### Shimmer

- Round prefix is added to shimmer lines.
- Round prefix includes a dim total elapsed timer since the current project started.
- Remaining context window is moved into the shimmer area.
- No `esc to interrupt` message (even though `Esc` interrupts running tasks).

### Other differences

Behavior related

- A customized banner on startup; the first-screen model label appends `[fast]` when layered Codex config resolves `service_tier = "fast"` and `features.fast_mode` remains enabled
- Home-relative `CODEX_HOME` values are expanded before resolving TUI config, themes, and skill roots (including Windows-native `~\...`)
- Additionally shows gitignore startup hint
- When a frame does not request a cursor position, hide the cursor before flushing frame diffs to avoid visible cursor movement (the cursor may be visible in the previous frame).
- Startup onboarding prompts:
  - Suggest adding `.codexpotter/` to the global gitignore.
  - If no `[tui].verbosity` is configured yet, prompt for a default verbosity level.
  - When both prompts are shown, they render `Setup 1/2` and `Setup 2/2` markers.
- Multi-agent collab is transcript-only: no agent thread picker UI (no per-agent transcript view).
- Resume picker UI reuses the projects overlay UI (same as `Ctrl+L` / `/list`) with `Enter` to resume and `Esc` to start a new project.
- Auto retry on errors (successful recoveries are transient-only; unrecoverable errors are surfaced).
- Customized update notification / self-update (and on-disk state under `~/.codexpotter/`).
- No desktop notifications when the terminal is unfocused.
- Esc triggers project interrupt with an action selection UI instead of turn interrupt.
- `Ctrl+L` (or `/list`) opens a full-screen projects list overlay with round summaries (also available on the prompt screen before any rounds start).
- Projects overlay stays open across round boundaries (does not auto-close when a round ends).
- Projects overlay auto-refreshes the list (and selected details) every minute while open, preserving selection + scroll positions when possible.
- Projects overlay supports `Tab` to toggle a maximized details view (hide the list pane).
- Projects overlay details text wraps to at most 100 columns while not maximized, even when the right pane is wider; maximized details view still uses the full pane width.
- Projects overlay details pane shows a plain-text preview of the original user task message (first 5 lines + `... (N more lines)`) above the per-round final message summaries.
- Projects overlay round headings show per-round duration when available: `ROUND N (took …) @ … ago` (otherwise `ROUND N`).
- Projects overlay status colors distinguish cancelled-before-completion projects (dim), round-budget exhaustion (red), and post-completed-round interruptions (orange).
- Project summary `Loop more rounds:` resume command includes the current process's non-default `codex-potter` global flags (aligns with the CLI exit resume note).

Engineering related:

- Unneeded logics and codes in codex TUI are intentionally removed to keep code tidy and focus (codex-potter's TUI is a _subset_ of codex's TUI):
  - `?` shortcuts overlay, /model selection, most slash commands
  - Rewind (esc)
  - Approval flows
  - Other interactive features not needed
  - Unneeded codes, tests and snapshots
- codex-potter explicitly forbids `pub(crate)` visibility in TUI code; only `pub` and private items are allowed.
- `bottom_pane::textarea::TextArea` keeps atomic text elements as anonymous ranges only; upstream named-element helpers stay removed until codex-potter needs those flows.
- codex-potter does not use Bazel.

## Conventions

- TUI should stay a pure UI module: business logic, filesystem access, and project-specific data loading belong outside `tui/`.
- Default rule: transcript rendering and long-lived task state should still be driven by `EventMsg` / upstream protocol events so upstream parity stays straightforward.
- Exception: TUI-owned popup state machines that are generic UI affordances may be driven by a narrow request/response provider instead of new protocol `Op` / `EventMsg` variants when that keeps protocol semantics cleaner. `projects overlay` is the canonical example:
  - TUI owns the popup state machine and key routing (`tui/src/app_server_render.rs`, `tui/src/projects_overlay.rs`).
  - CLI/workflow owns discovery + detail loading via `ProjectsOverlayProviderChannels` (`tui/src/potter_tui.rs`, `cli/src/workflow/projects_overlay_backend.rs`).
  - Do not move this popup back into protocol-only driver messages unless the popup stops being a TUI concern.

- Test: Always use snapshot tests (without ASCII escape sequences) for TUI rendering tests, so that it is visually clear what the output looks like, unless the test or code comes from upstream codex where non-snapshot tests are used, in which case you must preserve parity.

- IMPORTANT: Isolate divergent code paths: Prefer to use a new file to isolate changed logic from upstream codex, and keep the original file as a subset of the upstream's file, if the changed logic is significant. In this way, we can easily learn what has changed from upstream, and reduce merge conflicts when syncing from upstream.

## TUI Style conventions

See `styles.md`.

## TUI code conventions

- Use concise styling helpers from ratatui’s Stylize trait.
  - Basic spans: use "text".into()
  - Styled spans: use "text".red(), "text".green(), "text".magenta(), "text".dim(), etc.
  - Prefer these over constructing styles with `Span::styled` and `Style` directly.
  - Example: patch summary file lines
    - Desired: vec!["  └ ".into(), "M".red(), " ".dim(), "tui/src/app.rs".dim()]

### TUI Styling (ratatui)

- Prefer Stylize helpers: use "text".dim(), .bold(), .cyan(), .italic(), .underlined() instead of manual Style where possible.
- Prefer simple conversions: use "text".into() for spans and vec![…].into() for lines; when inference is ambiguous (e.g., Paragraph::new/Cell::from), use Line::from(spans) or Span::from(text).
- Computed styles: if the Style is computed at runtime, using `Span::styled` is OK (`Span::from(text).set_style(style)` is also acceptable).
- Avoid hardcoded white: do not use `.white()`; prefer the default foreground (no color).
- Chaining: combine helpers by chaining for readability (e.g., url.cyan().underlined()).
- Single items: prefer "text".into(); use Line::from(text) or Span::from(text) only when the target type isn’t obvious from context, or when using .into() would require extra type annotations.
- Building lines: use vec![…].into() to construct a Line when the target type is obvious and no extra type annotations are needed; otherwise use Line::from(vec![…]).
- Avoid churn: don’t refactor between equivalent forms (Span::styled ↔ set_style, Line::from ↔ .into()) without a clear readability or functional gain; follow file‑local conventions and do not introduce type annotations solely to satisfy .into().
- Compactness: prefer the form that stays on one line after rustfmt; if only one of Line::from(vec![…]) or vec![…].into() avoids wrapping, choose that. If both wrap, pick the one with fewer wrapped lines.

### Text wrapping

- Always use textwrap::wrap to wrap plain strings.
- If you have a ratatui Line and you want to wrap it, use the helpers in tui/src/wrapping.rs, e.g. word_wrap_lines / word_wrap_line.
- If you need to indent wrapped lines, use the initial_indent / subsequent_indent options from RtOptions if you can, rather than writing custom logic.
- If you have a list of lines and you need to prefix them all with some prefix (optionally different on the first vs subsequent lines), use the `prefix_lines` helper from line_utils.
