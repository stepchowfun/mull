# Mull

This extension provides language support for [Mull](https://github.com/stepchowfun/mull), a tool for managing a wiki stored in a plain text file.

It launches Mull's language server to report wiki errors and provides dimming of everything outside the page being edited, scrolling that snaps back to that page, format-on-save, document outlines, page navigation and previews with clickable text links, commands to jump to the start or end of the current page, a history of visited pages, link occurrence highlighting, reference search, text-link completion, page renaming, quick fixes for creating missing pages, syntax highlighting, square-bracket editing behavior, and prose-friendly word wrapping.

The extension requires the Mull executable. It uses `mull` from `PATH` by default; set `mull.executablePath` if Mull is installed elsewhere.

By default, everything outside the page being edited is dimmed. Set `mull.dimOtherPages` to `false` to turn this off. Set `mull.snapBackToCurrentPage` to `true` to have the view snap back to the page being edited after you scroll it out of view.

The wiki is checked again whenever it or the files it links to change. If its diagnostics ever seem out of date, run `Mull: Check Wiki` from the command palette to check it again.
