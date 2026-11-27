import type { ReactNode } from "react";
import { LogoMark } from "@/components/brand";

// The sign-in pages' right panel: a still picture of Crucible on the brand's ink, made of static markup (no
// queries run before sign-in). It is decoration, hidden from assistive technology, and shown on wide screens only.

const CALLS = [4, 6, 5, 8, 7, 11, 9, 6, 10, 13, 9, 12, 15, 11, 9, 14];

const SPARK = [6, 8, 7, 11, 9, 12, 10, 14, 12, 16, 13, 15, 18, 15, 17, 21, 19, 22];

const SPANS: { type: string; name: string; start: number; width: number; duration: string }[] = [
  { type: "Agent", name: "support agent", start: 0, width: 1, duration: "5.0 s" },
  {
    type: "Retriever",
    name: "search knowledge base",
    start: 0.02,
    width: 0.05,
    duration: "240 ms",
  },
  { type: "Generation", name: "draft reply", start: 0.08, width: 0.3, duration: "1.5 s" },
  { type: "Tool", name: "tools/call lookup_order", start: 0.4, width: 0.09, duration: "460 ms" },
  { type: "Generation", name: "polish reply", start: 0.52, width: 0.44, duration: "2.2 s" },
];

export function AuthShowcase() {
  return (
    <aside
      aria-hidden="true"
      className="relative hidden min-h-0 flex-col overflow-hidden rounded-[20px] bg-midnight-ink text-parchment-white lg:flex"
    >
      <div className="showcase-grid absolute inset-0" />

      <div className="relative flex flex-1 items-center justify-center px-10 pt-12">
        <div className="relative h-[420px] w-full max-w-[560px]">
          <TracesCard />
          <TraceCard />
          <ToolCard />
        </div>
      </div>

      <div className="relative flex flex-col items-center gap-4 px-12 pt-8 pb-10 text-center">
        <span className="grid size-14 place-items-center rounded-2xl bg-parchment-white text-midnight-ink">
          <LogoMark height={26} />
        </span>
        <h2 className="max-w-[560px] font-display text-heading text-balance">
          See every agent run and tool call{" "}
          <span className="text-parchment-white/55">on your own servers</span>
        </h2>
        <p className="max-w-[480px] text-body text-parchment-white/70">
          Traces, costs and MCP analytics for AI agents and MCP servers, in one binary next to your
          ClickHouse and Postgres.
        </p>
        <div className="mt-6 grid w-full max-w-[560px] grid-cols-3 gap-2">
          <span className="h-1 rounded-full bg-parchment-white" />
          <span className="h-1 rounded-full bg-parchment-white/20" />
          <span className="h-1 rounded-full bg-parchment-white/20" />
        </div>
      </div>
    </aside>
  );
}

function Card({ className, children }: { className: string; children: ReactNode }) {
  return (
    <div
      className={`absolute rounded-xl border border-ash-border bg-parchment-white p-4 text-midnight-ink ${className}`}
    >
      {children}
    </div>
  );
}

function CardTitle({ title, meta }: { title: string; meta: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <p className="text-data font-medium">{title}</p>
      <p className="text-label text-ink-soft">{meta}</p>
    </div>
  );
}

function TracesCard() {
  const max = Math.max(...SPARK);
  const points = SPARK.map(
    (v, i) => `${(i / (SPARK.length - 1)) * 200},${40 - (v / max) * 36}`,
  ).join(" ");
  return (
    <Card className="top-0 left-0 w-[250px]">
      <CardTitle title="Traces" meta="Last 24 hours" />
      <p className="mt-3 text-heading-sm font-medium tabular-nums">1,284</p>
      <p className="text-label text-ink-soft">2.4% with errors</p>
      <svg viewBox="0 0 200 42" className="mt-3 h-10 w-full" preserveAspectRatio="none">
        <polygon
          points={`0,42 ${points} 200,42`}
          fill="var(--color-midnight-ink)"
          fillOpacity={0.08}
        />
        <polyline
          points={points}
          fill="none"
          stroke="var(--color-midnight-ink)"
          strokeWidth={1.5}
          strokeLinejoin="round"
          vectorEffect="non-scaling-stroke"
        />
      </svg>
    </Card>
  );
}

function TraceCard() {
  return (
    <Card className="top-[46px] right-0 w-[330px]">
      <CardTitle title="support conversation" meta="5 spans · 5.0 s" />
      <ul className="mt-3 flex flex-col gap-2">
        {SPANS.map((span) => (
          <li key={span.name} className="grid grid-cols-[1fr_92px] items-center gap-3">
            <p className="truncate text-label">
              <span className="text-ink-soft">{span.type}</span> {span.name}
            </p>
            <span className="relative h-2 rounded-full bg-midnight-ink/5">
              <span
                className="absolute inset-y-0 rounded-full bg-midnight-ink"
                style={{ left: `${span.start * 100}%`, width: `${Math.max(span.width * 100, 4)}%` }}
              />
            </span>
          </li>
        ))}
      </ul>
    </Card>
  );
}

function ToolCard() {
  return (
    <Card className="bottom-0 left-[56px] w-[300px]">
      <CardTitle title="lookup_order" meta="orders-mcp" />
      <dl className="mt-3 grid grid-cols-3 gap-2">
        {[
          ["Calls", "1,382"],
          ["p50", "541 ms"],
          ["p99", "868 ms"],
        ].map(([label, value]) => (
          <div key={label}>
            <dt className="text-label text-ink-soft">{label}</dt>
            <dd className="text-data font-medium tabular-nums">{value}</dd>
          </div>
        ))}
      </dl>
      <svg viewBox="0 0 240 64" className="mt-3 h-16 w-full">
        <line x1="0" x2="240" y1="63.5" y2="63.5" stroke="var(--color-ash-border)" />
        {CALLS.map((calls, i) => {
          const max = Math.max(...CALLS);
          const width = 10;
          const x = 2 + i * 15;
          const top = 63 - (calls / max) * 56;
          return (
            <path
              key={x}
              d={`M${x},63V${top + 3}Q${x},${top} ${x + 3},${top}H${x + width - 3}Q${x + width},${top} ${x + width},${top + 3}V63Z`}
              fill="var(--color-midnight-ink)"
            />
          );
        })}
      </svg>
    </Card>
  );
}
