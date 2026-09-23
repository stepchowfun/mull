#!/usr/bin/env sh

# This installer script supports Linux and macOS machines running on AArch64 or x86-64.

# Usage examples:
#   ./install.sh
#   VERSION=x.y.z ./install.sh
#   PREFIX=/usr/local/bin ./install.sh

# We wrap everything in parentheses to prevent the shell from executing only a prefix of the script
# if the download is interrupted.
(
  # Where the binary will be installed
  DESTINATION="${PREFIX:-/usr/local/bin}/mull"

  # Determine which binary to download.
  FILENAME=''
  if uname -a | grep -qi 'x86_64.*GNU/Linux'; then
    echo 'x86-64 GNU Linux detected.'
    FILENAME=mull-x86_64-unknown-linux-gnu
  elif uname -a | grep -qi 'x86_64.*Linux'; then
    echo 'x86-64 non-GNU Linux detected.'
    FILENAME=mull-x86_64-unknown-linux-musl
  elif uname -a | grep -qi 'aarch64.*GNU/Linux'; then
    echo 'AArch64 GNU Linux detected.'
    FILENAME=mull-aarch64-unknown-linux-gnu
  elif uname -a | grep -qi 'aarch64.*Linux'; then
    echo 'AArch64 non-GNU Linux detected.'
    FILENAME=mull-aarch64-unknown-linux-musl
  elif uname -a | grep -qi 'Darwin.*x86_64'; then
    echo 'x86-64 macOS detected.'
    FILENAME=mull-x86_64-apple-darwin
  elif uname -a | grep -qi 'Darwin.*arm64'; then
    echo 'AArch64 macOS detected.'
    FILENAME=mull-aarch64-apple-darwin
  fi

  # Find a temporary location for the binary.
  TEMPDIR=$(mktemp -d /tmp/mull.XXXXXXXX)

  # This is a helper function to clean up and fail.
  fail() {
    echo "$1" >&2
    rm -rf "$TEMPDIR"
    exit 1
  }

  # Fail if there is no pre-built binary for this platform.
  if [ -z "$FILENAME" ]; then
    fail 'Unfortunately, there is no pre-built binary for this platform.'
  fi

  # Compute the full file path for the binary.
  SOURCE="$TEMPDIR/$FILENAME"

  # Locate the requested release, or the latest published release by default.
  if [ -n "${VERSION:-}" ]; then
    RELEASE_URL="https://github.com/stepchowfun/mull/releases/download/v$VERSION"
  else
    RELEASE_URL='https://github.com/stepchowfun/mull/releases/latest/download'
  fi

  # Download the binary.
  curl "$RELEASE_URL/$FILENAME" -o "$SOURCE" -LSf ||
    fail 'There was an error downloading the binary.'

  # Make it executable.
  chmod a+x "$SOURCE" || fail 'There was an error setting the permissions for the binary.'

  # Install it at the requested destination.
  # shellcheck disable=SC2024
  mv -f "$SOURCE" "$DESTINATION" 2> /dev/null ||
    sudo mv -f "$SOURCE" "$DESTINATION" < /dev/tty ||
    fail "Unable to install the binary at $DESTINATION."

  # If SELinux is installed, apply the default security context to the binary.
  # shellcheck disable=SC2024
  if command -v restorecon > /dev/null 2>&1; then
    restorecon "$DESTINATION" > /dev/null 2>&1 ||
    sudo restorecon "$DESTINATION" < /dev/tty ||
    fail 'Unable to set SELinux attributes on the binary.'
  fi

  # Let the user know if the installation was successful.
  "$DESTINATION" --version || fail 'There was an error installing the binary.'

  # Find supported editor command-line interfaces.
  VSCODE_COMMAND=''
  CURSOR_COMMAND=''
  if command -v code > /dev/null 2>&1; then
    VSCODE_COMMAND=code
  elif [ -x '/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code' ]; then
    VSCODE_COMMAND='/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code'
  fi
  if command -v cursor > /dev/null 2>&1; then
    CURSOR_COMMAND=cursor
  elif [ -x '/Applications/Cursor.app/Contents/Resources/app/bin/cursor' ]; then
    CURSOR_COMMAND='/Applications/Cursor.app/Contents/Resources/app/bin/cursor'
  fi

  # Install the extension in each editor that is available.
  if [ -n "$VSCODE_COMMAND" ] || [ -n "$CURSOR_COMMAND" ]; then
    EXTENSION_SOURCE="$TEMPDIR/mull.vsix"
    curl "$RELEASE_URL/mull.vsix" -o "$EXTENSION_SOURCE" -LSf ||
      fail 'There was an error downloading the editor extension.'

    # Install the extension in Visual Studio Code if it was found.
    if [ -n "$VSCODE_COMMAND" ]; then
      "$VSCODE_COMMAND" --install-extension "$EXTENSION_SOURCE" --force ||
        fail 'There was an error installing the extension in Visual Studio Code.'
    fi

    # Install the extension in Cursor if it was found.
    if [ -n "$CURSOR_COMMAND" ]; then
      "$CURSOR_COMMAND" --install-extension "$EXTENSION_SOURCE" --force ||
        fail 'There was an error installing the extension in Cursor.'
    fi
  fi

  # Remove the temporary directory.
  rm -rf "$TEMPDIR"
)
