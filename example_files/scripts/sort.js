#!/usr/bin/env node
const fs = require('node:fs');

// Group each top-level bullet with the nested lines beneath it, so they move together, and drop
// empty lines.
const items = [];
for (const line of fs.readFileSync(0, 'utf8').split('\n')) {
  if (line.trim() === '') {
    continue;
  }
  if (/^\s/.test(line) && items.length > 0) {
    items.at(-1).push(line);
  } else {
    items.push([line]);
  }
}

// Sort the items by their top-level lines, in descending order with `desc`.
const direction = process.argv.includes('desc') ? -1 : 1;
const collator = new Intl.Collator();
items.sort((a, b) => direction * collator.compare(a[0], b[0]));

// Print each item with its nested lines.
for (const item of items) {
  console.log(item.join('\n'));
}
