# Mull 🍇

[![Build status](https://github.com/stepchowfun/mull/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/stepchowfun/mull/actions?query=branch%3Amain)

*Mull* is a tool for managing a local personal knowledge base. A Mull *wiki* is a plain text file containing *nodes* with *links* between them. Nodes can also link to files and directories, so the knowledge graph serves as an index of the local file tree.

## Usage

Once Mull is [installed](#installation-instructions) for Visual Studio Code or Cursor, you can run it by opening a `.mull` file in the editor. Mull supports all the standard features you'd expect from a language plugin, such as formatting, error reporting, jumping to definitions, renaming nodes, hover previews, etc.

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
  language-server  Start the language server
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
