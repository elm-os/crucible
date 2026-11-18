// Wire types are generated from the Rust DTOs into ./generated (cargo xtask gen-ts). This file only
// derives the shapes the client needs on top of them.
import type { Problem } from "./generated/Problem";
import type { TraceListParams } from "./generated/TraceListParams";

export type TimeWindow = Pick<TraceListParams, "from" | "to">;

// A problem body as received: a proxy or another server may have answered, so every member may be
// missing or malformed and is checked before use.
export type ProblemBody = { [K in keyof Problem]?: unknown };
