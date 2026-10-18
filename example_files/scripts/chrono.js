#!/usr/bin/env node
import { readFileSync } from "node:fs";
//#region src/chrono.ts
const MONTHS = [
	"january",
	"february",
	"march",
	"april",
	"may",
	"june",
	"july",
	"august",
	"september",
	"october",
	"november",
	"december"
];
function fail(message) {
	process.stderr.write(`${message}\n`);
	process.exit(1);
}
function parseMonth(word) {
	if (word.length < 3) return null;
	const index = MONTHS.findIndex((month) => month.startsWith(word));
	return index === -1 ? null : index + 1;
}
function parseDate(text) {
	const fields = readFields(text);
	if (fields === null) return null;
	const { year, month, day } = fields;
	if (year !== null) return checkDate(text, year, month, day, false);
	if (month === null) return null;
	return checkDate(text, nextYear(month, day), month, day, true);
}
function readNumber(digits) {
	return digits ? Number(digits) : null;
}
function readFields(text) {
	const normalized = text.toLowerCase().replace(/,/g, " ").replace(/\./g, " ").trim();
	let match = normalized.match(/^(\d{4})(?:[-/](\d{1,2})(?:[-/](\d{1,2}))?)?$/);
	if (match) return {
		year: readNumber(match[1]),
		month: readNumber(match[2]),
		day: readNumber(match[3])
	};
	match = normalized.match(/^--(\d{1,2})(?:-(\d{1,2}))?$/);
	if (match) return {
		year: null,
		month: readNumber(match[1]),
		day: readNumber(match[2])
	};
	match = normalized.match(/^(\d{1,2})\/(\d{1,2})(?:\/(\d{4}))?$/);
	if (match) return {
		year: readNumber(match[3]),
		month: readNumber(match[1]),
		day: readNumber(match[2])
	};
	let year = null;
	let month = null;
	let day = null;
	for (const word of normalized.split(/\s+/)) if (/^\d{4}$/.test(word) && year === null) year = Number(word);
	else if (/^\d{1,2}(st|nd|rd|th)?$/.test(word) && day === null) day = Number(word.replace(/\D/g, ""));
	else if (parseMonth(word) !== null && month === null) month = parseMonth(word);
	else if (!/^((mon|tue|wed|thu|fri|sat|sun)[a-z]*|the|of)$/.test(word)) return null;
	return month === null ? null : {
		year,
		month,
		day
	};
}
function nextYear(month, day) {
	const today = /* @__PURE__ */ new Date();
	const thisYear = today.getFullYear();
	const thisMonth = today.getMonth() + 1;
	const thisDay = today.getDate();
	for (let year = thisYear; year < thisYear + 8; year += 1) if ((day === null || day <= new Date(Date.UTC(year, month, 0)).getUTCDate()) && !(year === thisYear && (month < thisMonth || month === thisMonth && day !== null && day < thisDay))) return year;
	return thisYear;
}
function checkDate(text, year, month, day, yearless) {
	if (month === null) return {
		year,
		month: 0,
		day: 0,
		precision: "year",
		yearless
	};
	if (month < 1 || month > 12) fail(`\`${text}\` has a month which doesn't exist.`);
	if (day === null) return {
		year,
		month,
		day: 0,
		precision: "month",
		yearless
	};
	const daysInMonth = new Date(Date.UTC(year, month, 0)).getUTCDate();
	if (day < 1 || day > daysInMonth) fail(`\`${text}\` has a day which doesn't exist.`);
	return {
		year,
		month,
		day,
		precision: "day",
		yearless
	};
}
function pad(number) {
	return String(number).padStart(2, "0");
}
function formatDate(date) {
	const year = date.yearless ? "-" : String(date.year);
	if (date.precision === "day") return `${year}-${pad(date.month)}-${pad(date.day)}`;
	if (date.precision === "month") return `${year}-${pad(date.month)}`;
	return year;
}
const items = [];
for (const line of readFileSync(0, "utf8").split("\n")) {
	if (line.trim() === "") continue;
	if (/^\s/.test(line) && items.length > 0) {
		items[items.length - 1].lines.push(line);
		continue;
	}
	const match = line.match(/^- ([^:]+):\s*(.*)$/);
	if (!match) fail(`\`${line}\` should look like \`- 2026-01-01: Something to do\`.`);
	const date = parseDate(match[1]);
	if (date === null) fail(`\`${match[1].trim()}\` isn't a date this script understands. Try a form like \`2026-01-01\`, \`January 1, 2026\`, \`Jan 1\`, or \`Jan 2026\`.`);
	items.push({
		date,
		text: match[2],
		lines: []
	});
}
const direction = process.argv.includes("desc") ? -1 : 1;
items.sort((a, b) => {
	for (const key of [
		"year",
		"month",
		"day"
	]) if (a.date[key] !== b.date[key]) return direction * (a.date[key] - b.date[key]);
	return 0;
});
for (const item of items) {
	const text = item.text === "" ? "" : ` ${item.text}`;
	process.stdout.write(`${[`- ${formatDate(item.date)}:${text}`, ...item.lines].join("\n")}\n`);
}
//#endregion
export {};
