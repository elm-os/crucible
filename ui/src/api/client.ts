import { fromHttpFailure, networkError, type UiError } from "./errors";
import type { TraceDetail } from "./generated/TraceDetail";
import type { TraceListPage } from "./generated/TraceListPage";
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
      headers: { Accept: "application/json" },
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
