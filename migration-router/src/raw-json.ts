// Slices of a JSON text, kept as the exact text they were served in. Notaries
// sign the msgpack bytes of a payload whose `carryover` is an ordered map, and a
// JavaScript parse and stringify reorders integer-like keys, so the router
// forwards what the daemons wrote rather than what it parsed.

/** The raw text of each top-level member of a JSON object text, or `undefined`
 * when `text` is not a JSON object. */
export function rawMembers(text: string): Map<string, string> | undefined {
  if (!isJson(text)) return undefined;
  let i = skipSpace(text, 0);
  if (text[i] !== "{") return undefined;
  const members = new Map<string, string>();
  i = skipSpace(text, i + 1);
  while (text[i] !== "}") {
    const keyEnd = valueEnd(text, i);
    const key = JSON.parse(text.slice(i, keyEnd)) as string;
    const start = skipSpace(text, skipSpace(text, keyEnd) + 1);
    const end = valueEnd(text, start);
    members.set(key, text.slice(start, end));
    i = skipSpace(text, end);
    if (text[i] === ",") i = skipSpace(text, i + 1);
  }
  return members;
}

/** The raw text of each element of a JSON array text, or `undefined` when
 * `text` is not a JSON array. */
export function rawElements(text: string): string[] | undefined {
  if (!isJson(text)) return undefined;
  let i = skipSpace(text, 0);
  if (text[i] !== "[") return undefined;
  const elements: string[] = [];
  i = skipSpace(text, i + 1);
  while (text[i] !== "]") {
    const end = valueEnd(text, i);
    elements.push(text.slice(i, end));
    i = skipSpace(text, end);
    if (text[i] === ",") i = skipSpace(text, i + 1);
  }
  return elements;
}

function isJson(text: string): boolean {
  try {
    JSON.parse(text);
    return true;
  } catch {
    return false;
  }
}

function skipSpace(text: string, i: number): number {
  while (i < text.length && " \t\n\r".includes(text[i])) i++;
  return i;
}

/** The index just past the JSON value starting at `i`, in text already known
 * to be valid JSON. */
function valueEnd(text: string, i: number): number {
  const c = text[i];
  if (c === '"') return stringEnd(text, i);
  if (c === "{" || c === "[") {
    let depth = 0;
    let j = i;
    while (j < text.length) {
      const d = text[j];
      if (d === '"') {
        j = stringEnd(text, j);
        continue;
      }
      if (d === "{" || d === "[") depth++;
      if (d === "}" || d === "]") depth--;
      j++;
      if (depth === 0) return j;
    }
    return j;
  }
  let j = i;
  while (j < text.length && !",}] \t\n\r".includes(text[j])) j++;
  return j;
}

function stringEnd(text: string, i: number): number {
  let j = i + 1;
  while (j < text.length && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
  return j + 1;
}
