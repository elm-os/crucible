import type { ReactNode } from "react";

// The middle of a dialog: when the dialog reaches the viewport's height it scrolls, and the title above and the
// actions below stay in view. The 4px inset keeps the fields' focus rings inside the scroll box.
export function DialogBody({
  className = "gap-4",
  children,
}: {
  className?: string;
  children: ReactNode;
}) {
  return (
    <div
      className={`-m-1 flex min-h-0 flex-col overflow-y-auto overscroll-contain p-1 ${className}`}
    >
      {children}
    </div>
  );
}
