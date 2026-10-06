# Mull

This extension provides language support for [Mull](https://github.com/stepchowfun/mull), a tool for managing a local personal knowledge base.

It launches Mull's language server to report wiki errors and provides dimming of everything outside the node being edited, scrolling that snaps back to that node, format-on-save, document outlines, node navigation and previews with clickable text links, commands to jump to the start or end of the current node, a history of visited nodes with a link back to the previous one, link occurrence highlighting, reference search, text-link completion, node renaming, quick fixes for creating missing nodes, syntax highlighting, square-bracket editing behavior, and prose-friendly word wrapping.

The extension requires the Mull executable. It uses `mull` from `PATH` by default; set `mull.executablePath` if Mull is installed elsewhere.

By default, everything outside the node being edited is dimmed. Set `mull.dimOtherNodes` to `false` to turn this off. After you scroll the node being edited out of view, the view snaps back to it; set `mull.snapBackToCurrentNode` to `false` to turn that off.
