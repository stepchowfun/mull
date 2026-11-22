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
// The arithmetic can use numbers with decimals and commas between their digits, like `2,400.50`,
// scientific notation like `1e-3`, as well as `+`, `-`, `*`, `/`, `^` for powers, and parentheses. Results are rounded to 12
// significant digits. Percentages are fractions (`10%` is `0.1`). Currency symbols prefix amounts
// (`$100 * 10% = $10`), and named units follow numbers (`5 kg + 2 kg = 7 kg`). Units combine
// and cancel in products and quotients (`10 m / 2 s = 5 m * s^-1`). Powers apply to unit suffixes
// (`3 m^2`); use parentheses to raise an entire quantity (`(3 m)^2`). Addition and subtraction
// require matching units. Unit labels are case-sensitive and do not perform conversions.

import { readFileSync } from 'node:fs';

// Report a problem with the input and stop, so Mull leaves the block as it was.
function fail(message: string): never {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

// Track each unit's exponent so multiplication and division can combine and cancel units.
type Quantity = { amount: number; units: Map<string, number> };

// Reject oversized dimensions before rounding can corrupt later unit cancellation.
function checkedExponent(exponent: number): number {
  if (!Number.isSafeInteger(exponent)) {
    throw new Error('The unit exponent is too large.');
  }
  return exponent;
}

// Combine the dimensions of a product or quotient, dropping canceled units.
function multiply(left: Quantity, right: Quantity, divide = false): Quantity {
  const units = new Map(left.units);
  for (const [unit, exponent] of right.units) {
    const combined = checkedExponent((units.get(unit) ?? 0) + (divide ? -exponent : exponent));
    if (combined === 0) {
      units.delete(unit);
    } else {
      units.set(unit, combined);
    }
  }
  return { amount: divide ? left.amount / right.amount : left.amount * right.amount, units };
}

// Evaluate arithmetic while preserving dimensions and treating percentages as fractions.
function evaluate(expression: string): Quantity {
  const tokens =
    expression.match(/(?:\d[\d,]*(?:\.\d+)?|\.\d+)(?:[eE][+-]?\d+)?|[\p{L}_][\p{L}\p{N}_]*|\S/gu) ??
    [];
  let position = 0;

  // Read a number, a unit, a currency amount, or a parenthesized expression.
  function primary(): Quantity {
    const token = tokens[position];
    position += 1;
    let value: Quantity;
    if (token === undefined) {
      throw new Error('Expected a number.');
    } else if (token === '(') {
      value = sum();
      if (tokens[position] !== ')') {
        throw new Error('Expected `)`.');
      }
      position += 1;
    } else if (/^\.?\d/u.test(token)) {
      value = { amount: Number(token.replaceAll(',', '')), units: new Map() };
    } else if (/^[\p{L}\p{Sc}_]/u.test(token)) {
      value = { amount: 1, units: new Map([[token, 1]]) };
      if (
        /^\p{Sc}$/u.test(token) &&
        (tokens[position] === '(' || /^\.?\d/u.test(tokens[position] ?? ''))
      ) {
        value = multiply(value, primary());
      }
    } else {
      throw new Error(`Unexpected \`${token}\`.`);
    }

    // A postfix percent scales any primary expression, including a parenthesized sum.
    while (tokens[position] === '%') {
      value.amount /= 100;
      position += 1;
    }
    return value;
  }

  // Read right-associative powers; dimensions require a finite integer exponent.
  function power(): Quantity {
    const base = primary();
    if (tokens[position] !== '^') {
      return base;
    }
    position += 1;
    const exponent = unary();
    if (exponent.units.size !== 0) {
      throw new Error('An exponent must be unitless.');
    }
    if (base.units.size !== 0 && !Number.isSafeInteger(exponent.amount)) {
      throw new Error('A quantity with units requires a safe integer exponent.');
    }
    const units = new Map<string, number>();
    for (const [unit, dimension] of base.units) {
      if (exponent.amount !== 0) {
        units.set(unit, checkedExponent(dimension * exponent.amount));
      }
    }
    return { amount: base.amount ** exponent.amount, units };
  }

  // Apply signs after powers, so `-2 ^ 2` remains `-4`.
  function unary(): Quantity {
    if (tokens[position] === '-' || tokens[position] === '+') {
      const sign = tokens[position] === '-' ? -1 : 1;
      position += 1;
      const value = unary();
      return { amount: sign * value.amount, units: value.units };
    }
    return power();
  }

  // Accept a unit suffix as multiplication, with powers binding to the unit: `3 m^2`.
  function quantity(): Quantity {
    let value = unary();
    while (tokens[position] !== undefined && /^[\p{L}\p{Sc}_]/u.test(tokens[position])) {
      value = multiply(value, power());
    }
    return value;
  }

  // Read products and quotients, treating each number and its suffix as one quantity.
  function product(): Quantity {
    let value = quantity();
    while (tokens[position] === '*' || tokens[position] === '/') {
      const operator = tokens[position];
      position += 1;
      value = multiply(value, quantity(), operator === '/');
    }
    return value;
  }

  // Only quantities with matching dimensions can be added or subtracted.
  function sum(): Quantity {
    let value = product();
    while (tokens[position] === '+' || tokens[position] === '-') {
      const operator = tokens[position];
      position += 1;
      const right = product();
      if (
        value.units.size !== right.units.size ||
        [...value.units].some(([unit, exponent]) => right.units.get(unit) !== exponent)
      ) {
        throw new Error('Cannot add or subtract quantities with different units.');
      }
      value = {
        amount: operator === '+' ? value.amount + right.amount : value.amount - right.amount,
        units: value.units,
      };
    }
    return value;
  }

  // Reject trailing input and nonfinite results, such as division by zero.
  const value = sum();
  if (position < tokens.length) {
    throw new Error(`Unexpected \`${tokens[position]}\`.`);
  }
  if (!Number.isFinite(value.amount)) {
    throw new Error("The result isn't a finite number.");
  }
  return value;
}

// Round away floating-point noise and display simple currencies with their symbol first.
function format(value: Quantity): string {
  const amount = Number(value.amount.toPrecision(12));
  const dimensions = [...value.units].toSorted(([left], [right]) => left.localeCompare(right));
  if (dimensions.length === 1 && /^\p{Sc}$/u.test(dimensions[0][0]) && dimensions[0][1] === 1) {
    return `${amount < 0 ? '-' : ''}${dimensions[0][0]}${Math.abs(amount)}`;
  }
  const suffix = dimensions
    .map(([unit, exponent]) => `${unit}${exponent === 1 ? '' : `^${exponent}`}`)
    .join(' * ');
  return `${amount}${suffix ? ` ${suffix}` : ''}`;
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
    process.stdout.write(`${expression} = ${format(evaluate(expression))}\n`);
  } catch (error) {
    fail(`Line ${index + 1}: ${error instanceof Error ? error.message : String(error)}`);
  }
}
