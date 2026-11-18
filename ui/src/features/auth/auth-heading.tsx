"use client";

import { type ReactNode, useEffect, useRef } from "react";

// `focus` moves focus to the heading as it mounts, for a result that replaces the form that asked
// for it, so screen readers read the result and focus isn't lost with the form.
export function AuthHeading({
  title,
  focus = false,
  children,
}: {
  title: string;
  focus?: boolean;
  children?: ReactNode;
}) {
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    if (focus) heading.current?.focus();
  }, [focus]);
  return (
    <div className="flex flex-col gap-2">
      <h1
        ref={heading}
        tabIndex={focus ? -1 : undefined}
        className="font-display text-heading outline-none"
      >
        {title}
      </h1>
      {children ? <p className="text-body text-ink-soft">{children}</p> : null}
    </div>
  );
}
