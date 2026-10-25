#!/usr/bin/env -S node --experimental-strip-types

// Work out arithmetic in a Mull script block, like this:
//
//   ::: calc
//   36 - 2 * 0.75 =
//   (3 + 5) * 2 ^ 3 =
//   :::
//
// On each line with an `=`, the script replaces whatever follows the `=` with the result of the
// arithmetic before it, so the lines above become `36 - 2 * 0.75 = 34.5` and
// `(3 + 5) * 2 ^ 3 = 64`. The result is worked out again each time the script runs, so it stays
// up to date as the arithmetic changes. Other lines are kept as written. A block of a single line
// doesn't need the `=`.
//
// In a divided block, the arithmetic above the divider is kept as written, and the script writes
// only the results below it, one line for each line of arithmetic, like this:
//
//   ::: calc
//   1850 * 12
//   ---
//   22200
//   :::
//
// There, a line needn't have an `=`, and anything after one is ignored. Blank lines stay blank.
//
// The arithmetic can use numbers with decimals and commas between their digits, like `2,400.50`,
// as well as `+`, `-`, `*`, `/`, `^` for powers, and parentheses. Results are rounded to 12
// significant digits.

import { readFileSync } from 'node:fs';

// Report a problem with the input and stop, so Mull leaves the block as it was.
function fail(message: string): never {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

// Evaluate arithmetic with numbers, which can have decimals and commas between their digits, `+`,
// `-`, `*`, `/`, `^` for powers, and parentheses, throwing an error which describes any problem.
function evaluate(expression: string): number {
  const tokens = expression.match(/\d[\d,]*(?:\.\d+)?|\.\d+|\S/gu) ?? [];
  let position = 0;

  // Read a number or a parenthesized expression.
  function primary(): number {
    if (position >= tokens.length) {
      throw new Error('Expected a number.');
    }
    const token = tokens[position];
    position += 1;
    if (token === '(') {
      const value = sum();
      if (tokens[position] !== ')') {
        throw new Error('Expected `)`.');
      }
      position += 1;
      return value;
    }
    if (!/^\.?\d/u.test(token)) {
      throw new Error(`Unexpected \`${token}\`.`);
    }
    return Number(token.replaceAll(',', ''));
  }

  // Read a power, which groups from the right, so `2 ^ 3 ^ 2` is `2 ^ 9`.
  function power(): number {
    const base = primary();
    if (tokens[position] !== '^') {
      return base;
    }
    position += 1;
    return base ** unary();
  }

  // Read a number with any signs before it, which apply after powers, so `-2 ^ 2` is `-4`.
  function unary(): number {
    if (tokens[position] === '-' || tokens[position] === '+') {
      const sign = tokens[position] === '-' ? -1 : 1;
      position += 1;
      return sign * unary();
    }
    return power();
  }

  // Read products and quotients.
  function product(): number {
    let value = unary();
    while (tokens[position] === '*' || tokens[position] === '/') {
      const operator = tokens[position];
      position += 1;
      value = operator === '*' ? value * unary() : value / unary();
    }
    return value;
  }

  // Read sums and differences.
  function sum(): number {
    let value = product();
    while (tokens[position] === '+' || tokens[position] === '-') {
      const operator = tokens[position];
      position += 1;
      value = operator === '+' ? value + product() : value - product();
    }
    return value;
  }

  // Read the whole expression, which must be finite, as when nothing is divided by zero.
  const value = sum();
  if (position < tokens.length) {
    throw new Error(`Unexpected \`${tokens[position]}\`.`);
  }
  if (!Number.isFinite(value)) {
    throw new Error("The result isn't a finite number.");
  }
  return value;
}

// Evaluate the arithmetic on a line, reporting a problem with it and stopping if it's invalid. The
// result is rounded to 12 significant digits to hide floating-point noise.
function result(expression: string, index: number): number {
  try {
    return Number(evaluate(expression).toPrecision(12));
  } catch (error) {
    return fail(`Line ${index + 1}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

// Read the lines.
const lines = readFileSync(0, 'utf8').split('\n');
if (lines.at(-1) === '') {
  lines.pop();
}

// In a divided block, write just the result of each line's arithmetic, before any `=`, keeping
// blank lines blank.
if (process.env.MULL_DIVIDED === '1') {
  for (const [index, line] of lines.entries()) {
    const expression = line.split('=')[0].trim();
    process.stdout.write(expression === '' ? '\n' : `${result(expression, index)}\n`);
  }
  process.exit(0);
}

// Otherwise, evaluate a block of a single line without `=` as if it ended with one, so a quick
// calculation needn't end with `=`.
if (lines.length === 1 && !lines[0].includes('=')) {
  lines[0] += ' =';
}

// Replace what follows the first `=` on each line with the value of the arithmetic before it, and
// keep other lines as written.
for (const [index, line] of lines.entries()) {
  const equals = line.indexOf('=');
  if (equals === -1) {
    process.stdout.write(`${line}\n`);
    continue;
  }
  const expression = line.slice(0, equals).trimEnd();
  process.stdout.write(`${expression} = ${result(expression, index)}\n`);
}
