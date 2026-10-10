# Mull 📖

[![Build status](https://github.com/stepchowfun/mull/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/stepchowfun/mull/actions?query=branch%3Amain)

*Mull* is a tool for managing a wiki stored in a plain text file. It has a [Visual Studio Code](https://code.visualstudio.com/) / [Cursor](https://cursor.com/) extension which provides syntax highlighting, hover previews, completions, diagnostics, formatting, refactoring, etc.

![Welcome to Mull.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/mull.png)

## An example wiki

A Mull *wiki* is a text file with a `.mull` extension. It contains *pages* which link to each other. Here's a small wiki with 3 pages:

````md
# Home

Welcome to the example wiki!

This is the [Home] page, which is the starting point for every wiki.

[My favorite art] is another page. It's easy to link to other pages!

# My favorite art

- My favorite poem is [To make a prairie].
- My favorite painting is [/mona_lisa.jpg] by Leonardo da Vinci.

# To make a prairie

> To make a prairie it takes a clover and one bee,
> One clover, and a bee,
> And revery.
> The revery alone will do,
> If bees are few.

—Emily Dickinson
````

Each page starts with a `# Title`, followed by the contents of the page on subsequent lines, up until the next page or the end of the file.

To link to a page, put the title in square brackets like `[My favorite art]`. When you type the opening `[`, autocomplete will kick in to help you find the target page.

If the wiki is called `my_wiki.mull`, you can put files (images, PDFs, spreadsheets, etc.) in a directory named `my_wiki_files` (in general, `<wiki>_files`) and link to them like `[/mona_lisa.jpg]`.

## Keyboard shortcuts

These common keyboard shortcuts are helpful when navigating and editing a wiki:

| Action | macOS | Windows |
| --- | --- | --- |
| Jump to a page from a link | `F12` or `Command + click` | `F12` or `Control + click` |
| Jump to links to a page (i.e., backlinks) | `Shift + F12` | `Shift + F12` |
| Jump back to the previously visited page | `Command + [` | `Alt + Left` |
| Jump forward to the next visited page | `Command + ]` | `Alt + Right` |
| Jump to the start of the current page | `Command + Up` | `Control + Home` |
| Jump to the end of the current page | `Command + Down` | `Control + End` |
| Jump to the next error | `F8` | `F8` |
| Jump to the previous error | `Shift + F8` | `Shift + F8` |
| Find a page | `Command + Shift + O` | `Control + Shift + O` |
| Code actions (e.g., create a missing page) | `Command + .` | `Control + .` |
| Show completions | `Control + Space` | `Control + Space` |
| Rename a page | `F2` | `F2` |
| Format the wiki | `Shift + Option + F` | `Shift + Alt + F` |

## What does Mull check?

Mull verifies these structural properties:

- The wiki has valid syntax (links are closed, etc.).
- Pages have unique titles.
- Links have valid targets. A link can point to a `[page]`, `[/file]`, or `[/directory/]`.

It also verifies these connectivity properties:

- All pages are reachable by following links from the `Home` page, which must exist.
- All files in the `<wiki>_files` directory (if it exists) are linked to from the wiki.

![A broken link.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/broken_link.png)

Mull formats the wiki for you. It chooses the ordering of the pages in the file, so you don't have to think about that. But when you create a new page, you must link to it from somewhere. If you remove all the links to a page, Mull will flag that the page isn't reachable. This promotes a basic form of organization that makes Mull different from other wiki software.

Just as every page must be linked to from the wiki, the same is true of files in the `<wiki>_files` directory (if it exists). The wiki serves as an index of those files. You can also link to subdirectories to achieve coverage of any nested files. If the wiki contains `[/]` (a link to the `<wiki>_files` directory itself), all files are covered.

## Usage

Once Mull is [installed](#installation-instructions) for Visual Studio Code or Cursor, you can run it by opening a `.mull` file in the editor.

You can also run Mull from the command line (e.g., in a CI job) as follows:

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

### Installation on macOS or Linux (AArch64 or x86-64)

If you're running macOS or Linux (AArch64 or x86-64), you can install Mull with this command:

```sh
curl https://raw.githubusercontent.com/stepchowfun/mull/main/install.sh -LSfs | sh
```

The same command can be used again to update to the latest version.

If Visual Studio Code or Cursor is installed, the installation script also installs the Mull extension in each editor it finds.

The installation script supports the following optional environment variables:

- `VERSION=x.y.z` (defaults to the latest version)
- `PREFIX=/path/to/install` (defaults to `/usr/local/bin`)

For example, the following will install the binary into the working directory:

```sh
curl https://raw.githubusercontent.com/stepchowfun/mull/main/install.sh -LSfs | PREFIX=. sh
```

Note that if you change the installation path, you may also need to change the `mull.executablePath` accordingly in Visual Studio Code or Cursor.

If you prefer not to use this installation method, you can download the binary from the [releases page](https://github.com/stepchowfun/mull/releases), make it executable (e.g., with `chmod`), and place it in some directory in your [`PATH`](https://en.wikipedia.org/wiki/PATH_\(variable\)) (e.g., `/usr/local/bin`). You can then follow the instructions [below](#installation-of-the-visual-studio-code--cursor-extension) to install the Visual Studio Code / Cursor extension.

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
