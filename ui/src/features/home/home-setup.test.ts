import { describe, expect, test } from "vitest";
import { UiError } from "@/api/errors";
import type { ApiKey } from "@/api/generated/ApiKey";
import type { ApiKeyList } from "@/api/generated/ApiKeyList";
import type { DataBounds } from "@/api/generated/DataBounds";
import type { Meta } from "@/api/generated/Meta";
import type { OrgMember } from "@/api/generated/OrgMember";
import type { OrgMemberList } from "@/api/generated/OrgMemberList";
import type { OrgRole } from "@/api/generated/OrgRole";
import type { TimeRange } from "@/api/generated/TimeRange";
import type { Auth } from "@/lib/auth";
import { PROJECT_A, PROJECT_B, sessionInfo, sessionProject } from "@/test/session";
import {
  inviteStep,
  keyStep,
  managesKeys,
  mcpStep,
  placement,
  type Read,
  type StepState,
  traceStep,
} from "./home-setup";

const WHOLE: Meta = { as_of: "2026-11-17T09:30:00.000000Z", partial: false, consistency: "strong" };
const PARTIAL: Meta = { ...WHOLE, partial: true, partial_reason: "store_unavailable" };
const LAGGING: Meta = { ...WHOLE, consistency: "lagging" };
const SEEN: TimeRange = {
  first: "2026-11-17T09:00:00.000000Z",
  last: "2026-11-17T09:25:00.000000Z",
};
const NONE: TimeRange = { first: null, last: null };
const AS_OF = Date.parse("2026-11-17T09:30:00Z");

const KEYS: Auth = { mode: "keys" };
const OWNER: Auth = { mode: "session", session: sessionInfo() };

function session(role: OrgRole, permissions: string[]): Auth {
  return {
    mode: "session",
    session: sessionInfo({
      org: { ...sessionInfo().org, role },
      projects: [sessionProject({ role: "member", permissions })],
    }),
  };
}

const ok = <T>(data: T): Read<T> => ({ status: "success", data });
const failed = (status: number, code: string): Read<never> => ({
  status: "error",
  error: new UiError({ kind: status === 403 ? "forbidden" : "unknown", status, code }),
});
const PENDING: Read<never> = { status: "pending" };

function bounds(spans: TimeRange, client: TimeRange, server: TimeRange, meta = WHOLE) {
  return ok<DataBounds>({
    spans,
    mcp: { client, server },
    retention_horizon: null,
    max_window_days: 30,
    server_time: WHOLE.as_of,
    meta,
  });
}

function key(fields: Partial<ApiKey> = {}): ApiKey {
  return {
    id: "k-1",
    public_key: "cpk_1",
    scopes: ["ingest"],
    note: "",
    expires_at: null,
    last_used_at: null,
    revoked_at: null,
    version: 1,
    ...fields,
  };
}

const keys = (...data: ApiKey[]) => ok<ApiKeyList>({ data });

const orgMember = (i: number): OrgMember => ({
  user_id: `u-${i}`,
  email: `u${i}@example.com`,
  name: `User ${i}`,
  role: i === 0 ? "owner" : "member",
  status: "active",
});

const members = (count: number) =>
  ok<OrgMemberList>({ data: Array.from({ length: count }, (_, i) => orgMember(i)) });

const status = (state: StepState) => state.status;

describe("the first-trace and MCP steps read the data bounds", () => {
  test("seen data is done, and no data in a whole answer is to do", () => {
    expect(status(traceStep(bounds(SEEN, NONE, NONE)))).toBe("done");
    expect(status(traceStep(bounds(NONE, NONE, NONE)))).toBe("todo");
    expect(status(mcpStep(bounds(SEEN, NONE, SEEN)))).toBe("done");
    expect(status(mcpStep(bounds(SEEN, SEEN, NONE)))).toBe("done");
    expect(status(mcpStep(bounds(SEEN, NONE, NONE)))).toBe("todo");
  });

  test("no data in a partial or lagging answer can't be told apart from unseen data", () => {
    for (const meta of [PARTIAL, LAGGING]) {
      expect(traceStep(bounds(NONE, NONE, NONE, meta))).toEqual({ status: "unknown", error: null });
      expect(mcpStep(bounds(SEEN, NONE, NONE, meta))).toEqual({ status: "unknown", error: null });
      // What was seen stays seen.
      expect(status(traceStep(bounds(SEEN, NONE, NONE, meta)))).toBe("done");
    }
  });

  test("an answer in flight is checking, and a failed one carries its error", () => {
    expect(status(traceStep(PENDING))).toBe("checking");
    expect(status(mcpStep(PENDING))).toBe("checking");
    const state = traceStep(failed(503, "telemetry_store_unavailable"));
    expect(state.status === "unknown" && state.error?.code).toBe("telemetry_store_unavailable");
  });
});

describe("the key step", () => {
  test("a key that is neither revoked nor expired is active", () => {
    expect(status(keyStep(OWNER, PROJECT_A, keys(key()), AS_OF))).toBe("done");
    const later = key({ expires_at: "2026-11-17T09:31:00.000000Z" });
    expect(status(keyStep(OWNER, PROJECT_A, keys(later), AS_OF))).toBe("done");
  });

  test("revoked, expired or no keys leave it to do", () => {
    const revoked = key({ revoked_at: "2026-11-01T00:00:00.000000Z" });
    const expired = key({ expires_at: "2026-11-17T09:30:00.000000Z" });
    expect(status(keyStep(OWNER, PROJECT_A, keys(revoked, expired), AS_OF))).toBe("todo");
    expect(status(keyStep(OWNER, PROJECT_A, keys(), AS_OF))).toBe("todo");
  });

  test("without the manage permission, or a control plane, it says who or what makes keys", () => {
    const member = session("member", ["read", "write_facts"]);
    expect(keyStep(member, PROJECT_A, PENDING, AS_OF)).toEqual({
      status: "unavailable",
      note: "Only project admins create API keys. Ask an admin of Checkout for one.",
    });
    expect(keyStep(OWNER, "not-a-project", PENDING, AS_OF)).toEqual({
      status: "unavailable",
      note: "Only project admins create API keys. Ask an admin of this project for one.",
    });
    expect(keyStep(KEYS, PROJECT_A, PENDING, AS_OF)).toEqual({
      status: "unavailable",
      note: "This server reads its keys from the keys file.",
    });
  });

  test("a refusal reads as someone else's step; other failures as unknown", () => {
    expect(status(keyStep(OWNER, PROJECT_A, failed(403, "forbidden"), AS_OF))).toBe("unavailable");
    expect(status(keyStep(OWNER, PROJECT_A, failed(500, "internal_error"), AS_OF))).toBe("unknown");
    expect(status(keyStep(OWNER, PROJECT_A, PENDING, AS_OF))).toBe("checking");
  });

  test("only the manage permission on this project lists keys", () => {
    expect(managesKeys(OWNER, PROJECT_A)).toBe(true);
    // The fixture session is a viewer on PROJECT_B.
    expect(managesKeys(OWNER, PROJECT_B)).toBe(false);
    expect(managesKeys(KEYS, PROJECT_A)).toBe(false);
  });
});

describe("the invite step", () => {
  test("a second member marks it done; a lone owner or admin has it to do", () => {
    expect(status(inviteStep(OWNER, members(2)))).toBe("done");
    expect(status(inviteStep(OWNER, members(1)))).toBe("todo");
    expect(status(inviteStep(session("admin", ["manage"]), members(1)))).toBe("todo");
  });

  test("anyone who can't invite is told who does, never to do", () => {
    expect(inviteStep(session("member", ["read"]), members(1))).toEqual({
      status: "unavailable",
      note: "Owners and admins of Analytical Engines invite people.",
    });
    expect(status(inviteStep(session("viewer", ["read"]), members(2)))).toBe("done");
    expect(status(inviteStep(OWNER, failed(403, "forbidden")))).toBe("unavailable");
  });

  test("without a control plane there is nobody to invite", () => {
    expect(status(inviteStep(KEYS, PENDING))).toBe("unavailable");
  });

  test("a read in flight is checking, and a failed one is unknown", () => {
    expect(status(inviteStep(OWNER, PENDING))).toBe("checking");
    expect(status(inviteStep(OWNER, failed(500, "internal_error")))).toBe("unknown");
  });
});

describe("whatever the role and the answers, a step someone can't do is never to do", () => {
  const roles: OrgRole[] = ["owner", "admin", "member", "viewer"];
  const permissionSets = [["read"], ["read", "write_facts", "edit_content"], ["read", "manage"]];
  const keyReads = [PENDING, keys(), keys(key()), failed(403, "forbidden"), failed(500, "x")];
  const memberReads = [PENDING, members(1), members(3), failed(403, "forbidden"), failed(500, "x")];

  test("keys: to do only with the manage permission", () => {
    for (const role of roles) {
      for (const permissions of permissionSets) {
        for (const read of keyReads) {
          const state = keyStep(session(role, permissions), PROJECT_A, read, AS_OF);
          if (state.status === "todo") expect(permissions).toContain("manage");
          if (read.status === "error" && read.error instanceof UiError) {
            if (read.error.kind === "forbidden") expect(state.status).toBe("unavailable");
          }
        }
      }
    }
  });

  test("invitations: to do only for owners and admins", () => {
    for (const role of roles) {
      for (const read of memberReads) {
        const state = inviteStep(session(role, ["read"]), read);
        if (state.status === "todo") expect(["owner", "admin"]).toContain(role);
        if (read.status === "error" && read.error instanceof UiError) {
          if (read.error.kind === "forbidden") expect(state.status).toBe("unavailable");
        }
      }
    }
  });
});

// The four steps in the card's order, by state.
const steps = (apiKey: StepState, trace: StepState, mcp: StepState, invite: StepState) => [
  { id: "api-key" as const, state: apiKey },
  { id: "first-trace" as const, state: trace },
  { id: "mcp-server" as const, state: mcp },
  { id: "invite" as const, state: invite },
];

describe("where the card sits", () => {
  const DONE: StepState = { status: "done" };
  const TODO: StepState = { status: "todo" };
  const CHECKING: StepState = { status: "checking" };
  const UNKNOWN: StepState = { status: "unknown", error: null };
  const ELSEWHERE: StepState = { status: "unavailable", note: "Someone else does it." };

  test("hidden once every step is done or someone else's", () => {
    expect(placement(steps(DONE, DONE, DONE, DONE))).toBe("hidden");
    expect(placement(steps(ELSEWHERE, DONE, DONE, ELSEWHERE))).toBe("hidden");
  });

  test("hidden while the steps left are being checked, so nothing flashes", () => {
    expect(placement(steps(CHECKING, CHECKING, CHECKING, CHECKING))).toBe("hidden");
    expect(placement(steps(CHECKING, DONE, DONE, CHECKING))).toBe("hidden");
  });

  test("hidden until the project's data is known, so it never moves once shown", () => {
    expect(placement(steps(TODO, CHECKING, CHECKING, TODO))).toBe("hidden");
    expect(placement(steps(UNKNOWN, CHECKING, CHECKING, DONE))).toBe("hidden");
  });

  test("first while the project has no data, whatever else is known", () => {
    expect(placement(steps(CHECKING, TODO, TODO, CHECKING))).toBe("first");
    expect(placement(steps(DONE, TODO, TODO, DONE))).toBe("first");
  });

  test("last when a step is left or unchecked on a project with data, or its data is unknown", () => {
    expect(placement(steps(TODO, DONE, DONE, DONE))).toBe("last");
    expect(placement(steps(DONE, DONE, TODO, ELSEWHERE))).toBe("last");
    expect(placement(steps(DONE, DONE, DONE, UNKNOWN))).toBe("last");
    expect(placement(steps(DONE, UNKNOWN, UNKNOWN, DONE))).toBe("last");
  });
});
