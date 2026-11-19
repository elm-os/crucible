import { QueryClientProvider } from "@tanstack/react-query";
import { render } from "@testing-library/react";
import type { ReactNode } from "react";
import type { Meta } from "@/api/generated/Meta";
import type { SessionItem } from "@/api/generated/SessionItem";
import type { SpanListItem } from "@/api/generated/SpanListItem";
import type { UserItem } from "@/api/generated/UserItem";
import { createQueryClient } from "@/components/providers";

export const AS_OF = "2026-11-17T14:00:05.000000Z";
export const WHOLE: Meta = { as_of: AS_OF, partial: false, consistency: "strong" };
export const LAGGING: Meta = { as_of: AS_OF, partial: false, consistency: "lagging" };

// Auto-retry counts are covered in errors.test.ts; here every failure lands at once.
export function renderWithClient(ui: ReactNode) {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(<QueryClientProvider client={client}>{ui}</QueryClientProvider>);
}

export const json = (body: unknown, status = 200, type = "application/json") =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": type } });

export const problem = (status: number, code: string) =>
  json(
    { code, title: "raw server title", request_id: "req-1" },
    status,
    "application/problem+json",
  );

export function page<T>(data: T[], fields: { next_cursor?: string | null; meta?: Meta } = {}) {
  return {
    data,
    next_cursor: fields.next_cursor ?? null,
    has_more: (fields.next_cursor ?? null) !== null,
    meta: fields.meta ?? WHOLE,
  };
}

export function session(fields: Partial<SessionItem> = {}): SessionItem {
  return {
    session_id: "chat-42",
    first_seen: "2026-11-17T13:00:00.000000Z",
    last_seen: "2026-11-17T13:30:00.000000Z",
    trace_count: 3,
    span_count: 21,
    error_count: 0,
    total_cost_usd: 0.012,
    input_tokens: 900,
    output_tokens: 300,
    total_tokens: 1_200,
    user_ids: ["ada"],
    saturated: false,
    ...fields,
  };
}

export function user(fields: Partial<UserItem> = {}): UserItem {
  return {
    user_id: "ada",
    first_seen: "2026-11-17T12:00:00.000000Z",
    last_seen: "2026-11-17T13:30:00.000000Z",
    trace_count: 5,
    session_count: 2,
    span_count: 40,
    error_count: 1,
    total_cost_usd: 0.03,
    input_tokens: 2_000,
    output_tokens: 500,
    total_tokens: 2_500,
    saturated: false,
    ...fields,
  };
}

export function spanItem(fields: Partial<SpanListItem> = {}): SpanListItem {
  return {
    trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
    span_id: "a000000000000002",
    parent_span_id: "a000000000000001",
    name: "plan reply",
    observation_type: "generation",
    start_time: "2026-11-17T14:00:00.000000Z",
    duration_us: 900_000,
    status_code: "ok",
    level: "default",
    model: "gpt-4.1",
    environment: "production",
    user_id: "ada",
    session_id: "chat-42",
    total_cost_usd: 0.0031,
    total_tokens: 1_380,
    saturated: false,
    ...fields,
  };
}

// The request URLs a fetch mock saw, parsed.
export function requests(calls: unknown[][]): URL[] {
  return calls.map((call) => new URL(String(call[0]), "http://localhost"));
}
