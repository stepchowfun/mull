#!/usr/bin/env node

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
  year: number | null;
  month: number | null;
  day: number | null;
}

// This is a date with a year, how precise it is, and whether it was written without its year. A
// month or day which isn't part of the date is 0.
interface ParsedDate {
  year: number;
  month: number;
  day: number;
  precision: 'year' | 'month' | 'day';
  yearless: boolean;
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
function parseMonth(word: string): number | null {
  if (word.length < 3) {
    return null;
  }
  const index = MONTHS.findIndex((month) => month.startsWith(word));
  return index === -1 ? null : index + 1;
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
// year in which it occurs, and it's marked as `yearless`. The result has a `precision` of `year`,
// `month`, or `day`.
function parseDate(text: string): ParsedDate | null {
  const fields = readFields(text);
  if (fields === null) {
    return null;
  }
  const { year, month, day } = fields;
  if (year !== null) {
    return checkDate(text, year, month, day, false);
  }
  if (month === null) {
    return null;
  }
  return checkDate(text, nextYear(month, day), month, day, true);
}

// Read a number from digits which may be missing.
function readNumber(digits: string | undefined): number | null {
  return digits ? Number(digits) : null;
}

// Read the year, month, and day written in a date, each of which may be missing.
function readFields(text: string): Fields | null {
  const normalized = text.toLowerCase().replace(/,/g, ' ').replace(/\./g, ' ').trim();

  // Read numeric dates, with the year first, missing, or last.
  let match = normalized.match(/^(\d{4})(?:[-/](\d{1,2})(?:[-/](\d{1,2}))?)?$/);
  if (match) {
    return { year: readNumber(match[1]), month: readNumber(match[2]), day: readNumber(match[3]) };
  }
  match = normalized.match(/^--(\d{1,2})(?:-(\d{1,2}))?$/);
  if (match) {
    return { year: null, month: readNumber(match[1]), day: readNumber(match[2]) };
  }
  match = normalized.match(/^(\d{1,2})\/(\d{1,2})(?:\/(\d{4}))?$/);
  if (match) {
    return { year: readNumber(match[3]), month: readNumber(match[1]), day: readNumber(match[2]) };
  }

  // Read dates with a month name, ignoring weekdays and ordinal suffixes like `1st`.
  let year: number | null = null;
  let month: number | null = null;
  let day: number | null = null;
  for (const word of normalized.split(/\s+/)) {
    if (/^\d{4}$/.test(word) && year === null) {
      year = Number(word);
    } else if (/^\d{1,2}(st|nd|rd|th)?$/.test(word) && day === null) {
      day = Number(word.replace(/\D/g, ''));
    } else if (parseMonth(word) !== null && month === null) {
      month = parseMonth(word);
    } else if (!/^((mon|tue|wed|thu|fri|sat|sun)[a-z]*|the|of)$/.test(word)) {
      return null;
    }
  }
  return month === null ? null : { year, month, day };
}

// Find the year of the next occurrence of a month, or of a day in it, counting today. February 29
// waits for a leap year. A day which never occurs gets the current year, to be rejected later.
function nextYear(month: number, day: number | null): number {
  const today = new Date();
  const thisYear = today.getFullYear();
  const thisMonth = today.getMonth() + 1;
  const thisDay = today.getDate();
  for (let year = thisYear; year < thisYear + 8; year += 1) {
    const exists = day === null || day <= new Date(Date.UTC(year, month, 0)).getUTCDate();
    const passed =
      year === thisYear &&
      (month < thisMonth || (month === thisMonth && day !== null && day < thisDay));
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
  month: number | null,
  day: number | null,
  yearless: boolean,
): ParsedDate {
  if (month === null) {
    return { year, month: 0, day: 0, precision: 'year', yearless };
  }
  if (month < 1 || month > 12) {
    fail(`\`${text}\` has a month which doesn't exist.`);
  }
  if (day === null) {
    return { year, month, day: 0, precision: 'month', yearless };
  }
  const daysInMonth = new Date(Date.UTC(year, month, 0)).getUTCDate();
  if (day < 1 || day > daysInMonth) {
    fail(`\`${text}\` has a day which doesn't exist.`);
  }
  return { year, month, day, precision: 'day', yearless };
}

// Write a number with at least two digits.
function pad(number: number): string {
  return String(number).padStart(2, '0');
}

// Write a date in its canonical form, like `2026-01-01`, `2026-01`, or `2026`. A date written
// without a year is written without one again, like `--01-01` or `--01`, so it moves to the next
// year once it passes.
function formatDate(date: ParsedDate): string {
  const year = date.yearless ? '-' : String(date.year);
  if (date.precision === 'day') {
    return `${year}-${pad(date.month)}-${pad(date.day)}`;
  }
  if (date.precision === 'month') {
    return `${year}-${pad(date.month)}`;
  }
  return year;
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
  const match = line.match(/^- ([^:]+):\s*(.*)$/);
  if (!match) {
    fail(`\`${line}\` should look like \`- 2026-01-01: Something to do\`.`);
  }
  const date = parseDate(match[1]);
  if (date === null) {
    fail(
      `\`${match[1].trim()}\` isn't a date this script understands. ` +
        'Try a form like `2026-01-01`, `January 1, 2026`, `Jan 1`, or `Jan 2026`.',
    );
  }
  items.push({ date, text: match[2], lines: [] });
}

// Sort the items by date, with a month or year before the days within it, in descending order
// with `desc`. Items with the same date keep their order.
const direction = process.argv.includes('desc') ? -1 : 1;
items.sort((a, b) => {
  for (const key of ['year', 'month', 'day'] as const) {
    if (a.date[key] !== b.date[key]) {
      return direction * (a.date[key] - b.date[key]);
    }
  }
  return 0;
});

// Print each item with its date in canonical form, followed by its nested lines.
for (const item of items) {
  const text = item.text === '' ? '' : ` ${item.text}`;
  process.stdout.write(`${[`- ${formatDate(item.date)}:${text}`, ...item.lines].join('\n')}\n`);
}
