# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
