// Exercise the script through stdin so parsing, formatting, and block behavior are tested together.
import assert from 'node:assert/strict';
import { spawnSync, type SpawnSyncReturns } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

// Run the calculator as a user would, preserving its exit status and output for assertions.
function calculate(input: string): SpawnSyncReturns<string> {
  return spawnSync(process.execPath, [fileURLToPath(new URL('./calc.ts', import.meta.url))], {
    input,
    encoding: 'utf8',
  });
}

// Cover arithmetic precedence, percentages, dimensions, and currency formatting.
const checks: Promise<void>[] = [];
const examples = [
  ['36 - 2 * 0.75', '34.5'],
  ['(3 + 5) * 2 ^ 3', '64'],
  ['2 ^ 3 ^ 2', '512'],
  ['-2 ^ 2', '-4'],
  ['2 ^ -2', '0.25'],
  ['2,400.50 + .5', '2401'],
  ['1e3 + 2E+2', '1200'],
  ['1e-7 m', '1e-7 m'],
  ['$1e21 * 10%', '$100000000000000000000'],
  ['.5e2 kg', '50 kg'],
  ['$100 * 10%', '$10'],
  ['$100 * (1 + 10%)', '$110'],
  ['$10 - $20', '-$10'],
  ['€80 * 25%', '€20'],
  ['(5 + 5)%', '0.1'],
  ['5 kg + 2 kg', '7 kg'],
  ['10 m / 2 s', '5 m * s^-1'],
  ['10 m / 2 m', '5'],
  ['3 m^2', '3 m^2'],
  ['(3 m)^2', '9 m^2'],
  ['(3 m)^0', '1'],
  ['$10 / $2', '5'],
  ['$2 * $3', '6 $^2'],
  ['6 $^2', '6 $^2'],
  ['5 m * s^-1', '5 m * s^-1'],
];
for (const [expression, expected] of examples) {
  checks.push(
    test(expression, () => {
      const result = calculate(expression);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(result.stdout, `${expression} = ${expected}\n`);

      // Formatted results should also be valid inputs, including scientific notation and units.
      const repeated = calculate(expected);
      assert.equal(repeated.status, 0, repeated.stderr);
      assert.equal(repeated.stdout, `${expected} = ${expected}\n`);
    }),
  );
}

// Invalid dimensions and arithmetic must fail so Mull can preserve the original block.
for (const expression of [
  '1 m + 1 s',
  '$1 + €1',
  '1 + 2 kg',
  '2 ^ (1 m)',
  '(2 m)^.5',
  '1 / 0',
  '2 +',
  '(2',
  '2 @ 3',
  '(m^9007199254740991 * m^2) / m^9007199254740991',
  '(m^9007199254740991)^2 / m^9007199254740991',
]) {
  checks.push(
    test(`reject ${expression}`, () => {
      const result = calculate(expression);
      assert.equal(result.status, 1);
      assert.match(result.stderr, /^Line 1:/u);
    }),
  );
}

// Recalculate existing answers while preserving prose and lines without an equals sign.
await test('multiline block', () => {
  const result = calculate('Budget\n$100 * 10% = old answer\n5 kg + 2 kg =\n');
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, 'Budget\n$100 * 10% = $10\n5 kg + 2 kg = 7 kg\n');
});

// Wait for all registered cases to finish before the test module exits.
await Promise.all(checks);
