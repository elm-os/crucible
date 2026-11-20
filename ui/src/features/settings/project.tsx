"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { type FormEvent, useState } from "react";
import { listProjects, renameProject } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { Project } from "@/api/generated/Project";
import type { ProjectList } from "@/api/generated/ProjectList";
import type { SessionProject } from "@/api/generated/SessionProject";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { TickIcon } from "@/components/icons";
import { Section, SectionFrame } from "@/components/section";
import { scopedHref } from "@/components/shell/nav";
import { currentAuth, setAuth, useAuth } from "@/lib/auth";
import { useProject } from "@/lib/project";

const TITLE = "Project details";
const PROJECTS_KEY = ["projects"];
// At least one character that isn't a space: the server trims the name before it checks it.
const NAME_PATTERN = ".*\\S.*";

export function ProjectSettings() {
  const auth = useAuth();
  const project = useProject();
  if (auth.mode !== "session") {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="Project settings need a control plane.">
          This server reads its projects from the keys file. Set postgres.url
          (CRUCIBLE_POSTGRES_URL) to give projects names and rename them here.
        </EmptyState>
      </SectionFrame>
    );
  }
  const { org, projects } = auth.session;
  if (projects.length === 0) {
    return (
      <SectionFrame title={TITLE}>
        {org.role === "owner" || org.role === "admin" ? (
          <EmptyState kind="first-use" title="This organization has no projects yet.">
            Create one with New project. Its name, slug and id then show here.
          </EmptyState>
        ) : (
          <EmptyState kind="unavailable" title="You aren't in any project yet.">
            Ask an organization owner or admin to add you to one.
          </EmptyState>
        )}
      </SectionFrame>
    );
  }
  const access = projects.find((visible) => visible.id === project);
  if (access === undefined) {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="This project isn't available to you.">
          Choose one of your projects in the project menu.
        </EmptyState>
      </SectionFrame>
    );
  }
  return (
    <div className="flex flex-col gap-6">
      <DetailsSection access={access} />
      <RelatedSettings name={access.name} />
    </div>
  );
}

// The session lists names, roles and permissions; only this list carries the version that If-Match needs.
// Its names are the newer ones, such as a rename made in another session.
function DetailsSection({ access }: { access: SessionProject }) {
  const query = useQuery({
    queryKey: PROJECTS_KEY,
    queryFn: async ({ signal }) => {
      const list = await listProjects(signal);
      showNames(list.data);
      return list;
    },
    select: (list) => list.data.find((row) => row.id === access.id) ?? null,
  });
  return (
    <Section
      title={TITLE}
      query={query}
      skeleton={<DetailsSkeleton />}
      isEmpty={(row) => row === null}
      empty={
        <EmptyState kind="unavailable" title="This project isn't available anymore.">
          It may have been deleted, or your access to it removed. Choose another project in the
          project menu.
        </EmptyState>
      }
    >
      {(row) =>
        row && (
          <Details key={row.id} project={row} canRename={access.permissions.includes("manage")} />
        )
      }
    </Section>
  );
}

const SKELETON_ROWS = ["name", "slug", "id"];

function DetailsSkeleton() {
  return (
    <div className="flex flex-col gap-5">
      {SKELETON_ROWS.map((row) => (
        <div key={row} className="flex flex-col gap-2">
          <div className="skeleton h-3 w-24" />
          <div className="skeleton h-4 w-72 max-w-full" />
        </div>
      ))}
    </div>
  );
}

function Details({ project, canRename }: { project: Project; canRename: boolean }) {
  return (
    <div className="flex min-w-0 flex-col gap-5">
      {canRename ? <RenameForm project={project} /> : null}
      <dl className="flex min-w-0 flex-col gap-4 text-data">
        {canRename ? null : (
          <div className="flex flex-col gap-1">
            <dt className="text-label font-medium">Name</dt>
            <dd className="break-words">{project.name}</dd>
            <dd className="text-ink-soft">
              Only project admins rename a project. Ask an admin of {project.name}.
            </dd>
          </div>
        )}
        <div className="flex flex-col gap-1">
          <dt className="text-label font-medium">Slug</dt>
          <dd className="font-mono wrap-anywhere">{project.slug}</dd>
          <dd className="text-ink-soft">
            Commands such as{" "}
            <code className="font-mono wrap-anywhere">
              crucible export --project {project.slug}
            </code>{" "}
            take it in place of the id.
          </dd>
        </div>
        <div className="flex flex-col gap-1">
          <dt className="text-label font-medium">Project id</dt>
          <dd className="flex flex-wrap items-center gap-x-3 gap-y-2">
            <span className="min-w-0 font-mono wrap-anywhere select-all">{project.id}</span>
            <CopyId id={project.id} />
          </dd>
          <dd className="text-ink-soft">
            REST calls name the project by it, as in{" "}
            <code className="font-mono wrap-anywhere">/api/v1/projects/{project.id}/traces</code>.
          </dd>
        </div>
      </dl>
    </div>
  );
}

// The project menu reads the session's copy of each name.
function showNames(read: Project[]) {
  const auth = currentAuth();
  if (auth.mode !== "session") return;
  const { session } = auth;
  const nameOf = (row: SessionProject) => read.find((project) => project.id === row.id)?.name;
  if (!session.projects.some((row) => (nameOf(row) ?? row.name) !== row.name)) return;
  const projects = session.projects.map((row) => ({ ...row, name: nameOf(row) ?? row.name }));
  setAuth({ mode: "session", session: { ...session, projects } });
}

const RENAME_COPY: CopyByCode = {
  precondition_failed: {
    title: "Someone renamed this project since the page loaded.",
    detail: "The field shows its current name now. Enter the new name again, then rename.",
  },
  forbidden: {
    title: "Only project admins rename a project.",
    detail: "Ask an admin of the project, or for the admin role on it.",
  },
  not_found: {
    title: "This project isn't there anymore.",
    detail: "It may have been deleted. Choose another project in the project menu.",
  },
};

function RenameForm({ project }: { project: Project }) {
  const queryClient = useQueryClient();
  // What the person typed; null while the field shows the stored name, so a reload shows through.
  const [draft, setDraft] = useState<string | null>(null);
  const rename = useMutation({
    mutationFn: (name: string) => renameProject(project, { name }),
    onSuccess: (renamed) => {
      queryClient.setQueryData<ProjectList>(PROJECTS_KEY, (list) =>
        list ? { data: list.data.map((row) => (row.id === renamed.id ? renamed : row)) } : list,
      );
      showNames([renamed]);
      setDraft(null);
    },
    onError: async (error) => {
      // A rename needs the version it replaces: read the stored project again.
      if (asUiError(error).code !== "precondition_failed") return;
      await queryClient.invalidateQueries({ queryKey: PROJECTS_KEY });
      setDraft(null);
    },
  });
  const failure = rename.isError ? asUiError(rename.error) : null;
  const value = draft ?? project.name;
  const changed = draft !== null && draft.trim() !== project.name;

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (changed && !rename.isPending) rename.mutate(value.trim());
  };

  return (
    <form className="flex max-w-prose min-w-0 flex-col gap-3" onSubmit={submit}>
      <Field name="name" invalid={fieldFailed(failure, "name") || undefined}>
        <FieldLabel>Project name</FieldLabel>
        <Input
          required
          maxLength={200}
          pattern={NAME_PATTERN}
          autoComplete="off"
          readOnly={rename.isPending}
          value={value}
          onValueChange={(next) => {
            setDraft(next);
            rename.reset();
          }}
          className="max-w-sm"
        />
        <FieldDescription>
          Shown in the project menu and on these settings pages. The slug and the id stay the same.
        </FieldDescription>
        <FieldError match="valueMissing">Enter a name for the project.</FieldError>
        <FieldError match="patternMismatch">Enter a name for the project.</FieldError>
        <FieldError match={fieldFailed(failure, "name")}>Use 1 to 200 characters.</FieldError>
      </Field>
      {failure ? (
        <FormAlert
          copy={failureCopy(failure, "Couldn't rename the project.", RENAME_COPY)}
          requestId={failure.requestId}
        />
      ) : null}
      <div className="flex flex-wrap items-center gap-3">
        <Button
          type="submit"
          data-variant="solid"
          disabled={!changed || rename.isPending}
          focusableWhenDisabled
          aria-busy={rename.isPending}
        >
          {rename.isPending ? "Renaming" : "Rename"}
        </Button>
        <output className="flex items-center gap-1.5 text-data text-ink-soft">
          {rename.isSuccess ? (
            <>
              <TickIcon aria-hidden="true" /> Renamed. The project menu shows the new name.
            </>
          ) : null}
        </output>
      </div>
    </form>
  );
}

// The browser has no clipboard API on plain-HTTP origins, so the id itself selects with one click
// and the button says when copying fails.
function CopyId({ id }: { id: string }) {
  const [copied, setCopied] = useState<boolean | null>(null);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(id);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };
  return (
    <span className="flex flex-wrap items-center gap-3">
      <Button onClick={copy}>Copy id</Button>
      <output className="text-label text-ink-soft">{copied ? "Copied." : ""}</output>
      {copied === false ? (
        <span role="alert" className="text-label">
          Couldn&apos;t copy here. Select the id and copy it yourself.
        </span>
      ) : null}
    </span>
  );
}

function RelatedSettings({ name }: { name: string }) {
  const search = useSearchParams();
  return (
    <SectionFrame title="Related settings">
      <ul className="flex max-w-prose flex-col gap-2 text-data">
        <li>
          <Link href={scopedHref("/settings/retention", search)}>Retention</Link> sets how long{" "}
          {name} keeps its data, and deletes the data of chosen traces, users or sessions.
        </li>
        <li>
          Organization owners and admins delete a whole project in the REST API, with{" "}
          <code className="font-mono wrap-anywhere">{"DELETE /api/v1/projects/{project_id}"}</code>.
        </li>
      </ul>
    </SectionFrame>
  );
}
