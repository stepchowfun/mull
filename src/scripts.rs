use crate::{
    analyzer::analyze,
    cancellation::CancellationFlag,
    error::{Error, format_errors},
    escaping::Context,
    format::CodeStr,
    line_index::LineIndex,
    parser::{self, backtick_run_length},
    spelled_path::{FileRoot, code_file_path},
    wiki::{Link, MIN_FENCE_LENGTH, Page, ScriptBlock, TITLE_PREFIX, Wiki},
};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    path::{self, Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

// Scripts live in this directory within the file root.
pub const SCRIPTS_DIRECTORY: &str = "scripts";

// A script which runs longer than this is stopped, and its block is left as it was.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

// Run every script block outside other blocks in a wiki without errors, in source order, after
// formatting the wiki, then format the result. Return the wiki's new contents, along with an error
// for each script block which failed, which is left as it was. The errors locate the blocks in the
// formatted wiki.
pub fn run(wiki_path: &Path, source_contents: &str) -> Result<(String, Vec<Error>), Vec<Error>> {
    let formatted_contents = analyze(
        Some(wiki_path),
        source_contents,
        &CancellationFlag::default(),
    )
    .assume_completed()?
    .to_string();
    run_formatted(wiki_path, &formatted_contents)
}

// Run the script blocks outside other blocks in a formatted wiki, as `run` does.
pub fn run_formatted(
    wiki_path: &Path,
    formatted_contents: &str,
) -> Result<(String, Vec<Error>), Vec<Error>> {
    // Read the wiki, whose blocks' bodies are already in their canonical form.
    let line_index = LineIndex::new(formatted_contents);
    let (wiki, errors) = parser::parse(Some(wiki_path), formatted_contents, &line_index);
    if !errors.is_empty() {
        return Err(errors);
    }
    let file_root = FileRoot::new(wiki_path)
        .map_err(|message| vec![Error::new(&message, None, None, None, None)])?;

    // Run each block, noting its replacement or why it failed.
    let mut pages = wiki.pages.values().collect::<Vec<_>>();
    pages.sort_by_key(|page| page.source_range.start);
    let mut replacements = Vec::new();
    let mut failures = Vec::new();
    for page in pages {
        for block in &page.script_blocks {
            match run_block(
                formatted_contents,
                &wiki,
                page,
                block,
                &file_root,
                wiki_path,
            ) {
                Ok(replacement) => replacements.push((block.source_range, replacement)),
                Err((message, reason)) => failures.push(Error::new(
                    &message,
                    Some(wiki_path),
                    Some((formatted_contents, &line_index, block.name_source_range)),
                    reason,
                    None,
                )),
            }
        }
    }

    // Replace the blocks, from the end so earlier offsets stay valid, and format the result.
    let mut run_contents = formatted_contents.to_owned();
    for (source_range, replacement) in replacements.into_iter().rev() {
        run_contents.replace_range(source_range.start..source_range.end, &replacement);
    }
    let run_line_index = LineIndex::new(&run_contents);
    let (run_wiki, errors) = parser::parse(Some(wiki_path), &run_contents, &run_line_index);
    if errors.is_empty() {
        Ok((run_wiki.to_string(), failures))
    } else {
        Err(errors)
    }
}

// This is why a script block couldn't be run, with any underlying cause.
type Failure = (String, Option<Arc<dyn std::error::Error + Send + Sync>>);

// Run a script block, and write it anew with the script's output as its body, indented like its
// opening fence and within fences longer than any in it.
fn run_block(
    source_contents: &str,
    wiki: &Wiki,
    page: &Page,
    block: &ScriptBlock,
    file_root: &FileRoot,
    wiki_path: &Path,
) -> Result<String, Failure> {
    // Run the script on the block's body. Paths are made absolute, since the script runs in the
    // file root rather than the current directory.
    let script = script_path(file_root, &block.name).map_err(|message| (message, None))?;
    let absolute_script = path::absolute(&script).map_err(|error| {
        (
            "Its path couldn't be found.".to_owned(),
            Some(Arc::new(error) as _),
        )
    })?;
    let absolute_file_root = path::absolute(file_root.path()).map_err(|error| {
        (
            "The file root couldn't be found.".to_owned(),
            Some(Arc::new(error) as _),
        )
    })?;
    let arguments = arguments(source_contents, page, block, &absolute_file_root);
    let input = wiki
        .body(block.source_range)
        .map(|(body, _line_starts)| body)
        .unwrap_or_default();
    let output = execute(
        &absolute_script,
        &arguments,
        &input,
        &absolute_file_root,
        wiki_path,
        &page.title.text(Context::Verbatim),
        SCRIPT_TIMEOUT,
    )
    .map_err(|(message, reason)| {
        (
            format!(
                "The script {} failed. {message}",
                script_display(file_root, &script),
            ),
            reason,
        )
    })?;

    // Require the output to be readable as a page's content.
    let output_source = format!(
        "{TITLE_PREFIX}{}\n\n{output}",
        page.title.text(Context::Verbatim),
    );
    let (output_wiki, errors) =
        parser::parse(None, &output_source, &LineIndex::new(&output_source));
    if !errors.is_empty() || output_wiki.pages.len() != 1 {
        return Err((
            format!(
                "The output of {} can't be read as the content of a page.{}",
                script_display(file_root, &script),
                if errors.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{}", format_errors(&errors))
                },
            ),
            None,
        ));
    }

    // Write the block with the output as its body.
    let indentation = wiki.indentation(block.source_range.start);
    let longest_run = output
        .lines()
        .map(str::trim_start)
        .map(|line| line.len() - line.trim_start_matches(':').len())
        .max()
        .unwrap_or(0);
    let fence = ":".repeat((longest_run + 1).max(MIN_FENCE_LENGTH));
    let mut replacement = format!(
        "{fence} {}\n",
        &source_contents[block.name_source_range.start..block.arguments_source_range.end],
    );
    for line in output.lines() {
        replacement.push_str(indentation);
        replacement.push_str(line);
        replacement.push('\n');
    }
    replacement.push_str(indentation);
    replacement.push_str(&fence);
    Ok(replacement)
}

// Find the script with a name in the file root's scripts directory: the one file there whose name,
// without its extension, is the given name.
pub fn script_path(file_root: &FileRoot, name: &str) -> Result<PathBuf, String> {
    let directory = file_root.path().join(SCRIPTS_DIRECTORY);
    let mut paths = fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.file_stem() == Some(OsStr::new(name)))
        .collect::<Vec<_>>();
    paths.sort();
    match paths.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(format!(
            "No script named {} in {}.",
            name.code_str(),
            code_file_path(Path::new(SCRIPTS_DIRECTORY)),
        )),
        _ => Err(format!(
            "More than one script is named {} in {}: {}.",
            name.code_str(),
            code_file_path(Path::new(SCRIPTS_DIRECTORY)),
            paths
                .iter()
                .map(|path| script_display(file_root, path))
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

// List the names of the scripts in the file root's scripts directory, without duplicates.
pub fn script_names(file_root: &FileRoot) -> Vec<String> {
    let mut names = fs::read_dir(file_root.path().join(SCRIPTS_DIRECTORY))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter_map(|path| Some(path.file_stem()?.to_str()?.to_owned()))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

// Format a script's path as a link to it would be written.
fn script_display(file_root: &FileRoot, script: &Path) -> String {
    code_file_path(script.strip_prefix(file_root.path()).unwrap_or(script)).to_string()
}

// Find the values of a script block's arguments. Text outside links and code is split on
// whitespace, with its escapes resolved. A link to a page stands for the page's title, and a link
// to a file or directory stands for its full path. Code stands for its text as written. Pieces
// which aren't separated by whitespace form one argument.
fn arguments(
    source_contents: &str,
    page: &Page,
    block: &ScriptBlock,
    file_root: &Path,
) -> Vec<String> {
    // Find the links among the arguments.
    let source_range = block.arguments_source_range;
    let links = page
        .links
        .iter()
        .filter(|link| {
            source_range.start <= link.source_range().start
                && link.source_range().end <= source_range.end
        })
        .map(|link| (link.source_range().start, link))
        .collect::<HashMap<_, _>>();

    // Read the arguments a piece at a time.
    let mut arguments = Vec::new();
    let mut argument = None::<String>;
    let mut position = source_range.start;
    while let Some(character) = source_contents[position..source_range.end].chars().next() {
        // Take a link's value.
        if let Some(link) = links.get(&position) {
            let value = match link {
                Link::Text { title, .. } => title.text(Context::Verbatim),
                Link::Filesystem { target, .. } => {
                    file_root.join(target.path()).to_string_lossy().into_owned()
                }
                Link::Malformed { source_range, .. } => {
                    source_contents[source_range.start..source_range.end].to_owned()
                }
            };
            argument.get_or_insert_default().push_str(&value);
            position = link.source_range().end;
            continue;
        }

        // Split on whitespace, take code as written, and resolve escapes.
        let rest = &source_contents[position..source_range.end];
        if character.is_whitespace() {
            arguments.extend(argument.take());
            position += character.len_utf8();
        } else if character == '`' {
            // Code ends at the next run of exactly as many backticks.
            let length = backtick_run_length(rest);
            let code = &rest[length..];
            let mut code_end = code.len();
            let mut closing_length = 0;
            let mut search_start = 0;
            while let Some(index) = code[search_start..].find('`') {
                let run_start = search_start + index;
                let run_length = backtick_run_length(&code[run_start..]);
                if run_length == length {
                    code_end = run_start;
                    closing_length = run_length;
                    break;
                }
                search_start = run_start + run_length;
            }
            argument.get_or_insert_default().push_str(&code[..code_end]);
            position += length + code_end + closing_length;
        } else if character == '\\' {
            let escaped = rest[1..].chars().next().unwrap_or('\\');
            argument.get_or_insert_default().push(escaped);
            position += '\\'.len_utf8() + escaped.len_utf8();
        } else {
            argument.get_or_insert_default().push(character);
            position += character.len_utf8();
        }
    }
    arguments.extend(argument);
    arguments
}

// Run a script with arguments in the file root, with an input on its standard input and the
// wiki's path and the page's title in its environment, and return what it prints. The operating
// system starts it, so on Windows the program associated with its extension runs it. A script
// which fails or prints anything but UTF-8 is reported with what it printed as errors, and one
// which runs too long is stopped.
fn execute(
    script: &Path,
    arguments: &[String],
    input: &str,
    file_root: &Path,
    wiki_path: &Path,
    title: &str,
    timeout: Duration,
) -> Result<String, Failure> {
    // Start the script.
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(script);
        command
    } else {
        Command::new(script)
    };
    let mut child = command
        .args(arguments)
        .current_dir(file_root)
        .env(
            "MULL_WIKI",
            path::absolute(wiki_path).unwrap_or_else(|_| wiki_path.to_owned()),
        )
        .env("MULL_PAGE", title)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            let message = if error.kind() == io::ErrorKind::PermissionDenied {
                "It isn't executable.".to_owned()
            } else {
                "It couldn't be started.".to_owned()
            };
            (message, Some(Arc::new(error) as _))
        })?;

    // Feed the input and collect the output on separate threads, so neither can block the other.
    let mut stdin = child
        .stdin
        .take()
        .expect("The script's input should be piped.");
    let input = input.to_owned();
    let writer = thread::spawn(move || stdin.write_all(input.as_bytes()));
    let stdout_reader = read_in_background(
        child
            .stdout
            .take()
            .expect("The script's output should be piped."),
    );
    let stderr_reader = read_in_background(
        child
            .stderr
            .take()
            .expect("The script's errors should be piped."),
    );

    // Wait for the script, stopping it if it runs too long.
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            (
                "It couldn't be awaited.".to_owned(),
                Some(Arc::new(error) as _),
            )
        })? {
            break status;
        }
        if start.elapsed() > timeout {
            // Don't wait for the output, which a process the script started may hold open.
            let _ = child.kill();
            let _ = child.wait();
            return Err((
                format!("It ran for longer than {} seconds.", timeout.as_secs_f64()),
                None,
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let _ = writer.join();
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
    let with_errors = |message: String| {
        if stderr.is_empty() {
            message
        } else {
            format!("{message}\n\n{stderr}")
        }
    };

    // Report a failure, or return the output.
    if !status.success() {
        let message = status.code().map_or_else(
            || "It was stopped by a signal.".to_owned(),
            |code| format!("It exited with status {code}."),
        );
        return Err((with_errors(message), None));
    }
    String::from_utf8(stdout).map_err(|_| {
        (
            with_errors("Its output isn't valid UTF-8.".to_owned()),
            None,
        )
    })
}

// Read everything from a stream on another thread, so reading it can't block anything else. A
// stream which can't be read yields what was read before the failure.
fn read_in_background<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        let _ = reader.read_to_end(&mut output);
        output
    })
}

#[cfg(test)]
mod tests {
    use super::arguments;
    use crate::{line_index::LineIndex, parser};
    use std::path::Path;
    #[cfg(unix)]
    use {
        super::{execute, run},
        crate::error::Error,
        std::{
            env, fs,
            os::unix::fs::PermissionsExt,
            path::PathBuf,
            process,
            sync::atomic::{AtomicUsize, Ordering},
            time::Duration,
        },
    };

    // Give each test its own directory when tests run concurrently.
    #[cfg(unix)]
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    // This is a wiki in a temporary directory, with its file root beside it, which is removed when
    // the test finishes. Scripts are made executable, which only Unix needs.
    #[cfg(unix)]
    struct TestWiki(PathBuf);

    #[cfg(unix)]
    impl TestWiki {
        // Write a wiki and the scripts in its file root, each made executable.
        fn new(source: &str, scripts: &[(&str, &str)]) -> Self {
            let directory = env::temp_dir().join(format!(
                "mull-scripts-{}-{}",
                process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir_all(directory.join("wiki_files/scripts")).unwrap();
            fs::write(directory.join("wiki.mull"), source).unwrap();
            for (name, contents) in scripts {
                let path = directory.join("wiki_files/scripts").join(name);
                fs::write(&path, contents).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self(directory)
        }

        // Locate the wiki.
        fn path(&self) -> PathBuf {
            self.0.join("wiki.mull")
        }

        // Run the wiki's scripts.
        fn run(&self) -> Result<(String, Vec<Error>), Vec<Error>> {
            run(&self.path(), &fs::read_to_string(self.path()).unwrap())
        }
    }

    // Remove the wiki and its file root.
    #[cfg(unix)]
    impl Drop for TestWiki {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Replace each block's body with its script's output, indented like the block and within
    // fences longer than any in it, filling in a block written on one line.
    #[cfg(unix)]
    #[test]
    fn runs_replace_bodies() {
        let wiki = TestWiki::new(
            concat!(
                "# Home\n\n[/scripts/] [Other]\n\n",
                "- List:\n  ::: sort\n  - b\n  - a\n  :::\n\n",
                "::: fence :::\n\n",
                "# Other\n",
            ),
            &[
                ("sort.sh", "#!/bin/sh\nsort\n"),
                (
                    "fence.sh",
                    "#!/bin/sh\necho \"$MULL_PAGE\"\necho '::: [Other] :::'\n",
                ),
            ],
        );

        let (contents, failures) = wiki.run().unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            contents,
            concat!(
                "# Home\n\n[/scripts/] [Other]\n\n",
                "- List:\n  ::: sort\n  - a\n  - b\n  :::\n\n",
                ":::: fence\nHome\n::: [Other] :::\n::::\n\n",
                "# Other\n",
            ),
        );
    }

    // Report a script which fails, isn't executable, or prints something which isn't a page's
    // content, at its name, leaving its block as it was.
    #[cfg(unix)]
    #[test]
    fn failed_runs() {
        let wiki = TestWiki::new(
            "# Home\n\n[/scripts/]\n\n::: fail :::\n\n::: bracket :::\n\n::: plain :::\n",
            &[
                ("fail.sh", "#!/bin/sh\necho nope >&2\nexit 3\n"),
                ("bracket.sh", "#!/bin/sh\necho ]\n"),
                ("plain.txt", "Not a script."),
            ],
        );
        let plain_script = wiki.0.join("wiki_files/scripts/plain.txt");
        fs::set_permissions(&plain_script, fs::Permissions::from_mode(0o644)).unwrap();

        let (contents, failures) = wiki.run().unwrap();
        assert_eq!(contents, fs::read_to_string(wiki.path()).unwrap());
        let messages = failures.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(messages.len(), 3);
        assert!(
            messages[0]
                .contains("The script `/scripts/fail.sh` failed. It exited with status 3.\n\nnope"),
        );
        assert!(messages[1].contains(
            "The output of `/scripts/bracket.sh` can't be read as the content of a page.",
        ));
        assert!(
            messages[2].contains("The script `/scripts/plain.txt` failed. It isn't executable."),
        );
    }

    // Take links to pages as their titles and links to files as their paths, code as written, and
    // the rest split on whitespace with its escapes resolved, joining pieces which aren't
    // separated.
    #[test]
    fn script_arguments() {
        let source =
            "# Home\n::: tidy [Home] [/notes.txt] `a\\d b` x\\]y two  words --page=[Home] :::";
        let (wiki, errors) = parser::parse(None, source, &LineIndex::new(source));
        assert!(errors.is_empty());
        let page = wiki.page("Home");

        assert_eq!(
            arguments(source, page, &page.script_blocks[0], Path::new("/root")),
            vec![
                "Home",
                &Path::new("/root").join("notes.txt").to_string_lossy(),
                "a\\d b",
                "x]y",
                "two",
                "words",
                "--page=Home",
            ],
        );
    }

    // Stop a script which runs too long.
    #[cfg(unix)]
    #[test]
    fn runs_time_out() {
        let wiki = TestWiki::new("# Home\n", &[("slow.sh", "#!/bin/sh\nsleep 5\n")]);

        let (message, _reason) = execute(
            &wiki.0.join("wiki_files/scripts/slow.sh"),
            &[],
            "",
            &wiki.0.join("wiki_files"),
            &wiki.path(),
            "Home",
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(message, "It ran for longer than 0.1 seconds.");
    }
}
