import { defineConfig } from 'vite-plus';

export default defineConfig({
  fmt: {
    ignorePatterns: ['**/*.md'],
    singleQuote: true,
  },
  lint: {
    categories: {
      correctness: 'deny',
      perf: 'deny',
      restriction: 'deny',
      suspicious: 'deny',
    },
    jsPlugins: [
      {
        name: 'vite-plus',
        specifier: 'vite-plus/oxlint-plugin',
      },
    ],
    options: {
      typeAware: true,
      typeCheck: true,
    },
    plugins: ['oxc', 'typescript', 'unicorn'],
    rules: {
      'no-undefined': 'allow',
      'no-use-before-define': ['deny', { functions: false }],
    },
  },
  // Build each script on its own into the example wiki's scripts directory, as an executable which
  // keeps its `#!` line.
  pack: {
    entry: ['src/calc.ts', 'src/chrono.ts', 'src/sort.ts'],
    outDir: '../example_files/scripts',
    format: 'esm',
    platform: 'node',
    target: 'node24',
    fixedExtension: false,
    dts: false,
  },
});
