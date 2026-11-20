import type { ReactNode } from "react";

// first-use: nothing exists yet, so invite the first action. filtered: data may exist outside the
// current range or filters, so offer to widen them. incomplete: the server said data may be missing
// or not caught up yet, so offer to check again. unavailable: this server or this role can't use it,
// so say which setting or person changes that.
export type EmptyKind = "first-use" | "filtered" | "incomplete" | "unavailable";

export function EmptyState({
  kind,
  title,
  children,
  action,
}: {
  kind: EmptyKind;
  title: string;
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div data-empty={kind} className="flex flex-col items-start gap-2 py-8">
      <p className="text-body font-medium">{title}</p>
      {children ? <div className="max-w-prose text-data text-ink-soft">{children}</div> : null}
      {action ? <div className="mt-2">{action}</div> : null}
    </div>
  );
}
