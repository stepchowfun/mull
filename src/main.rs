mod document;
mod format;
mod parser;
mod path_util;
mod scoring;
mod validator;

use crate::{
    document::DOCUMENT_EXTENSION,
    format::{CodePath, CodeStr},
    path_util::relative_path,
};
use clap::{ArgAction, Parser, Subcommand as ClapSubcommand};
use colored::Colorize;
use similar::TextDiff;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::exit,
};

// Collect logical errors separately so their boundaries are preserved for presentation.
type Errors = Vec<String>;

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

    #[arg(long, value_name = "PATH", help = "Specify the path to the document")]
    path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Subcommand>,
}

// These are the operations the program can perform.
#[derive(ClapSubcommand)]
enum Subcommand {
    #[command(about = "Check a document")]
    Check,

    #[command(about = "Fix a document (default)")]
    Fix,
}

// Find the nearest document in the current directory or one of its ancestors.
fn find_document() -> Result<PathBuf, Errors> {
    // Start the search in the current working directory.
    let current_directory = env::current_dir().map_err(|error| {
        vec![format!(
            "Failed to determine the current directory: {error}",
        )]
    })?;

    // Search each directory from nearest to farthest, choosing files deterministically.
    for directory in current_directory.ancestors() {
        let entries = fs::read_dir(directory)
            .map_err(|error| vec![format!("Failed to read {}: {error}", directory.code_path())])?;
        let mut documents = Vec::<PathBuf>::new();

        // Inspect each directory entry and retain regular documents.
        for entry in entries {
            let entry = entry.map_err(|error| {
                vec![format!(
                    "Failed to read an entry in {}: {error}",
                    directory.code_path(),
                )]
            })?;
            let path = entry.path();
            let has_document_extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case(DOCUMENT_EXTENSION));
            if has_document_extension {
                let metadata = fs::metadata(&path).map_err(|error| {
                    vec![format!("Failed to inspect {}: {error}", path.code_path())]
                })?;
                if metadata.is_file() {
                    documents.push(path);
                }
            }
        }
        documents.sort();

        // Reject multiple documents because there is no unambiguous choice.
        if documents.len() > 1 {
            let file_names = documents
                .iter()
                .filter_map(|path| path.file_name())
                .map(|file_name| Path::new(file_name).code_path().to_string())
                .collect::<Vec<String>>()
                .join(", ");
            return Err(vec![format!(
                "Found multiple documents in {}: {file_names}",
                directory.code_path(),
            )]);
        }

        // Return the document in this directory, if one exists.
        if let Some(document) = documents.into_iter().next() {
            return Ok(document);
        }
    }

    // Report that the search completed without finding a document.
    Err(vec![format!(
        "No document found in {} or its ancestors.",
        current_directory.code_path(),
    )])
}

// Run the requested operation.
fn entry() -> Result<(), Errors> {
    // Parse the command-line arguments.
    let cli = Cli::parse();

    // Use the requested document or search for one when no path was supplied.
    let document_path = cli.path.map_or_else(find_document, Ok)?;

    // Prefer a path relative to the current directory when the document is contained within it.
    let current_directory = env::current_dir().map_err(|error| {
        vec![format!(
            "Failed to determine the current directory: {error}",
        )]
    })?;
    let display_path = relative_path(&current_directory, &document_path).to_owned();

    // Load the document and require its contents to be valid UTF-8.
    let document_bytes = fs::read(&document_path).map_err(|error| {
        vec![format!(
            "Failed to read {}: {error}",
            display_path.code_path(),
        )]
    })?;
    let document_contents = String::from_utf8(document_bytes).map_err(|error| {
        vec![format!(
            "Document {} is not valid UTF-8: {error}",
            display_path.code_path(),
        )]
    })?;

    // Parse and score the document.
    let document = parser::parse(&document_contents).map_err(|errors| {
        errors
            .into_iter()
            .map(|error| format!("Failed to parse {}: {error}", display_path.code_path()))
            .collect::<Errors>()
    })?;

    // Validate the node graph and surrounding filesystem.
    validator::validate(&document, &document_path).map_err(|errors| {
        errors
            .into_iter()
            .map(|error| format!("Failed to validate {}: {error}", display_path.code_path()))
            .collect::<Errors>()
    })?;

    // Render the document once for checking or fixing.
    let rendered_document = document.to_string();

    // Use the fix command when no subcommand is provided and report the selected document.
    match cli.command.unwrap_or(Subcommand::Fix) {
        Subcommand::Check => {
            // Compare the original document with its canonical rendering.
            if document_contents != rendered_document {
                let diff = TextDiff::from_lines(&document_contents, &rendered_document)
                    .unified_diff()
                    .header("document", "rendered")
                    .to_string();
                return Err(vec![format!(
                    "Document {} is not formatted correctly:\n\n{diff}\n{} can fix it.",
                    display_path.code_path(),
                    "mull fix".code_str(),
                )]);
            }

            // Report that the document passed the check.
            println!("Document {} looks good.", display_path.code_path());
        }
        Subcommand::Fix => {
            // Avoid rewriting a document that already has its canonical rendering.
            if document_contents == rendered_document {
                println!("Document {} looks good.", display_path.code_path());
            } else {
                fs::write(&document_path, rendered_document).map_err(|error| {
                    vec![format!(
                        "Failed to write {}: {error}",
                        display_path.code_path(),
                    )]
                })?;

                // Report that the document was fixed.
                println!("Fixed {}.", display_path.code_path());
            }
        }
    }

    // Everything succeeded.
    Ok(())
}

// Print each logical error with one colored prefix, regardless of its number of lines.
fn print_errors(errors: &[String]) {
    for error in errors {
        eprintln!("{} {error}", "[Error]".red().bold());
    }
}

// Let the fun begin!
fn main() {
    // Jump to the entrypoint and handle any resulting errors.
    if let Err(errors) = entry() {
        print_errors(&errors);
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
    }

    // Ensure an explicit document path can be parsed.
    #[test]
    fn parse_path() {
        let cli = Cli::try_parse_from(["mull", "--path", "notes.mull", "check"]).unwrap();

        assert_eq!(cli.path, Some(PathBuf::from("notes.mull")));
    }
}
