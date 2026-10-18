#!/usr/bin/env node

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
    if (!/^[\d.]/u.test(token)) {
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

// Read the lines. A block of a single line without `=` is evaluated as if it ended with one, so a
// quick calculation needn't end with `=`.
const lines = readFileSync(0, 'utf8').split('\n');
if (lines.at(-1) === '') {
  lines.pop();
}
if (lines.length === 1 && !lines[0].includes('=')) {
  lines[0] += ' =';
}

// Replace what follows the first `=` on each line with the value of the arithmetic before it,
// rounded to 12 significant digits to hide floating-point noise, and keep other lines as written.
for (const [index, line] of lines.entries()) {
  const equals = line.indexOf('=');
  if (equals === -1) {
    process.stdout.write(`${line}\n`);
    continue;
  }
  const expression = line.slice(0, equals).trimEnd();
  try {
    process.stdout.write(`${expression} = ${Number(evaluate(expression).toPrecision(12))}\n`);
  } catch (error) {
    fail(`Line ${index + 1}: ${error instanceof Error ? error.message : String(error)}`);
  }
}
