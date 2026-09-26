//! `mcpls help`, which with `--full` prints every command's long help in one
//! run.

use std::fmt::Write as _;

use clap::CommandFactory as _;

use crate::args::Args;

/// The long help of the command `path` names, or with `full`, of it and
/// every command beneath it.
///
/// # Errors
///
/// Returns the first name in `path` that names no command.
pub fn render(path: &[String], full: bool) -> Result<String, String> {
    let mut root = Args::command();
    root.build();
    let mut command = &mut root;
    for name in path {
        command = command
            .find_subcommand_mut(name)
            .ok_or_else(|| format!("unrecognized command '{name}'"))?;
    }
    if !full {
        return Ok(command.render_long_help().to_string());
    }
    let mut out = String::new();
    write_tree(command, &mut out);
    Ok(out)
}

/// Each command under a `# <path>` heading, leaving out hidden commands and
/// the `help` clap generates under every command with subcommands.
fn write_tree(command: &mut clap::Command, out: &mut String) {
    let path = command
        .get_bin_name()
        .unwrap_or_else(|| command.get_name())
        .to_owned();
    let _ = write!(out, "# {path}\n\n{}\n", command.render_long_help());
    for child in command.get_subcommands_mut() {
        if !child.is_hide_set() && child.get_name() != "help" {
            write_tree(child, out);
        }
    }
}
