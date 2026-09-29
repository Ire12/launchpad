import { defineConfig, globalIgnores } from "eslint/config";
import nextVitals from "eslint-config-next/core-web-vitals";
import nextTs from "eslint-config-next/typescript";
import reactHooks from "eslint-plugin-react-hooks";

const eslintConfig = defineConfig([
  ...nextVitals,
  ...nextTs,
  {
    plugins: {
      "react-hooks": reactHooks,
    },
    // React Compiler rules are disabled repo-wide without comment (#413).
    // These rules catch real defects (setState in effects, broken memoization,
    // incompatible libraries). All hits have been fixed or documented with
    // inline disables below, so the rules are enabled as errors.
    rules: {
      "react-hooks/set-state-in-effect": "error",
      "react-hooks/refs": "error",
      "react-hooks/static-components": "error",
      "react-hooks/preserve-manual-memoization": "error",
      "react-hooks/incompatible-library": "error",
      "no-console": ["error", { "allow": ["warn", "error"] }],
    },
  },
  {
    files: ["app/**/*.{ts,tsx}", "components/**/*.{ts,tsx}", "lib/**/*.{ts,tsx}"],
    rules: {
      // Enforce i18n: warn on JSX string literals in app/ and components/
      "react/jsx-no-literals": ["warn", {
        "noStrings": true,
        "allowedStrings": ["", "/", ":", ".", "…", " ", "-", "#", "%", "(", ")", "{", "}", "C", "G"]
      }],
    },
  },
  {
    // Prevent hand-rolled token-amount scaling. Every amount conversion must
    // go through toBaseUnits / fromBaseUnits from @/lib/utils.
    // This is the fourth wave of this bug (#29, #54, #83, #395, now here).
    files: ["app/**/*.{ts,tsx}", "components/**/*.{ts,tsx}"],
    plugins: {
      "no-restricted-syntax-plugin": {
        rules: {
          "no-hardcoded-scaling": {
            create(context) {
              const SCALING_RE = /(?:1e7|10\s*\*\*\s*\w)/;
              return {
                Literal(node) {
                  if (typeof node.value === "number" && node.value === 1e7) {
                    context.report({
                      node,
                      message:
                        "Hard-coded 1e7 scaling ignores token decimals. Use toBaseUnits(amount, decimals) from @/lib/utils instead.",
                    });
                  }
                },
                BinaryExpression(node) {
                  if (node.operator === "**") {
                    context.report({
                      node,
                      message:
                        "Hand-rolled 10 ** decimals scaling is error-prone. Use toBaseUnits(amount, decimals) or fromBaseUnits(raw, decimals) from @/lib/utils instead.",
                    });
                  }
                },
              };
            },
          },
        },
      },
    },
    rules: {
      "no-restricted-syntax-plugin/no-hardcoded-scaling": "error",
    },
  },
  {
    files: ["**/__tests__/**", "**/*.test.*", "**/*.spec.*", "**/tests/**"],
    rules: {
      "react/jsx-no-literals": "off",
    },
  },
  // Override default ignores of eslint-config-next.
  globalIgnores([
    // Default ignores of eslint-config-next:
    ".next/**",
    "out/**",
    "build/**",
    "next-env.d.ts",
    "coverage/**",
  ]),
]);

export default eslintConfig;
