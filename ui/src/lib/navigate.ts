// Signing in or out loads a fresh page, so nothing cached for one person survives into the next.
export function loadPage(href: string): void {
  window.location.assign(href);
}
