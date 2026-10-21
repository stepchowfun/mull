#!/usr/bin/env -S node --experimental-strip-types

// Sort a list in a Mull script block, like this:
//
//   ::: sort
//   - Pears
//   - Apples
//     - Honeycrisp
//   - Cherries
//   :::
//
// Each top-level line is sorted alphabetically, ignoring any leading characters which aren't
// letters or digits, such as a bullet or a link's opening bracket. Indented lines are nested under
// the line above them and move with it, keeping their own order. Empty lines are dropped.
//
// Write `::: sort desc` to sort in reverse order.

import { readFileSync } from 'node:fs';

// Group each top-level bullet with the nested lines beneath it, so they move together, and drop
// empty lines.
const items: string[][] = [];
for (const line of readFileSync(0, 'utf8').split('\n')) {
  if (line.trim() === '') {
    continue;
  }
  if (/^\s/.test(line) && items.length > 0) {
    items[items.length - 1].push(line);
  } else {
    items.push([line]);
  }
}

// Sort the items by their top-level lines, ignoring any leading characters which aren't letters or
// digits, such as a bullet or a link's opening bracket, in descending order with `desc`.
const direction = process.argv.includes('desc') ? -1 : 1;
const collator = new Intl.Collator();
const key = (line: string): string => line.replace(/^[^\p{L}\p{N}]+/u, '');
items.sort((a, b) => direction * collator.compare(key(a[0]), key(b[0])));

// Print each item with its nested lines.
for (const item of items) {
  process.stdout.write(`${item.join('\n')}\n`);
}
