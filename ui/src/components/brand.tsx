import type { SVGProps } from "react";

// The brand mark, shared with rightmodeler: two bars, the left one full height, the right one two-thirds. The viewBox
// is the mark's own bounding box. Decorative: it always sits beside the wordmark, which carries the name.
export function LogoMark({ height = 18, ...props }: SVGProps<SVGSVGElement> & { height?: number }) {
  return (
    <svg
      height={height}
      width={(height * 810) / 1007}
      viewBox="0 0 810 1007"
      fill="currentColor"
      aria-hidden="true"
      focusable="false"
      {...props}
    >
      <rect x="0" y="0" width="381" height="1007" />
      <rect x="444" y="352" width="366" height="655" />
    </svg>
  );
}

/** The mark and the wordmark together, set as rightmodeler's navigation sets them. */
export function Brand({ height = 18 }: { height?: number }) {
  return (
    <span className="inline-flex items-center gap-2 text-midnight-ink">
      <LogoMark height={height} />
      <span className="wordmark">Crucible</span>
    </span>
  );
}
