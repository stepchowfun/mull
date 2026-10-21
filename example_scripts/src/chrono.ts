#!/usr/bin/env -S node --experimental-strip-types

// Sort a list of dates in a Mull script block, like this:
//
//   ::: chrono
//   - July 4, 2026: Picnic in the park
//   - Jan 15: Car registration renewal
//   - 2026-03: Spring cleaning
//   :::
//
// Each top-level line is a bullet with a date, a colon, and some text. The dates can be written in
// many forms, like `2026-01-01`, `1/1/2026`, `January 1, 2026`, or `Jan 2026`, and the script
// rewrites them in a standard form, like `2026-01-01`, `2026-01`, or `2026`. A month or a year
// comes before the days within it. Indented lines are nested under the line above them and move
// with it. Empty lines are dropped.
//
// The year can be left out, like `Jan 15`, for something which happens every year. It's written
// back without a year, like `--01-15`, and sorted as its next occurrence, counting today, so it
// moves to the end of the list once it passes and the script runs again.
//
// Write `::: chrono desc` to sort in reverse order.

import { readFileSync } from 'node:fs';

// These are the month names, which can also be abbreviated to their first three or more letters.
const MONTHS = [
  'january',
  'february',
  'march',
  'april',
  'may',
  'june',
  'july',
  'august',
  'september',
  'october',
  'november',
  'december',
];

// These are the parts of a date as written, each of which may be missing.
interface Fields {
  year: number | undefined;
  month: number | undefined;
  day: number | undefined;
}

// This is a date as written, along with the year of its next occurrence if it has no year.
interface ParsedDate extends Fields {
  inferredYear: number | undefined;
}

// This is a top-level bullet with its date and text, along with the nested lines beneath it.
interface Item {
  date: ParsedDate;
  text: string;
  lines: string[];
}

// Report a problem with the input and stop, so Mull leaves the block as it was.
function fail(message: string): never {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

// Find a month from its name or an abbreviation of it, like `Sep` or `Sept`.
function parseMonth(word: string): number | undefined {
  if (word.length < 3) {
    return undefined;
  }
  const index = MONTHS.findIndex((month) => month.startsWith(word));
  return index === -1 ? undefined : index + 1;
}

// Read a date with optionally a month and a day, in any of these forms:
//
// - `2026`
// - `2026-01`, `2026-01-01`, or `2026/1/1`
// - `--01` or `--01-01`, which have no year
// - `1/1/2026` or `1/1`, read as month, day, and year
// - `January 2026`, `Jan 1, 2026`, `1 Jan 2026`, or `Thursday, January 1st, 2026`
// - `January`, `Jan 1`, or `1 Jan`
//
// A date without a year is in the current year if it hasn't passed yet, and otherwise in the next
// year in which it occurs, which becomes its `inferredYear`.
function parseDate(text: string): ParsedDate | undefined {
  const fields = readFields(text);
  if (fields === undefined) {
    return undefined;
  }
  const { year, month, day } = fields;
  if (year !== undefined) {
    checkDate(text, year, month, day);
    return { year, month, day, inferredYear: undefined };
  }
  if (month === undefined) {
    return undefined;
  }
  const inferredYear = nextYear(month, day);
  checkDate(text, inferredYear, month, day);
  return { year, month, day, inferredYear };
}

// Read a number from digits which may be missing.
function readNumber(digits: string | undefined): number | undefined {
  return digits ? Number(digits) : undefined;
}

// Read the year, month, and day written in a date, each of which may be missing.
function readFields(text: string): Fields | undefined {
  const normalized = text.toLowerCase().replace(/,/g, ' ').replace(/\./g, ' ').trim();

  // Read numeric dates, with the year first, missing, or last.
  let match = normalized.match(/^(\d{4})(?:[-/](\d{1,2})(?:[-/](\d{1,2}))?)?$/);
  if (match) {
    return { year: readNumber(match[1]), month: readNumber(match[2]), day: readNumber(match[3]) };
  }
  match = normalized.match(/^--(\d{1,2})(?:-(\d{1,2}))?$/);
  if (match) {
    return { year: undefined, month: readNumber(match[1]), day: readNumber(match[2]) };
  }
  match = normalized.match(/^(\d{1,2})\/(\d{1,2})(?:\/(\d{4}))?$/);
  if (match) {
    return { year: readNumber(match[3]), month: readNumber(match[1]), day: readNumber(match[2]) };
  }

  // Read dates with a month name, ignoring weekdays and ordinal suffixes like `1st`.
  let year: number | undefined = undefined;
  let month: number | undefined = undefined;
  let day: number | undefined = undefined;
  for (const word of normalized.split(/\s+/)) {
    if (/^\d{4}$/.test(word) && year === undefined) {
      year = Number(word);
    } else if (/^\d{1,2}(st|nd|rd|th)?$/.test(word) && day === undefined) {
      day = Number(word.replace(/\D/g, ''));
    } else if (parseMonth(word) !== undefined && month === undefined) {
      month = parseMonth(word);
    } else if (!/^((mon|tue|wed|thu|fri|sat|sun)[a-z]*|the|of)$/.test(word)) {
      return undefined;
    }
  }
  return month === undefined ? undefined : { year, month, day };
}

// Find the year of the next occurrence of a month, or of a day in it, counting today. February 29
// waits for a leap year. A day which never occurs gets the current year, to be rejected later.
function nextYear(month: number, day: number | undefined): number {
  const today = new Date();
  const thisYear = today.getFullYear();
  const thisMonth = today.getMonth() + 1;
  const thisDay = today.getDate();
  for (let year = thisYear; year < thisYear + 8; year += 1) {
    const exists = day === undefined || day <= new Date(Date.UTC(year, month, 0)).getUTCDate();
    const passed =
      year === thisYear &&
      (month < thisMonth || (month === thisMonth && day !== undefined && day < thisDay));
    if (exists && !passed) {
      return year;
    }
  }
  return thisYear;
}

// Check that a date exists, such as rejecting February 30.
function checkDate(
  text: string,
  year: number,
  month: number | undefined,
  day: number | undefined,
): void {
  if (month !== undefined) {
    if (month < 1 || month > 12) {
      fail(`\`${text}\` has a month which doesn't exist.`);
    }
    const daysInMonth = new Date(Date.UTC(year, month, 0)).getUTCDate();
    if (day !== undefined && (day < 1 || day > daysInMonth)) {
      fail(`\`${text}\` has a day which doesn't exist.`);
    }
  }
}

// Write a number with at least two digits.
function pad(number: number): string {
  return String(number).padStart(2, '0');
}

// Write a date in its canonical form, like `2026-01-01`, `2026-01`, or `2026`. A date written
// without a year is written without one again, like `--01-01` or `--01`, so it moves to the next
// year once it passes.
function formatDate(date: ParsedDate): string {
  const year = date.year === undefined ? '-' : String(date.year);
  if (date.month === undefined) {
    return year;
  }
  if (date.day === undefined) {
    return `${year}-${pad(date.month)}`;
  }
  return `${year}-${pad(date.month)}-${pad(date.day)}`;
}

// Find the numbers to sort a date by: its year, as written or inferred, then its month and day. A
// missing month or day is 0 to come first, since real ones start at 1.
function sortKey(date: ParsedDate): number[] {
  return [date.year ?? date.inferredYear ?? 0, date.month ?? 0, date.day ?? 0];
}

// Group each top-level bullet with the nested lines beneath it, so they move together, and drop
// empty lines.
const items: Item[] = [];
for (const line of readFileSync(0, 'utf8').split('\n')) {
  if (line.trim() === '') {
    continue;
  }
  if (/^\s/.test(line) && items.length > 0) {
    items[items.length - 1].lines.push(line);
    continue;
  }

  // Read the date at the start of each top-level bullet, before the first colon.
  const match = line.match(/^-([^:]+):\s*(.*)$/);
  if (!match) {
    fail(`\`${line}\` should look like \`- 2026-01-01: Something to do\`.`);
  }
  const dateText = match[1].trim();
  const date = parseDate(dateText);
  if (date === undefined) {
    fail(
      `\`${dateText}\` isn't a date this script understands. ` +
        'Try a form like `2026-01-01`, `January 1, 2026`, `Jan 1`, or `Jan 2026`.',
    );
  }
  items.push({ date, text: match[2], lines: [] });
}

// Sort the items by date, with a month or year before the days within it, in descending order
// with `desc`. Items with the same date keep their order.
const direction = process.argv.includes('desc') ? -1 : 1;
items.sort((a, b) => {
  const aKey = sortKey(a.date);
  const bKey = sortKey(b.date);
  for (const [index, value] of aKey.entries()) {
    if (value !== bKey[index]) {
      return direction * (value - bKey[index]);
    }
  }
  return 0;
});

// Print each item with its date in canonical form, followed by its nested lines.
for (const item of items) {
  const text = item.text === '' ? '' : ` ${item.text}`;
  process.stdout.write(`${[`- ${formatDate(item.date)}:${text}`, ...item.lines].join('\n')}\n`);
}
