// The smallest single edit between two texts, independent of VS Code.

/** The single replacement turning `current` into `target`. */
export function minimalReplacement(
  current: string,
  target: string,
): { start: number; end: number; text: string } {
  let start = 0;
  const limit = Math.min(current.length, target.length);
  while (start < limit && current.charCodeAt(start) === target.charCodeAt(start)) {
    start++;
  }
  let suffix = 0;
  while (
    suffix < limit - start &&
    current.charCodeAt(current.length - 1 - suffix) === target.charCodeAt(target.length - 1 - suffix)
  ) {
    suffix++;
  }
  return { start, end: current.length - suffix, text: target.slice(start, target.length - suffix) };
}
