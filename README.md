# Mull 📖

[![Build status](https://github.com/stepchowfun/mull/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/stepchowfun/mull/actions?query=branch%3Amain)

*Mull* is a tool for managing a wiki stored in a plain text file. It has a [Visual Studio Code](https://code.visualstudio.com/) / [Cursor](https://cursor.com/) extension which provides syntax highlighting, hover previews, completions, diagnostics, formatting, refactoring, etc.

![Welcome to Mull.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/mull.png)

## A quick tour

### Pages and links

A *wiki* is a text file with a `.mull` extension. It contains *pages* which link to each other, all in the same file. Here's a little wiki with 3 pages:

```md
# Home

My go-to breakfast is [Pancakes] with fresh [Lemons].

# Lemons

Great on [Pancakes].

# Pancakes

- 1 cup of flour
- 1 cup of milk
- 1 egg
- A squeeze of [Lemons]
```

Each page starts with a `# Title`, followed by the contents of the page. To link to a page, use square brackets like `[Lemons]`.

![Autocomplete.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/completions.png)

A wiki starts with a `Home` page. Mull requires that every other page is *reachable* (directly or indirectly) by following links, starting from `Home`. This rule promotes a basic form of organization that doesn't involve any hierarchical structure. Pages serve the roles traditionally filled by files *and* directories.

Mull formats the wiki when you save the file (default), or when you explicitly request it. It chooses the ordering of the pages for you, putting related pages near to each other. Linking to pages is the only way to organize them.

### Linking to files and directories

You can link to files like photos, spreadsheets, or anything else. Mull checks that the linked files actually exist, and you can preview them without leaving the editor.

![A file preview.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/file_preview.png)

Files go in a directory next to the wiki named after the wiki, with the `.mull` extension replaced with `_files`. For example, if the wiki is `my_wiki.mull`, the files go in `my_wiki_files`. If you have a file called `my_wiki_files/photos/beach.jpg`, you can link it with `[/photos/beach.jpg]`. You can drag-and-drop or paste files directly into the editor. You can also link to directories like `[/photos/]`.

Just as every page must be reachable from `Home`, every file in `<wiki>_files` (if it exists) must be linked to from the wiki. A link to a directory counts as linking to everything inside it.

### Opening a portal to another page

A portal is a window into another page. It holds a copy of the page's contents, and Mull keeps them in sync. For example, if you have a page called `Pancakes`, you can open a portal to it from another page like this:

```md
This is my favorite breakfast:

::: [Pancakes] :::
```

When the wiki is formatted (e.g., on save), that line will be expanded into a copy of the page:

```md
This is my favorite breakfast:

::: [Pancakes]
- 1 cup of flour
- 1 cup of milk
- 1 egg
- A squeeze of [Lemons]
:::
```

Any edits to the original page will be propagated to the portal when the wiki is formatted. You can also edit the content in the portal directly, and the edits will sync back to the original.

### Extending Mull with scripts

Suppose you have a list that you want to keep sorted:

```md
The contents of my pantry:

- Garlic
- Kosher salt
- Miso
- Olive oil
- Rice
```

Mull can help with this; you just have to teach it about sorting. Create a file called `<wiki>_files/scripts/sort.sh` with the following contents:

```sh
#!/bin/sh
sort "$@"
```

Make it executable with `chmod +x <wiki>_files/scripts/sort.sh`.

Now you can put the list in a `sort` block:

```md
The contents of my pantry:

::: sort
- Garlic
- Kosher salt
- Miso
- Olive oil
- Rice
:::
```

Mull will then sort the list automatically whenever the wiki is formatted (e.g., on save). That sorting script accepts arguments, so you can, for example, sort in reverse order with `::: sort -r`.

Using a script in a block counts as linking to it, so it doesn't need to be linked to from the wiki like other files in `<wiki>_files`. An unused script still needs a link (e.g., `[/scripts/sort.sh]`) somewhere in the wiki.

### Escaping and code

To use a special symbol like `[` or `]` literally, put a backslash before it, like `\[`. You can also use backticks to denote verbatim text like `` `this` ``, as in Markdown. Fenced code blocks are supported too, with syntax highlighting for common languages.

````md
# Home

I'm learning Python, currently working on a [Dice game].

# Dice game

```python
import random

for turn in range(3):
    roll = random.randint(1, 6)
    print(f"Roll {turn + 1}: {roll}")

print("Thanks for playing!")
```
````

No escaping is necessary inside page titles or code.

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
| Rename a page | `F2` | `F2` |
| Format the wiki | `Shift + Option + F` | `Shift + Alt + F` |

Most of these keyboard shortcuts are standard in Visual Studio Code and Cursor, so they may be familiar to you already. You can remap them.

## Usage

Once Mull is [installed](#installation-instructions) for Visual Studio Code or Cursor, you can run it by opening a `.mull` file in the editor.

You can also run Mull from the command line (e.g., in a CI job) as follows:

```sh
mull check --path wiki.mull
```

Here are the supported command-line options:

```
Usage: mull [COMMAND]

Commands:
  check            Check that a wiki is valid and formatted, with its scripts run
  format           Format a wiki, running its scripts (default)
  language-server  Start the language server (editors use this)
  help             Print this message or the help of the given subcommand(s)

Options:
  -v, --version  Print version
  -h, --help     Print help
```

## Installation instructions

To use Mull, you must install the binary and optionally the Visual Studio Code / Cursor extension. The binary does all the work and can be used by itself from the command line. The IDE extensions use the binary to provide a rich wiki editing experience.

### Installation on macOS or Linux (AArch64 or x86-64)

If you're running macOS or Linux (AArch64 or x86-64), you can install Mull with this command:

```sh
curl https://raw.githubusercontent.com/stepchowfun/mull/main/install.sh -LSfs | sh
```

It first installs the binary, then it installs the extension for Visual Studio Code and/or Cursor.

The same command can be used again to update to the latest version.

If you prefer not to use this installation method, you can download the binary from the [latest release](https://github.com/stepchowfun/mull/releases/latest), make it executable (e.g., with `chmod`), and place it in some directory in your [`PATH`](https://en.wikipedia.org/wiki/PATH_\(variable\)) (e.g., `/usr/local/bin`). You can then follow the instructions [below](#installing-the-visual-studio-code--cursor-extension) to install the Visual Studio Code / Cursor extension.

### Installing the binary on Windows (AArch64 or x86-64)

If you're running Windows (AArch64 or x86-64), download the binary from the [latest release](https://github.com/stepchowfun/mull/releases/latest) and rename it to `mull` (or `mull.exe` if you have file extensions visible). Create a directory called `Mull` in your `%PROGRAMFILES%` directory (e.g., `C:\Program Files\Mull`), and place the renamed binary in there. Then, in the "Advanced" tab of the "System Properties" section of Control Panel, click on "Environment Variables..." and add the full path to the new `Mull` directory to the `PATH` variable under "System variables". Note that the `Program Files` directory might have a different name if Windows is configured for a language other than English.

To update an existing installation, simply replace the existing binary.

### Installing the binary with Cargo

If you have [Cargo](https://doc.rust-lang.org/cargo/), you can install Mull as follows:

```sh
cargo install mull
```

You can run that command with `--force` to update an existing installation.

### Installing the Visual Studio Code / Cursor extension

First, make sure you have [Visual Studio Code](https://code.visualstudio.com/) or [Cursor](https://cursor.com/) installed.

To install the Mull extension, download the `mull.vsix` file from the [latest release](https://github.com/stepchowfun/mull/releases/latest) and install it via the "Install from VSIX..." option in the `...` menu at the top of the Extensions view of the Primary Side Bar.

![Installation.](https://raw.githubusercontent.com/stepchowfun/mull/main/screenshots/installation.png)

Note that the macOS / Linux installation command will install the extension automatically, so you can skip this section if you use that installation method.

To update an existing installation, simply re-install it.
