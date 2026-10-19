import js from '@eslint/js';
import vue from 'eslint-plugin-vue';
import globals from 'globals';
import tseslint from 'typescript-eslint';

export default tseslint.config(
  {
    ignores: ['**/dist/**', '**/node_modules/**', 'target/**', 'app/src-tauri/**'],
  },
  js.configs.recommended,
  ...tseslint.configs.strict,
  ...vue.configs['flat/recommended'],
  {
    files: ['**/*.{ts,vue}'],
    languageOptions: {
      globals: { ...globals.browser },
      parserOptions: {
        parser: tseslint.parser,
        extraFileExtensions: ['.vue'],
        sourceType: 'module',
      },
    },
    rules: {
      'vue/multi-word-component-names': 'off',
      '@typescript-eslint/consistent-type-imports': 'error',
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
      'no-restricted-imports': [
        'error',
        {
          paths: [{ name: '@varve/ui', message: 'Import a concrete module, for example @varve/ui/createVarveApp.' }],
          patterns: [{ group: ['**/index', '**/index.ts', '**/index.js'], message: 'Import the concrete file, not a barrel.' }],
        },
      ],
    },
  },
  {
    files: ['scripts/**/*.mjs', '**/*.config.{js,ts}'],
    languageOptions: { globals: { ...globals.node } },
  },
);
