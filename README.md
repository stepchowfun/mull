# Mull 🍇

[![Build status](https://github.com/stepchowfun/mull/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/stepchowfun/mull/actions?query=branch%3Amain)

Organize your knowledge.

## Usage

Once Mull is [installed](#installation-instructions), you can run it from the command line as
follows:

```sh
mull
```

Here are the supported command-line options:

```
Usage: mull [OPTIONS] [COMMAND]

Commands:
  check  Check a document
  fix    Fix a document (default)
  help   Print this message or the help of the given subcommand(s)

Options:
  -v, --version      Print version
      --path <PATH>  Specify the path to the document
  -h, --help         Print help
```

## Document format

Each node starts with `# ` followed by its title. Its content continues until the next title. A text link such as `[Greeting]` refers to another node. Every document must contain a node called `Home`, and every node must be transitively reachable from it through text links.

File links such as [file:README.md] and directory links such as [dir:src] refer to paths relative to the directory containing the document. A directory link implicitly references everything recursively contained within that directory. Mull checks that these targets exist and that every other non-ignored file and directory alongside the document is referenced. Ignore files such as `.gitignore` and `.ignore` are respected; hidden entries are otherwise included.

## Installation instructions

### Installation on macOS or Linux (AArch64 or x86-64)

If you're running macOS or Linux (AArch64 or x86-64), you can install Mull with this command:

```sh
curl https://raw.githubusercontent.com/stepchowfun/mull/main/install.sh -LSfs | sh
```

The same command can be used again to update to the latest version.

The installation script supports the following optional environment variables:

- `VERSION=x.y.z` (defaults to the latest version)
- `PREFIX=/path/to/install` (defaults to `/usr/local/bin`)

For example, the following will install Mull into the working directory:

```sh
curl https://raw.githubusercontent.com/stepchowfun/mull/main/install.sh -LSfs | PREFIX=. sh
```

If you prefer not to use this installation method, you can download the binary from the [releases page](https://github.com/stepchowfun/mull/releases), make it executable (e.g., with `chmod`), and place it in some directory in your [`PATH`](https://en.wikipedia.org/wiki/PATH_\(variable\)) (e.g., `/usr/local/bin`).

### Installation on Windows (AArch64 or x86-64)

If you're running Windows (AArch64 or x86-64), download the latest binary from the [releases page](https://github.com/stepchowfun/mull/releases) and rename it to `mull` (or `mull.exe` if you have file extensions visible). Create a directory called `Mull` in your `%PROGRAMFILES%` directory (e.g., `C:\Program Files\Mull`), and place the renamed binary in there. Then, in the "Advanced" tab of the "System Properties" section of Control Panel, click on "Environment Variables..." and add the full path to the new `Mull` directory to the `PATH` variable under "System variables". Note that the `Program Files` directory might have a different name if Windows is configured for a language other than English.

To update an existing installation, simply replace the existing binary.

### Installation with Cargo

If you have [Cargo](https://doc.rust-lang.org/cargo/), you can install Mull as follows:

```sh
cargo install mull
```

You can run that command with `--force` to update an existing installation.
