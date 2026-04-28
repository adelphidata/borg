//! Shared argument parsing for `borg update` (CLI) and `/update` (TUI slash).
//!
//! Backs both surfaces with the same clap struct so flags stay in sync.

use clap::Parser;

/// Flags accepted by both `borg update` and the TUI `/update` slash command.
#[derive(Parser, Debug, Default, PartialEq, Eq, Clone)]
#[command(name = "update", no_binary_name = true)]
pub struct UpdateArgs {
    /// Install the latest pre-release (experimental) build instead of stable.
    #[arg(long, short = 'p', alias = "dev", visible_alias = "pre")]
    pub prerelease: bool,
    /// Check for updates without installing.
    #[arg(long, short = 'c')]
    pub check: bool,
}

impl UpdateArgs {
    /// Parse the tail of a `/update ...` slash command (everything after the literal `/update`).
    /// An empty `rest` returns defaults.
    pub fn parse_slash(rest: &str) -> Result<Self, clap::Error> {
        // No quoted args expected — flag tokens only.
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        Self::try_parse_from(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(s: &str) -> Result<UpdateArgs, clap::Error> {
        UpdateArgs::parse_slash(s)
    }

    #[test]
    fn parse_slash_table() {
        let cases: &[(&str, bool, bool)] = &[
            ("", false, false),
            ("--prerelease", true, false),
            ("--pre", true, false),
            ("--dev", true, false),
            ("-p", true, false),
            ("--check", false, true),
            ("-c", false, true),
            ("-pc", true, true),
            ("-cp", true, true),
            ("--prerelease --check", true, true),
            ("--check --dev", true, true),
            ("  --pre   --check  ", true, true),
        ];
        for (input, want_pre, want_check) in cases {
            let got = parse(input).unwrap_or_else(|e| panic!("input {input:?} failed: {e}"));
            assert_eq!(
                got,
                UpdateArgs {
                    prerelease: *want_pre,
                    check: *want_check
                },
                "input {input:?}"
            );
        }
    }

    #[test]
    fn parse_slash_rejects_unknown_flag() {
        let err = parse("--bogus").expect_err("--bogus must error");
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "error must name token: {msg}");
    }

    #[test]
    fn parse_slash_rejects_positional() {
        let err = parse("foo").expect_err("positional must error");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("foo")
                || msg.to_lowercase().contains("unexpected")
                || msg.to_lowercase().contains("argument"),
            "error must surface the bad token: {msg}"
        );
    }

    #[test]
    fn parse_slash_rejects_value_on_bool_flag() {
        let err = parse("--prerelease=yes").expect_err("bool flag must reject =value");
        // clap's exact wording varies; assert it's an error of the right shape.
        let kind = err.kind();
        assert!(
            matches!(
                kind,
                clap::error::ErrorKind::UnknownArgument
                    | clap::error::ErrorKind::ValueValidation
                    | clap::error::ErrorKind::TooManyValues
                    | clap::error::ErrorKind::InvalidValue
                    | clap::error::ErrorKind::NoEquals
            ),
            "unexpected error kind: {kind:?}"
        );
    }

    #[test]
    fn default_matches_empty_parse() {
        assert_eq!(parse("").unwrap(), UpdateArgs::default());
    }

    #[test]
    fn help_advertises_standard_flags_and_hides_dev() {
        let help = UpdateArgs::command().render_help().to_string();
        assert!(help.contains("--prerelease"), "help missing --prerelease");
        assert!(help.contains("--pre"), "help missing visible alias --pre");
        assert!(help.contains("-p"), "help missing -p short");
        assert!(help.contains("--check"), "help missing --check");
        assert!(help.contains("-c"), "help missing -c short");
        // --dev is a hidden alias and must not appear in rendered help.
        assert!(
            !help.contains("--dev"),
            "help must not advertise hidden alias --dev: {help}"
        );
    }
}
