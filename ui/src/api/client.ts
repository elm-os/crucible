import { currentAuth } from "@/lib/auth";
import { type Connection, connectionKey } from "@/lib/connection";
import { loadPage } from "@/lib/navigate";
import { currentPath, loginHref } from "@/lib/next-path";
import { fromHttpFailure, networkError, UiError } from "./errors";
import type { ApiKey } from "./generated/ApiKey";
import type { ApiKeyCreate } from "./generated/ApiKeyCreate";
import type { ApiKeyCreated } from "./generated/ApiKeyCreated";
import type { ApiKeyList } from "./generated/ApiKeyList";
import type { DeletionRequest } from "./generated/DeletionRequest";
import type { DeletionRequestCreate } from "./generated/DeletionRequestCreate";
import type { DeletionRequestList } from "./generated/DeletionRequestList";
import type { FactWritten } from "./generated/FactWritten";
import type { FeedbackRequest } from "./generated/FeedbackRequest";
import type { FlagRequest } from "./generated/FlagRequest";
import type { InvitationAccept } from "./generated/InvitationAccept";
import type { InvitationCreate } from "./generated/InvitationCreate";
import type { InvitationCreated } from "./generated/InvitationCreated";
import type { InvitationList } from "./generated/InvitationList";
import type { LoginRequest } from "./generated/LoginRequest";
import type { MetricsQueryRequest } from "./generated/MetricsQueryRequest";
import type { MetricsResponse } from "./generated/MetricsResponse";
import type { OrgMember } from "./generated/OrgMember";
import type { OrgMemberList } from "./generated/OrgMemberList";
import type { OrgMemberPatch } from "./generated/OrgMemberPatch";
import type { PasswordResetConfirm } from "./generated/PasswordResetConfirm";
import type { PasswordResetRequest } from "./generated/PasswordResetRequest";
import type { PayloadField } from "./generated/PayloadField";
import type { PayloadParams } from "./generated/PayloadParams";
import type { Project } from "./generated/Project";
import type { ProjectCreate } from "./generated/ProjectCreate";
import type { Retention } from "./generated/Retention";
import type { RetentionUpdate } from "./generated/RetentionUpdate";
import type { SearchPage } from "./generated/SearchPage";
import type { SearchRequest } from "./generated/SearchRequest";
import type { SessionInfo } from "./generated/SessionInfo";
import type { SetupRequest } from "./generated/SetupRequest";
import type { SignInOptions } from "./generated/SignInOptions";
import type { SpanPage } from "./generated/SpanPage";
import type { SpanPageParams } from "./generated/SpanPageParams";
import type { TraceDetail } from "./generated/TraceDetail";
import type { TraceListPage } from "./generated/TraceListPage";
import type { TagRequest } from "./generated/TagRequest";
import type { TraceListParams } from "./generated/TraceListParams";
import type { ProblemBody, TimeWindow } from "./types";

const BASE = "/api/v1";

type Query = Record<string, string | number | undefined>;

// Who a request comes from (contract 11.4). "telemetry" reads go with the session cookie, or without a
// control plane with the Connect dialog's key; "session" calls send the session's CSRF token on writes;
// "prelogin" is the sign-in forms' writes; "public" sends nothing.
type Sender = "telemetry" | "session" | "prelogin" | "public";

interface Init {
  method?: "GET" | "POST" | "PATCH" | "PUT" | "DELETE";
  query?: Query;
  body?: unknown;
  version?: number;
  headers?: Record<string, string>;
  // A status the caller reads itself, returned instead of thrown.
  passStatus?: number;
  signal?: AbortSignal;
}

async function call(path: string, sender: Sender, init: Init = {}): Promise<Response> {
  const method = init.method ?? "GET";
  const auth = currentAuth();
  const headers: Record<string, string> = { Accept: "application/json" };
  // A key in `Authorization` makes the server ignore the session cookie, so a session never sends one.
  if (sender === "telemetry" && auth.mode !== "session") {
    Object.assign(headers, authorization(connectionKey()));
  }
  const csrf =
    sender === "prelogin" ? "prelogin" : auth.mode === "session" ? auth.session.csrf_token : null;
  if (method !== "GET" && csrf !== null) headers["X-Crucible-CSRF"] = csrf;
  if (init.body !== undefined) headers["Content-Type"] = "application/json";
  if (init.version !== undefined) headers["If-Match"] = `"${init.version}"`;
  Object.assign(headers, init.headers);

  let response: Response;
  try {
    response = await fetch(`${BASE}${path}${search(init.query)}`, {
      method,
      signal: init.signal,
      credentials: "same-origin",
      headers,
      body: init.body === undefined ? undefined : JSON.stringify(init.body),
    });
  } catch (error) {
    if (init.signal?.aborted) throw error;
    throw networkError();
  }
  if (response.ok || response.status === init.passStatus) return response;

  const error = await failure(response);
  const withSession = sender === "session" || (sender === "telemetry" && auth.mode === "session");
  // A wrong current password is a 401 too, and doesn't end the session.
  if (withSession && error.kind === "unauthenticated" && error.code !== "invalid_credentials") {
    // Only the server's own answer moves the page; another 401, like a proxy's, offers sign-in.
    if (error.code === "unauthenticated") loadPage(loginHref(currentPath()));
    throw new UiError({
      kind: "signed_out",
      status: error.status,
      code: error.code,
      requestId: error.requestId,
    });
  }
  throw error;
}

function search(query: Query = {}): string {
  const params = new URLSearchParams();
  for (const [name, value] of Object.entries(query)) {
    if (value !== undefined) params.set(name, String(value));
  }
  return params.size > 0 ? `?${params}` : "";
}

async function read<T>(response: Response): Promise<T> {
  // A static host or a proxy can answer 200 with HTML; that is a failure, never an empty list.
  if (!isJson(response)) throw await failure(response, false);
  try {
    return (await response.json()) as T;
  } catch {
    throw await failure(response, false);
  }
}

export async function apiGet<T>(path: string, query: Query, signal?: AbortSignal): Promise<T> {
  return read<T>(await call(path, "telemetry", { query, signal }));
}

// Temporary (milestone 1): without a control plane, the Connect dialog's key stands in for a session.
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

// Writes carry an Idempotency-Key, so a retry of a request whose answer was lost writes nothing twice.
export async function apiPost<T>(
  path: string,
  body: unknown,
  options: { signal?: AbortSignal; idempotencyKey?: string } = {},
): Promise<T> {
  const headers = options.idempotencyKey
    ? { "Idempotency-Key": options.idempotencyKey }
    : undefined;
  return read<T>(
    await call(path, "telemetry", { method: "POST", body, headers, signal: options.signal }),
  );
}

// 32 hex characters from `crypto.getRandomValues`, which, unlike `crypto.randomUUID`, also works outside a secure
// context: over plain HTTP on an internal network (contract 1.7).
export function newIdempotencyKey(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
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
  const response = await call(path, "telemetry", {
    query,
    headers: { Accept: "*/*", Range: `bytes=${first}-${last ?? ""}` },
    passStatus: 416,
    signal,
  });
  if (response.status === 416) {
    return { bytes: new Uint8Array(0), total: rangeTotal(response) ?? 0 };
  }
  const bytes = new Uint8Array(await response.arrayBuffer());
  const total = response.status === 206 ? (rangeTotal(response) ?? bytes.length) : bytes.length;
  return { bytes, total };
}

export type SignInMode = { mode: "keys" } | { mode: "session"; options: SignInOptions };

// A server without a control plane answers 404 `feature_disabled`: it has no sign-in, and the Connect
// dialog applies. Any other failure leaves the mode unknown.
export async function getSignInMode(signal?: AbortSignal): Promise<SignInMode> {
  try {
    const options = await read<SignInOptions>(await call("/auth/providers", "public", { signal }));
    return { mode: "session", options };
  } catch (error) {
    if (error instanceof UiError && error.status === 404 && error.code === "feature_disabled") {
      return { mode: "keys" };
    }
    throw error;
  }
}

export async function getSession(signal?: AbortSignal): Promise<SessionInfo> {
  return read<SessionInfo>(await call("/auth/session", "session", { signal }));
}

export async function login(body: LoginRequest): Promise<SessionInfo> {
  return read<SessionInfo>(await call("/auth/login", "prelogin", { method: "POST", body }));
}

export async function logout(): Promise<void> {
  await call("/auth/logout", "session", { method: "POST" });
}

export async function setupOwner(body: SetupRequest): Promise<SessionInfo> {
  return read<SessionInfo>(await call("/auth/setup", "prelogin", { method: "POST", body }));
}

export async function acceptInvitation(body: InvitationAccept): Promise<SessionInfo> {
  return read<SessionInfo>(
    await call("/auth/invitations/accept", "prelogin", { method: "POST", body }),
  );
}

export async function requestPasswordReset(body: PasswordResetRequest): Promise<void> {
  await call("/auth/password/reset-request", "prelogin", { method: "POST", body });
}

export async function confirmPasswordReset(body: PasswordResetConfirm): Promise<void> {
  await call("/auth/password/reset-confirm", "prelogin", { method: "POST", body });
}

export async function listOrgMembers(signal?: AbortSignal): Promise<OrgMemberList> {
  return read<OrgMemberList>(await call("/org/members", "session", { signal }));
}

export async function setOrgMemberRole(userId: string, body: OrgMemberPatch): Promise<OrgMember> {
  const path = `/org/members/${encodeURIComponent(userId)}`;
  return read<OrgMember>(await call(path, "session", { method: "PATCH", body }));
}

export async function removeOrgMember(userId: string): Promise<void> {
  await call(`/org/members/${encodeURIComponent(userId)}`, "session", { method: "DELETE" });
}

export async function listInvitations(signal?: AbortSignal): Promise<InvitationList> {
  return read<InvitationList>(await call("/org/invitations", "session", { signal }));
}

export async function createInvitation(body: InvitationCreate): Promise<InvitationCreated> {
  return read<InvitationCreated>(
    await call("/org/invitations", "session", { method: "POST", body }),
  );
}

export async function revokeInvitation(invitationId: string): Promise<void> {
  const path = `/org/invitations/${encodeURIComponent(invitationId)}`;
  await call(path, "session", { method: "DELETE" });
}

export async function listApiKeys(project: string, signal?: AbortSignal): Promise<ApiKeyList> {
  return read<ApiKeyList>(await call(projectPath(project, "/api-keys"), "session", { signal }));
}

export async function createApiKey(project: string, body: ApiKeyCreate): Promise<ApiKeyCreated> {
  const path = projectPath(project, "/api-keys");
  return read<ApiKeyCreated>(await call(path, "session", { method: "POST", body }));
}

export async function createProject(body: ProjectCreate): Promise<Project> {
  return read<Project>(await call("/projects", "session", { method: "POST", body }));
}

export async function revokeApiKey(
  project: string,
  key: Pick<ApiKey, "id" | "version">,
): Promise<void> {
  const path = projectPath(project, `/api-keys/${encodeURIComponent(key.id)}`);
  await call(path, "session", { method: "DELETE", version: key.version });
}

export async function getRetention(project: string, signal?: AbortSignal): Promise<Retention> {
  return read<Retention>(await call(projectPath(project, "/retention"), "session", { signal }));
}

export async function setRetention(
  project: string,
  body: RetentionUpdate,
  version: number,
): Promise<Retention> {
  const path = projectPath(project, "/retention");
  return read<Retention>(await call(path, "session", { method: "PUT", body, version }));
}

export async function listDeletionRequests(
  project: string,
  signal?: AbortSignal,
): Promise<DeletionRequestList> {
  const path = projectPath(project, "/deletion-requests");
  return read<DeletionRequestList>(await call(path, "session", { signal }));
}

// `idempotencyKey` makes a retry of the same request safe: the server answers with the first request.
export async function createDeletionRequest(
  project: string,
  body: DeletionRequestCreate,
  idempotencyKey: string,
): Promise<DeletionRequest> {
  const path = projectPath(project, "/deletion-requests");
  const headers = { "Idempotency-Key": idempotencyKey };
  return read<DeletionRequest>(await call(path, "session", { method: "POST", body, headers }));
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
