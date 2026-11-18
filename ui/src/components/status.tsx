import type { Meta } from "@/api/generated/Meta";
import type { StatusCode } from "@/api/generated/StatusCode";
import { AlertIcon, CheckIcon, UnsetIcon } from "./icons";

const ICONS = { error: AlertIcon, ok: CheckIcon, unset: UnsetIcon } as const;

// Status is never carried by color: each state has its own icon, and the label says it in words.
export function StatusLabel({ status, label }: { status: StatusCode; label: string }) {
  const Icon = ICONS[status];
  return (
    <span data-status={status} className="flex items-center gap-1.5">
      <Icon className="shrink-0" />
      {label}
    </span>
  );
}

// A response can be whole, or say that rows may be missing (the store didn't answer) or that the newest
// rows may not be shown yet (ingest is catching up).
export function MetaNotice({ meta, noun }: { meta: Meta; noun: string }) {
  let text: string | null = null;
  if (meta.partial) text = `Some ${noun} may be missing. Part of the data couldn't be read.`;
  else if (meta.consistency === "lagging") text = `The newest ${noun} may not be shown yet.`;
  if (text === null) return null;
  return (
    <p data-notice className="mb-3 flex items-center gap-1.5 text-label text-ink-soft">
      <AlertIcon className="shrink-0" />
      {text}
    </p>
  );
}
