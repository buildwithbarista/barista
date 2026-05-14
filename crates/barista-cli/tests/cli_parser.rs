//! Integration tests for the `barista` CLI parser.
//!
//! These tests drive `Cli::try_parse_from` directly so they don't
//! shell out to the binary. Help-text snapshots use `insta`.

use barista_cli::cli::{
    Cli, Command, GrindCommand, MavenCompatFlag, OutputFormat, ScopeArg, TreeFormat,
};
use clap::CommandFactory;
use clap::Parser;

/// Helper: parse argv and panic on error with the rendered clap
/// message. Tests that expect *success* call this.
fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).unwrap_or_else(|e| panic!("parse failed: {e}"))
}

#[test]
fn no_subcommand_is_an_error() {
    // `arg_required_else_help = true` makes a bare `barista`
    // invocation exit with an error (clap renders help to stderr).
    let result = Cli::try_parse_from(["barista"]);
    assert!(result.is_err(), "expected error for bare `barista`");
}

#[test]
fn pull_defaults() {
    let cli = parse(&["barista", "pull"]);
    match cli.command {
        Command::Pull(args) => {
            assert!(!args.update);
            assert!(!args.no_fetch);
            assert!(!args.explain);
            assert_eq!(args.scope, ScopeArg::Compile);
        }
        _ => panic!("expected Pull"),
    }
}

#[test]
fn pull_update_strict_scope_test() {
    let cli = parse(&["barista", "pull", "--update", "--strict", "--scope", "test"]);
    match cli.command {
        Command::Pull(args) => {
            assert!(args.update);
            assert_eq!(args.scope, ScopeArg::Test);
        }
        _ => panic!("expected Pull"),
    }
    assert!(cli.global.strict, "--strict is a global flag");
}

#[test]
fn pull_no_fetch_and_explain() {
    let cli = parse(&["barista", "pull", "--no-fetch", "--explain"]);
    match cli.command {
        Command::Pull(args) => {
            assert!(args.no_fetch);
            assert!(args.explain);
        }
        _ => panic!("expected Pull"),
    }
}

#[test]
fn grind_tree_defaults_to_text() {
    let cli = parse(&["barista", "grind", "tree"]);
    match cli.command {
        Command::Grind { subcommand } => match subcommand {
            GrindCommand::Tree(args) => assert_eq!(args.format, TreeFormat::Text),
            _ => panic!("expected Grind::Tree"),
        },
        _ => panic!("expected Grind"),
    }
}

#[test]
fn grind_tree_format_json() {
    let cli = parse(&["barista", "grind", "tree", "--format", "json"]);
    match cli.command {
        Command::Grind {
            subcommand: GrindCommand::Tree(args),
        } => {
            assert_eq!(args.format, TreeFormat::Json);
        }
        _ => panic!("expected Grind::Tree"),
    }
}

#[test]
fn grind_diff_with_base_ref() {
    let cli = parse(&["barista", "grind", "diff", "main"]);
    match cli.command {
        Command::Grind {
            subcommand: GrindCommand::Diff(args),
        } => {
            assert_eq!(args.base_ref, "main");
        }
        _ => panic!("expected Grind::Diff"),
    }
}

#[test]
fn grind_diff_default_base_ref() {
    let cli = parse(&["barista", "grind", "diff"]);
    match cli.command {
        Command::Grind {
            subcommand: GrindCommand::Diff(args),
        } => {
            assert_eq!(args.base_ref, "HEAD");
        }
        _ => panic!("expected Grind::Diff"),
    }
}

#[test]
fn grind_why_requires_coord() {
    let cli = parse(&["barista", "grind", "why", "org.example:lib"]);
    match cli.command {
        Command::Grind {
            subcommand: GrindCommand::Why(args),
        } => {
            assert_eq!(args.coords, "org.example:lib");
        }
        _ => panic!("expected Grind::Why"),
    }

    // Missing coord is a parse error.
    let err = Cli::try_parse_from(["barista", "grind", "why"]);
    assert!(err.is_err());
}

#[test]
fn pour_target() {
    let cli = parse(&["barista", "pour", "--target", "/tmp/m2"]);
    match cli.command {
        Command::Pour(args) => {
            assert_eq!(
                args.target.as_deref(),
                Some(std::path::Path::new("/tmp/m2"))
            );
        }
        _ => panic!("expected Pour"),
    }
}

#[test]
fn dial_in_non_interactive() {
    let cli = parse(&["barista", "dial-in", "--non-interactive"]);
    match cli.command {
        Command::DialIn(args) => assert!(args.non_interactive),
        _ => panic!("expected DialIn"),
    }
}

#[test]
fn dial_in_output_path_and_force() {
    let cli = parse(&[
        "barista",
        "dial-in",
        "--output-path",
        "/tmp/cfg.toml",
        "--force",
    ]);
    match cli.command {
        Command::DialIn(args) => {
            assert_eq!(
                args.output_path.as_deref(),
                Some(std::path::Path::new("/tmp/cfg.toml"))
            );
            assert!(args.force);
            assert!(!args.non_interactive);
        }
        _ => panic!("expected DialIn"),
    }
}

#[test]
fn shot_passes_through_args() {
    let cli = parse(&["barista", "shot", "mvn", "-v"]);
    match cli.command {
        Command::Shot(args) => {
            assert_eq!(args.args, vec!["mvn".to_string(), "-v".to_string()]);
        }
        _ => panic!("expected Shot"),
    }
}

#[test]
fn wrapper_version() {
    let cli = parse(&["barista", "wrapper", "--version", "0.1.0"]);
    match cli.command {
        Command::Wrapper(args) => {
            assert_eq!(args.version.as_deref(), Some("0.1.0"));
        }
        _ => panic!("expected Wrapper"),
    }
}

#[test]
fn maven_vocab_compile() {
    let cli = parse(&["barista", "compile"]);
    match cli.command {
        Command::Compile(args) => assert!(args.args.is_empty()),
        _ => panic!("expected Compile"),
    }
}

#[test]
fn maven_vocab_install_passes_through_dprops() {
    let cli = parse(&["barista", "install", "-DskipTests", "-Dprop=value"]);
    match cli.command {
        Command::Install(args) => {
            assert_eq!(
                args.args,
                vec!["-DskipTests".to_string(), "-Dprop=value".to_string()]
            );
        }
        _ => panic!("expected Install"),
    }
}

#[test]
fn global_flags_with_pull() {
    let cli = parse(&["barista", "--ci", "--output", "json", "pull"]);
    assert!(cli.global.ci);
    assert_eq!(cli.global.output, OutputFormat::Json);
    assert!(matches!(cli.command, Command::Pull(_)));
}

#[test]
fn maven_compat_three_nine() {
    let cli = parse(&["barista", "--maven-compat", "3.9", "compile"]);
    assert_eq!(cli.global.maven_compat, Some(MavenCompatFlag::ThreeNine));
    assert!(matches!(cli.command, Command::Compile(_)));
}

#[test]
fn root_override() {
    let cli = parse(&["barista", "--root", "/workspaces/foo", "pull"]);
    assert_eq!(
        cli.global.root.as_deref(),
        Some(std::path::Path::new("/workspaces/foo"))
    );
    assert!(matches!(cli.command, Command::Pull(_)));
}

#[test]
fn verbose_stacks() {
    let cli = parse(&["barista", "-v", "-v", "-v", "pull"]);
    assert_eq!(cli.global.verbose, 3);
}

#[test]
fn quiet_flag() {
    let cli = parse(&["barista", "-q", "pull"]);
    assert!(cli.global.quiet);
}

#[test]
fn no_color_and_no_daemon() {
    let cli = parse(&["barista", "--no-color", "--no-daemon", "pull"]);
    assert!(cli.global.no_color);
    assert!(cli.global.no_daemon);
}

#[test]
fn file_override_short_flag() {
    let cli = parse(&["barista", "-f", "/repo/pom.xml", "compile"]);
    assert_eq!(
        cli.global.file.as_deref(),
        Some(std::path::Path::new("/repo/pom.xml"))
    );
}

#[test]
fn top_level_help_lists_every_command() {
    // Render the long help and assert the value-add verbs and
    // every Maven-vocab phase are present.
    let mut cmd = Cli::command();
    let help = cmd.render_long_help().to_string();

    for verb in ["pull", "grind", "pour", "dial-in", "shot", "wrapper"] {
        assert!(help.contains(verb), "help missing verb `{verb}`:\n{help}");
    }

    for phase in [
        "clean", "compile", "test", "package", "verify", "install", "deploy", "site",
    ] {
        assert!(
            help.contains(phase),
            "help missing maven phase `{phase}`:\n{help}",
        );
    }
}

#[test]
fn bad_subcommand_is_a_parse_error() {
    let err = Cli::try_parse_from(["barista", "definitely-not-a-command"]);
    assert!(err.is_err());
}

// ---------- snapshot tests ---------------------------------------
//
// These pin the rendered help text for the four representative
// surfaces called out in the milestone acceptance criteria.

fn render_help(args: &[&str]) -> String {
    // `try_parse_from` returns an `Err(clap::Error)` for `--help`
    // whose Display impl renders the same text the user would see.
    match Cli::try_parse_from(args) {
        Ok(_) => panic!("expected --help to short-circuit parsing"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn snapshot_top_level_help() {
    insta::assert_snapshot!("top_level_help", render_help(&["barista", "--help"]));
}

#[test]
fn snapshot_pull_help() {
    insta::assert_snapshot!("pull_help", render_help(&["barista", "pull", "--help"]));
}

#[test]
fn snapshot_grind_help() {
    insta::assert_snapshot!("grind_help", render_help(&["barista", "grind", "--help"]));
}

#[test]
fn snapshot_grind_tree_help() {
    insta::assert_snapshot!(
        "grind_tree_help",
        render_help(&["barista", "grind", "tree", "--help"]),
    );
}
