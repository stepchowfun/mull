# Mull 🍇

[![Build status](https://github.com/stepchowfun/mull/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/stepchowfun/mull/actions?query=branch%3Amain)

*Mull* is a tool for managing a wiki stored in a plain text file. There's a [Visual Studio Code](https://code.visualstudio.com/) / [Cursor](https://cursor.com/) extension which provides syntax highlighting, completions, diagnostics, formatting, refactoring, hover previews, etc.

![Welcome to Mull.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/mull.png)

## An example wiki

A *wiki* is a text file with a `.mull` extension. It contains *nodes* with *links* between them.

Each node starts with a `# Title`. The content comes after the title and is written in Markdown.

To link to a node, put the title in square brackets like `[My favorite art]`. You can also link to local files and directories like `[/mona_lisa.jpg]`. These paths start at the *attachments directory*, which sits beside the wiki and is named after it, with `_attachments` in place of the `.mull` extension. For example, if the wiki is `notes.mull`, then `[/mona_lisa.jpg]` refers to `notes_attachments/mona_lisa.jpg`. A wiki that doesn't link to any files doesn't need an attachments directory.

Here's an example wiki with 3 nodes:

````md
# Home

Welcome to the example wiki!

This is the [Home] node, which is the starting point for every wiki.

[My favorite art] is another node. It's easy to link to other nodes!

# My favorite art

- My favorite poem is [To make a prairie].
- My favorite painting is [/mona_lisa.jpg] by Leonardo da Vinci.

# To make a prairie

```
To make a prairie it takes a clover and one bee,
One clover, and a bee,
And revery.
The revery alone will do,
If bees are few.
```

—Emily Dickinson
````

## What does Mull check?

Mull verifies these structural properties:

- The wiki has valid syntax (links are closed, etc.).
- Nodes have unique titles.
- Links have valid targets. A link can point to a `[node]`, `[/file]`, or `[/directory/]`.

It also verifies these connectivity properties:

- All nodes are reachable by following links from the `Home` node, which must exist.
- All files nested in the attachments directory are linked to from the wiki.

![A broken link.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/broken_link.png)

Mull formats the wiki for you. It chooses the ordering of the nodes in the file, so you don't have to think about that. But when you create a new node, you must link to it from somewhere. If you remove all the links to a node, Mull will flag that the node isn't reachable. This promotes a basic form of organization that makes Mull different from other wiki software.

Just as every node must be linked to from the wiki, the same is true of files nested in the attachments directory. Thus, the wiki serves as an index of those files. You can put files in subdirectories and link to the subdirectories to achieve coverage of the nested files. If the wiki contains `[/]` (a link to the attachments directory itself), all files are covered.

## What does the IDE extension do?

The extension turns Visual Studio Code or Cursor into a powerful wiki editor! It comes with features such as:

- Clickable links with hover previews
- Completions for node titles and file paths
- Diagnostics and quick fixes
- Dimming of everything outside the current node
- Formatting (manual and on save)
- History of visited nodes
- Link occurrence highlighting
- Navigation to the start or end of a node
- Node and file renaming
- Outline view
- Prose-friendly word wrapping
- Reference search (backlinks)
- Scrolling that snaps back to the current node
- Syntax highlighting

![Autocomplete.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/completion.png)

### Keyboard shortcuts

These common keyboard shortcuts are helpful when navigating and editing a wiki:

| Action | macOS | Windows |
| --- | --- | --- |
| Find a node | `Command + Shift + O` | `Control + Shift + O` |
| Rename a node | `F2` | `F2` |
| Jump to a node from a link | `F12` | `F12` |
| Jump to links to a node | `Shift + F12` | `Shift + F12` |
| Jump back | `Control + -` | `Alt + Left` |
| Show completions | `Control + Space` | `Control + Space` |
| Jump to the next error | `F8` | `F8` |
| Jump to the previous error | `Shift + F8` | `Shift + F8` |
| Code actions (e.g., create a missing node) | `Command + .` | `Control + .` |
| Format the wiki | `Shift + Option + F` | `Shift + Alt + F` |
| Jump to the start of the current node | `Command + Up` | `Control + Home` |
| Jump to the end of the current node | `Command + Down` | `Control + End` |
| Go back to the previously visited node | `Command + [` | `Alt + Left` |
| Go forward to the next visited node | `Command + ]` | `Alt + Right` |

## Usage

Once Mull is [installed](#installation-instructions) for Visual Studio Code or Cursor, you can run it by opening a `.mull` file in the editor.

You can also run Mull from the command line as follows:

```sh
mull
```

Here are the supported command-line options:

```
Usage: mull [COMMAND]

Commands:
  check            Check a wiki
  fix              Fix a wiki (default)
  language-server  Start the language server (editors use this)
  help             Print this message or the help of the given subcommand(s)

Options:
  -v, --version  Print version
  -h, --help     Print help
```

## Installation instructions

To use Mull, you must install the binary and optionally the Visual Studio Code / Cursor extension.

### Installing the binary on macOS or Linux (AArch64 or x86-64)

If you're running macOS or Linux (AArch64 or x86-64), you can install Mull with this command:

```sh
curl https://raw.githubusercontent.com/stepchowfun/mull/main/install.sh -LSfs | sh
```

The same command can be used again to update to the latest version.

If Visual Studio Code or Cursor is installed, the installation script also installs the Mull extension in each editor it finds.

The installation script supports the following optional environment variables:

- `VERSION=x.y.z` (defaults to the latest version)
- `PREFIX=/path/to/install` (defaults to `/usr/local/bin`)

For example, the following will install Mull into the working directory:

```sh
curl https://raw.githubusercontent.com/stepchowfun/mull/main/install.sh -LSfs | PREFIX=. sh
```

Note that if you change the installation path, you may also need to change the `mull.executablePath` accordingly in Visual Studio Code or Cursor.

If you prefer not to use this installation method, you can download the binary from the [releases page](https://github.com/stepchowfun/mull/releases), make it executable (e.g., with `chmod`), and place it in some directory in your [`PATH`](https://en.wikipedia.org/wiki/PATH_\(variable\)) (e.g., `/usr/local/bin`).

### Installing the binary on Windows (AArch64 or x86-64)

If you're running Windows (AArch64 or x86-64), download the latest binary from the [releases page](https://github.com/stepchowfun/mull/releases) and rename it to `mull` (or `mull.exe` if you have file extensions visible). Create a directory called `Mull` in your `%PROGRAMFILES%` directory (e.g., `C:\Program Files\Mull`), and place the renamed binary in there. Then, in the "Advanced" tab of the "System Properties" section of Control Panel, click on "Environment Variables..." and add the full path to the new `Mull` directory to the `PATH` variable under "System variables". Note that the `Program Files` directory might have a different name if Windows is configured for a language other than English.

To update an existing installation, simply replace the existing binary.

### Installing the binary with Cargo

If you have [Cargo](https://doc.rust-lang.org/cargo/), you can install Mull as follows:

```sh
cargo install mull
```

You can run that command with `--force` to update an existing installation.

### Installation of the Visual Studio Code / Cursor extension

To install the Visual Studio Code / Cursor extension, download the `mull.vsix` file from the latest [release](https://github.com/stepchowfun/mull/releases) and install it via the "Install from VSIX..." option in the `...` menu at the top of the Extensions view of the Primary Side Bar.

Note that the macOS / Linux installation script will install the extension automatically, so you can skip this section if you use that installation method.

To update an existing installation, simply re-install it.
