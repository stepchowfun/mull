#!/usr/bin/env node
import { readFileSync } from "node:fs";
//#region src/sort.ts
const items = [];
for (const line of readFileSync(0, "utf8").split("\n")) {
	if (line.trim() === "") continue;
	if (/^\s/.test(line) && items.length > 0) items[items.length - 1].push(line);
	else items.push([line]);
}
const direction = process.argv.includes("desc") ? -1 : 1;
const collator = new Intl.Collator();
const key = (line) => line.replace(/^[^\p{L}\p{N}]+/u, "");
items.sort((a, b) => direction * collator.compare(key(a[0]), key(b[0])));
for (const item of items) process.stdout.write(`${item.join("\n")}\n`);
//#endregion
export {};
