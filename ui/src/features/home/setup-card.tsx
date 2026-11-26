"use client";

import { Button } from "@ui-sdk/react/button";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import type { ReactNode } from "react";
import { AlertIcon, CheckIcon, TodoIcon, UnsetIcon } from "@/components/icons";
import { SectionFrame, useLoadingPhase } from "@/components/section";
import { scopedHref } from "@/components/shell/nav";
import type { SetupStep, StepId, StepState } from "./home-setup";

const GUIDES = "https://github.com/elm-os/crucible/blob/main/docs/guides";

function Guide({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a href={href} target="_blank" rel="noreferrer">
      {children}
    </a>
  );
}

const STEPS: Record<StepId, { title: string; how: (search: URLSearchParams | null) => ReactNode }> =
  {
    "api-key": {
      title: "Create an API key",
      how: (search) => (
        <>
          Create one in Settings, under{" "}
          <Link href={scopedHref("/settings/api-keys", search)}>API keys</Link>.
        </>
      ),
    },
    "first-trace": {
      title: "Send your first trace",
      how: () => (
        <>
          Point an OpenTelemetry exporter at this server&apos;s OTLP endpoint.{" "}
          <Guide href={`${GUIDES}/sending-traces.md`}>How to send traces</Guide>
        </>
      ),
    },
    "mcp-server": {
      title: "Instrument an MCP server",
      how: () => (
        <>
          Wrap the server with a Crucible SDK, or run it behind the Crucible proxy.{" "}
          <Guide href={`${GUIDES}/mcp.md`}>How to instrument an MCP server</Guide>
        </>
      ),
    },
    invite: {
      title: "Invite your team",
      how: (search) => (
        <>
          Invite people in Settings, under{" "}
          <Link href={scopedHref("/settings/members", search)}>Members</Link>.
        </>
      ),
    },
  };

const LABEL: Partial<Record<StepState["status"], string>> = {
  todo: "To do",
  done: "Done",
  unknown: "Couldn't check",
};

export function SetupCard({ steps }: { steps: SetupStep[] }) {
  const search = useSearchParams();
  const checking = steps.some((step) => step.state.status === "checking");
  const phase = useLoadingPhase(checking);
  return (
    <SectionFrame title="Set up" busy={checking}>
      {checking ? (
        <output className="sr-only">{phase === "hidden" ? "" : "Checking the set-up steps"}</output>
      ) : null}
      <ol className="flex flex-col">
        {steps.map((step) => (
          <Step key={step.id} step={step} search={search} skeleton={phase !== "hidden"} />
        ))}
      </ol>
    </SectionFrame>
  );
}

function Step({
  step,
  search,
  skeleton,
}: {
  step: SetupStep;
  search: URLSearchParams | null;
  skeleton: boolean;
}) {
  const { title, how } = STEPS[step.id];
  const { state } = step;
  const label = LABEL[state.status];
  return (
    <li className="flex items-start gap-3 border-t border-ash-border py-3 first:border-t-0">
      <StepIcon state={state} skeleton={skeleton} />
      <div className="flex min-w-0 flex-col gap-0.5">
        <p className="text-data font-medium">
          {title}
          {label ? (
            <span className="ms-2 text-label font-normal text-ink-soft">{label}</span>
          ) : null}
        </p>
        {state.status === "done" ? null : (
          <p className="text-label text-ink-soft">
            {state.status === "unavailable" ? state.note : how(search)}
          </p>
        )}
        {state.status === "unknown" ? (
          <Unknown title={title} error={state.error} retry={step.retry} />
        ) : null}
      </div>
    </li>
  );
}

function StepIcon({ state, skeleton }: { state: StepState; skeleton: boolean }) {
  const className = "mt-0.5 shrink-0 text-ink-soft";
  switch (state.status) {
    case "done":
      return <CheckIcon className={className} />;
    case "todo":
      return <TodoIcon className={className} />;
    case "unknown":
      return <AlertIcon className={className} />;
    case "unavailable":
      return <UnsetIcon className={className} />;
    case "checking":
      return (
        <span
          aria-hidden="true"
          className={`mt-0.5 size-4 shrink-0 ${skeleton ? "skeleton" : "invisible"}`}
        />
      );
  }
}

function Unknown({
  title,
  error,
  retry,
}: {
  title: string;
  error: Extract<StepState, { status: "unknown" }>["error"];
  retry: () => Promise<unknown>;
}) {
  return (
    <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-label text-ink-soft">
      <span>{error ? error.title : "Some data may be missing from the answer."}</span>
      {error === null || error.action === "retry" ? (
        <Button aria-label={`Check again: ${title}`} onClick={retry}>
          Check again
        </Button>
      ) : null}
      {error?.requestId ? (
        <span>
          Reference <span className="font-mono select-all">{error.requestId}</span>
        </span>
      ) : null}
    </div>
  );
}
