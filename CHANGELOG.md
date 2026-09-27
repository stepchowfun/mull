# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
