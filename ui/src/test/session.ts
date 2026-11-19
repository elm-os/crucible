import type { SessionInfo } from "@/api/generated/SessionInfo";
import type { SessionProject } from "@/api/generated/SessionProject";

export const PROJECT_A = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b";
export const PROJECT_B = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a2c";

const ADMIN = ["read", "write_facts", "edit_content", "manage"];

export function sessionProject(fields: Partial<SessionProject> = {}): SessionProject {
  return {
    id: PROJECT_A,
    name: "Checkout",
    slug: "checkout",
    role: "admin",
    permissions: ADMIN,
    ...fields,
  };
}

export function sessionInfo(fields: Partial<SessionInfo> = {}): SessionInfo {
  return {
    user: {
      id: "0193a1b2-0000-7000-8000-000000000001",
      email: "ada@example.com",
      name: "Ada Lovelace",
    },
    org: { id: "0193a1b2-0000-7000-8000-0000000000aa", name: "Analytical Engines", role: "owner" },
    projects: [
      sessionProject(),
      sessionProject({
        id: PROJECT_B,
        name: "Search",
        slug: "search",
        role: "viewer",
        permissions: ["read", "run_saved_query"],
      }),
    ],
    providers: [],
    password_login_enabled: true,
    csrf_token: "csrf-token-1",
    session: {
      expires_at: "2026-12-18T12:00:00.000000Z",
      idle_expires_at: "2026-11-25T12:00:00.000000Z",
      auth_method: "password",
    },
    features: {
      sql_workspace: { enabled: false },
      media: { enabled: false, max_bytes: 0 },
      smtp: false,
      signup_mode: "invite_only",
    },
    server: { version: "0.4.0" },
    ...fields,
  };
}
