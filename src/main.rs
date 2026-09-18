use clap::{ArgAction, Parser, Subcommand as ClapSubcommand};
use colored::Colorize;
use std::{env, fs, path::PathBuf, process::exit};

// Provide the Mull data model and parsing logic.
pub mod node;
pub mod parse;

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

    #[arg(
        long,
        value_name = "PATH",
        help = "Specify the path to the Mull document"
    )]
    path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Subcommand>,
}

// These are the operations Mull can perform.
#[derive(ClapSubcommand)]
enum Subcommand {
    #[command(about = "Check a Mull document")]
    Check,

    #[command(about = "Fix a Mull document (default)")]
    Fix,
}

// Find the nearest Mull document in the current directory or one of its ancestors.
fn find_mull_document() -> Result<PathBuf, String> {
    // Start the search in the current working directory.
    let current_directory = env::current_dir()
        .map_err(|error| format!("Failed to determine the current directory: {error}"))?;

    // Search each directory from nearest to farthest, choosing files deterministically.
    for directory in current_directory.ancestors() {
        let entries = fs::read_dir(directory)
            .map_err(|error| format!("Failed to read {}: {error}", directory.display()))?;
        let mut mull_documents = Vec::<PathBuf>::new();

        // Inspect each directory entry and retain regular Mull documents.
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "Failed to read an entry in {}: {error}",
                    directory.display(),
                )
            })?;
            let path = entry.path();
            let has_mull_extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mull"));
            if has_mull_extension {
                let metadata = fs::metadata(&path)
                    .map_err(|error| format!("Failed to inspect {}: {error}", path.display()))?;
                if metadata.is_file() {
                    mull_documents.push(path);
                }
            }
        }
        mull_documents.sort();

        // Reject multiple documents because there is no unambiguous choice.
        if mull_documents.len() > 1 {
            let file_names = mull_documents
                .iter()
                .filter_map(|path| path.file_name())
                .map(|file_name| file_name.to_string_lossy().into_owned())
                .collect::<Vec<String>>()
                .join(", ");
            return Err(format!(
                "Found multiple Mull documents in {}: {file_names}",
                directory.display(),
            ));
        }

        // Return the Mull document in this directory, if one exists.
        if let Some(mull_document) = mull_documents.into_iter().next() {
            return Ok(mull_document);
        }
    }

    // Report that the search completed without finding a Mull document.
    Err(format!(
        "No Mull document found in {} or its ancestors.",
        current_directory.display(),
    ))
}

// Run the requested operation.
fn entry() -> Result<(), String> {
    // Parse the command-line arguments.
    let cli = Cli::parse();

    // Use the requested Mull document or search for one when no path was supplied.
    let mull_document = cli.path.map_or_else(find_mull_document, Ok)?;

    // Load the Mull document and require its contents to be valid UTF-8.
    let document_bytes = fs::read(&mull_document)
        .map_err(|error| format!("Failed to read {}: {error}", mull_document.display()))?;
    let document = String::from_utf8(document_bytes).map_err(|error| {
        format!(
            "Mull document {} is not valid UTF-8: {error}",
            mull_document.display(),
        )
    })?;

    // Parse the Mull document into nodes.
    let _nodes = parse::parse(&document);

    // Use the fix command when no subcommand is provided and report the selected document.
    match cli.command.unwrap_or(Subcommand::Fix) {
        Subcommand::Check => println!("Checking {}.", mull_document.display()),
        Subcommand::Fix => println!("Fixing {}.", mull_document.display()),
    }

    // Everything succeeded.
    Ok(())
}

// Let the fun begin!
fn main() {
    // Jump to the entrypoint and handle any resulting errors.
    if let Err(e) = entry() {
        eprintln!("{}", e.red());
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

    // Ensure an explicit Mull document path can be parsed.
    #[test]
    fn parse_path() {
        let cli = Cli::try_parse_from(["mull", "--path", "notes.mull", "check"]).unwrap();

        assert_eq!(cli.path, Some(PathBuf::from("notes.mull")));
    }
}
