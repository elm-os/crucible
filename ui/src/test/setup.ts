import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";
import { setAuth } from "@/lib/auth";

// UI SDK reads this global by name; strict mode turns its development warnings into failures.
declare global {
  // oxlint-disable-next-line no-underscore-dangle
  var __UI_SDK_STRICT__: boolean | undefined;
}
// oxlint-disable-next-line no-underscore-dangle
globalThis.__UI_SDK_STRICT__ = true;

// jsdom lacks these, or has them without the parts UI SDK relies on.
HTMLElement.prototype.showPopover = () => {};
HTMLElement.prototype.hidePopover = () => {};
Element.prototype.getAnimations = () => [];
Element.prototype.scrollIntoView = () => {};
Element.prototype.checkVisibility = () => true;
Element.prototype.getClientRects = () => [] as unknown as DOMRectList;
document.elementFromPoint = () => null;
document.elementsFromPoint = () => [];
globalThis.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
};
document.head.append(
  Object.assign(document.createElement("style"), {
    textContent: "[popover] { display: block !important }",
  }),
);

// Suites run against a server without a control plane unless they set a session or test the check.
setAuth({ mode: "keys" });

afterEach(() => {
  cleanup();
  localStorage.clear();
  sessionStorage.clear();
  window.history.replaceState(null, "", "/");
  setAuth({ mode: "keys" });
});
