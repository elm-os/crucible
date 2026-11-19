import { networkError } from "@/api/errors";
import type { SavedQuery } from "@/api/generated/SavedQuery";
import type { SavedQueryInput } from "@/api/generated/SavedQueryInput";
import type { SavedQueryList } from "@/api/generated/SavedQueryList";
import type { SavedQueryRunRequest } from "@/api/generated/SavedQueryRunRequest";
import type { SqlExportFormat } from "@/api/generated/SqlExportFormat";
import type { SqlExportRequest } from "@/api/generated/SqlExportRequest";
import type { SqlResult } from "@/api/generated/SqlResult";
import type { SqlRunList } from "@/api/generated/SqlRunList";
import type { SqlRunRequest } from "@/api/generated/SqlRunRequest";
import type { SqlSchema } from "@/api/generated/SqlSchema";
import type { ProblemBody } from "@/api/types";
import { currentAuth } from "@/lib/auth";
import { connectionKey } from "@/lib/connection";
import { sqlErrorFrom } from "./errors";

interface Options {
  method?: "GET" | "POST" | "PATCH" | "DELETE";
  body?: unknown;
  signal?: AbortSignal;
  ifMatch?: string;
  accept?: string;
}

const EXTENSION: Record<SqlExportFormat, string> = {
  csv: "csv",
  jsonl: "jsonl",
  parquet: "parquet",
};

export const sqlKeys = {
  schema: (project: string) => [project, "sql", "schema"] as const,
  saved: (project: string) => [project, "sql", "saved-queries"] as const,
  runs: (project: string) => [project, "sql", "runs"] as const,
};

function sqlPath(project: string, rest: string): string {
  return `/api/v1/projects/${encodeURIComponent(project)}/sql/${rest}`;
}

async function send(path: string, options: Options = {}): Promise<Response> {
  const method = options.method ?? "GET";
  const auth = currentAuth();
  const headers: Record<string, string> = { Accept: options.accept ?? "application/json" };
  if (auth.mode === "session") {
    // A key in `Authorization` makes the server ignore the session cookie, so a session never sends one;
    // its writes carry the session's CSRF token (contract 11.4).
    if (method !== "GET") headers["X-Crucible-CSRF"] = auth.session.csrf_token;
  } else {
    const key = connectionKey();
    if (key !== null) headers.Authorization = `Bearer ${key}`;
  }
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  if (options.ifMatch !== undefined) headers["If-Match"] = options.ifMatch;
  let response: Response;
  try {
    response = await fetch(path, {
      method,
      signal: options.signal,
      credentials: "same-origin",
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
    });
  } catch (error) {
    if (options.signal?.aborted) throw error;
    throw networkError();
  }
  if (!response.ok) throw await failure(response);
  return response;
}

async function json<T>(path: string, options: Options = {}): Promise<T> {
  const response = await send(path, options);
  // A static host or a proxy can answer 200 with HTML; that is a failure, never an empty answer.
  if (!(response.headers.get("content-type") ?? "").includes("json")) {
    throw await failure(response, false);
  }
  try {
    return (await response.json()) as T;
  } catch (error) {
    if (options.signal?.aborted) throw error;
    throw await failure(response, false);
  }
}

async function failure(response: Response, readBody = true) {
  return sqlErrorFrom({
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

export function getSchema(project: string, signal?: AbortSignal): Promise<SqlSchema> {
  return json(sqlPath(project, "schema"), { signal });
}

export function runSql(
  project: string,
  request: SqlRunRequest,
  signal: AbortSignal,
): Promise<SqlResult> {
  return json(sqlPath(project, "run"), { method: "POST", body: request, signal });
}

export function listRuns(project: string, signal?: AbortSignal): Promise<SqlRunList> {
  return json(sqlPath(project, "runs?limit=50"), { signal });
}

export interface Download {
  blob: Blob;
  filename: string;
}

export async function exportSql(project: string, request: SqlExportRequest): Promise<Download> {
  const response = await send(sqlPath(project, "export"), {
    method: "POST",
    body: request,
    accept: "*/*",
  });
  const named = /filename="([^"/\\]+)"/.exec(response.headers.get("content-disposition") ?? "");
  let blob: Blob;
  try {
    blob = await response.blob();
  } catch {
    throw networkError();
  }
  return { blob, filename: named?.[1] ?? `crucible-sql.${EXTENSION[request.format]}` };
}

export function listSavedQueries(project: string, signal?: AbortSignal): Promise<SavedQueryList> {
  return json(sqlPath(project, "saved-queries"), { signal });
}

export function getSavedQuery(project: string, id: string): Promise<SavedQuery> {
  return json(sqlPath(project, `saved-queries/${encodeURIComponent(id)}`));
}

export function createSavedQuery(project: string, input: SavedQueryInput): Promise<SavedQuery> {
  return json(sqlPath(project, "saved-queries"), { method: "POST", body: input });
}

export function updateSavedQuery(
  project: string,
  query: Pick<SavedQuery, "id" | "version">,
  input: SavedQueryInput,
): Promise<SavedQuery> {
  return json(sqlPath(project, `saved-queries/${encodeURIComponent(query.id)}`), {
    method: "PATCH",
    body: input,
    ifMatch: `"${query.version}"`,
  });
}

export async function deleteSavedQuery(project: string, id: string): Promise<void> {
  await send(sqlPath(project, `saved-queries/${encodeURIComponent(id)}`), { method: "DELETE" });
}

export function runSavedQuery(
  project: string,
  id: string,
  request: SavedQueryRunRequest,
  signal: AbortSignal,
): Promise<SqlResult> {
  return json(sqlPath(project, `saved-queries/${encodeURIComponent(id)}/run`), {
    method: "POST",
    body: request,
    signal,
  });
}
