import { currentAuth } from "@/lib/auth";
import { type Connection, connectionKey } from "@/lib/connection";
import { loadPage } from "@/lib/navigate";
import { currentPath, loginHref } from "@/lib/next-path";
import { fromHttpFailure, networkError, UiError } from "./errors";
import type { AlertHistory } from "./generated/AlertHistory";
import type { AlertPreview } from "./generated/AlertPreview";
import type { AlertRule } from "./generated/AlertRule";
import type { AlertRuleInput } from "./generated/AlertRuleInput";
import type { AlertRuleList } from "./generated/AlertRuleList";
import type { AlertRulePatch } from "./generated/AlertRulePatch";
import type { ApiKey } from "./generated/ApiKey";
import type { ApiKeyCreate } from "./generated/ApiKeyCreate";
import type { ApiKeyCreated } from "./generated/ApiKeyCreated";
import type { ApiKeyList } from "./generated/ApiKeyList";
import type { Channel } from "./generated/Channel";
import type { ChannelCreate } from "./generated/ChannelCreate";
import type { ChannelList } from "./generated/ChannelList";
import type { ChannelPatch } from "./generated/ChannelPatch";
import type { ChannelTestResult } from "./generated/ChannelTestResult";
import type { Dashboard } from "./generated/Dashboard";
import type { DashboardCreate } from "./generated/DashboardCreate";
import type { DashboardDuplicate } from "./generated/DashboardDuplicate";
import type { DashboardList } from "./generated/DashboardList";
import type { DashboardPatch } from "./generated/DashboardPatch";
import type { DeletionRequest } from "./generated/DeletionRequest";
import type { DeletionRequestCreate } from "./generated/DeletionRequestCreate";
import type { DeletionRequestList } from "./generated/DeletionRequestList";
import type { EntityParams } from "./generated/EntityParams";
import type { Export } from "./generated/Export";
import type { ExportList } from "./generated/ExportList";
import type { ExportRequest } from "./generated/ExportRequest";
import type { FactWritten } from "./generated/FactWritten";
import type { FeedbackRequest } from "./generated/FeedbackRequest";
import type { FlagRequest } from "./generated/FlagRequest";
import type { InvitationAccept } from "./generated/InvitationAccept";
import type { InvitationCreate } from "./generated/InvitationCreate";
import type { InvitationCreated } from "./generated/InvitationCreated";
import type { InvitationList } from "./generated/InvitationList";
import type { LoginRequest } from "./generated/LoginRequest";
import type { McpClientList } from "./generated/McpClientList";
import type { McpCompare } from "./generated/McpCompare";
import type { McpDeployList } from "./generated/McpDeployList";
import type { McpErrors } from "./generated/McpErrors";
import type { McpOperationDetail } from "./generated/McpOperationDetail";
import type { McpOperationPage } from "./generated/McpOperationPage";
import type { McpServerList } from "./generated/McpServerList";
import type { McpSessionList } from "./generated/McpSessionList";
import type { McpSideParam } from "./generated/McpSideParam";
import type { McpToolDetail } from "./generated/McpToolDetail";
import type { McpToolList } from "./generated/McpToolList";
import type { McpUserList } from "./generated/McpUserList";
import type { McpView } from "./generated/McpView";
import type { SourceParam } from "./generated/SourceParam";
import type { MetricsQueryRequest } from "./generated/MetricsQueryRequest";
import type { MetricsResponse } from "./generated/MetricsResponse";
import type { MetricsSchema } from "./generated/MetricsSchema";
import type { OrgMember } from "./generated/OrgMember";
import type { OrgMemberList } from "./generated/OrgMemberList";
import type { OrgMemberPatch } from "./generated/OrgMemberPatch";
import type { PasswordResetConfirm } from "./generated/PasswordResetConfirm";
import type { PasswordResetRequest } from "./generated/PasswordResetRequest";
import type { PayloadField } from "./generated/PayloadField";
import type { PayloadParams } from "./generated/PayloadParams";
import type { Project } from "./generated/Project";
import type { ProjectCreate } from "./generated/ProjectCreate";
import type { ProjectList } from "./generated/ProjectList";
import type { ProjectPatch } from "./generated/ProjectPatch";
import type { Retention } from "./generated/Retention";
import type { RetentionUpdate } from "./generated/RetentionUpdate";
import type { SearchPage } from "./generated/SearchPage";
import type { SearchRequest } from "./generated/SearchRequest";
import type { SessionDetail } from "./generated/SessionDetail";
import type { SessionInfo } from "./generated/SessionInfo";
import type { SessionListParams } from "./generated/SessionListParams";
import type { SessionPage } from "./generated/SessionPage";
import type { SetupRequest } from "./generated/SetupRequest";
import type { SignInOptions } from "./generated/SignInOptions";
import type { SpanListPage } from "./generated/SpanListPage";
import type { SpanListParams } from "./generated/SpanListParams";
import type { SpanPage } from "./generated/SpanPage";
import type { SpanPageParams } from "./generated/SpanPageParams";
import type { TraceDetail } from "./generated/TraceDetail";
import type { TraceListPage } from "./generated/TraceListPage";
import type { TagRequest } from "./generated/TagRequest";
import type { TraceListParams } from "./generated/TraceListParams";
import type { UserDetail } from "./generated/UserDetail";
import type { UserListParams } from "./generated/UserListParams";
import type { UserPage } from "./generated/UserPage";
import type { UserSessionPage } from "./generated/UserSessionPage";
import type { ListFilters, ProblemBody, TimeWindow } from "./types";

const BASE = "/api/v1";

// A list value is sent as the parameter repeated once per value.
type Query = Record<string, string | number | readonly string[] | undefined>;

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
    if (value === undefined) continue;
    if (typeof value === "object") for (const item of value) params.append(name, item);
    else params.set(name, String(value));
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
  filters: ListFilters = {},
  signal?: AbortSignal,
  cursor?: string,
): Promise<TraceListPage> {
  const query: TraceListParams = { ...filters, from: timeWindow.from, to: timeWindow.to, cursor };
  return apiGet(projectPath(project, "/traces"), query, signal);
}

// Live tail: the traces whose head arrived after `since`, a head cursor of the same window and filters.
export function listNewTraces(
  project: string,
  timeWindow: TimeWindow,
  filters: ListFilters,
  since: string,
  signal?: AbortSignal,
): Promise<TraceListPage> {
  const query: TraceListParams = {
    ...filters,
    from: timeWindow.from,
    to: timeWindow.to,
    since,
    limit: 1,
  };
  return apiGet(projectPath(project, "/traces"), query, signal);
}

export function listSpans(
  project: string,
  timeWindow: TimeWindow,
  filters: ListFilters,
  cursor: string | null,
  signal?: AbortSignal,
): Promise<SpanListPage> {
  const query: SpanListParams = {
    ...filters,
    from: timeWindow.from,
    to: timeWindow.to,
    cursor: cursor ?? undefined,
  };
  return apiGet(projectPath(project, "/spans"), query, signal);
}

export function listSessions(
  project: string,
  timeWindow: TimeWindow,
  cursor: string | null,
  signal?: AbortSignal,
): Promise<SessionPage> {
  const query: SessionListParams = { ...timeWindow, cursor: cursor ?? undefined };
  return apiGet(projectPath(project, "/sessions"), query, signal);
}

export function getSessionDetail(
  project: string,
  session: string,
  timeWindow: TimeWindow,
  signal?: AbortSignal,
): Promise<SessionDetail> {
  const query: EntityParams = { ...timeWindow };
  return apiGet(projectPath(project, `/sessions/${encodeURIComponent(session)}`), query, signal);
}

export function listUsers(
  project: string,
  timeWindow: TimeWindow,
  cursor: string | null,
  signal?: AbortSignal,
): Promise<UserPage> {
  const query: UserListParams = { ...timeWindow, cursor: cursor ?? undefined };
  return apiGet(projectPath(project, "/users"), query, signal);
}

export function getUser(
  project: string,
  user: string,
  timeWindow: TimeWindow,
  signal?: AbortSignal,
): Promise<UserDetail> {
  const query: EntityParams = { ...timeWindow };
  return apiGet(projectPath(project, `/users/${encodeURIComponent(user)}`), query, signal);
}

export function listUserSessions(
  project: string,
  user: string,
  timeWindow: TimeWindow,
  cursor: string | null,
  signal?: AbortSignal,
): Promise<UserSessionPage> {
  const query: UserListParams = { ...timeWindow, cursor: cursor ?? undefined };
  return apiGet(projectPath(project, `/users/${encodeURIComponent(user)}/sessions`), query, signal);
}

export function getTrace(
  project: string,
  traceId: string,
  signal?: AbortSignal,
): Promise<TraceDetail> {
  return apiGet(projectPath(project, `/traces/${encodeURIComponent(traceId)}`), {}, signal);
}

// What every MCP analytics read takes besides its window (contract 9.9).
export interface McpScope {
  side: McpSideParam;
  source: SourceParam;
  faults: boolean;
  server: string | null;
}

function mcpGet<T>(
  project: string,
  rest: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  extra: Query,
  signal?: AbortSignal,
): Promise<T> {
  const query: Query = { side: scope.side, from: timeWindow.from, to: timeWindow.to, ...extra };
  if (scope.source !== "auto") query.source = scope.source;
  if (scope.faults) query.include_caller_faults = "true";
  if (scope.server) query.server = scope.server;
  return apiGet(projectPath(project, `/mcp/${rest}`), query, signal);
}

export function listMcpServers(
  project: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpServerList> {
  return mcpGet(project, "servers", timeWindow, scope, {}, signal);
}

export function listMcpTools(
  project: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpToolList> {
  return mcpGet(project, "tools", timeWindow, scope, {}, signal);
}

export function getMcpTool(
  project: string,
  name: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpToolDetail> {
  return mcpGet(project, `tools/${encodeURIComponent(name)}`, timeWindow, scope, {}, signal);
}

export function getMcpErrors(
  project: string,
  view: McpView,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpErrors> {
  return mcpGet(project, "errors", timeWindow, scope, { view }, signal);
}

export function getMcpClients(
  project: string,
  view: McpView,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpClientList> {
  return mcpGet(project, "clients", timeWindow, scope, { view }, signal);
}

export function listMcpUsers(
  project: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpUserList> {
  return mcpGet(project, "users", timeWindow, scope, {}, signal);
}

export function listMcpSessions(
  project: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpSessionList> {
  return mcpGet(project, "sessions", timeWindow, scope, {}, signal);
}

export interface McpOperationFilter {
  target?: string;
  client?: string;
  session?: string;
  user?: string;
  errorsOnly?: boolean;
}

export function listMcpOperations(
  project: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  filter: McpOperationFilter,
  cursor: string | null,
  signal?: AbortSignal,
): Promise<McpOperationPage> {
  const extra: Query = {
    target: filter.target,
    client: filter.client,
    session: filter.session,
    user: filter.user,
    errors_only: filter.errorsOnly ? "true" : undefined,
    cursor: cursor ?? undefined,
  };
  return mcpGet(project, "operations", timeWindow, scope, extra, signal);
}

export function getMcpOperation(
  project: string,
  traceId: string,
  spanId: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpOperationDetail> {
  const rest = `operations/${encodeURIComponent(traceId)}/${encodeURIComponent(spanId)}`;
  return mcpGet(project, rest, timeWindow, scope, {}, signal);
}

export function listMcpDeploys(
  project: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpDeployList> {
  return mcpGet(project, "deploys", timeWindow, scope, {}, signal);
}

export function compareMcpDeploys(
  project: string,
  baseline: string,
  candidate: string,
  timeWindow: TimeWindow,
  scope: McpScope,
  signal?: AbortSignal,
): Promise<McpCompare> {
  const extra = { baseline_deploy: baseline, candidate_deploy: candidate };
  return mcpGet(project, "compare", timeWindow, scope, extra, signal);
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

export function getMetricsSchema(project: string, signal?: AbortSignal): Promise<MetricsSchema> {
  return apiGet(projectPath(project, "/metrics/schema"), {}, signal);
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

export async function listExports(project: string, signal?: AbortSignal): Promise<ExportList> {
  return read<ExportList>(await call(projectPath(project, "/exports"), "session", { signal }));
}

// The key makes a retry of a request whose answer was lost answer with the export it created.
export async function createExport(
  project: string,
  body: ExportRequest,
  idempotencyKey: string,
): Promise<Export> {
  const path = projectPath(project, "/exports");
  const headers = { "Idempotency-Key": idempotencyKey };
  return read<Export>(await call(path, "session", { method: "POST", body, headers }));
}

export async function createProject(body: ProjectCreate): Promise<Project> {
  return read<Project>(await call("/projects", "session", { method: "POST", body }));
}

export async function listProjects(signal?: AbortSignal): Promise<ProjectList> {
  return read<ProjectList>(await call("/projects", "session", { signal }));
}

export async function renameProject(
  project: Pick<Project, "id" | "version">,
  body: ProjectPatch,
): Promise<Project> {
  const init: Init = { method: "PATCH", body, version: project.version };
  return read<Project>(await call(projectPath(project.id, ""), "session", init));
}

export async function revokeApiKey(
  project: string,
  key: Pick<ApiKey, "id" | "version">,
): Promise<void> {
  const path = projectPath(project, `/api-keys/${encodeURIComponent(key.id)}`);
  await call(path, "session", { method: "DELETE", version: key.version });
}

// Alert rules and channels (contract 12.6). Reads go as telemetry reads do; writes need a session.

export async function listAlerts(project: string, signal?: AbortSignal): Promise<AlertRuleList> {
  return read<AlertRuleList>(await call(projectPath(project, "/alerts"), "telemetry", { signal }));
}

function alertPath(project: string, id: string, rest = ""): string {
  return projectPath(project, `/alerts/${encodeURIComponent(id)}${rest}`);
}

export async function getAlert(
  project: string,
  id: string,
  signal?: AbortSignal,
): Promise<AlertRule> {
  return read<AlertRule>(await call(alertPath(project, id), "telemetry", { signal }));
}

export async function createAlert(project: string, body: AlertRuleInput): Promise<AlertRule> {
  const path = projectPath(project, "/alerts");
  return read<AlertRule>(await call(path, "session", { method: "POST", body }));
}

export async function patchAlert(
  project: string,
  rule: Pick<AlertRule, "id" | "version">,
  body: AlertRulePatch,
): Promise<AlertRule> {
  const init: Init = { method: "PATCH", body, version: rule.version };
  return read<AlertRule>(await call(alertPath(project, rule.id), "session", init));
}

export async function deleteAlert(
  project: string,
  rule: Pick<AlertRule, "id" | "version">,
): Promise<void> {
  await call(alertPath(project, rule.id), "session", { method: "DELETE", version: rule.version });
}

export async function getAlertHistory(
  project: string,
  id: string,
  cursor: string | undefined,
  signal?: AbortSignal,
): Promise<AlertHistory> {
  const init: Init = { query: { limit: 50, cursor }, signal };
  return read<AlertHistory>(await call(alertPath(project, id, "/history"), "telemetry", init));
}

export async function previewAlert(
  project: string,
  body: AlertRuleInput,
  signal?: AbortSignal,
): Promise<AlertPreview> {
  const init: Init = { method: "POST", body, signal };
  return read<AlertPreview>(await call(projectPath(project, "/alerts/preview"), "telemetry", init));
}

export async function listChannels(project: string, signal?: AbortSignal): Promise<ChannelList> {
  return read<ChannelList>(await call(projectPath(project, "/channels"), "telemetry", { signal }));
}

function channelPath(project: string, id: string, rest = ""): string {
  return projectPath(project, `/channels/${encodeURIComponent(id)}${rest}`);
}

export async function createChannel(project: string, body: ChannelCreate): Promise<Channel> {
  const path = projectPath(project, "/channels");
  return read<Channel>(await call(path, "session", { method: "POST", body }));
}

export async function patchChannel(
  project: string,
  channel: Pick<Channel, "id" | "version">,
  body: ChannelPatch,
): Promise<Channel> {
  const init: Init = { method: "PATCH", body, version: channel.version };
  return read<Channel>(await call(channelPath(project, channel.id), "session", init));
}

export async function deleteChannel(
  project: string,
  channel: Pick<Channel, "id" | "version">,
): Promise<void> {
  const init: Init = { method: "DELETE", version: channel.version };
  await call(channelPath(project, channel.id), "session", init);
}

export async function testChannel(project: string, id: string): Promise<ChannelTestResult> {
  const path = channelPath(project, id, "/test");
  return read<ChannelTestResult>(await call(path, "session", { method: "POST", body: {} }));
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

// Dashboards (contract 12.6). Reads go as telemetry reads do, so the built-in dashboards work without a
// control plane; writes need a session. A built-in dashboard's id is its key.
export async function listDashboards(
  project: string,
  signal?: AbortSignal,
): Promise<DashboardList> {
  return read<DashboardList>(
    await call(projectPath(project, "/dashboards"), "telemetry", { signal }),
  );
}

function dashboardPath(project: string, id: string, rest = ""): string {
  return projectPath(project, `/dashboards/${encodeURIComponent(id)}${rest}`);
}

export async function getDashboard(
  project: string,
  id: string,
  signal?: AbortSignal,
): Promise<Dashboard> {
  return read<Dashboard>(await call(dashboardPath(project, id), "telemetry", { signal }));
}

export async function createDashboard(project: string, body: DashboardCreate): Promise<Dashboard> {
  const path = projectPath(project, "/dashboards");
  return read<Dashboard>(await call(path, "session", { method: "POST", body }));
}

export async function patchDashboard(
  project: string,
  dashboard: Pick<Dashboard, "id" | "version">,
  body: DashboardPatch,
): Promise<Dashboard> {
  const init: Init = { method: "PATCH", body, version: dashboard.version };
  return read<Dashboard>(await call(dashboardPath(project, dashboard.id), "session", init));
}

export async function deleteDashboard(
  project: string,
  dashboard: Pick<Dashboard, "id" | "version">,
): Promise<void> {
  const init: Init = { method: "DELETE", version: dashboard.version };
  await call(dashboardPath(project, dashboard.id), "session", init);
}

export async function duplicateDashboard(
  project: string,
  id: string,
  body: DashboardDuplicate,
): Promise<Dashboard> {
  const path = dashboardPath(project, id, "/duplicate");
  return read<Dashboard>(await call(path, "session", { method: "POST", body }));
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
