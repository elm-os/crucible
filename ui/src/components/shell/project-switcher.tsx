"use client";

import {
  Select,
  SelectContent,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { Suspense } from "react";
import { PLACEHOLDER_PROJECT, useProject } from "@/lib/project";

// Placeholder: lists only the current project until the session endpoint returns the user's
// projects. Switching will keep the route family and drop entity ids.
function ProjectSelect({ project }: { project: string | null }) {
  const items =
    project === null
      ? []
      : [{ value: project, label: project === PLACEHOLDER_PROJECT ? "Default project" : project }];
  return (
    <Select items={items} value={project} placeholder="Project">
      <SelectLabel className="sr-only">Project</SelectLabel>
      <SelectTrigger className="min-w-0 max-w-56 text-data">
        <SelectValue className="truncate" />
      </SelectTrigger>
      <SelectContent>
        {items.map((item) => (
          <SelectItem key={item.value} value={item.value}>
            {item.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function ScopedProjectSelect() {
  return <ProjectSelect project={useProject()} />;
}

export function ProjectSwitcher() {
  return (
    <Suspense fallback={<ProjectSelect project={null} />}>
      <ScopedProjectSelect />
    </Suspense>
  );
}
