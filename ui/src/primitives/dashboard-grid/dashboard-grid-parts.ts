"use client";

import { createParts } from "@ui-sdk/react/primitive";
import { dashboardGrid } from "./dashboard-grid";

const {
  Root: DashboardGrid,
  Box: DashboardGridBox,
  Handle: DashboardGridHandle,
} = createParts(dashboardGrid, { root: "div", box: "div", handle: "button" });

export { DashboardGrid, DashboardGridBox, DashboardGridHandle };
