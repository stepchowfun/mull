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
      'oxc/no-async-await': 'allow',
    },
  },
});
