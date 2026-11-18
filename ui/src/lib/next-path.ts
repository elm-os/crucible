const CONTROL = /\p{Cc}/u;
const API = /^\/api(?:[/?#]|$)/;

// Where to go after signing in (contract 11.4): a path on this server that starts with one `/`, with
// no control character and nothing under /api/. Anything else becomes `/`.
export function safeNext(value: string | null | undefined): string {
  if (!value?.startsWith("/") || value.startsWith("//") || value.startsWith("/\\")) return "/";
  if (CONTROL.test(value) || API.test(value)) return "/";
  return value;
}

export function loginHref(next: string): string {
  const path = safeNext(next);
  return path === "/" ? "/login/" : `/login/?${new URLSearchParams({ next: path })}`;
}

export function currentPath(): string {
  return `${window.location.pathname}${window.location.search}`;
}
