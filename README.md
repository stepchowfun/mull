# Mull 🍇

[![Build status](https://github.com/stepchowfun/mull/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/stepchowfun/mull/actions?query=branch%3Amain)

![Welcome to Mull.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/mull.png)

*Mull* is a tool for managing a local personal knowledge wiki as a plain text file. The wiki contains *nodes* with *links* between them. There's a Visual Studio Code / Cursor extension which provides syntax highlighting, formatting, validation, and language server features like navigation, refactoring, hover previews, etc.

## An example wiki

A wiki is just a text file with a `.mull` extension.

Each node starts with a `# Heading`, serving as the title of the node. The content comes after the title and is written in Markdown.

To link to a node, put the title in square brackets like `[My favorite art]`. You can also link to local files and directories like `[/mona_lisa.jpg]`.

````md
# Home

Welcome to the example wiki!

This is the [Home] node, which is the starting point for every wiki.

[My favorite art] is another node. It's easy to link to other nodes!

# My favorite art

- My favorite poem is [First Fig].
- My favorite painting is [/mona_lisa.jpg] by Leonardo da Vinci.

# First Fig

```
My candle burns at both ends;
    It will not last the night;
But ah, my foes, and oh, my friends—
    It gives a lovely light!
```

—Edna St. Vincent Millay
````

## What does Mull check?

![A broken link.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/broken_link.png)

Mull verifies the following:

- The wiki has valid syntax (links are closed, etc.).
- Node titles are unique.
- Links have valid targets. Links can point to other nodes or local files or directories.
- All nodes and files in the directory containing the wiki are reachable from the special `Home` node, which must exist.

Mull also formats the wiki for you. It determines the ordering of the nodes in the file, so you don't have to think about that. But when you create a new node, you need to link to it from somewhere. Dangling nodes are not allowed. This promotes a basic form of organization that makes Mull different from other wiki software. If you delete all the links to a node, Mull will report that the node isn't reachable.

Just as every node must be linked from the wiki, every file in the directory containing the wiki must also be linked from the wiki. Thus, the wiki serves as an index of the local file system. You can put files in subdirectories and link to the subdirectories to achieve coverage of the contained files. If the wiki contains `[/]` (a link to the wiki root directory), then all files are covered.

## What does the IDE extension do?

![Renaming a link.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/refactoring.png)

The extension turns Visual Studio Code or Cursor into a powerful wiki editor! You get all the familiar trappings of a programming language plugin:

- Autocomplete for links
- Clickable links with hover previews
- Document outline
- Formatting (manual and on save)
- Good editor defaults like prose-friendly word wrapping
- Link occurrence highlighting
- Node and file renaming
- Reference search (backlinks)
- Syntax highlighting
- Validation diagnostics and quick fixes

## Usage

Once Mull is [installed](#installation-instructions) for Visual Studio Code or Cursor, you can run it by opening a `.mull` file in the editor.

You can also run Mull from the command line as follows:

```sh
mull
```

Here are the supported command-line options:

```
Usage: mull [OPTIONS] [COMMAND]

Commands:
  check            Check a wiki
  fix              Fix a wiki (default)
  language-server  Start the language server (editors use this)
  help             Print this message or the help of the given subcommand(s)

Options:
  -v, --version      Print version
      --path <PATH>  Specify the path to the wiki
  -h, --help         Print help
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

Note that if you change the installation path via `PREFIX`, you will also need to change the `mull.executablePath` accordingly in Visual Studio Code or Cursor.

For example, the following will install Mull into the working directory:

```sh
curl https://raw.githubusercontent.com/stepchowfun/mull/main/install.sh -LSfs | PREFIX=. sh
```

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
