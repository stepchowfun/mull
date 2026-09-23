# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
