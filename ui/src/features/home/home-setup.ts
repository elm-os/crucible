"use client";

import { type UseQueryResult, useQuery } from "@tanstack/react-query";
import { getDataBounds, listApiKeys, listOrgMembers } from "@/api/client";
import { asUiError, type UiError } from "@/api/errors";
import type { ApiKey } from "@/api/generated/ApiKey";
import type { ApiKeyList } from "@/api/generated/ApiKeyList";
import type { DataBounds } from "@/api/generated/DataBounds";
import type { Meta } from "@/api/generated/Meta";
import type { OrgMemberList } from "@/api/generated/OrgMemberList";
import { isWhole } from "@/components/status";
import { type Auth, useAuth } from "@/lib/auth";

export type StepId = "api-key" | "first-trace" | "mcp-server" | "invite";

// checking: its read is in flight. unknown: the read failed, or (error null) its answer may miss rows.
// unavailable: this role or this server doesn't do it here, and the note says who or what does.
export type StepState =
  | { status: "checking" }
  | { status: "unknown"; error: UiError | null }
  | { status: "todo" }
  | { status: "done" }
  | { status: "unavailable"; note: string };

// A read in flight, one that failed before any answer, or the latest answer (kept when a refetch fails).
export type Read<T> =
  | { status: "pending" }
  | { status: "error"; error: unknown }
  | { status: "success"; data: T };

export function readOf<T>(query: UseQueryResult<T>): Read<T> {
  if (query.data !== undefined) return { status: "success", data: query.data };
  if (query.isError) return { status: "error", error: query.error };
  return { status: "pending" };
}

// `done` answers null when the data can't tell. A refusal of the role reads as `forbidden`, when given.
function settle<T>(
  read: Read<T>,
  done: (data: T) => boolean | null,
  forbidden?: StepState,
): StepState {
  if (read.status === "pending") return { status: "checking" };
  if (read.status === "error") {
    const error = asUiError(read.error);
    if (forbidden && error.kind === "forbidden") return forbidden;
    return { status: "unknown", error };
  }
  const answer = done(read.data);
  if (answer === null) return { status: "unknown", error: null };
  return { status: answer ? "done" : "todo" };
}

// No data counts as none only when the answer is whole; otherwise rows may exist unseen.
function found(any: boolean, meta: Meta): boolean | null {
  if (any) return true;
  return isWhole(meta) ? false : null;
}

export function traceStep(bounds: Read<DataBounds>): StepState {
  return settle(bounds, (b) => found(b.spans.first !== null, b.meta));
}

export function mcpStep(bounds: Read<DataBounds>): StepState {
  return settle(bounds, (b) =>
    found(b.mcp.server.first !== null || b.mcp.client.first !== null, b.meta),
  );
}

// Listing keys needs the permission that creates them (contract 12.6), so without it none are asked for.
export function managesKeys(auth: Auth, project: string): boolean {
  return (
    auth.mode === "session" &&
    auth.session.projects.some(
      (visible) => visible.id === project && visible.permissions.includes("manage"),
    )
  );
}

// As on the API keys page: a key works until it is revoked or expires.
function active(key: ApiKey, asOf: number): boolean {
  return key.revoked_at === null && (key.expires_at === null || Date.parse(key.expires_at) > asOf);
}

export function keyStep(
  auth: Auth,
  project: string,
  keys: Read<ApiKeyList>,
  asOf: number,
): StepState {
  if (auth.mode !== "session") {
    return { status: "unavailable", note: "This server reads its keys from the keys file." };
  }
  const name = auth.session.projects.find((visible) => visible.id === project)?.name;
  const admins: StepState = {
    status: "unavailable",
    note: `Only project admins create API keys. Ask an admin of ${name ?? "this project"} for one.`,
  };
  if (!managesKeys(auth, project)) return admins;
  return settle(keys, (list) => list.data.some((key) => active(key, asOf)), admins);
}

export function inviteStep(auth: Auth, members: Read<OrgMemberList>): StepState {
  if (auth.mode !== "session") {
    return {
      status: "unavailable",
      note: "This server runs without a control plane, so it has no sign-in or members. Set postgres.url (CRUCIBLE_POSTGRES_URL) to turn them on.",
    };
  }
  const { org } = auth.session;
  const admins: StepState = {
    status: "unavailable",
    note: `Owners and admins of ${org.name} invite people.`,
  };
  const state = settle(members, (list) => list.data.length > 1, admins);
  // Only owners and admins invite (contract 11.6), so anyone else is told who does.
  const invites = org.role === "owner" || org.role === "admin";
  return state.status === "todo" && !invites ? admins : state;
}

export interface SetupStep {
  id: StepId;
  state: StepState;
  retry: () => Promise<unknown>;
}

export function useSetupSteps(project: string): SetupStep[] {
  const auth = useAuth();
  const bounds = useQuery({
    queryKey: [project, "data-bounds"],
    queryFn: ({ signal }) => getDataBounds(project, signal),
  });
  // The Settings pages' own cache entries: a key made or a member added there shows here at once.
  const keys = useQuery({
    queryKey: [project, "api-keys"],
    queryFn: ({ signal }) => listApiKeys(project, signal),
    enabled: managesKeys(auth, project),
  });
  const members = useQuery({
    queryKey: ["org", "members"],
    queryFn: ({ signal }) => listOrgMembers(signal),
    enabled: auth.mode === "session",
  });
  const read = readOf(bounds);
  return [
    {
      id: "api-key",
      state: keyStep(auth, project, readOf(keys), keys.dataUpdatedAt),
      retry: () => keys.refetch(),
    },
    { id: "first-trace", state: traceStep(read), retry: () => bounds.refetch() },
    { id: "mcp-server", state: mcpStep(read), retry: () => bounds.refetch() },
    { id: "invite", state: inviteStep(auth, readOf(members)), retry: () => members.refetch() },
  ];
}
