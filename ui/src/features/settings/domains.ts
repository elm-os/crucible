// Email domains as people write them: separated by commas, semicolons, spaces or new lines, with or
// without a leading @, in any case. An entry holding another @ or a slash isn't a domain, which is
// what the server refuses too (contract 11.5).
export function parseDomains(text: string): string[] | "invalid" {
  const domains: string[] = [];
  for (const entry of text.split(/[\s,;]+/)) {
    if (entry === "") continue;
    const domain = entry.replace(/^@+/, "").toLowerCase();
    if (domain === "" || /[@/]/.test(domain)) return "invalid";
    if (!domains.includes(domain)) domains.push(domain);
  }
  return domains;
}

export const sameDomains = (a: readonly string[], b: readonly string[]) =>
  a.length === b.length && a.every((domain, i) => domain === b[i]);
