mod assertions;
mod checker;
mod error;
mod format;
mod language_server;
mod parser;
mod path_util;
mod scoring;
mod validator;
mod wiki;

use crate::{
    checker::{analyze, check},
    error::{Error, format_errors},
    format::CodePath,
    path_util::relative_path,
    wiki::WIKI_EXTENSION,
};
use clap::{ArgAction, Parser, Subcommand as ClapSubcommand};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::exit,
    rc::Rc,
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
    disable_version_flag = true
)]
struct Cli {
    #[arg(short, long, help = "Print version", action = ArgAction::Version)]
    _version: Option<bool>,

    #[arg(long, value_name = "PATH", help = "Specify the path to the wiki")]
    path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Subcommand>,
}

// These are the operations the program can perform.
#[derive(ClapSubcommand)]
enum Subcommand {
    #[command(about = "Check a wiki")]
    Check,

    #[command(about = "Fix a wiki (default)")]
    Fix,

    #[command(about = "Start the language server")]
    LanguageServer,
}

// Find the nearest wiki in the current directory or one of its ancestors.
fn find_wiki() -> Result<PathBuf, Error> {
    // Start the search in the current working directory.
    let current_directory = env::current_dir().map_err(|error| {
        Error::new(
            "Unable to determine the current directory.",
            None,
            None,
            Some(Rc::new(error)),
        )
    })?;

    // Search each directory from nearest to farthest, choosing files deterministically.
    for directory in current_directory.ancestors() {
        let entries = fs::read_dir(directory).map_err(|error| {
            Error::new(
                &format!("Unable to read {}.", directory.code_path()),
                None,
                None,
                Some(Rc::new(error)),
            )
        })?;
        let mut wikis = Vec::<PathBuf>::new();

        // Inspect each directory entry and retain regular wikis.
        for entry in entries {
            let entry = entry.map_err(|error| {
                Error::new(
                    &format!("Unable to read an entry in {}.", directory.code_path()),
                    None,
                    None,
                    Some(Rc::new(error)),
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
                        &format!("Unable to inspect {}.", path.code_path()),
                        None,
                        None,
                        Some(Rc::new(error)),
                    )
                })?;
                if metadata.is_file() {
                    wikis.push(path);
                }
            }
        }
        wikis.sort();

        // Reject multiple wikis because there is no unambiguous choice.
        if wikis.len() > 1 {
            let file_names = wikis
                .iter()
                .filter_map(|path| path.file_name())
                .map(|file_name| Path::new(file_name).code_path().to_string())
                .collect::<Vec<String>>()
                .join(", ");
            return Err(Error::new(
                &format!(
                    "Found multiple wikis in {}: {file_names}",
                    directory.code_path(),
                ),
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
            current_directory.code_path(),
        ),
        None,
        None,
        None,
    ))
}

// Run the requested operation.
async fn entry() -> Result<(), Vec<Error>> {
    // Parse the command-line arguments.
    let cli = Cli::parse();

    // Start the language server without requiring a wiki, or select the requested wiki operation.
    let should_fix = match cli.command.unwrap_or(Subcommand::Fix) {
        Subcommand::Check => false,
        Subcommand::Fix => true,
        Subcommand::LanguageServer => {
            language_server::run().await;
            return Ok(());
        }
    };

    // Select the wiki and make its path relative when it is contained in the current directory.
    let wiki_path = relative_path(
        &env::current_dir().map_err(|error| {
            vec![Error::new(
                "Unable to determine the current directory.",
                None,
                None,
                Some(Rc::new(error)),
            )]
        })?,
        &cli.path
            .map_or_else(find_wiki, Ok)
            .map_err(|error| vec![error])?,
    )
    .to_owned();

    // Load the wiki and require its contents to be valid UTF-8.
    let wiki_bytes = fs::read(&wiki_path).map_err(|error| {
        vec![Error::new(
            "Unable to read the wiki.",
            Some(&wiki_path),
            None,
            Some(Rc::new(error)),
        )]
    })?;
    let wiki_contents = String::from_utf8(wiki_bytes).map_err(|error| {
        vec![Error::new(
            "The wiki is not valid UTF-8.",
            Some(&wiki_path),
            None,
            Some(Rc::new(error)),
        )]
    })?;

    // Analyze the wiki and additionally check its formatting when no fix was requested.
    let wiki = if should_fix {
        analyze(Some(&wiki_path), &wiki_contents)
    } else {
        check(Some(&wiki_path), &wiki_contents)
    }?;

    // Render the wiki once for checking or fixing.
    let rendered_wiki = wiki.to_string();

    // Fix the wiki when requested, avoiding a rewrite when it is already canonical.
    if should_fix {
        if wiki_contents == rendered_wiki {
            println!("Wiki {} looks good.", wiki_path.code_path());
        } else {
            fs::write(&wiki_path, rendered_wiki).map_err(|error| {
                vec![Error::new(
                    "Unable to write the wiki.",
                    Some(&wiki_path),
                    None,
                    Some(Rc::new(error)),
                )]
            })?;

            // Report that the wiki was fixed.
            println!("Fixed {}.", wiki_path.code_path());
        }
    } else {
        // Report that the wiki passed the check.
        println!("Wiki {} looks good.", wiki_path.code_path());
    }

    // Everything succeeded.
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{Cli, Subcommand};
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
            Some(Subcommand::Check),
        ));
        assert!(matches!(
            Cli::try_parse_from(["mull", "fix"]).unwrap().command,
            Some(Subcommand::Fix),
        ));
        assert!(matches!(
            Cli::try_parse_from(["mull", "language-server"])
                .unwrap()
                .command,
            Some(Subcommand::LanguageServer),
        ));
    }

    // Ensure an explicit wiki path can be parsed.
    #[test]
    fn parse_path() {
        let cli = Cli::try_parse_from(["mull", "--path", "notes.mull", "check"]).unwrap();

        assert_eq!(cli.path, Some(PathBuf::from("notes.mull")));
    }
}
