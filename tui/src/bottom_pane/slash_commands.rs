//! Shared helpers for filtering and matching built-in slash commands.
//!
//! This is a small subset of upstream Codex CLI behavior.

use crate::slash_command::SlashCommand;
use crate::slash_command::built_in_slash_commands;

use super::fuzzy_match::fuzzy_match;

/// Return the built-ins that should be visible/usable for the current input.
pub fn builtins_for_input() -> Vec<(&'static str, SlashCommand)> {
    built_in_slash_commands()
}

/// Find a single built-in command by exact name.
pub fn find_builtin_command(name: &str) -> Option<SlashCommand> {
    builtins_for_input()
        .into_iter()
        .find(|(command_name, _)| *command_name == name)
        .map(|(_, cmd)| cmd)
}

/// Whether any visible built-in fuzzily matches the provided prefix.
pub fn has_builtin_prefix(name: &str) -> bool {
    builtins_for_input()
        .into_iter()
        .any(|(command_name, _)| fuzzy_match(command_name, name).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn mention_command_resolves_for_dispatch() {
        assert_eq!(find_builtin_command("mention"), Some(SlashCommand::Mention));
    }

    #[test]
    fn yolo_command_resolves_for_dispatch() {
        assert_eq!(find_builtin_command("yolo"), Some(SlashCommand::Yolo));
    }

    #[test]
    fn compact_kb_command_resolves_for_dispatch() {
        assert_eq!(
            find_builtin_command("compact-kb"),
            Some(SlashCommand::CompactKb)
        );
    }

    #[test]
    fn ps_command_resolves_for_dispatch() {
        assert_eq!(find_builtin_command("ps"), Some(SlashCommand::Ps));
    }

    #[test]
    fn stop_command_resolves_for_dispatch() {
        assert_eq!(find_builtin_command("stop"), Some(SlashCommand::Stop));
    }
}
