"use client";

import { Button } from "@ui-sdk/react/button";
import { useState } from "react";
import type { Message, MessagePart } from "./payload-model";

// Text longer than this shows its start until the reader asks for all of it.
const CLAMP = 2_000;

const ROLE_LABEL: Record<string, string> = {
  system: "System",
  developer: "Developer",
  user: "User",
  assistant: "Assistant",
  tool: "Tool",
};

function Clamped({ text, mono }: { text: string; mono?: boolean }) {
  const [all, setAll] = useState(false);
  const long = text.length > CLAMP;
  return (
    <div className="flex flex-col items-start gap-1">
      <p
        className={`max-w-full text-data break-words whitespace-pre-wrap ${mono ? "font-mono" : ""}`}
      >
        {long && !all ? `${text.slice(0, CLAMP)}…` : text}
      </p>
      {long && !all ? (
        <Button onClick={() => setAll(true)}>
          Show all {text.length.toLocaleString("en-US")} characters
        </Button>
      ) : null}
    </div>
  );
}

function Part({ part }: { part: MessagePart }) {
  switch (part.kind) {
    case "text":
      return <Clamped text={part.text} />;
    case "tool_call":
      return (
        <div className="flex flex-col gap-1 rounded-md border border-ash-border px-3 py-2">
          <p className="text-label text-ink-soft">
            Calls tool <span className="font-mono text-midnight-ink">{part.name}</span>
          </p>
          <Clamped text={part.text} mono />
        </div>
      );
    case "tool_result":
      return (
        <div className="flex flex-col gap-1 rounded-md border border-ash-border px-3 py-2">
          <p className="text-label text-ink-soft">Tool result</p>
          <Clamped text={part.text} mono />
        </div>
      );
    default:
      return <Clamped text={part.text} mono />;
  }
}

// A conversation: one block per message, labelled by its role, with its text and tool calls.
export function MessageList({ label, messages }: { label: string; messages: Message[] }) {
  return (
    <ol aria-label={label} className="flex flex-col gap-3">
      {messages.map((message) => (
        <li
          key={message.id}
          className="flex flex-col gap-1.5 rounded-md border border-ash-border bg-parchment-white px-3 py-2"
        >
          <p className="text-label font-medium text-ink-soft">
            {ROLE_LABEL[message.role] ?? message.role}
          </p>
          {message.parts.length === 0 ? (
            <p className="text-data text-ink-soft">No content.</p>
          ) : (
            message.parts.map((part, at) => (
              // Parts never move within a message.
              <Part key={`${message.id}-${at}`} part={part} />
            ))
          )}
        </li>
      ))}
    </ol>
  );
}
