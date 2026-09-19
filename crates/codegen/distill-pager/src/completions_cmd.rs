//! `grok completions <shell>`: generate shell completion scripts.
//!
//! Used by the installers and npm postinstall; must stay side-effect free (no network, auth, tracing, or tokio).

use clap::CommandFactory as _;
use clap_complete::{Shell, generate};

use crate::app::PagerArgs;

/// Generate and print the completion script for the given shell.
pub fn run(shell: Shell) {
    // Ensure the script always uses the public "grok" name (matches historical behavior and what the installers and docs expect)
    let mut cmd = PagerArgs::command().name("grok");
    if shell != Shell::Zsh {
        generate(shell, &mut cmd, "grok", &mut std::io::stdout());
        return;
    }
    // zsh needs post-processing (see fix_zsh_root_prompt_positional).
    let mut buf = Vec::new();
    generate(shell, &mut cmd, "grok", &mut buf);
    match String::from_utf8(buf) {
        Ok(script) => print!("{}", fix_zsh_root_prompt_positional(&script)),
        // clap_complete output is generated from Rust strings, so this arm is unreachable in practice
        // But the installers run this command, so emit the unmodified script rather than panic
        Err(e) => {
            use std::io::Write as _;
            let _ = std::io::stdout().write_all(e.as_bytes());
        }
    }
}

/// Work around clap_complete's broken zsh output for an optional free-form positional (`[PROMPT]`) preceding the
/// subcommand slot. Upstream bug: <https://github.com/clap-rs/clap/issues/6282>. `grok worktree <TAB>` then
/// re-offers every top-level command. Delete this whole workaround once upstream fixes the generator.
fn fix_zsh_root_prompt_positional(script: &str) -> String {
    let mut out = String::with_capacity(script.len());
    script
        .lines()
        // "prompt" is the clap arg id of the root positional.
        .filter(|line| !line.starts_with("'::prompt -- "))
        .for_each(|line| {
            out.push_str(line);
            out.push('\n');
        });
    for (from, to) in [
        (
            r#"words=($line[2] "${words[@]}")"#,
            r#"words=($line[1] "${words[@]}")"#,
        ),
        (
            r#"curcontext="${curcontext%:*:*}:grok-command-$line[2]:""#,
            r#"curcontext="${curcontext%:*:*}:grok-command-$line[1]:""#,
        ),
        (r#"case $line[2] in"#, r#"case $line[1] in"#),
    ] {
        out = out.replacen(from, to, 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate the zsh completion script exactly like `run` does.
    fn zsh_script() -> String {
        let mut cmd = PagerArgs::command().name("grok");
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut cmd, "grok", &mut buf);
        String::from_utf8(buf).expect("completion script is UTF-8")
    }

    // The optional `[PROMPT]` positional (app/cli.rs) makes clap_complete emit a `::prompt` slot before the subcommand slot
    // Dispatch happens on `$line[2]`, so `grok worktree <TAB>` re-offered every top-level command (upstream clap-rs/clap#6282)
    #[test]
    fn zsh_completions_drop_prompt_slot_and_dispatch_on_line_1() {
        let raw = zsh_script();
        // Preconditions: the workaround is still needed
        // If these start failing, clap_complete fixed the positional handling; delete `fix_zsh_root_prompt_positional` instead of updating the test
        assert!(raw.contains("'::prompt -- "), "raw script has prompt slot");
        assert!(
            raw.contains("case $line[2] in"),
            "raw root dispatch on $line[2]"
        );

        let fixed = fix_zsh_root_prompt_positional(&raw);
        assert!(
            !fixed.contains("::prompt"),
            "prompt positional must not appear in the emitted zsh script"
        );
        assert!(
            !fixed.contains("$line[2]"),
            "root dispatch must be shifted to $line[1]"
        );
        assert!(
            fixed.contains(r#"curcontext="${curcontext%:*:*}:grok-command-$line[1]:""#),
            "root dispatch context must use $line[1]"
        );
        // Subcommand dispatch blocks (already on $line[1]) must survive.
        assert!(
            fixed.contains("grok-worktree-command-$line[1]"),
            "nested subcommand dispatch must be untouched"
        );
        // The subcommand list itself must still be offered at the root.
        assert!(fixed.contains("_grok_commands"), "root command list intact");
    }
}
