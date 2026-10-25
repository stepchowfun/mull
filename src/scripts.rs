use crate::{
    analyzer::analyze,
    cancellation::CancellationFlag,
    error::{Error, SourceRange, format_errors},
    escaping::Context,
    format::CodeStr,
    line_index::LineIndex,
    parser::{self, backtick_run_length, closing_run_end},
    spelled_path::{FileRoot, code_file_path},
    wiki::{
        Link, Page, PageTitle, ScriptBlock, TITLE_PREFIX, Target, Wiki, write_block,
        write_divided_block,
    },
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
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

// This environment variable tells a script whether its block is divided.
const DIVIDED_VARIABLE: &str = "MULL_DIVIDED";

// This is a script block which couldn't be run, identified by its page, its position among the
// page's script blocks, and its script's name, so it can be found again after the wiki changes. The
// summary is a short explanation for an editor to show instead of the error's own, for a script
// which itself failed: what it printed as errors, or a plain statement that it failed if it printed
// none.
#[derive(Clone, Debug)]
pub struct ScriptFailure {
    pub error: Error,
    pub summary: Option<String>,
    pub title: PageTitle,
    pub index: usize,
    pub name: String,
}

// This is why a script block couldn't be run, with any underlying cause and any summary of it, as
// for a `ScriptFailure`.
struct Failure {
    message: String,
    reason: Option<Arc<dyn std::error::Error + Send + Sync>>,
    summary: Option<String>,
}

impl Failure {
    // Explain why a script block couldn't be run, with any underlying cause.
    fn new(message: String, reason: Option<Arc<dyn std::error::Error + Send + Sync>>) -> Self {
        Self {
            message,
            reason,
            summary: None,
        }
    }
}

// Run the script blocks outside portals and other script blocks in a wiki without errors, each
// after the blocks whose outputs it reads and otherwise in source order, after formatting the
// wiki, then format the result. A block whose hash shows that its output is already its script's
// output for what it reads is skipped unless every block is to be rerun, and each block which runs
// is given a new hash. Return the wiki's new contents, along with a failure for each script block
// which couldn't be run, which is left as it was. The failures' errors locate the blocks in the
// wiki as it stood when they ran.
pub fn run(
    wiki_path: &Path,
    source_contents: &str,
    rerun: bool,
) -> Result<(String, Vec<ScriptFailure>), Vec<Error>> {
    let formatted_contents = analyze(
        Some(wiki_path),
        source_contents,
        &CancellationFlag::default(),
    )
    .assume_completed()?
    .to_string();
    run_formatted(wiki_path, &formatted_contents, rerun)
}

// Run the script blocks outside portals and other script blocks in a formatted wiki, as `run` does.
// Blocks run in rounds, each after the blocks whose outputs its links stand for, so it reads their
// new outputs. The blocks in a round run on the wiki as it stands after the rounds before it.
pub fn run_formatted(
    wiki_path: &Path,
    formatted_contents: &str,
    rerun: bool,
) -> Result<(String, Vec<ScriptFailure>), Vec<Error>> {
    // Read the wiki, whose blocks' bodies are already in their canonical form, and find the rounds
    // to run its blocks in.
    let line_index = LineIndex::new(formatted_contents);
    let (wiki, errors) = parser::parse(Some(wiki_path), formatted_contents, &line_index);
    if !errors.is_empty() {
        return Err(errors);
    }
    let file_root = FileRoot::new(wiki_path)
        .map_err(|message| vec![Error::new(&message, None, None, None, None)])?;
    let rounds = rounds(&wiki);

    // Run each block which needs it, noting its replacement or why it failed. The blocks which ran
    // are noted by their pages and their positions among the pages' blocks, which don't change.
    // After each round, replace the blocks which ran, from the end so earlier offsets stay valid,
    // and format the result.
    let mut run_contents = formatted_contents.to_owned();
    let mut ran = Vec::new();
    let mut failures = Vec::new();
    for round in rounds {
        let round_line_index = LineIndex::new(&run_contents);
        let (round_wiki, errors) = parser::parse(Some(wiki_path), &run_contents, &round_line_index);
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut replacements = Vec::new();
        for (title, index) in round {
            let page = &round_wiki.pages[&title];
            let block = &page.script_blocks[index];
            if !rerun
                && block.hash.is_some()
                && block.hash == hash(&run_contents, &round_wiki, page, block, &file_root)
            {
                continue;
            }
            match run_block(&run_contents, &round_wiki, page, block, &file_root) {
                Ok(replacement) => {
                    replacements.push((block.source_range, replacement));
                    ran.push((title, index));
                }
                Err(failure) => failures.push(ScriptFailure {
                    error: Error::new(
                        &failure.message,
                        Some(wiki_path),
                        Some((&run_contents, &round_line_index, block.name_source_range)),
                        failure.reason,
                        None,
                    ),
                    summary: failure.summary,
                    title: page.title.clone(),
                    index,
                    name: block.name.clone(),
                }),
            }
        }
        if replacements.is_empty() {
            continue;
        }
        replacements.sort_by_key(|(source_range, _replacement)| source_range.start);
        let mut round_contents = run_contents.clone();
        for (source_range, replacement) in replacements.into_iter().rev() {
            round_contents.replace_range(source_range.start..source_range.end, &replacement);
        }
        run_contents = format(wiki_path, &round_contents)?;
    }

    // Follow each block which ran with its hash, which is computed once the block is formatted so
    // it still matches when the wiki is next formatted, and format the result again so the copies
    // of the blocks' pages have the hashes too.
    let run_line_index = LineIndex::new(&run_contents);
    let (run_wiki, errors) = parser::parse(Some(wiki_path), &run_contents, &run_line_index);
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut hashes = ran
        .into_iter()
        .filter_map(|(title, index)| {
            let page = run_wiki.pages.get(&title)?;
            let block = &page.script_blocks[index];
            Some((
                block.source_range.end,
                hash(&run_contents, &run_wiki, page, block, &file_root)?,
            ))
        })
        .collect::<Vec<_>>();
    hashes.sort_by_key(|(end, _hash)| *end);
    let mut hashed_contents = run_contents.clone();
    for (end, hash) in hashes.into_iter().rev() {
        hashed_contents.insert_str(end, &format!(" #{hash}"));
    }
    Ok((format(wiki_path, &hashed_contents)?, failures))
}

// Format a wiki without syntax errors.
fn format(wiki_path: &Path, contents: &str) -> Result<String, Vec<Error>> {
    let line_index = LineIndex::new(contents);
    let (wiki, errors) = parser::parse(Some(wiki_path), contents, &line_index);
    if errors.is_empty() {
        Ok(wiki.to_string())
    } else {
        Err(errors)
    }
}

// Find the rounds to run a wiki's script blocks in, each block in a round after those of the
// blocks it depends on, and the blocks in a round in source order. Blocks in a cycle, which
// validation reports, run in a last round.
fn rounds(wiki: &Wiki) -> Vec<Vec<BlockId>> {
    // Find each block's dependencies.
    let mut remaining = dependencies(wiki)
        .into_iter()
        .map(|(block, dependencies)| {
            let dependencies = dependencies
                .into_iter()
                .map(|(dependency, _link_source_range)| dependency)
                .filter(|dependency| *dependency != block)
                .collect::<HashSet<_>>();
            (block, dependencies)
        })
        .collect::<Vec<_>>();

    // Take each round from the blocks whose dependencies have all been taken.
    let mut rounds = Vec::new();
    let mut taken = HashSet::new();
    while !remaining.is_empty() {
        let (ready, waiting) = remaining
            .into_iter()
            .partition::<Vec<_>, _>(|(_block, dependencies)| dependencies.is_subset(&taken));
        let round = if ready.is_empty() {
            remaining = Vec::new();
            waiting
        } else {
            remaining = waiting;
            ready
        };
        let round = round
            .into_iter()
            .map(|(block, _dependencies)| block)
            .collect::<Vec<_>>();
        taken.extend(round.iter().cloned());
        rounds.push(round);
    }
    rounds
}

// Find the links which close cycles of script blocks depending on each other's outputs, with a
// depth-first search over the blocks in source order. A link to what a block reads through a block
// whose search is still in progress closes a cycle, including a link to what contains the block
// itself. Every cycle has such a link, and without them there would be no cycles.
pub fn dependency_cycle_links(wiki: &Wiki) -> Vec<SourceRange> {
    let dependencies = dependencies(wiki).into_iter().collect::<HashMap<_, _>>();
    let mut cycle_links = Vec::new();
    let mut in_progress = HashSet::new();
    let mut done = HashSet::new();
    for (start, _dependencies) in block_ids(wiki) {
        // Search from a block which hasn't been searched, keeping each block on the current path
        // along with its dependencies which remain to be followed, last first.
        if done.contains(&start) {
            continue;
        }
        in_progress.insert(start.clone());
        let mut path = vec![(start.clone(), reversed(&dependencies[&start]))];
        while let Some((block, block_dependencies)) = path.last_mut() {
            // Finish a block once all of its dependencies have been followed.
            let Some((dependency, link_source_range)) = block_dependencies.pop() else {
                in_progress.remove(block);
                done.insert(block.clone());
                path.pop();
                continue;
            };

            // Follow the next dependency, unless it closes a cycle or has been searched.
            if in_progress.contains(&dependency) {
                cycle_links.push(link_source_range);
            } else if !done.contains(&dependency) {
                in_progress.insert(dependency.clone());
                let next = reversed(&dependencies[&dependency]);
                path.push((dependency, next));
            }
        }
    }
    cycle_links.sort_by_key(|source_range| (source_range.start, source_range.end));
    cycle_links.dedup_by_key(|source_range| (source_range.start, source_range.end));
    cycle_links
}

// Copy a list in reverse order.
fn reversed<T: Clone>(items: &[T]) -> Vec<T> {
    items.iter().rev().cloned().collect()
}

// This identifies a script block by the title of its page and its position among the page's
// script blocks, which running scripts doesn't change.
type BlockId = (PageTitle, usize);

// List the script blocks of a wiki in source order, each with its page.
fn block_ids(wiki: &Wiki) -> Vec<(BlockId, &ScriptBlock)> {
    let mut pages = wiki.pages.values().collect::<Vec<_>>();
    pages.sort_by_key(|page| page.source_range.start);
    pages
        .into_iter()
        .flat_map(|page| {
            page.script_blocks
                .iter()
                .enumerate()
                .map(|(index, block)| ((page.title.clone(), index), block))
        })
        .collect()
}

// Find the blocks each script block depends on, in source order: those whose outputs appear in
// the rendered view of what a link in its arguments or input stands for, through the portals in
// it, each along with that link.
fn dependencies(wiki: &Wiki) -> Vec<(BlockId, Vec<(BlockId, SourceRange)>)> {
    block_ids(wiki)
        .into_iter()
        .map(|(block_id, block)| {
            let page = &wiki.pages[&block_id.0];
            let mut block_dependencies = Vec::new();
            for link in value_links(wiki, page, block) {
                let Link::Text {
                    target,
                    source_range,
                    ..
                } = link
                else {
                    continue;
                };
                let mut blocks = Vec::new();
                blocks_in_view(wiki, target, &mut HashSet::new(), &mut blocks);
                block_dependencies.extend(blocks.into_iter().map(|block| (block, *source_range)));
            }
            (block_id, block_dependencies)
        })
        .collect()
}

// Find the script blocks whose outputs appear in the rendered view of a page or a section, which
// are those within it and within what the portals in it show, skipping targets already visited.
fn blocks_in_view(
    wiki: &Wiki,
    target: &Target,
    visited: &mut HashSet<Target>,
    blocks: &mut Vec<BlockId>,
) {
    let Some((page, section)) = wiki.resolve(target) else {
        return;
    };
    if !visited.insert(target.clone()) {
        return;
    }
    let source_range = section.map_or(page.source_range, |section| section.source_range);
    blocks.extend(
        page.script_blocks
            .iter()
            .enumerate()
            .filter(|(_index, block)| {
                (source_range.start..source_range.end).contains(&block.source_range.start)
            })
            .map(|(index, _block)| (page.title.clone(), index)),
    );
    for portal in page.portals_within(section) {
        blocks_in_view(wiki, &portal.target, visited, blocks);
    }
}

// List the links which stand for values in a script block, in source order: those in its
// arguments, and those in its input if it's divided.
fn value_links<'a>(wiki: &Wiki, page: &'a Page, block: &ScriptBlock) -> Vec<&'a Link> {
    let ranges = [
        Some(block.arguments_source_range),
        wiki.script_input_source_range(block),
    ];
    let mut links = page
        .links
        .iter()
        .filter(|link| {
            let source_range = link.source_range();
            ranges
                .iter()
                .flatten()
                .any(|range| range.start <= source_range.start && source_range.end <= range.end)
        })
        .collect::<Vec<_>>();
    links.sort_by_key(|link| link.source_range().start);
    links
}

// Find the value a link in a script block stands for: the rendered view of a page or a section, or
// the full path of a file or a directory. A malformed link, which a wiki without syntax errors
// doesn't have, stands for itself as written.
fn link_value(source_contents: &str, wiki: &Wiki, link: &Link, file_root: &Path) -> String {
    match link {
        Link::Text { target, .. } => wiki.resolve(target).map_or_else(
            || target.link_text(false),
            |(page, section)| wiki.rendered_view(page, section),
        ),
        Link::Filesystem { target, .. } => {
            file_root.join(target.path()).to_string_lossy().into_owned()
        }
        Link::Malformed { source_range, .. } => {
            source_contents[source_range.start..source_range.end].to_owned()
        }
    }
}

// Hash a script block's script, its arguments and its body as written, and the values its links to
// pages and sections stand for, so it needn't run again while they stay the same. A divided
// block's input and output are hashed separately. The hash is the first 48 bits of their SHA-256
// hash, in hexadecimal. A block whose script can't be read has no hash.
fn hash(
    source_contents: &str,
    wiki: &Wiki,
    page: &Page,
    block: &ScriptBlock,
    file_root: &FileRoot,
) -> Option<String> {
    let script = fs::read(script_path(file_root, &block.name).ok()?).ok()?;
    let arguments =
        &source_contents[block.arguments_source_range.start..block.arguments_source_range.end];
    let (input, output) = wiki.script_body(block);
    let values = value_links(wiki, page, block)
        .into_iter()
        .filter(|link| matches!(link, Link::Text { .. }))
        .map(|link| link_value(source_contents, wiki, link, file_root.path()))
        .collect::<Vec<_>>();
    let mut hasher = Sha256::new();
    for part in [script.as_slice(), arguments.as_bytes(), input.as_bytes()]
        .into_iter()
        .chain(output.as_deref().map(str::as_bytes))
        .chain(values.iter().map(String::as_bytes))
    {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut prefix = [0; 8];
    prefix[2..].copy_from_slice(&digest[..6]);
    Some(format!("{:012x}", u64::from_be_bytes(prefix)))
}

// Run a script block on what it reads, and write it anew with the script's output as its body, or
// below its input if it's divided, indented like its opening fence and within fences longer than
// any in it.
fn run_block(
    source_contents: &str,
    wiki: &Wiki,
    page: &Page,
    block: &ScriptBlock,
    file_root: &FileRoot,
) -> Result<String, Failure> {
    // Run the script on the block's body. Paths are made absolute, since the script runs in the
    // file root rather than the current directory.
    let script =
        script_path(file_root, &block.name).map_err(|message| Failure::new(message, None))?;
    let absolute_script = path::absolute(&script).map_err(|error| {
        Failure::new(
            "Its path couldn't be found.".to_owned(),
            Some(Arc::new(error) as _),
        )
    })?;
    let absolute_file_root = path::absolute(file_root.path()).map_err(|error| {
        Failure::new(
            "The file root couldn't be found.".to_owned(),
            Some(Arc::new(error) as _),
        )
    })?;
    let arguments = arguments(source_contents, wiki, page, block, &absolute_file_root);
    let (input, old_output) = wiki.script_body(block);
    let is_divided = old_output.is_some();
    let output = execute(
        &absolute_script,
        &arguments,
        &expanded_input(source_contents, wiki, page, block, &absolute_file_root),
        is_divided,
        &absolute_file_root,
        SCRIPT_TIMEOUT,
    )
    .map_err(|failure| {
        let display = script_display(file_root, &script);
        Failure {
            message: format!("The script {display} failed. {}", failure.message),
            reason: failure.reason,
            summary: failure.summary.map(|errors| {
                if errors.is_empty() {
                    format!("The script {display} failed.")
                } else {
                    errors
                }
            }),
        }
    })?;

    // Require the output to be readable as a page's content within the block, where title lines
    // don't start sections.
    let output_source = format!(
        "{TITLE_PREFIX}{}\n\n{}",
        page.title.text(Context::Verbatim),
        write_block("", &block.name, &output),
    );
    let (output_wiki, errors) =
        parser::parse(None, &output_source, &LineIndex::new(&output_source));
    if !errors.is_empty() || output_wiki.pages.len() != 1 {
        return Err(Failure::new(
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

    // Write the block with the output as its body, or below its input if it's divided.
    let indentation = wiki.indentation(block.source_range.start);
    let argument = block.canonical_argument(source_contents);
    Ok(if is_divided {
        write_divided_block(indentation, &argument, &input, &output)
    } else {
        write_block(indentation, &argument, &output)
    })
}

// Find what a script block's script reads: a divided block's input with each link replaced by its
// value, its escapes resolved, and code taken as written, or an undivided block's body as written,
// either without the indentation of the block's opening fence.
fn expanded_input(
    source_contents: &str,
    wiki: &Wiki,
    page: &Page,
    block: &ScriptBlock,
    file_root: &Path,
) -> String {
    // Find the links in a divided block's input.
    let Some(source_range) = wiki.script_input_source_range(block) else {
        return wiki.script_body(block).0;
    };
    let indentation = wiki.indentation(block.source_range.start);
    let links = page
        .links
        .iter()
        .filter(|link| {
            source_range.start <= link.source_range().start
                && link.source_range().end <= source_range.end
        })
        .map(|link| (link.source_range().start, link))
        .collect::<HashMap<_, _>>();

    // Read the input a piece at a time, removing the indentation at the start of each line, even
    // within code.
    let mut input = String::new();
    let mut position = source_range.start;
    let mut code_end = position;
    let mut is_at_line_start = true;
    while let Some(character) = source_contents[position..source_range.end].chars().next() {
        let rest = &source_contents[position..source_range.end];
        if is_at_line_start {
            is_at_line_start = false;
            position += if rest.starts_with(indentation) {
                indentation.len()
            } else {
                rest.len() - rest.trim_start_matches([' ', '\t']).len()
            };
            continue;
        }
        if position < code_end {
            input.push(character);
            is_at_line_start = character == '\n';
            position += character.len_utf8();
        } else if let Some(link) = links.get(&position) {
            input.push_str(&link_value(source_contents, wiki, link, file_root));
            position = link.source_range().end;
        } else if character == '`' {
            code_end = closing_run_end(
                source_contents,
                position + backtick_run_length(rest),
                backtick_run_length(rest),
            )
            .map_or(source_range.end, |run_end| run_end.min(source_range.end));
        } else if character == '\\' {
            let escaped = rest[1..].chars().next().unwrap_or('\\');
            input.push(escaped);
            position += '\\'.len_utf8() + escaped.len_utf8();
        } else {
            input.push(character);
            is_at_line_start = character == '\n';
            position += character.len_utf8();
        }
    }
    input
}

// Find the values of a script block's arguments. Text outside links and code is split on
// whitespace, with its escapes resolved. A link stands for its value, even if it has whitespace in
// it. Code stands for its text as written. Pieces which aren't separated by whitespace form one
// argument.
fn arguments(
    source_contents: &str,
    wiki: &Wiki,
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
            argument.get_or_insert_default().push_str(&link_value(
                source_contents,
                wiki,
                link,
                file_root,
            ));
            position = link.source_range().end;
            continue;
        }

        // Split on whitespace, take code as written, and resolve escapes.
        let rest = &source_contents[position..source_range.end];
        if character.is_whitespace() {
            arguments.extend(argument.take());
            position += character.len_utf8();
        } else if character == '`' {
            // Code ends at the next run of exactly as many backticks, or else with the arguments.
            let length = backtick_run_length(rest);
            let code_start = position + length;
            let (code_end, code_run_end) = closing_run_end(source_contents, code_start, length)
                .filter(|&run_end| run_end <= source_range.end)
                .map_or((source_range.end, source_range.end), |run_end| {
                    (run_end - length, run_end)
                });
            argument
                .get_or_insert_default()
                .push_str(&source_contents[code_start..code_end]);
            position = code_run_end;
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

// Run a script with arguments in the file root, with an input on its standard input, and return
// what it prints. The `MULL_DIVIDED` environment variable is `1` if the script's block is divided,
// so its output goes below its input, and `0` if the output replaces the input. The operating
// system starts it, so on Windows the program associated with its extension runs it. A script
// which fails or prints anything but UTF-8 is reported with what it printed as errors, and one
// which runs too long is stopped.
fn execute(
    script: &Path,
    arguments: &[String],
    input: &str,
    is_divided: bool,
    file_root: &Path,
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
        .env(DIVIDED_VARIABLE, if is_divided { "1" } else { "0" })
        .current_dir(file_root)
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
            Failure::new(message, Some(Arc::new(error) as _))
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
            Failure::new(
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
            return Err(Failure::new(
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

    // Report a failure, summarized by what the script printed as errors if the script itself
    // failed, or return the output.
    if !status.success() {
        let message = status.code().map_or_else(
            || "It was stopped by a signal.".to_owned(),
            |code| format!("It exited with status {code}."),
        );
        return Err(Failure {
            message: with_errors(message),
            reason: None,
            summary: Some(stderr.clone()),
        });
    }
    String::from_utf8(stdout).map_err(|_| {
        Failure::new(
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

// Replace each hash after a closing fence with `#hash`, so tests needn't spell out hashes. A hash
// has no whitespace in it, unlike a fenced section's title.
#[cfg(all(test, unix))]
pub fn hide_hashes(contents: &str) -> String {
    contents
        .split_inclusive('\n')
        .map(|line| match line.split_once(" #") {
            Some((fence, hash))
                if fence.trim_start().starts_with(':')
                    && fence.trim_start().trim_start_matches(':').is_empty()
                    && !hash.trim_end().contains(char::is_whitespace) =>
            {
                format!(
                    "{fence} #hash{}",
                    if line.ends_with('\n') { "\n" } else { "" },
                )
            }
            _ => line.to_owned(),
        })
        .collect()
}

// Keep tests which write and run scripts from running at the same time. Linux won't run a file
// which is open for writing, as a script being written by one test can be in a process which
// another test forked to run its own script.
#[cfg(all(test, unix))]
pub fn lock_script_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{arguments, expanded_input};
    use crate::{line_index::LineIndex, parser};
    use std::path::Path;
    #[cfg(unix)]
    use {
        super::{ScriptFailure, execute, hide_hashes, lock_script_tests, run},
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

        // Run the wiki's scripts, with the hashes after closing fences hidden.
        fn run(&self) -> Result<(String, Vec<ScriptFailure>), Vec<Error>> {
            run(
                &self.path(),
                &fs::read_to_string(self.path()).unwrap(),
                false,
            )
            .map(|(contents, failures)| (hide_hashes(&contents), failures))
        }

        // Run the wiki's scripts and save the result, returning how many times a script which
        // counts its runs has run.
        fn run_and_save(&self, rerun: bool) -> usize {
            let (contents, failures) = run(
                &self.path(),
                &fs::read_to_string(self.path()).unwrap(),
                rerun,
            )
            .unwrap();
            assert!(failures.is_empty(), "{failures:?}");
            fs::write(self.path(), contents).unwrap();
            fs::read_to_string(self.0.join("wiki_files/runs"))
                .unwrap_or_default()
                .lines()
                .count()
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
    // fences longer than any in it and followed by a hash, filling in a block written on one line.
    // A block within a fenced section is run too, and title lines in the output don't start
    // sections.
    #[cfg(unix)]
    #[test]
    fn runs_replace_bodies() {
        let _lock = lock_script_tests();
        let wiki = TestWiki::new(
            concat!(
                "# Home\n\n[/scripts/] [Other]\n\n",
                "- List:\n  ::: sort\n  - b\n  - a\n  :::\n\n",
                "::: fence :::\n\n",
                ":::: ## Section\n::: title :::\n::::\n\n",
                "# Other\n",
            ),
            &[
                ("sort.sh", "#!/bin/sh\nsort\n"),
                ("fence.sh", "#!/bin/sh\necho '::: [Other] :::'\n"),
                ("title.sh", "#!/bin/sh\necho '#### Deep'\n"),
            ],
        );

        let (contents, failures) = wiki.run().unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            contents,
            concat!(
                "# Home\n\n[/scripts/] [Other]\n\n",
                "- List:\n  ::: sort\n  - a\n  - b\n  ::: #hash\n\n",
                ":::: fence\n::: [Other] :::\n:::: #hash\n\n",
                ":::: ## Section\n::: title\n#### Deep\n::: #hash\n::::\n\n",
                "# Other\n",
            ),
        );
    }

    // Report a script which fails, isn't executable, or prints something which isn't a page's
    // content, at its name, leaving its block as it was. A script which fails is summarized by what
    // it printed as errors, or by a plain statement that it failed if it printed none.
    #[cfg(unix)]
    #[test]
    fn failed_runs() {
        let _lock = lock_script_tests();
        let wiki = TestWiki::new(
            concat!(
                "# Home\n\n[/scripts/]\n\n",
                "::: fail :::\n\n::: bracket :::\n\n::: plain :::\n\n::: quiet :::\n",
            ),
            &[
                ("fail.sh", "#!/bin/sh\necho nope >&2\nexit 3\n"),
                ("quiet.sh", "#!/bin/sh\nexit 1\n"),
                ("bracket.sh", "#!/bin/sh\necho ]\n"),
                ("plain.txt", "Not a script."),
            ],
        );
        let plain_script = wiki.0.join("wiki_files/scripts/plain.txt");
        fs::set_permissions(&plain_script, fs::Permissions::from_mode(0o644)).unwrap();

        let (contents, failures) = wiki.run().unwrap();
        assert_eq!(contents, fs::read_to_string(wiki.path()).unwrap());
        let messages = failures
            .iter()
            .map(|failure| failure.error.to_string())
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 4);
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
        assert!(
            messages[3].contains("The script `/scripts/quiet.sh` failed. It exited with status 1."),
        );
        assert_eq!(
            failures
                .iter()
                .map(|failure| failure.summary.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("nope"),
                None,
                None,
                Some("The script `/scripts/quiet.sh` failed."),
            ],
        );
    }

    // Skip a block whose hash matches its script, its arguments, and its body, unless every block
    // is to be rerun, and run it again once any of them changes.
    #[cfg(unix)]
    #[test]
    fn runs_skip_unchanged_blocks() {
        let _lock = lock_script_tests();
        let wiki = TestWiki::new(
            "# Home\n\n[/runs] [/scripts/]\n\n::: count a\n- b\n:::\n",
            &[("count.sh", "#!/bin/sh\necho run >> runs\ncat\n")],
        );
        fs::write(wiki.0.join("wiki_files/runs"), "").unwrap();

        assert_eq!(wiki.run_and_save(false), 1);
        assert_eq!(wiki.run_and_save(false), 1);
        assert_eq!(wiki.run_and_save(true), 2);
        let source = fs::read_to_string(wiki.path()).unwrap();
        fs::write(wiki.path(), source.replace("- b", "- c")).unwrap();
        assert_eq!(wiki.run_and_save(false), 3);
        let source = fs::read_to_string(wiki.path()).unwrap();
        fs::write(wiki.path(), source.replace("count a", "count b")).unwrap();
        assert_eq!(wiki.run_and_save(false), 4);
        let script = wiki.0.join("wiki_files/scripts/count.sh");
        fs::write(&script, "#!/bin/sh\necho run >> runs\ncat\ntrue\n").unwrap();
        assert_eq!(wiki.run_and_save(false), 5);
        assert_eq!(wiki.run_and_save(false), 5);
    }

    // Run a divided block's script on its input alone, telling it the block is divided, and replace
    // only its output, keeping the input as written. Rerun the block once its output is edited.
    #[cfg(unix)]
    #[test]
    fn runs_replace_outputs() {
        let _lock = lock_script_tests();
        let wiki = TestWiki::new(
            concat!(
                "# Home\n\n[/runs] [/scripts/]\n\n",
                "- List:\n  ::: count\n  - b\n  - a\n  ---\n  stale\n  :::\n\n",
                "::: count :::\n",
            ),
            &[(
                "count.sh",
                "#!/bin/sh\necho run >> runs\necho \"divided: $MULL_DIVIDED\"\nsort\n",
            )],
        );
        fs::write(wiki.0.join("wiki_files/runs"), "").unwrap();

        assert_eq!(
            wiki.run().unwrap().0,
            concat!(
                "# Home\n\n[/runs] [/scripts/]\n\n",
                "- List:\n  ::: count\n  - b\n  - a\n  ---\n  divided: 1\n  - a\n  - b\n",
                "  ::: #hash\n\n",
                "::: count\ndivided: 0\n::: #hash\n",
            ),
        );
        fs::write(wiki.0.join("wiki_files/runs"), "").unwrap();
        assert_eq!(wiki.run_and_save(false), 2);
        assert_eq!(wiki.run_and_save(false), 2);
        let source = fs::read_to_string(wiki.path()).unwrap();
        fs::write(
            wiki.path(),
            source.replacen("  - a\n  - b\n", "  - edited\n", 1),
        )
        .unwrap();
        assert_eq!(wiki.run_and_save(false), 3);
        assert!(
            fs::read_to_string(wiki.path())
                .unwrap()
                .contains("  ---\n  divided: 1\n  - a\n  - b\n"),
        );
    }

    // Run each block after the blocks whose outputs its links stand for, even when it comes first,
    // and run it again once one of those outputs changes.
    #[cfg(unix)]
    #[test]
    fn runs_follow_dependencies() {
        let _lock = lock_script_tests();
        let wiki = TestWiki::new(
            concat!(
                "# Home\n\n[/runs] [/scripts/]\n\n",
                "::: ## Total\n::: add\n[#Base] + 1\n---\n:::\n:::\n\n",
                "::: ## Base\n::: add\n1 + 1\n---\n:::\n:::\n",
            ),
            &[("add.sh", "#!/bin/sh\necho run >> runs\nexpr $(cat)\n")],
        );
        fs::write(wiki.0.join("wiki_files/runs"), "").unwrap();

        assert_eq!(wiki.run_and_save(false), 2);
        assert_eq!(
            hide_hashes(&fs::read_to_string(wiki.path()).unwrap()),
            concat!(
                "# Home\n\n[/runs] [/scripts/]\n\n",
                ":::: ## Total\n::: add\n[#Base] + 1\n---\n3\n::: #hash\n::::\n\n",
                ":::: ## Base\n::: add\n1 + 1\n---\n2\n::: #hash\n::::\n",
            ),
        );
        assert_eq!(wiki.run_and_save(false), 2);
        let source = fs::read_to_string(wiki.path()).unwrap();
        fs::write(wiki.path(), source.replace("1 + 1", "2 + 2")).unwrap();
        assert_eq!(wiki.run_and_save(false), 4);
        assert!(
            fs::read_to_string(wiki.path())
                .unwrap()
                .contains("[#Base] + 1\n---\n5\n"),
        );
    }

    // Take links to pages and sections as their rendered views, each in one argument, and links to
    // files as their paths, code as written, and the rest split on whitespace with its escapes
    // resolved, joining pieces which aren't separated.
    #[test]
    fn script_arguments() {
        let source = concat!(
            "# Home\n::: tidy [Other] [/notes.txt] `a\\d b` x\\]y two  words --page=[Other] ",
            "[Other#Notes] :::\n# Other\nHello there.\n## Notes\nSome notes.",
        );
        let (wiki, errors) = parser::parse(None, source, &LineIndex::new(source));
        assert!(errors.is_empty());
        let page = wiki.page("Home");

        assert_eq!(
            arguments(
                source,
                &wiki,
                page,
                &page.script_blocks[0],
                Path::new("/root"),
            ),
            vec![
                "Hello there.\n\n## Notes\n\nSome notes.",
                &Path::new("/root").join("notes.txt").to_string_lossy(),
                "a\\d b",
                "x]y",
                "two",
                "words",
                "--page=Hello there.\n\n## Notes\n\nSome notes.",
                "Some notes.",
            ],
        );
    }

    // Take a divided block's input with each link replaced by its value, escapes resolved, code
    // taken as written, and the indentation of its opening fence removed from each line, even in
    // code, but take an undivided block's body as written.
    #[test]
    fn script_inputs() {
        let source = concat!(
            "# Home\n- List:\n  ::: tidy\n  sum [#Rent] [/a.txt] \\[x\\]\n",
            "  ```\n  [#Rent]\n  ```\n",
            "  ---\n  :::\n\n::: tidy\n[#Rent]\n:::\n\n::: ## Rent\n::: tidy\n1\n---\n12\n:::\n:::",
        );
        let (wiki, errors) = parser::parse(None, source, &LineIndex::new(source));
        assert!(errors.is_empty());
        let page = wiki.page("Home");

        assert_eq!(
            expanded_input(
                source,
                &wiki,
                page,
                &page.script_blocks[0],
                Path::new("/root"),
            ),
            format!(
                "sum 12 {} [x]\n```\n[#Rent]\n```\n",
                Path::new("/root").join("a.txt").to_string_lossy(),
            ),
        );
        assert_eq!(
            expanded_input(
                source,
                &wiki,
                page,
                &page.script_blocks[1],
                Path::new("/root"),
            ),
            "[#Rent]\n",
        );
    }

    // Stop a script which runs too long.
    #[cfg(unix)]
    #[test]
    fn runs_time_out() {
        let _lock = lock_script_tests();
        let wiki = TestWiki::new("# Home\n", &[("slow.sh", "#!/bin/sh\nsleep 5\n")]);

        let failure = execute(
            &wiki.0.join("wiki_files/scripts/slow.sh"),
            &[],
            "",
            false,
            &wiki.0.join("wiki_files"),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(failure.message, "It ran for longer than 0.1 seconds.");
    }
}
