"use client";

import { createParts } from "@ui-sdk/react/primitive";
import { agentGraph } from "./agent-graph";

const {
  Root: AgentGraph,
  World: AgentGraphWorld,
  Node: AgentGraphNode,
} = createParts(agentGraph, { root: "div", world: "div", node: "div" });

export { AgentGraph, AgentGraphNode, AgentGraphWorld };
