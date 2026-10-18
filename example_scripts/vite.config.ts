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
    // Type-check with `tsc` instead, since only it applies tsconfig.json to the scripts outside this
    // directory.
    options: {
      typeAware: true,
      typeCheck: false,
    },
    plugins: ['oxc', 'typescript', 'unicorn'],
    rules: {
      'no-undefined': 'allow',
      'no-use-before-define': ['deny', { functions: false }],
    },
  },
});
