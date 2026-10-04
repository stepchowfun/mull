# Mull

This extension provides language support for [Mull](https://github.com/stepchowfun/mull), a tool for managing a local personal knowledge base.

It launches Mull's language server to report wiki errors and provides folding that shows only the node being edited, format-on-save, document outlines, node navigation and previews with clickable text links, commands to jump to the start or end of the current node, link occurrence highlighting, reference search, text-link completion, node renaming, quick fixes for creating missing nodes, syntax highlighting, square-bracket editing behavior, and prose-friendly word wrapping.

The extension requires the Mull executable. It uses `mull` from `PATH` by default; set `mull.executablePath` if Mull is installed elsewhere.

By default, everything outside the node being edited is folded away. Set `mull.foldOtherNodes` to `false` to keep the whole wiki unfolded.
