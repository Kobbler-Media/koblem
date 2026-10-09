const js = require('@eslint/js');
const tsParser = require('@typescript-eslint/parser');
const tsPlugin = require('@typescript-eslint/eslint-plugin');
const svelte = require('eslint-plugin-svelte');
const svelteParser = require('svelte-eslint-parser');
const prettier = require('eslint-config-prettier');
const globals = require('globals');

module.exports = [
  {
    ignores: ['.svelte-kit/**', 'dist/**', 'build/**']
  },
  js.configs.recommended,
  {
    files: ['**/*.{js,ts}'],
    ignores: ['**/*.svelte.ts'],
    languageOptions: {
      parser: tsParser,
      ecmaVersion: 'latest',
      sourceType: 'module',
      globals: {
        ...globals.browser,
        ...globals.node
      }
    },
    plugins: {
      '@typescript-eslint': tsPlugin
    },
    rules: {
      ...tsPlugin.configs.recommended.rules,
      // tsc already rejects unknown identifiers, and `no-undef` only knows runtime globals — it
      // reports every type-only DOM name (`CanvasImageSource`, `GPUDevice`, …) as undefined.
      'no-undef': 'off'
    }
  },
  {
    files: ['**/*.svelte.ts'],
    languageOptions: {
      parser: svelteParser,
      parserOptions: {
        parser: tsParser
      },
      globals: {
        ...globals.browser,
        $state: 'readonly',
        $derived: 'readonly',
        $effect: 'readonly',
        $props: 'readonly',
        $bindable: 'readonly'
      }
    },
    plugins: {
      '@typescript-eslint': tsPlugin
    },
    rules: {
      ...tsPlugin.configs.recommended.rules,
      'no-undef': 'off'
    }
  },
  ...svelte.configs.recommended,
  {
    files: ['**/*.svelte'],
    languageOptions: {
      parser: svelteParser,
      parserOptions: {
        parser: tsParser,
        extraFileExtensions: ['.svelte']
      },
      globals: {
        ...globals.browser
      }
    },
    plugins: {
      '@typescript-eslint': tsPlugin
    },
    rules: {
      'svelte/prefer-svelte-reactivity': 'off',
      'svelte/no-navigation-without-resolve': 'off',
      'no-useless-assignment': 'off',
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': [
        'error',
        { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }
      ]
    }
  },
  prettier
];
