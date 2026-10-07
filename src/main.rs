mod analyzer;
mod assertions;
mod attachments_tree;
mod cancellation;
mod error;
mod format;
mod language_server;
mod line_index;
mod lsp_position;
mod parser;
mod path_util;
mod scoring;
mod spelled_path;
mod validator;
mod wiki;

use crate::{
    analyzer::analyze,
    cancellation::CancellationFlag,
    error::{Error, format_errors},
    format::CodeStr,
    path_util::relative_path,
    wiki::WIKI_EXTENSION,
};
use clap::{ArgAction, Args, Parser, Subcommand as ClapSubcommand};
use similar::TextDiff;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::exit,
    sync::Arc,
};

// This struct represents the command-line arguments.
#[derive(Parser)]
#[command(
    about = concat!(
        env!("CARGO_PKG_DESCRIPTION"),
        "\n\n",
        "More information can be found at: ",
        env!("CARGO_PKG_HOMEPAGE"),
    ),
    version,
    display_name = "Mull",
    disable_version_flag = true
)]
struct Cli {
    #[arg(short, long, help = "Print version", action = ArgAction::Version)]
    _version: Option<bool>,

    #[command(subcommand)]
    command: Option<Subcommand>,
}

// These are the operations the program can perform.
#[derive(ClapSubcommand)]
enum Subcommand {
    #[command(about = "Check a wiki")]
    Check(WikiArgs),

    #[command(about = "Fix a wiki (default)")]
    Fix(WikiArgs),

    #[command(about = "Start the language server (editors use this)")]
    LanguageServer,
}

// These are the arguments for the operations that act on a wiki.
#[derive(Args, Default)]
struct WikiArgs {
    #[arg(long, value_name = "PATH", help = "Specify the path to the wiki")]
    path: Option<PathBuf>,
}

// Let the fun begin!
#[tokio::main]
async fn main() {
    // Jump to the entrypoint and handle any resulting errors.
    if let Err(errors) = entry().await {
        eprintln!("{}", format_errors(&errors));
        exit(1);
    }
}

// Run the requested operation.
async fn entry() -> Result<(), Vec<Error>> {
    // Parse the command-line arguments.
    let cli = Cli::parse();

    // Start the language server without requiring a wiki, or select the requested wiki operation.
    let (should_fix, WikiArgs { path }) =
        match cli.command.unwrap_or(Subcommand::Fix(WikiArgs::default())) {
            Subcommand::Check(wiki_args) => (false, wiki_args),
            Subcommand::Fix(wiki_args) => (true, wiki_args),
            Subcommand::LanguageServer => {
                language_server::run().await;
                return Ok(());
            }
        };

    // Select the wiki and make its path relative when it's contained in the current directory.
    let current_directory = env::current_dir().map_err(|error| {
        vec![Error::new(
            "Unable to determine the current directory.",
            None,
            None,
            Some(Arc::new(error)),
            None,
        )]
    })?;
    let wiki_path = relative_path(
        &current_directory,
        &path
            .map_or_else(|| find_wiki(&current_directory), Ok)
            .map_err(|error| vec![error])?,
    )
    .to_owned();

    // Load the wiki and require its contents to be valid UTF-8.
    let wiki_bytes = fs::read(&wiki_path).map_err(|error| {
        vec![Error::new(
            "Unable to read the wiki.",
            Some(&wiki_path),
            None,
            Some(Arc::new(error)),
            None,
        )]
    })?;
    let wiki_contents = String::from_utf8(wiki_bytes).map_err(|error| {
        vec![Error::new(
            "The wiki isn't valid UTF-8.",
            Some(&wiki_path),
            None,
            Some(Arc::new(error)),
            None,
        )]
    })?;

    // Analyze and render the wiki. The command line has nothing to cancel, so the analysis always
    // runs to completion.
    let rendered_wiki = analyze(
        Some(&wiki_path),
        &wiki_contents,
        &CancellationFlag::default(),
    )
    .assume_completed()?
    .to_string();

    // Accept a canonical wiki, then either fix a noncanonical one or reject it with a diff.
    if wiki_contents == rendered_wiki {
        println!("Wiki {} looks good.", wiki_path.code_str());
    } else if should_fix {
        fs::write(&wiki_path, rendered_wiki).map_err(|error| {
            vec![Error::new(
                "Unable to write the wiki.",
                Some(&wiki_path),
                None,
                Some(Arc::new(error)),
                None,
            )]
        })?;

        // Report that the wiki was fixed.
        println!("Fixed {}.", wiki_path.code_str());
    } else {
        return Err(vec![Error::new(
            &format!(
                "The wiki isn't formatted correctly. {} can fix it.\n\n{}",
                "mull fix".code_str(),
                TextDiff::from_lines(&wiki_contents, &rendered_wiki)
                    .unified_diff()
                    .header("wiki", "rendered"),
            ),
            Some(&wiki_path),
            None,
            None,
            None,
        )]);
    }

    // Everything succeeded.
    Ok(())
}

// Find the nearest wiki in the current directory or one of its ancestors.
fn find_wiki(current_directory: &Path) -> Result<PathBuf, Error> {
    // Search each directory from nearest to farthest, choosing files deterministically.
    for directory in current_directory.ancestors() {
        let entries = fs::read_dir(directory).map_err(|error| {
            Error::new(
                &format!("Unable to read {}.", directory.code_str()),
                None,
                None,
                Some(Arc::new(error)),
                None,
            )
        })?;
        let mut wikis = Vec::<PathBuf>::new();

        // Inspect each directory entry and retain regular wikis.
        for entry in entries {
            let entry = entry.map_err(|error| {
                Error::new(
                    &format!("Unable to read an entry in {}.", directory.code_str()),
                    None,
                    None,
                    Some(Arc::new(error)),
                    None,
                )
            })?;
            let path = entry.path();
            let has_wiki_extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case(WIKI_EXTENSION));
            if has_wiki_extension {
                let metadata = fs::metadata(&path).map_err(|error| {
                    Error::new(
                        &format!("Unable to inspect {}.", path.code_str()),
                        None,
                        None,
                        Some(Arc::new(error)),
                        None,
                    )
                })?;
                if metadata.is_file() {
                    wikis.push(path);
                }
            }
        }
        wikis.sort();

        // Reject multiple wikis because there's no unambiguous choice.
        if wikis.len() > 1 {
            let file_names = wikis
                .iter()
                .map(|path| {
                    Path::new(
                        path.file_name()
                            .expect("A directory entry's path should end with its name."),
                    )
                    .code_str()
                    .to_string()
                })
                .collect::<Vec<String>>()
                .join(", ");
            return Err(Error::new(
                &format!(
                    "Found multiple wikis in {}: {file_names}",
                    directory.code_str(),
                ),
                None,
                None,
                None,
                None,
            ));
        }

        // Return the wiki in this directory, if one exists.
        if let Some(wiki) = wikis.into_iter().next() {
            return Ok(wiki);
        }
    }

    // Report that the search completed without finding a wiki.
    Err(Error::new(
        &format!(
            "No wiki found in {} or its ancestors.",
            current_directory.code_str(),
        ),
        None,
        None,
        None,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::{Cli, Subcommand, WikiArgs};
    use clap::{CommandFactory, Parser};
    use std::path::PathBuf;

    #[test]
    fn verify_cli() {
        Cli::command().debug_assert();
    }

    // Ensure all supported subcommands can be parsed.
    #[test]
    fn parse_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["mull", "check"]).unwrap().command,
            Some(Subcommand::Check(_)),
        ));
        assert!(matches!(
            Cli::try_parse_from(["mull", "fix"]).unwrap().command,
            Some(Subcommand::Fix(_)),
        ));
        assert!(matches!(
            Cli::try_parse_from(["mull", "language-server"])
                .unwrap()
                .command,
            Some(Subcommand::LanguageServer),
        ));
    }

    // Accept an explicit wiki path after the subcommand that acts on the wiki.
    #[test]
    fn parse_path() {
        for subcommand in ["check", "fix"] {
            let Some(Subcommand::Check(WikiArgs { path }) | Subcommand::Fix(WikiArgs { path })) =
                Cli::try_parse_from(["mull", subcommand, "--path", "notes.mull"])
                    .unwrap()
                    .command
            else {
                panic!("The {subcommand} subcommand should be parsed.");
            };
            assert_eq!(path, Some(PathBuf::from("notes.mull")));
        }
    }
}
