import { type Connection, connectionKey } from "@/lib/connection";
import { fromHttpFailure, networkError, type UiError } from "./errors";
import type { FactWritten } from "./generated/FactWritten";
import type { FeedbackRequest } from "./generated/FeedbackRequest";
import type { FlagRequest } from "./generated/FlagRequest";
import type { MetricsQueryRequest } from "./generated/MetricsQueryRequest";
import type { MetricsResponse } from "./generated/MetricsResponse";
import type { PayloadField } from "./generated/PayloadField";
import type { PayloadParams } from "./generated/PayloadParams";
import type { SearchPage } from "./generated/SearchPage";
import type { SearchRequest } from "./generated/SearchRequest";
import type { SpanPage } from "./generated/SpanPage";
import type { SpanPageParams } from "./generated/SpanPageParams";
import type { TraceDetail } from "./generated/TraceDetail";
import type { TraceListPage } from "./generated/TraceListPage";
import type { TagRequest } from "./generated/TagRequest";
import type { TraceListParams } from "./generated/TraceListParams";
import type { ProblemBody, TimeWindow } from "./types";

const BASE = "/api/v1";

type Query = Record<string, string | number | undefined>;

export async function apiGet<T>(path: string, query: Query, signal?: AbortSignal): Promise<T> {
  const params = new URLSearchParams();
  for (const [name, value] of Object.entries(query)) {
    if (value !== undefined) params.set(name, String(value));
  }
  const search = params.size > 0 ? `?${params}` : "";
  let response: Response;
  try {
    response = await fetch(`${BASE}${path}${search}`, {
      signal,
      credentials: "same-origin",
      headers: { Accept: "application/json", ...authorization(connectionKey()) },
    });
  } catch (error) {
    if (signal?.aborted) throw error;
    throw networkError();
  }
  if (!response.ok) throw await failure(response);
  // A static host or a proxy can answer 200 with HTML; that is a failure, never an empty list.
  if (!isJson(response)) throw await failure(response, false);
  try {
    return (await response.json()) as T;
  } catch {
    throw await failure(response, false);
  }
}

// Writes carry an Idempotency-Key, so a retry of a request whose answer was lost writes nothing twice.
export async function apiPost<T>(
  path: string,
  body: unknown,
  options: { signal?: AbortSignal; idempotencyKey?: string } = {},
): Promise<T> {
  const headers: Record<string, string> = {
    Accept: "application/json",
    "Content-Type": "application/json",
    ...authorization(connectionKey()),
  };
  if (options.idempotencyKey) headers["Idempotency-Key"] = options.idempotencyKey;
  let response: Response;
  try {
    response = await fetch(`${BASE}${path}`, {
      method: "POST",
      signal: options.signal,
      credentials: "same-origin",
      headers,
      body: JSON.stringify(body),
    });
  } catch (error) {
    if (options.signal?.aborted) throw error;
    throw networkError();
  }
  if (!response.ok) throw await failure(response);
  if (!isJson(response)) throw await failure(response, false);
  try {
    return (await response.json()) as T;
  } catch {
    throw await failure(response, false);
  }
}

// Temporary (milestone 1): the Connect dialog's key stands in for a session.
function authorization(key: string | null): Record<string, string> {
  return key === null ? {} : { Authorization: `Bearer ${key}` };
}

export type ConnectionCheck = "ok" | "rejected" | "wrong_project" | "unreachable";

// Asks the server whether it accepts the key for the project. Any answer past authentication counts
// as accepted, including a store that is down: the sections then say so themselves.
export async function checkConnection(connection: Connection): Promise<ConnectionCheck> {
  try {
    const response = await fetch(`${BASE}${projectPath(connection.project, "/data-bounds")}`, {
      headers: { Accept: "application/json", ...authorization(connection.key) },
    });
    if (response.status === 401) return "rejected";
    if (response.status === 404) return "wrong_project";
    return "ok";
  } catch {
    return "unreachable";
  }
}

function projectPath(project: string, rest: string): string {
  return `/projects/${encodeURIComponent(project)}${rest}`;
}

export function listTraces(
  project: string,
  timeWindow: TimeWindow,
  signal?: AbortSignal,
): Promise<TraceListPage> {
  const query: TraceListParams = { from: timeWindow.from, to: timeWindow.to };
  return apiGet(projectPath(project, "/traces"), query, signal);
}

export function getTrace(
  project: string,
  traceId: string,
  signal?: AbortSignal,
): Promise<TraceDetail> {
  return apiGet(projectPath(project, `/traces/${encodeURIComponent(traceId)}`), {}, signal);
}

export function listSpanChildren(
  project: string,
  traceId: string,
  parentSpanId: string,
  cursor: string | null,
  signal?: AbortSignal,
): Promise<SpanPage> {
  const query: SpanPageParams = { parent_span_id: parentSpanId, cursor: cursor ?? undefined };
  return apiGet(
    projectPath(project, `/traces/${encodeURIComponent(traceId)}/spans`),
    query,
    signal,
  );
}

export function newIdempotencyKey(): string {
  return crypto.randomUUID();
}

export function writeTag(project: string, body: TagRequest, key: string): Promise<FactWritten> {
  return apiPost(projectPath(project, "/tags"), body, { idempotencyKey: key });
}

export function writeFlag(project: string, body: FlagRequest, key: string): Promise<FactWritten> {
  return apiPost(projectPath(project, "/flags"), body, { idempotencyKey: key });
}

export function writeFeedback(
  project: string,
  body: FeedbackRequest,
  key: string,
): Promise<FactWritten> {
  return apiPost(projectPath(project, "/feedback"), body, { idempotencyKey: key });
}

export function queryMetrics(
  project: string,
  body: MetricsQueryRequest,
  signal?: AbortSignal,
): Promise<MetricsResponse> {
  return apiPost(projectPath(project, "/metrics/query"), body, { signal });
}

async function send(url: string, init: RequestInit): Promise<Response> {
  try {
    return await fetch(url, { credentials: "same-origin", ...init });
  } catch (error) {
    if (init.signal?.aborted) throw error;
    throw networkError();
  }
}

export function searchSpans(
  project: string,
  request: SearchRequest,
  signal?: AbortSignal,
): Promise<SearchPage> {
  return apiPost(projectPath(project, "/search"), request, { signal });
}

export interface PayloadSpan {
  traceId: string;
  spanId: string;
  startTime: string;
}

export interface PayloadBytes {
  bytes: Uint8Array;
  // The length of the whole field.
  total: number;
}

// The field's length from `Content-Range: bytes <first>-<last>/<length>` or `bytes */<length>`.
function rangeTotal(response: Response): number | null {
  const match = /\/(\d+)\s*$/.exec(response.headers.get("content-range") ?? "");
  return match ? Number(match[1]) : null;
}

// Bytes `first` to `last` (inclusive; to the end when null) of one payload field. The server sends an
// empty field whole, and answers a range past the end with its length only.
export async function getPayloadRange(
  project: string,
  span: PayloadSpan,
  field: PayloadField,
  first: number,
  last: number | null,
  signal?: AbortSignal,
): Promise<PayloadBytes> {
  const query: PayloadParams = { field, start_time: span.startTime };
  const path = projectPath(
    project,
    `/spans/${encodeURIComponent(span.traceId)}/${encodeURIComponent(span.spanId)}/payload`,
  );
  const params = new URLSearchParams({ field: query.field, start_time: query.start_time });
  const response = await send(`${BASE}${path}?${params}`, {
    signal,
    headers: { Range: `bytes=${first}-${last ?? ""}`, ...authorization(connectionKey()) },
  });
  if (response.status === 416) {
    return { bytes: new Uint8Array(0), total: rangeTotal(response) ?? 0 };
  }
  if (!response.ok) throw await failure(response);
  const bytes = new Uint8Array(await response.arrayBuffer());
  const total = response.status === 206 ? (rangeTotal(response) ?? bytes.length) : bytes.length;
  return { bytes, total };
}

async function failure(response: Response, readBody = true): Promise<UiError> {
  return fromHttpFailure({
    status: response.status,
    problem: readBody ? await problemBody(response) : null,
    requestIdHeader: response.headers.get("x-request-id"),
    retryAfterHeader: response.headers.get("retry-after"),
  });
}

async function problemBody(response: Response): Promise<ProblemBody | null> {
  if (!response.headers.get("content-type")?.includes("application/problem+json")) return null;
  try {
    const body: unknown = await response.json();
    return typeof body === "object" && body !== null && !Array.isArray(body)
      ? (body as ProblemBody)
      : null;
  } catch {
    // An unreadable body is treated as a non-problem response, which still maps to an error.
    return null;
  }
}

function isJson(response: Response): boolean {
  const type = response.headers.get("content-type") ?? "";
  return type.includes("application/json") || type.includes("+json");
}
