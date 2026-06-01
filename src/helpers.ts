/**
 * Compile-time exhaustiveness check. Accepts only `never`, so TypeScript
 * emits an error if the value could still be a valid type (i.e., a case
 * was not handled).
 */
export function never(value: never): never {
  throw new Error(`Unexpected value: ${safeToString(value)}`);
}

function safeToString(value: unknown) {
  try {
    return JSON.stringify(value);
  } catch {}

  try {
    return `${value}`;
  } catch {}

  return '(string conversion failed)';
}
