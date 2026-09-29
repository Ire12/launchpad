/**
 * Utility function for combining classnames
 */
export function cn(...classes: (string | undefined | null | false)[]): string {
  return classes
    .filter((c) => c && typeof c === "string")
    .join(" ")
    .trim();
}

/**
 * Expand a `number` into a plain decimal string, without exponent notation.
 *
 * `String(1e21)` is `"\e+21"` and `String(1e-7)` is `"1e-7"`; neither can be
 * scaled by string surgery. This rewrites both into positional form. The
 * result is the double's shortest round-trip representation — i.e. exactly
 * what the user would see — not an approximation introduced here.
 */
function toDecimalString(value: number): string {
  if (!Number.isFinite(value)) return "";

  const text = String(value);
  const match = /^([+-]?)(\d+)(?:\.(\d+))?[eE]([+-]?\d+)$/.exec(text);
  if (!match) return text;

  const [, sign, intPart, fracPart = "", expPart] = match;
  const digits = intPart + fracPart;
  const pointIndex = intPart.length + Number(expPart);

  if (pointIndex <= 0) {
    return `${sign}0.${"0".repeat(-pointIndex)}${digits}`;
  }
  if (pointIndex >= digits.length) {
    return `${sign}${digits}${"0".repeat(pointIndex - digits.length)}`;
  }
  return `${sign}${digits.slice(0, pointIndex)}.${digits.slice(pointIndex)}`;
}

/**
 * Converts a display amount to base units based on decimals.
 *
 * This is the single authoritative scaling primitive for all token amount
 * conversions. Do NOT hand-roll `1e7`, `10 ** decimals`, or `parseFloat * N`
 * in app/ or components/ — import this instead.
 *
 * The scaling is done by moving the decimal point through string surgery and
 * then parsing once with `BigInt`. A string amount never passes through a
 * `Number`, because the multiply `amount * 10 ** decimals` is only exact
 * while `supply * 5 ** decimals < 2 ** 53` — a ceiling that collapses as
 * decimals rise (about 115 billion at 7 decimals, but only 2,361 at 18).
 * Wrapping a float result in `BigInt` cannot recover precision already lost:
 * 1,000,000 at 18 decimals came out 16,777,216 base units short (#395).
 *
 * Behaviour at the edges:
 * - An absent or unparseable amount yields `0n`, so preview and preflight
 *   paths that run against a half-filled form do not throw.
 * - A well-formed amount with more fraction digits than the token has
 *   decimals throws, rather than silently rounding. Rounding here would send
 *   an amount the user did not ask for, which is the same class of bug this
 *   function exists to prevent.
 *
 * Passing a `number` is supported for convenience, but any value beyond
 * `Number.MAX_SAFE_INTEGER` or with more than ~15 significant digits has
 * already lost precision before this function is called — pass a string.
 */
export function toBaseUnits(display: number | string, decimals: number): bigint {
  if (!Number.isInteger(decimals) || decimals < 0) {
    throw new RangeError(
      `toBaseUnits: decimals must be a non-negative integer, got ${decimals}`,
    );
  }

  const text =
    typeof display === "number" ? toDecimalString(display) : display.trim();

  // `^(sign)(whole)(.fraction)?$` — no exponent, no stray characters.
  const parsed = /^([+-]?)(\d*)(?:\.(\d*))?$/.exec(text);
  const whole = parsed?.[2] ?? "";
  const fraction = parsed?.[3] ?? "";
  if (!parsed || (whole === "" && fraction === "")) return 0n;

  if (fraction.length > decimals) {
    throw new RangeError(
      `toBaseUnits: "${text}" has ${fraction.length} decimal places but this token supports ${decimals}`,
    );
  }

  const magnitude = BigInt(`${whole || "0"}${fraction.padEnd(decimals, "0")}`);
  return parsed[1] === "-" ? -magnitude : magnitude;
}

/**
 * Converts a raw base-unit bigint back to a human-readable decimal string.
 *
 * This is the single authoritative inverse of `toBaseUnits`. Do NOT hand-roll
 * `Number(amount) / 10 ** decimals` in app/ or components/ — import this instead.
 */
export function fromBaseUnits(raw: bigint, decimals: number): string {
  if (decimals === 0) return raw.toString();
  const divisor = BigInt(10) ** BigInt(decimals);
  const whole = raw / divisor;
  const frac = raw % divisor;
  if (frac === 0n) return whole.toString();
  const fracStr = frac.toString().padStart(decimals, "0").replace(/0+$/, "");
  return `${whole}.${fracStr}`;
}

/**
 * Safely convert a display amount to base units, returning a discriminated
 * result instead of throwing. Wraps `toBaseUnits` so callers (form submit
 * handlers, render paths) can deal with the `RangeError` as a field-level
 * error rather than an uncaught exception.
 *
 * Returns `{ ok: true, value }` on success, or `{ ok: false, error }` where
 * `error` is the original `RangeError` message.
 */
export type ToBaseUnitsResult =
  | { ok: true; value: bigint }
  | { ok: false; error: string };

export function tryToBaseUnits(
  display: number | string,
  decimals: number,
): ToBaseUnitsResult {
  try {
    return { ok: true, value: toBaseUnits(display, decimals) };
  } catch (err) {
    if (err instanceof RangeError) {
      return { ok: false, error: err.message };
    }
    throw err;
  }
}
