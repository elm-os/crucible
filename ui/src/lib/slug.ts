// A project slug, as the server accepts it: 1 to 64 of a-z, 0-9 and `-`.
export const SLUG_PATTERN = "[a-z0-9\\-]{1,64}";

// The slug a project name suggests: lowercase, accents dropped from their letters, spaces as `-`,
// anything else left out.
export function slugFromName(name: string): string {
  return name
    .normalize("NFKD")
    .replace(/\p{M}/gu, "")
    .toLowerCase()
    .trim()
    .replace(/\s+/g, "-")
    .replace(/[^a-z0-9-]/g, "")
    .replace(/-{2,}/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64)
    .replace(/-+$/, "");
}
