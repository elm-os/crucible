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
import { useAuth } from "@/lib/auth";
import { PLACEHOLDER_PROJECT, useProject } from "@/lib/project";

interface Item {
  value: string;
  label: string;
}

function ProjectSelect({
  project,
  items,
  onChange,
}: {
  project: string | null;
  items: Item[];
  onChange?: (project: string) => void;
}) {
  const value = items.some((item) => item.value === project) ? project : null;
  return (
    <Select
      items={items}
      value={value}
      placeholder={items.length === 0 && onChange ? "No projects" : "Project"}
      onValueChange={(next) => {
        if (next !== null) onChange?.(next);
      }}
    >
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

// Another project's entity ids mean nothing, so switching keeps the page and the time range only.
function showProject(project: string) {
  const params = new URLSearchParams({ project });
  const range = new URLSearchParams(window.location.search).get("range");
  if (range) params.set("range", range);
  window.history.replaceState(null, "", `?${params}`);
}

function ScopedProjectSelect() {
  const project = useProject();
  const auth = useAuth();
  if (auth.mode === "session") {
    const items = auth.session.projects.map(({ id, name }) => ({ value: id, label: name }));
    return <ProjectSelect project={project} items={items} onChange={showProject} />;
  }
  // Without a control plane only the connected project is known.
  const label = project === PLACEHOLDER_PROJECT ? "Default project" : project;
  return <ProjectSelect project={project} items={[{ value: project, label }]} />;
}

export function ProjectSwitcher() {
  return (
    <Suspense fallback={<ProjectSelect project={null} items={[]} />}>
      <ScopedProjectSelect />
    </Suspense>
  );
}
