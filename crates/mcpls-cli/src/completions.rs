//! Shell completion script generation for the `completions` subcommand.

use std::io::{self, Write};

use clap::{CommandFactory, ValueEnum};
use clap_complete::Generator;

use crate::args::Args;

/// A shell mcpls can emit a completion script for.
///
/// `clap_complete::Shell` carries no nushell variant and cannot be extended
/// from outside that crate, so this is the superset the CLI accepts; each
/// variant forwards to whichever crate owns that shell's generator. The value
/// strings match `clap_complete::Shell`'s, so renaming one would break the
/// invocation people already have in their shell config.
// Each variant's doc comment is the value's `--help` text, so shell names are
// spelled the way their own projects spell them and stay free of the backticks
// rustdoc would want, which would reach the terminal verbatim.
#[allow(clippy::doc_markdown, clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    /// Bourne Again SHell (bash)
    Bash,
    /// Elvish shell
    Elvish,
    /// Friendly Interactive SHell (fish)
    Fish,
    /// Nushell (nu)
    Nushell,
    /// PowerShell
    #[value(name = "powershell")]
    PowerShell,
    /// Z SHell (zsh)
    Zsh,
}

impl Shell {
    fn generator(self) -> &'static dyn Generator {
        match self {
            Self::Bash => &clap_complete::Shell::Bash,
            Self::Elvish => &clap_complete::Shell::Elvish,
            Self::Fish => &clap_complete::Shell::Fish,
            Self::Nushell => &clap_complete_nushell::Nushell,
            Self::PowerShell => &clap_complete::Shell::PowerShell,
            Self::Zsh => &clap_complete::Shell::Zsh,
        }
    }
}

/// Writes `shell`'s completion script for the `mcpls` binary to `out`.
///
/// The script is derived from the live `Args` definition, so it covers exactly
/// the flags this build accepts: without the `transport-http` feature there is
/// no `--listen` or `--http-path` to complete.
///
/// The script is rendered into memory before it reaches `out`, because the
/// generators panic on a failed write rather than reporting it: fish's
/// `try_generate` writes its subcommand helpers through a nested
/// `expect`, so the fallible entry point is only fallible in part. A `Vec`
/// never fails that write, which leaves the single write below as the only
/// one that can, and there a reader that closed the pipe early
/// (`mcpls completions fish | head`) counts as the reader being done rather
/// than as an error.
pub fn emit(shell: Shell, out: &mut impl Write) -> io::Result<()> {
    let mut command = Args::command();
    let bin_name = command.get_name().to_string();
    command.set_bin_name(bin_name);
    command.build();

    let mut script = Vec::new();
    shell.generator().try_generate(&command, &mut script)?;

    match out.write_all(&script).and_then(|()| out.flush()) {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant must produce a non-empty script through its own
    /// generator, so a variant added without a `generator()` arm or wired to
    /// the wrong crate is caught here rather than at the first user's prompt.
    #[test]
    fn test_every_shell_emits_a_script() {
        for shell in Shell::value_variants() {
            let mut script = Vec::new();
            assert!(
                emit(*shell, &mut script).is_ok(),
                "{shell:?} failed to generate"
            );
            assert!(!script.is_empty(), "{shell:?} generated an empty script");
        }
    }

    /// Help text reaches the generated scripts verbatim. Windows PowerShell
    /// 5.1 reads a BOM-less UTF-8 `.ps1` as cp1252, where the trailing byte
    /// of an em dash, ellipsis or arrow decodes to a curly quote, which
    /// PowerShell accepts as a string delimiter: it closes a completion
    /// string early and fails the whole file to parse. Keeping every doc
    /// comment on `Args` ASCII avoids it.
    #[test]
    fn test_scripts_are_ascii_only() {
        for shell in Shell::value_variants() {
            let mut script = Vec::new();
            assert!(emit(*shell, &mut script).is_ok());

            let text = String::from_utf8_lossy(&script);
            let offender = text.lines().find(|line| !line.is_ascii());
            assert!(
                offender.is_none(),
                "{shell:?} script has a non-ASCII help string: {}",
                offender.unwrap_or_default()
            );
        }
    }

    /// The value strings are what users type and what the docs print, and
    /// they have to keep matching `clap_complete::Shell`'s spellings.
    #[test]
    fn test_shell_value_names() {
        let names: Vec<_> = Shell::value_variants()
            .iter()
            .filter_map(|shell| {
                shell
                    .to_possible_value()
                    .map(|value| value.get_name().to_owned())
            })
            .collect();

        assert_eq!(
            names,
            ["bash", "elvish", "fish", "nushell", "powershell", "zsh"]
        );
    }
}
