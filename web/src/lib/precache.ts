export function precachePathnames(
  scope: string,
  lists: Array<Array<{ path: string }>>
): Set<string> {
  return new Set(lists.flat().map(({ path }) => new URL(path, scope).pathname));
}
