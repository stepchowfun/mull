mod analyzer;
mod assertions;
mod cancellation;
mod error;
mod escaping;
mod file_tree;
mod format;
mod language_server;
mod line_index;
mod lsp_position;
mod parser;
mod path_util;
mod propagation;
mod scoring;
mod scripts;
mod spelled_path;
mod validator;
mod wiki;

use crate::{
    analyzer::analyze,
    cancellation::CancellationFlag,
    error::{Error, format_errors},
    format::CodeStr,
    line_index::LineIndex,
    path_util::relative_path,
    wiki::{PageTitle, WIKI_EXTENSION},
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
    #[command(about = "Check that a wiki is valid and formatted, with its scripts run")]
    Check(FormatArgs),

    #[command(about = "Format a wiki, running its scripts (default)")]
    Format(FormatArgs),

    #[command(about = "Print the content of a page in a wiki")]
    Page(PageArgs),

    #[command(about = "Start the language server (editors use this)")]
    LanguageServer,
}

// These are the arguments for the operations that act on a wiki. Without a path, the wiki is the
// one a script is running in, if any, or else the nearest one.
#[derive(Args, Default)]
struct WikiArgs {
    #[arg(long, value_name = "PATH", help = "Specify the path to the wiki")]
    path: Option<PathBuf>,
}

// These are the arguments for checking and formatting a wiki, which runs the scripts whose blocks
// have changed unless asked to run none or all of them.
#[derive(Args, Default)]
struct FormatArgs {
    #[arg(long, help = "Don't run the wiki's scripts")]
    no_scripts: bool,

    #[arg(
        long,
        conflicts_with = "no_scripts",
        help = "Run all the wiki's scripts, even those whose blocks haven't changed"
    )]
    rerun_scripts: bool,

    #[command(flatten)]
    wiki_args: WikiArgs,
}

impl FormatArgs {
    // Determine which scripts to run.
    fn scripts(&self) -> Scripts {
        if self.no_scripts {
            Scripts::Skip
        } else if self.rerun_scripts {
            Scripts::Rerun
        } else {
            Scripts::Run
        }
    }
}

// These are the arguments for printing a page.
#[derive(Args)]
struct PageArgs {
    #[arg(help = "The page's title")]
    title: String,

    #[command(flatten)]
    wiki_args: WikiArgs,
}

// These are the operations which act on a wiki once it's loaded. Checking and formatting note
// which of the wiki's scripts to run.
enum Operation {
    Check(Scripts),
    Format(Scripts),
    Page(String),
}

// These are the ways to treat a wiki's scripts: running none, running those whose blocks have
// changed, or running all of them.
#[derive(Clone, Copy)]
enum Scripts {
    Skip,
    Run,
    Rerun,
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
#[allow(
    clippy::too_many_lines,
    reason = "The function dispatches each subcommand once the wiki they share is loaded."
)]
async fn entry() -> Result<(), Vec<Error>> {
    // Parse the command-line arguments.
    let cli = Cli::parse();

    // Start the language server without requiring a wiki, or select the requested wiki operation.
    let (operation, WikiArgs { path }) = match cli
        .command
        .unwrap_or(Subcommand::Format(FormatArgs::default()))
    {
        Subcommand::Check(format_args) => (
            Operation::Check(format_args.scripts()),
            format_args.wiki_args,
        ),
        Subcommand::Format(format_args) => (
            Operation::Format(format_args.scripts()),
            format_args.wiki_args,
        ),
        Subcommand::Page(PageArgs { title, wiki_args }) => (Operation::Page(title), wiki_args),
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
            .or_else(|| env::var_os("MULL_WIKI").map(PathBuf::from))
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

    // Print a page, or note which scripts to run while checking or formatting.
    let (scripts_to_run, should_format) = match operation {
        Operation::Page(title) => return print_page(&wiki_path, &wiki_contents, &title),
        Operation::Check(scripts_to_run) => (scripts_to_run, false),
        Operation::Format(scripts_to_run) => (scripts_to_run, true),
    };

    // Analyze and render the wiki, running its scripts as asked and noting any which failed. The
    // command line has nothing to cancel, so the analysis always runs to completion.
    let (rendered_wiki, mut errors) = match scripts_to_run {
        Scripts::Skip => (
            analyze(
                Some(&wiki_path),
                &wiki_contents,
                &CancellationFlag::default(),
            )
            .assume_completed()?
            .to_string(),
            Vec::new(),
        ),
        Scripts::Run => scripts::run(&wiki_path, &wiki_contents, false)?,
        Scripts::Rerun => scripts::run(&wiki_path, &wiki_contents, true)?,
    };

    // Accept a canonical wiki, then either format a noncanonical one or reject it with a diff,
    // reporting any scripts which failed either way.
    if wiki_contents == rendered_wiki {
        if errors.is_empty() {
            println!("Wiki {} looks good.", wiki_path.code_str());
        }
    } else if should_format {
        fs::write(&wiki_path, rendered_wiki).map_err(|error| {
            vec![Error::new(
                "Unable to write the wiki.",
                Some(&wiki_path),
                None,
                Some(Arc::new(error)),
                None,
            )]
        })?;

        // Report that the wiki was formatted.
        println!("Formatted {}.", wiki_path.code_str());
    } else {
        errors.push(Error::new(
            &format!(
                "The wiki isn't formatted correctly. Run {} to format it.\n\n{}",
                "mull format".code_str(),
                TextDiff::from_lines(&wiki_contents, &rendered_wiki)
                    .unified_diff()
                    .header("wiki", "rendered"),
            ),
            Some(&wiki_path),
            None,
            None,
            None,
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
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

// Print a page's content as formatting would write it, for scripts to read.
fn print_page(wiki_path: &Path, wiki_contents: &str, title: &str) -> Result<(), Vec<Error>> {
    // Read the wiki without validating it, which needs only its syntax.
    let (wiki, errors) = parser::parse(
        Some(wiki_path),
        wiki_contents,
        &LineIndex::new(wiki_contents),
    );
    if !errors.is_empty() {
        return Err(errors);
    }

    // Find the page.
    let page = PageTitle::parse(title)
        .ok()
        .and_then(|title| wiki.pages.get(&title))
        .ok_or_else(|| {
            vec![Error::new(
                &format!("Page {} not found.", title.code_str()),
                Some(wiki_path),
                None,
                None,
                None,
            )]
        })?;
    println!("{}", wiki.page_content(page));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Cli, FormatArgs, PageArgs, Subcommand, WikiArgs};
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
            Cli::try_parse_from(["mull", "format"]).unwrap().command,
            Some(Subcommand::Format(_)),
        ));
        let Some(Subcommand::Page(PageArgs { title, .. })) =
            Cli::try_parse_from(["mull", "page", "Home"])
                .unwrap()
                .command
        else {
            panic!("The page subcommand should be parsed.");
        };
        assert_eq!(title, "Home");
        assert!(matches!(
            Cli::try_parse_from(["mull", "language-server"])
                .unwrap()
                .command,
            Some(Subcommand::LanguageServer),
        ));
    }

    // Accept an explicit wiki path, and a request not to run scripts, after the subcommands that
    // check or format the wiki, but not a request both to run none and to run all of them.
    #[test]
    fn parse_format_args() {
        for subcommand in ["check", "format"] {
            let Some(
                Subcommand::Check(FormatArgs {
                    no_scripts,
                    wiki_args: WikiArgs { path },
                    ..
                })
                | Subcommand::Format(FormatArgs {
                    no_scripts,
                    wiki_args: WikiArgs { path },
                    ..
                }),
            ) = Cli::try_parse_from(["mull", subcommand, "--path", "notes.mull", "--no-scripts"])
                .unwrap()
                .command
            else {
                panic!("The {subcommand} subcommand should be parsed.");
            };
            assert_eq!(path, Some(PathBuf::from("notes.mull")));
            assert!(no_scripts);
            assert!(
                Cli::try_parse_from(["mull", subcommand, "--no-scripts", "--rerun-scripts"])
                    .is_err(),
            );
        }
    }
}
