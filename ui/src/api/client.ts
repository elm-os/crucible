import { fromHttpFailure, networkError, type UiError } from "./errors";
import type { Page, Problem, TimeWindow, TraceListItem } from "./types";

const BASE = "/api/v1";

export async function apiGet<T>(
  path: string,
  query: Record<string, string>,
  signal?: AbortSignal,
): Promise<T> {
  let response: Response;
  try {
    response = await fetch(`${BASE}${path}?${new URLSearchParams(query)}`, {
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

export function listTraces(
  project: string,
  timeWindow: TimeWindow,
  signal?: AbortSignal,
): Promise<Page<TraceListItem>> {
  return apiGet(
    `/projects/${encodeURIComponent(project)}/traces`,
    { from: timeWindow.from, to: timeWindow.to },
    signal,
  );
}

async function failure(response: Response, readBody = true): Promise<UiError> {
  return fromHttpFailure({
    status: response.status,
    problem: readBody ? await problemBody(response) : null,
    requestIdHeader: response.headers.get("x-request-id"),
    retryAfterHeader: response.headers.get("retry-after"),
  });
}

async function problemBody(response: Response): Promise<Problem | null> {
  if (!response.headers.get("content-type")?.includes("application/problem+json")) return null;
  try {
    const body: unknown = await response.json();
    return typeof body === "object" && body !== null && !Array.isArray(body)
      ? (body as Problem)
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
