#[cfg(test)]
mod unit_tests {
    use crate::cli::{Cli, Command, QueueModeArg, RuntimeModeArg};
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
        let cli = Cli::try_parse_from(["takka", "run", "my task"]).unwrap();
        match cli.command {
            Some(Command::Run { task, .. }) => assert_eq!(task, "my task"),
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_interactive_command_parsing() {
        let cli = Cli::try_parse_from(["takka", "interactive"]).unwrap();
        match cli.command {
            Some(Command::Interactive) => {}
            _ => panic!("Expected Interactive command"),
        }
    }

    #[test]
    fn test_cli_version() {
        let cli = Cli::try_parse_from(["takka", "version"]).unwrap();
        match cli.command {
            Some(Command::Version) => {}
            _ => panic!("Expected Version command"),
        }
    }

    #[test]
    fn test_cli_attach_command_parsing() {
        let cli = Cli::try_parse_from(["takka", "attach"]).unwrap();
        match cli.command {
            Some(Command::Attach { shutdown }) => assert!(!shutdown),
            _ => panic!("Expected Attach command"),
        }
    }

    #[test]
    fn test_cli_attach_shutdown_flag_parsing() {
        let cli = Cli::try_parse_from(["takka", "attach", "--shutdown"]).unwrap();
        match cli.command {
            Some(Command::Attach { shutdown }) => assert!(shutdown),
            _ => panic!("Expected Attach command with --shutdown"),
        }
    }

    #[test]
    fn runtime_mode_flag_accepts_daemon() {
        let cli = Cli::try_parse_from(["takka", "--runtime", "daemon", "run", "task"]).unwrap();
        assert_eq!(cli.runtime, RuntimeModeArg::Daemon);
        // Global flag also works after the subcommand (clap global args).
        let cli = Cli::try_parse_from(["takka", "run", "task", "--runtime", "daemon"]).unwrap();
        assert_eq!(cli.runtime, RuntimeModeArg::Daemon);
    }

    #[test]
    fn queue_mode_flag_defaults_to_follow_up() {
        let cli = Cli::try_parse_from(["takka", "run", "task"]).unwrap();
        assert_eq!(cli.queue_mode, QueueModeArg::FollowUp);
    }

    #[test]
    fn queue_mode_flag_accepts_interrupt_before_subcommand() {
        let cli =
            Cli::try_parse_from(["takka", "--queue-mode", "interrupt", "run", "task"]).unwrap();
        assert_eq!(cli.queue_mode, QueueModeArg::Interrupt);
    }

    #[test]
    fn queue_mode_flag_accepts_interrupt_after_subcommand() {
        let cli =
            Cli::try_parse_from(["takka", "interactive", "--queue-mode", "interrupt"]).unwrap();
        assert_eq!(cli.queue_mode, QueueModeArg::Interrupt);
    }

    #[test]
    fn queue_mode_flag_rejects_unknown_value() {
        assert!(Cli::try_parse_from(["takka", "run", "--queue-mode", "bogus", "task"]).is_err());
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
    // `takka_core::Config` (multi-provider), and the legacy single-
    // provider shape these tests asserted on no longer exists. Coverage
    // for the active config layer lives in takka-core's own test suite.
    //
    // Tests for `CommandError` variants live alongside the type in
    // `error.rs`.
}
