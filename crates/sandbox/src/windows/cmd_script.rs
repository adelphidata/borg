//! The one command line `cmd.exe /c SCRIPT` needs.
//!
//! The run_shell tool hands the Windows sandbox `cmd.exe /C <script>`, where
//! the script is shell syntax the user or the model wrote. Serialising that
//! argv with the C-runtime quoting rules (`"` becomes `\"`, backslashes
//! before quotes double) produces a string the C runtime would parse back
//! correctly — but `cmd.exe` is not a C-runtime program. It reads its command
//! tail with its own rules, in which `\"` is a backslash followed by a quote,
//! so `echo "hello world"` reaches the interpreter as `echo \"hello world\"`
//! and anything with quotes, pipes or redirections inside them breaks.
//!
//! The interpreter's own escape hatch is `/s`: with `/s /c "SCRIPT"` cmd strips
//! exactly the outer pair of quotes and runs what is between them verbatim.

/// The command line for a `cmd.exe /c SCRIPT` invocation, or `None` when
/// `program`/`args` are not that shape and the caller's generic quoting
/// applies.
///
/// Recognises `cmd.exe` and `cmd` by basename (any path, any case) with a
/// `/c` or `/C` switch and exactly one script argument. A program path
/// containing a quote cannot be quoted for cmd at all, and a NUL cannot be
/// in a Win32 command line, so those also fall back and let the generic path
/// report them.
pub fn cmd_script_command_line(program: &str, args: &[String]) -> Option<String> {
    let [switch, script] = args else {
        return None;
    };
    let name = program.rsplit(['/', '\\']).next()?;
    let is_cmd = name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd");
    if !is_cmd
        || !switch.eq_ignore_ascii_case("/c")
        || program.contains(['"', '\0'])
        || script.contains('\0')
    {
        return None;
    }
    Some(format!("\"{program}\" /s /c \"{script}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Spec: the script travels verbatim between one outer pair of quotes,
    /// under `/s`, so quotes the script itself contains are not escaped
    /// into a form cmd would run differently.
    #[test]
    fn script_quotes_are_preserved_verbatim() {
        let script = r#"echo "hello world" && "C:\Program Files\Git\bin\git.exe" status"#;
        let line = cmd_script_command_line("cmd.exe", &args(&["/C", script])).unwrap();
        assert_eq!(line, format!(r#""cmd.exe" /s /c "{script}""#));
        assert!(
            !line.contains(r#"\""#),
            "CRT escaping must not leak into a cmd script: {line}"
        );
    }

    /// Spec: any spelling of the interpreter the sandbox may receive — a full
    /// path, mixed case, no extension — gets the cmd-specific line, and the
    /// switch is matched case-insensitively like cmd itself does.
    #[test]
    fn recognises_cmd_by_basename_and_switch_case_insensitively() {
        for program in [
            r"C:\Windows\System32\cmd.exe",
            "CMD.EXE",
            "cmd",
            "C:/W/cmd.exe",
        ] {
            for switch in ["/c", "/C"] {
                let line = cmd_script_command_line(program, &args(&[switch, "dir"]))
                    .unwrap_or_else(|| panic!("{program} {switch} must be recognised"));
                assert_eq!(line, format!(r#""{program}" /s /c "dir""#));
            }
        }
    }

    /// Spec: anything that is not exactly `cmd /c SCRIPT` keeps the generic
    /// C-runtime quoting — a different program, another switch, a different
    /// argument count — and so does a shape cmd cannot be quoted for.
    #[test]
    fn other_shapes_fall_back_to_generic_quoting() {
        let cases: Vec<(&str, Vec<String>)> = vec![
            ("powershell.exe", args(&["-Command", "dir"])),
            ("cmd.exe", args(&["/k", "dir"])),
            ("cmd.exe", args(&["/c"])),
            ("cmd.exe", args(&["/c", "dir", "extra"])),
            ("cmd.exe", args(&[])),
            ("cmdlet.exe", args(&["/c", "dir"])),
            (r#"C:\odd"name\cmd.exe"#, args(&["/c", "dir"])),
            ("cmd.exe", args(&["/c", "dir\0evil"])),
        ];
        for (program, args) in cases {
            assert!(
                cmd_script_command_line(program, &args).is_none(),
                "{program} {args:?} must not be treated as a cmd script"
            );
        }
    }
}
