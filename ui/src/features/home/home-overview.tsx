import { TodoIcon } from "@/components/icons";
import { SectionFrame } from "@/components/section";

const SETUP_STEPS = [
  { id: "api-key", title: "Create an API key", body: "Keys will be created in Settings." },
  {
    id: "first-trace",
    title: "Send your first trace",
    body: "Point an OpenTelemetry exporter at this server's OTLP endpoint.",
  },
  {
    id: "mcp-server",
    title: "Instrument an MCP server",
    body: "Wrap the server with a Crucible SDK, or run it behind the Crucible proxy.",
  },
  { id: "invite", title: "Invite your team", body: "Members will be managed in Settings." },
] as const;

const KPIS = [
  { id: "traces", title: "Traces" },
  { id: "error-rate", title: "Error rate" },
  { id: "latency", title: "p95 latency" },
  { id: "cost", title: "Cost" },
  { id: "tokens", title: "Tokens" },
] as const;

// Placeholders: the checklist will read data bounds and API keys, and each tile will run its own
// metrics query inside a Section, so one failing tile never blanks the others.
export function HomeOverview() {
  return (
    <div className="flex flex-col gap-6">
      <SectionFrame title="Set up">
        <ol className="flex flex-col">
          {SETUP_STEPS.map((step) => (
            <li
              key={step.id}
              className="flex items-start gap-3 border-t border-ash-border py-3 first:border-t-0"
            >
              <TodoIcon className="mt-0.5 shrink-0 text-ink-soft" />
              <div className="flex flex-col gap-0.5">
                <p className="text-data font-medium">
                  {step.title}
                  <span className="ms-2 text-label font-normal text-ink-soft">To do</span>
                </p>
                <p className="text-label text-ink-soft">{step.body}</p>
              </div>
            </li>
          ))}
        </ol>
      </SectionFrame>
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-5">
        {KPIS.map((kpi) => (
          <SectionFrame key={kpi.id} title={kpi.title}>
            <p className="text-heading-sm text-ink-soft">Planned</p>
            <p className="mt-1 text-label text-ink-soft">Will show once the metrics API ships.</p>
          </SectionFrame>
        ))}
      </div>
    </div>
  );
}
