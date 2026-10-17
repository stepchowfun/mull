# Mull

This extension provides language support for [Mull](https://github.com/stepchowfun/mull), a tool for managing a wiki stored in a plain text file.

It launches Mull's language server to report wiki errors and provides dimming of everything outside the page being edited, format-on-save (which also runs scripts), document outlines, page navigation, clickable links, hover previews, commands to jump to the start or end of the current page, a history of visited pages, link occurrence highlighting, reference search, text-link completion, opening a portal to a page or running a script by typing `:::`, pasting or dropping files (hold Shift while dropping) as links to them, copying them into `<wiki>_files` if needed, page renaming, quick fixes for creating missing pages, syntax highlighting, square-bracket editing behavior, and prose-friendly word wrapping.

The extension requires the Mull executable. It uses `mull` from `PATH` by default; set `mull.executablePath` if Mull is installed elsewhere.

By default, everything outside the page being edited is dimmed. Set `mull.dimOtherPages` to `false` to turn this off.

The wiki is checked again whenever it or the files it links to change. If its diagnostics ever seem out of date, run `Mull: Check Wiki` from the command palette to check it again and see the result.
