/// Built-in slash commands supported by the CodexPotter TUI.
///
/// This is intentionally a small subset of upstream Codex CLI. The command picker (`/`) and
/// dispatch logic rely on these definitions for names and descriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlashCommand {
    /// Insert a file mention trigger (`@`) into the composer.
    Mention,
    /// Open the projects list overlay (`/list`).
    List,
    /// Insert a canned prompt that asks CodexPotter to compact its local knowledge base.
    CompactKb,
    /// Configure whether to enable YOLO by default.
    Yolo,
    /// Insert the `/potter:xmodel` marker into the composer.
    PotterXModel,
    /// Open the syntax theme picker (`/theme`).
    Theme,
    /// Open the transcript verbosity picker (`/verbosity`).
    Verbosity,
    /// List background terminals (`/ps`).
    Ps,
    /// Stop all background terminals (`/stop`).
    Stop,
    /// Exit the TUI (`/exit`).
    Exit,
}

impl SlashCommand {
    /// User-visible description shown in the `/` command popup.
    pub fn description(self) -> &'static str {
        match self {
            SlashCommand::Mention => "mention a file",
            SlashCommand::List => "(ctrl+l) open the projects list overlay",
            SlashCommand::CompactKb => "compact CodexPotter's knowledge base",
            SlashCommand::Yolo => "configure whether to enable YOLO by default",
            SlashCommand::PotterXModel => {
                "(Experimental) Enable cross model review (round 1~3: GPT 5.2 xhigh, round 4+: GPT 5.5 xhigh)"
            }
            SlashCommand::Theme => "choose a syntax highlighting theme",
            SlashCommand::Verbosity => "choose how much detail to show",
            SlashCommand::Ps => "list background terminals",
            SlashCommand::Stop => "stop all background terminals",
            SlashCommand::Exit => "exit Codex",
        }
    }

    /// Command string without the leading '/'.
    pub fn command(self) -> &'static str {
        match self {
            SlashCommand::Mention => "mention",
            SlashCommand::List => "list",
            SlashCommand::CompactKb => "compact-kb",
            SlashCommand::Yolo => "yolo",
            SlashCommand::PotterXModel => "potter:xmodel",
            SlashCommand::Theme => "theme",
            SlashCommand::Verbosity => "verbosity",
            SlashCommand::Ps => "ps",
            SlashCommand::Stop => "stop",
            SlashCommand::Exit => "exit",
        }
    }

    /// Whether this command can be run while a task is in progress.
    pub fn available_during_task(self) -> bool {
        match self {
            SlashCommand::Theme => false,
            SlashCommand::Mention
            | SlashCommand::List
            | SlashCommand::CompactKb
            | SlashCommand::Yolo
            | SlashCommand::PotterXModel
            | SlashCommand::Verbosity
            | SlashCommand::Ps
            | SlashCommand::Stop
            | SlashCommand::Exit => true,
        }
    }

    /// Whether this command supports inline args (e.g. `/review ...`).
    pub fn supports_inline_args(self) -> bool {
        false
    }
}

/// Return all built-in commands in popup presentation order.
pub fn built_in_slash_commands() -> Vec<(&'static str, SlashCommand)> {
    // Keep order aligned with upstream Codex CLI for the subset we support.
    vec![
        (SlashCommand::Mention.command(), SlashCommand::Mention),
        (SlashCommand::List.command(), SlashCommand::List),
        (SlashCommand::Theme.command(), SlashCommand::Theme),
        (SlashCommand::Verbosity.command(), SlashCommand::Verbosity),
        (SlashCommand::Yolo.command(), SlashCommand::Yolo),
        (SlashCommand::CompactKb.command(), SlashCommand::CompactKb),
        (SlashCommand::Exit.command(), SlashCommand::Exit),
        (SlashCommand::Ps.command(), SlashCommand::Ps),
        (SlashCommand::Stop.command(), SlashCommand::Stop),
        (
            SlashCommand::PotterXModel.command(),
            SlashCommand::PotterXModel,
        ),
    ]
}
