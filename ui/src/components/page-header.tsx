import type { ReactNode } from "react";

export function PageHeader({ title, description }: { title: string; description?: ReactNode }) {
  return (
    <div className="mb-6 flex flex-col gap-1">
      <h1 className="text-heading-sm font-medium">{title}</h1>
      {description ? <p className="text-data text-ink-soft">{description}</p> : null}
    </div>
  );
}
