# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.36.24] - 2026-10-05

### Added
- The VS Code extension now keeps a history of the nodes visited in each wiki. A code lens above the current node's title links back to the previous node, like a browser's back button, and hovering it shows the whole trail. The trail always starts with `Home`, so visiting `Home` starts it over. Commands go back (`Command + [` on macOS, `Alt + Left` elsewhere) and forward (`Command + ]`, `Alt + Right`).

## [0.36.23] - 2026-10-05

### Changed
- The VS Code extension's snap-back scrolling is simpler and more reliable. It no longer eases the view back whenever too much of a neighboring node is in view. Instead, once the node containing the cursor is scrolled entirely out of view, the extension scrolls just far enough to bring the node's nearer edge back. It now also works in editors that don't have focus.

## [0.36.22] - 2026-10-05

### Fixed
- The VS Code extension no longer scrolls away blank lines at the end of the node containing the cursor. Before, enough of them would be treated as the next node and eased back out of view, so they couldn't be reached by scrolling.

## [0.36.21] - 2026-10-05

### Added
- The VS Code extension now works in Restricted Mode, for workspaces that aren't trusted. There, it ignores the workspace's `mull.executablePath` setting, which could otherwise make it run any program.

## [0.36.20] - 2026-10-05

### Added
- After scrolling past the node containing the cursor, the VS Code extension now eases the view back onto it once the scrolling stops, like a rubber band. It also pulls a node into view when the cursor moves to it, as when clicking a dimmed node. The `mull.snapBackToCurrentNode` setting turns this off.

### Changed
- The VS Code extension now scrolls Mull files smoothly by default.

## [0.36.19] - 2026-10-04

### Changed
- The error for a node that can't be reached from `Home` now reads "Node `Title` can't be reached by following links from `Home`." rather than "There's no way to get to `Title` starting from `Home`."

## [0.36.18] - 2026-10-04

### Fixed
- Renaming a node, and the other features that act on the node under the cursor, now work with the cursor at the end of the node's title line, including after trailing whitespace.

## [0.36.17] - 2026-10-04

### Added
- The VS Code extension now dims everything outside the node containing the cursor, so the node being edited stands out. The `mull.dimOtherNodes` setting turns this off.

### Removed
- The VS Code extension no longer folds everything outside the node being edited, which dimming replaces, so the `mull.foldOtherNodes` setting is gone.

## [0.36.16] - 2026-10-04

### Fixed
- Renaming a node, and the other features that act on the node under the cursor, now work with the cursor at the start of the node's title line, before the `#`, where navigating to a node leaves it.

## [0.36.15] - 2026-10-04

### Changed
- Following a link in a hover preview, or choosing a node in Go to Symbol, the outline, or the breadcrumbs, now puts the cursor at the start of the node instead of selecting its title, as following a link in the editor does.

## [0.36.14] - 2026-10-04

### Changed
- The code action that creates a missing node now places the cursor at the start of the new node, right before its `#`, rather than at the end of its title.

## [0.36.13] - 2026-10-04

### Changed
- The VS Code extension now colors filesystem links like links to nodes.
- Following a link to a node now puts the cursor at the start of the node instead of selecting its title.

### Fixed
- Hovering over a link with the go-to-definition modifier held, such as `Command` on macOS, no longer shows the raw source of the destination node below its preview.

## [0.36.12] - 2026-10-04

### Changed
- `mull --version` now prints the program's name capitalized, as in `Mull 0.36.12`.

### Fixed
- In the VS Code extension, following a link to a later node in the wiki once again folds away everything before that node. This broke in 0.36.11.

## [0.36.11] - 2026-10-04

### Added
- The VS Code extension now has a `mull.foldOtherNodes` setting, which turns off folding everything outside the node containing the cursor.
- The VS Code extension now warns when the Mull executable's version differs from its own, as when only one of them was upgraded. The extension's version now matches Mull's.

## [0.36.10] - 2026-10-04

### Added
- The VS Code extension now folds away everything outside the node containing the cursor, so the node being edited appears to be the whole document. The folds follow the cursor to whichever node it moves to, as when following a link.
- The VS Code extension now has commands to jump to the start or end of the node containing the cursor, bound to `Command + Up` and `Command + Down` on macOS and `Control + Home` and `Control + End` elsewhere, the usual shortcuts for the top and bottom of a document. Holding `Shift` selects text along the way.

### Changed
- The VS Code extension now hides line numbers in wikis by default, since they count from the top of the wiki's file rather than the node being edited. It also hides fold controls, since folding follows the node being edited, and the unused glyph margin.

## [0.36.9] - 2026-10-04

### Changed
- The code action that creates a missing node now inserts it where formatting the wiki would put it, rather than after the node with the broken link, at the start of the wiki for the home node, or at the end. Formatting the wiki afterward no longer moves the new node.

## [0.36.8] - 2026-10-04

### Changed
- The language server now ignores any text an editor includes when it reports a save, which the server doesn't ask for. The edits the editor reports before the save already contain the saved text.

## [0.36.7] - 2026-10-03

### Fixed
- Made completing links faster while typing in large wikis. Completion no longer waits for each new version of the wiki to be parsed. It finds the link on the cursor's line, and until the new version is parsed, it offers the titles of the most recently parsed version. In a 10 MB wiki, the wait for completions after each keystroke went from about 80 milliseconds to about 30. Completion is no longer offered with the cursor right before a link's opening delimiter.

## [0.36.6] - 2026-10-03

### Fixed
- Made completing links much faster in large wikis. The language server now offers only the titles which contain the typed characters in order, at most 100 of them, rather than sending every title to the editor to filter. When more titles match, those which start with the typed text are offered first, and the editor asks again as more is typed. In a 10 MB wiki, completion went from about 70 milliseconds to a few milliseconds.

## [0.36.5] - 2026-10-03

### Fixed
- Made completing a link which hasn't been closed yet faster in large wikis. The language server now finds the link on the cursor's line rather than parsing a copy of the wiki with the link closed. In a 10 MB wiki, completion went from about 150 milliseconds to 70.

## [0.36.4] - 2026-10-03

### Fixed
- Kept the editor responsive while it parses a large wiki. The language server now parses on a separate thread, so a parse no longer holds up other messages from the editor. When five edits to a 10 MB wiki were each followed by a hover preview, all of the previews were ready after 0.2 seconds instead of 0.36 seconds.

## [0.36.3] - 2026-10-02

### Fixed
- Made reporting many errors much faster for large wikis. Each error's source listing no longer scans the wiki from the start to find the error's line, and the command line no longer rebuilds its output for each error it prints. For a 10 MB wiki where no node is reachable from Home, the editor's diagnostics went from about 12 seconds to 0.2 seconds, and `mull check` went from about 13 seconds to under 0.1 seconds.

## [0.36.2] - 2026-10-02

### Fixed
- Made language server features faster for large wikis by parsing each version of a wiki at most once and sharing the result across features and diagnostics, rather than parsing it again for every request. For a 10 MB wiki, navigation, hover previews, references, link highlighting, and clickable links went from about 55 milliseconds to a few milliseconds per request.

## [0.36.1] - 2026-10-02

### Fixed
- Made language server features much faster for large wikis. Converting between source offsets and editor positions no longer scans the wiki from the start each time, so features like the outline, which convert many positions, no longer take time proportional to the square of the wiki's size. For a 10 MB wiki, the outline went from about a minute to under 200 milliseconds.

## [0.36.0] - 2026-09-30

### Changed
- Ordered formatted nodes by a depth-first traversal from Home instead of by their minimum text-link distance from Home, so each node appears right after its parent. Nodes are placed under the first of their closest-to-Home parents, and the targets of each node's links are visited in title order.

## [0.35.3] - 2026-09-30

### Changed
- Showed a ruler at column 80 in `.mull` files in the VS Code extension.

## [0.35.2] - 2026-09-29

### Changed
- Moved the cursor to the end of the new node's title after the "Create node" quick fix in the VS Code extension.

## [0.35.1] - 2026-09-29

### Changed
- Allowed renaming in a wiki with syntax errors. A link that's malformed by a syntax error isn't updated, but it's reported as a broken link once the syntax error is fixed.

## [0.35.0] - 2026-09-29

### Changed
- Kept navigation, hover previews, references, link highlighting, the document outline, clickable filesystem links, completion, and quick fixes working in a wiki with syntax errors. A hover preview of a node with a syntax error shows only its title.
- Explained that renaming requires fixing the wiki's syntax errors first, rather than silently doing nothing.

## [0.34.0] - 2026-09-28

### Changed
- Reported validation errors, like broken links and unreachable nodes, even when the wiki has syntax errors. A node with a syntax error keeps the links in it which are well-formed, so the nodes and files it links to aren't reported as unlinked. The links in a duplicate node, under an invalid title, or before the first title are still ignored until the syntax error is fixed. A wiki with syntax errors is still never formatted.
- Reported syntax errors in the content under an invalid title or before the first title.

## [0.33.2] - 2026-09-27

### Changed
- Reported every broken filesystem link. The limit of 50 errors now applies only to files that aren't linked to, which a large directory could otherwise flood the output with.

## [0.33.1] - 2026-09-27

### Changed
- Updated the project description to "A tool for managing a wiki stored in a plain text file."

## [0.33.0] - 2026-09-27

### Changed
- Removed Mull's escapes from content before it reaches Markdown in hover previews, so any Markdown can be written. For example, `\[text\](url)` is now a Markdown link rather than literal text. Write `\\\[` for a bracket that Markdown should also treat as literal.
- Let a backslash escape a following `#` in content, like `\# Heading` for a Markdown heading, which a line starting with `#` can't express because it starts a node. In an existing wiki, this changes the meaning of `\#` inside a link.
- Highlighted escapes outside links in the VS Code extension.

### Fixed
- Kept the text before a link in a hover preview from changing the link. A `!` right before a link made it an image, and a literal backslash right before a link hid it.

## [0.32.0] - 2026-09-27

### Changed
- Let a backslash escape a following backslash in a node's content, as in Markdown, in addition to `[` and `]`. So `\\[Home]` is a literal backslash followed by a link, and `[Five\\]` links to a node titled `Five\`, which previously couldn't be linked to. Any other backslash is still a literal character. In an existing wiki, this changes the meaning of `\\` before a square bracket and of `\\` inside a link.
- Escaped backslashes in the titles and paths that completions and renames insert.

### Fixed
- Showed a backslash before an escaped square bracket correctly in hover previews, rather than as a raw `&#91;`. Content outside links now reaches the preview unchanged, since Mull's escapes are also Markdown escapes.

## [0.31.3] - 2026-09-27

### Fixed
- Treated a position past the end of a line in a language server request as the end of that line, as the Language Server Protocol specifies. Such requests previously got no response.

## [0.31.2] - 2026-09-27

### Changed
- Inserted the node that a quick fix creates for a missing link destination right after the first node linking to it, rather than at the end of the wiki. Formatting still moves it to its usual place.

### Fixed
- Attached the quick fix that creates a missing `Home` node only to the diagnostic reporting it. It was also offered as the preferred fix for every other diagnostic without a source location, such as a file that isn't linked to, since all of them are reported at the start of the document. Quick fixes now come from the diagnostics they fix, so one fix covers every diagnostic it resolves, such as several links to the same missing node.

## [0.31.1] - 2026-09-27

### Fixed
- Kept a directory that a rename moves a file into through a symlink. Previously, renaming `d/x.txt` to `lnk/y.txt`, where `lnk` is a symlink to `d`, deleted `d` along with the file it had just moved into it.
- Refused to move a directory into itself through a symlink, which rewrote its links before failing to move it.

## [0.31.0] - 2026-09-27

### Changed
- Stopped guessing which name on disk a misspelled filesystem link refers to. A misspelling is now reported as ``` `x` doesn't match the spelling of any name on disk.```, and the link no longer covers the file it was meant to, which is reported as unlinked until the link is fixed. The guess could name the wrong file, such as another hard link to the same file.
- Reported a wiki whose own path is spelled differently than on disk, as when running `mull check --path WIKI.mull` for `wiki.mull` on a filesystem that ignores case. Previously, the wiki's spelling on disk was guessed.

## [0.30.0] - 2026-09-27

### Changed
- Moved the `--path` option from the top-level command to the `check` and `fix` subcommands, so it's written after them, as in `mull check --path notes/wiki.mull`. Placing it before the subcommand, as in `mull --path notes/wiki.mull check`, is no longer accepted.

## [0.29.7] - 2026-09-27

### Changed
- Reworded error messages so that similar problems are described the same way, and so that a broken rule is stated in general terms. For example, an unlinked file is reported as ``File `x` isn't linked to.``, a duplicate heading as ``Node `X` already exists.``, and an empty heading as "A node title can't be empty.", just as a rename to such a title is refused.

## [0.29.6] - 2026-09-27

### Changed
- Represented text as it appears in a node's content, where link delimiters are escaped, and Markdown for hover previews with distinct types, so neither can be mixed up with plain text such as a node title. There's no change in behavior.

## [0.29.5] - 2026-09-27

### Changed
- Treated `[/.]`, which names the wiki directory, as a directory link, formatted as `[/]`. Previously, it was a file link, which the checker reported since the wiki directory isn't a file.
- Parsed each filesystem link's target into a canonical form that determines how the link is written, rather than normalizing and rendering link paths separately. There's no other change in behavior.

## [0.29.4] - 2026-09-27

### Changed
- Reported a filesystem link that goes through a directory that can't be listed, since the spelling of its path can't be checked. Previously, the path was accepted as written. Such a link can't start a rename or be followed, and a rename into such a directory is refused.

## [0.29.3] - 2026-09-27

### Changed
- Represented filesystem paths spelled as on disk and link paths as written with distinct types, so the two can no longer be compared by accident. This caused the case-sensitivity bugs fixed in the previous few releases. There's no change in behavior.

## [0.29.2] - 2026-09-26

### Fixed
- Recognized the wiki itself when its path is spelled differently than on disk, as when checking `WIKI.mull` for `wiki.mull` on a filesystem that ignores case. The checker reported the wiki as unreferenced, completion offered it, and a link to it could rename it.

## [0.29.1] - 2026-09-26

### Changed
- Deleted a directory that a rename leaves empty even if a directory link names it, since such a link only stands for the files within the directory.

### Fixed
- Rejected a rename whose new path goes through an existing directory spelled differently than on disk. On filesystems that ignore case, such a rename moved the file and then deleted the directory it had moved into, along with the file.
- Rejected a rename starting from a link spelled differently than on disk, which updated only the links spelled like that one and broke any spelled correctly.
- Stopped making links spelled differently than on disk clickable, just as for links whose targets are missing.
- Rejected a rename that only changes the case of a name. VS Code skipped renaming the file but still updated its links, leaving them misspelled.

## [0.29.0] - 2026-09-26

### Changed
- Required a directory link to point to a directory containing at least one file that isn't ignored, however deeply nested, since directories only matter for the files in them.
- Rejected links to ignored files and directories, including anything within an ignored directory or a `.git` or `.hg` directory.
- Stopped offering directories without such files as completions.

### Fixed
- Reported the correct spelling of a misspelled symlink when other symlinks point to the same target, instead of suggesting one of the others and reporting the symlink as unreferenced.

## [0.28.0] - 2026-09-25

### Changed
- Required filesystem links to spell each path exactly as it is on disk. On filesystems that ignore case or Unicode normalization, a link with a different spelling was accepted, but its target was then reported as unreferenced, and the link would break on other filesystems. Such links are now reported with the spelling on disk.
- Reported a directory link to a file as needing no trailing `/`, and a link to something that's neither a file nor a directory without suggesting a change to its trailing `/`.

## [0.27.1] - 2026-09-25

### Changed
- Started the rename of a linked file or directory from its path without the leading `/` or a directory's trailing `/`, since the rename adds them itself.

## [0.27.0] - 2026-09-25

### Added
- Allowed renaming a linked directory to a path within itself, such as moving `images` to `images/raw`, which pushes its contents down a level and updates every link to it or to anything within it.

## [0.26.1] - 2026-09-24

### Changed
- Highlighted text links and filesystem links in the VS Code extension with the scopes that Markdown uses for link text and link destinations, so they're distinct and match each theme's Markdown links.

## [0.26.0] - 2026-09-24

### Changed
- Replaced the `./` prefix of filesystem links with `/`. Paths remain relative to the wiki's directory, so `[/notes.txt]` is a file, `[/images/]` is a directory, and `[/]` is the wiki directory. Node titles may no longer start with `/`.
- Accepted new names with or without a leading `/` when renaming a linked file or directory.

## [0.25.0] - 2026-09-24

### Added
- Made filesystem links in language-server hover previews clickable: a file link opens the file, and a directory link reveals the directory in the explorer.

## [0.24.0] - 2026-09-24

### Changed
- Replaced the `file:` and `dir:` link prefixes with `./`. A link whose target starts with `./` is a filesystem link, and a trailing `/` marks a directory, so `[./notes.txt]` is a file, `[./images/]` is a directory, and `[./]` is the wiki directory. Node titles may no longer start with `./`.
- Formatted filesystem links with their `./` and, for a directory, a trailing `/`.
- Reported a filesystem link whose trailing `/` doesn't match the kind of its target.
- Offered both files and directories when completing any filesystem link.

## [0.23.0] - 2026-09-24

### Changed
- Stopped preserving a trailing `/` when formatting or renaming filesystem link paths.
- Omitted the underlying reason from errors about missing link targets.

## [0.22.0] - 2026-09-24

### Added
- Made filesystem links clickable in the editor through the language server: a file link opens the file, and a directory link reveals the directory in the explorer.

### Changed
- Removed whitespace at the end of each line when formatting.

## [0.21.0] - 2026-09-24

### Changed
- Formatted filesystem link paths in their normalized form, keeping a leading `./` or a trailing `/`, so formatting and renaming write links the same way.
- Ignored whitespace between the `file:` or `dir:` prefix of a link and its path, so the path no longer starts with that whitespace, and formatting removes it.

## [0.20.0] - 2026-09-24

### Added
- Added language-server renaming of linked files and directories, which renames the file or directory on disk, updates every link to it or to anything within it, creates any missing directories, and deletes directories the rename leaves empty.

## [0.19.1] - 2026-09-23

### Changed
- Reported absolute filesystem link paths and paths containing `..` with separate, more specific error messages.

## [0.19.0] - 2026-09-23

### Added
- Added language-server completion of paths in file and directory links, suggesting the entries of the typed directory one level at a time.

## [0.18.1] - 2026-09-23

### Fixed
- Rendered node titles literally in language-server hover previews instead of interpreting them as Markdown.

## [0.18.0] - 2026-09-23

### Added
- Rechecked open wikis through the language server when files in the workspace change, so filesystem-link errors stay current without an edit.

## [0.17.0] - 2026-09-23

### Added
- Added language-server quick fixes that create a missing `Home` node or the missing destination node of a text link.

## [0.16.0] - 2026-09-23

### Added
- Made language-server go to definition on a node's title return the node itself, so editors can fall back to finding its references.

### Changed
- Rejected node titles that start with `file:` or `dir:`, since text links can't target them.
- Made language-server completions replace the whole text link, including its delimiters.
- Excluded trailing whitespace from the node ranges used by language-server outlines and go to definition.

## [0.15.2] - 2026-09-23

### Changed
- Formatted wikis through the language server whenever they parse, even if they fail validation.
- Stopped superseded language-server checks partway through instead of letting them finish.
- Mentioned in the `language-server` command's help text that editors use it.

## [0.15.1] - 2026-09-23

### Added
- Added the VS Code extension package to GitHub releases.

## [0.15.0] - 2026-09-22

### Added
- Added language-server document highlights for text nodes and filesystem links.

## [0.14.2] - 2026-09-22

### Changed
- Consolidated the language server's declaration and text-link resolution into a single lookup.
- Renamed the `mull.openNode` editor command to `mull.revealRange`.

## [0.14.1] - 2026-09-22

### Changed
- Completed text links with their closing delimiter so the cursor lands after the link.

## [0.14.0] - 2026-09-22

### Added
- Added language-server document symbols for editor outlines and document navigation.

## [0.13.0] - 2026-09-22

### Added
- Made resolved text links in language-server hover previews clickable.

### Changed
- Rendered filesystem links in language-server hover previews as inline code.

## [0.12.0] - 2026-09-22

### Added
- Added language-server completion for text links.
- Added language-server support for renaming text nodes and their links.

### Changed
- Rendered language-server node previews as Markdown and made them available from node titles.

## [0.11.1] - 2026-09-22

### Changed
- Used one optional source path consistently for wiki diagnostics and filesystem validation.

### Fixed
- Enabled language-server formatting for untitled wikis.

## [0.11.0] - 2026-09-22

### Added
- Added language-server diagnostics and text-node navigation for untitled wikis.
- Logged language-server initialization and shutdown.

### Changed
- Reported filesystem links in untitled wikis as errors until the wiki is saved.
- Continued validating open wikis when their backing files disappear.

## [0.10.0] - 2026-09-21

### Added
- Added language-server support for jumping to nodes, previewing nodes on hover, and finding references.

## [0.9.1] - 2026-09-21

### Changed
- Clarified user-facing error messages.

### Fixed
- Stopped treating content after an empty title as content before the first title.
- Stopped reporting formatting differences as language-server diagnostics.

## [0.9.0] - 2026-09-21

### Added
- Formatted valid wikis on save through the language server and VS Code extension.

### Changed
- Simplified source-located diagnostics by removing redundant node context.

## [0.8.0] - 2026-09-21

### Added
- Reported parsing, validation, and formatting errors as live language-server diagnostics.

## [0.7.1] - 2026-09-20

### Changed
- Reported a bare title marker as an empty title while continuing to treat other hash-prefixed lines as content.

## [0.7.0] - 2026-09-20

### Changed
- Added source-aware diagnostics that quote and underline the relevant portions of the wiki.

## [0.6.2] - 2026-09-20

### Changed
- Reported distinct errors for empty filesystem link paths and paths that escape the wiki tree.
- Documented unreachable filesystem-link branches and clarified filesystem validation comments.

## [0.6.1] - 2026-09-20

### Changed
- Limited filesystem validation to 50 errors to avoid excessive diagnostics and unnecessary work.

## [0.6.0] - 2026-09-19

### Changed
- Inferred directory references from their contents while continuing to support explicit recursive directory links.
- Followed symbolic links while preserving distinct logical paths and validating filesystem-link path syntax.

## [0.5.2] - 2026-09-19

### Changed
- Preserved logical error boundaries so each independent error receives its own command-line prefix.
- Avoided redundant errors for the contents of unreferenced directories.

## [0.5.1] - 2026-09-19

### Changed
- Formatted code-like values distinctly in command output and diagnostics.
- Separated wiki parsing, scoring, and validation into dedicated modules.

## [0.5.0] - 2026-09-18

### Changed
- Ordered formatted nodes by their minimum text-link distance from Home, using titles to break ties.
- Reported independent parsing and validation errors together.

## [0.4.0] - 2026-09-18

### Added
- Added file and directory links, filesystem coverage validation, and node reachability validation.

## [0.3.0] - 2026-09-18

### Changed
- Normalized whitespace within links and rejected links containing line breaks.

## [0.2.1] - 2026-09-18

### Changed
- Improved command feedback and avoided rewriting wikis that already look good.

## [0.2.0] - 2026-09-18

### Added
- Added link parsing and validation.

## [0.1.0] - 2026-09-18

### Added
- Added wiki discovery, parsing, format checking, and automatic fixing.

## [0.0.0] - 2026-09-17

### Added
- Initial release.
