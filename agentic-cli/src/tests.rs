#[cfg(test)]
mod unit_tests {
    use crate::cli::{Cli, Command, QueueModeArg};
    use clap::Parser;

    #[test]
    fn keyboard_release_events_are_ignored_but_press_and_repeat_are_processed() {
        use crossterm::event::KeyEventKind;

        assert!(crate::keyboard::should_process_key_kind(
            KeyEventKind::Press
        ));
        assert!(crate::keyboard::should_process_key_kind(
            KeyEventKind::Repeat
        ));
        assert!(!crate::keyboard::should_process_key_kind(
            KeyEventKind::Release
        ));
    }

    #[test]
    fn test_cli_run_command_parsing() {
        let cli = Cli::try_parse_from(["agentic", "run", "my task"]).unwrap();
        match cli.command {
            Some(Command::Run { task, .. }) => assert_eq!(task, "my task"),
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_interactive_command_parsing() {
        let cli = Cli::try_parse_from(["agentic", "interactive"]).unwrap();
        match cli.command {
            Some(Command::Interactive) => {}
            _ => panic!("Expected Interactive command"),
        }
    }

    #[test]
    fn test_cli_version() {
        let cli = Cli::try_parse_from(["agentic", "version"]).unwrap();
        match cli.command {
            Some(Command::Version) => {}
            _ => panic!("Expected Version command"),
        }
    }

    #[test]
    fn queue_mode_flag_defaults_to_follow_up() {
        let cli = Cli::try_parse_from(["agentic", "run", "task"]).unwrap();
        assert_eq!(cli.queue_mode, QueueModeArg::FollowUp);
    }

    #[test]
    fn queue_mode_flag_accepts_interrupt_before_subcommand() {
        let cli =
            Cli::try_parse_from(["agentic", "--queue-mode", "interrupt", "run", "task"]).unwrap();
        assert_eq!(cli.queue_mode, QueueModeArg::Interrupt);
    }

    #[test]
    fn queue_mode_flag_accepts_interrupt_after_subcommand() {
        let cli =
            Cli::try_parse_from(["agentic", "interactive", "--queue-mode", "interrupt"]).unwrap();
        assert_eq!(cli.queue_mode, QueueModeArg::Interrupt);
    }

    #[test]
    fn queue_mode_flag_rejects_unknown_value() {
        assert!(Cli::try_parse_from(["agentic", "run", "--queue-mode", "bogus", "task"]).is_err());
    }

    #[test]
    fn queue_mode_arg_converts_to_runtime_queue_mode() {
        let mode: crate::commands::QueueMode = QueueModeArg::Interrupt.into();
        assert_eq!(mode, crate::commands::QueueMode::Interrupt);
        let mode: crate::commands::QueueMode = QueueModeArg::FollowUp.into();
        assert_eq!(mode, crate::commands::QueueMode::FollowUp);
    }

    // The two former tests for `crate::config::Config` were removed when
    // that module was deleted as dead code: the live config flow uses
    // `core_agentic::Config` (multi-provider), and the legacy single-
    // provider shape these tests asserted on no longer exists. Coverage
    // for the active config layer lives in core-agentic's own test suite.
    //
    // Tests for `CommandError` variants live alongside the type in
    // `error.rs`.
}
