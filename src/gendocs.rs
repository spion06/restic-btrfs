//! The command reference (`docs/commands/`), generated from the CLI definition so
//! it cannot drift from the real flags. Regenerate with
//! `rbtrfs gendocs docs/commands`; a test fails if the checked-in files are stale.

use std::path::Path;

use anyhow::{Context, Result};
use clap::{Arg, Command, CommandFactory};

use crate::cli::Cli;

/// `(file name, contents)` for every page.
pub fn render_all() -> Vec<(String, String)> {
    let mut root = Cli::command();
    root.build();

    let subs: Vec<&Command> = root
        .get_subcommands()
        .filter(|c| !c.is_hide_set() && c.get_name() != "help")
        .collect();
    let mut pages = vec![("index.md".to_string(), index(&root, &subs))];
    for sub in subs {
        pages.push((format!("rbtrfs_{}.md", sub.get_name()), page(&root, sub)));
    }
    pages
}

pub fn write_all(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, body) in render_all() {
        std::fs::write(dir.join(&name), body).with_context(|| format!("writing {name}"))?;
    }
    Ok(())
}

fn index(root: &Command, subs: &[&Command]) -> String {
    let mut out = String::from("# Command reference\n\n");
    if let Some(about) = root.get_about() {
        out.push_str(&format!("{}\n\n", sentence(&about.to_string())));
    }
    out.push_str("Every command also accepts `-c, --config <CONFIG>`.\n\n");
    for c in subs {
        let about = c
            .get_about()
            .map(|a| sentence(&a.to_string()))
            .unwrap_or_default();
        out.push_str(&format!(
            "- [`rbtrfs {name}`](rbtrfs_{name}.md): {about}\n",
            name = c.get_name()
        ));
    }
    out
}

fn page(root: &Command, sub: &Command) -> String {
    let name = sub.get_name();
    let mut out = format!("# rbtrfs {name}\n\n");

    let text = sub
        .get_long_about()
        .or(sub.get_about())
        .map(|t| t.to_string());
    if let Some(text) = text {
        out.push_str(&format!("{text}\n\n"));
    }

    let mut usage_cmd = sub.clone();
    let usage = usage_cmd.render_usage().to_string();
    let usage = usage.trim().strip_prefix("Usage:").unwrap_or(&usage).trim();
    out.push_str(&format!("## Usage\n\n```\n{usage}\n```\n"));

    let args: Vec<&Arg> = sub
        .get_arguments()
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .collect();
    let (positional, options): (Vec<&Arg>, Vec<&Arg>) =
        args.into_iter().partition(|a| a.is_positional());
    if !positional.is_empty() {
        out.push_str("\n## Arguments\n");
        for a in positional {
            out.push_str(&arg_section(a));
        }
    }
    if !options.is_empty() {
        out.push_str("\n## Options\n");
        for a in options {
            out.push_str(&arg_section(a));
        }
    }
    let _ = root;
    out
}

/// clap drops the final period of one-line help; put it back.
fn sentence(s: &str) -> String {
    let s = s.trim_end();
    if s.ends_with(['.', ':', '!', '?']) {
        s.to_string()
    } else {
        format!("{s}.")
    }
}

fn arg_section(a: &Arg) -> String {
    let value = a
        .get_value_names()
        .and_then(|v| v.first().map(|s| s.to_string()))
        .unwrap_or_else(|| a.get_id().as_str().to_uppercase().replace('-', "_"));
    let takes_value = a.get_action().takes_values();

    let heading = if a.is_positional() {
        format!("`<{value}>`")
    } else {
        let mut names = Vec::new();
        if let Some(s) = a.get_short() {
            names.push(format!("-{s}"));
        }
        if let Some(l) = a.get_long() {
            names.push(format!("--{l}"));
        }
        let names = names.join(", ");
        if takes_value {
            format!("`{names} <{value}>`")
        } else {
            format!("`{names}`")
        }
    };

    let mut out = format!("\n### {heading}\n\n");
    if let Some(help) = a.get_long_help().or(a.get_help()) {
        out.push_str(&format!("{}\n", sentence(&help.to_string())));
    }
    let defaults: Vec<String> = a
        .get_default_values()
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    if takes_value && !defaults.is_empty() {
        out.push_str(&format!("\nDefault: `{}`\n", defaults.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_command_reference_is_current() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/commands");
        let generated = render_all();
        for (name, want) in &generated {
            let have = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
            assert_eq!(
                &have, want,
                "docs/commands/{name} is out of date; run `cargo run -- gendocs docs/commands`"
            );
        }
        let known: Vec<_> = generated.iter().map(|(n, _)| n.as_str()).collect();
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let n = entry.file_name().to_string_lossy().into_owned();
            assert!(
                known.contains(&n.as_str()),
                "stale file docs/commands/{n}; remove it"
            );
        }
    }

    #[test]
    fn pages_cover_every_visible_command_and_flag() {
        let pages = render_all();
        let backup = &pages
            .iter()
            .find(|(n, _)| n == "rbtrfs_backup.md")
            .unwrap()
            .1;
        assert!(backup.contains("--dry-run") && backup.contains("--profile <PROFILE>"));
        assert!(backup.contains("Default: `default`"));
        assert!(
            !pages.iter().any(|(n, _)| n.contains("gendocs")),
            "hidden command stays hidden"
        );
        let restore = &pages
            .iter()
            .find(|(n, _)| n == "rbtrfs_restore.md")
            .unwrap()
            .1;
        assert!(restore.contains("`<SNAPSHOT>`") && restore.contains("--as-subvolume"));
    }
}
