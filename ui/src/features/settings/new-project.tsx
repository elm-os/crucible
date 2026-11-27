"use client";

import { useMutation } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import {
  Dialog,
  DialogBackdrop,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogTitle,
  DialogTrigger,
} from "@ui-sdk/react/dialog";
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import { useRouter } from "next/navigation";
import { type FormEvent, useState } from "react";
import { createProject, getSession } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { Project } from "@/api/generated/Project";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { scopedHref } from "@/components/shell/nav";
import { setAuth, useAuth } from "@/lib/auth";
import { loadPage } from "@/lib/navigate";
import { SLUG_PATTERN, slugFromName } from "@/lib/slug";

const PROJECT_COPY: CopyByCode = {
  conflict: {
    title: "Another project already uses that slug.",
    detail: "Choose a different slug; each project's slug is unique in the organization.",
  },
  forbidden: {
    title: "Your role can't create projects.",
    detail: "Ask an organization owner or admin to create it.",
  },
};

// Creating projects is for organization owners and admins (contract 11.6).
export function NewProjectControl() {
  const auth = useAuth();
  const [open, setOpen] = useState(false);
  const [announcement, setAnnouncement] = useState("");
  if (auth.mode !== "session") return null;
  const { role } = auth.session.org;
  if (role !== "owner" && role !== "admin") return null;

  return (
    <div className="mb-2">
      <output className="sr-only">{announcement}</output>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogTrigger className="text-data">New project</DialogTrigger>
        <DialogBackdrop />
        <DialogContent>
          <NewProjectForm
            onCreated={(project) => {
              setOpen(false);
              setAnnouncement(`Created ${project.name}. Its API keys page is open.`);
            }}
          />
        </DialogContent>
      </Dialog>
    </div>
  );
}

// Unmounts with the dialog, so it opens empty each time.
function NewProjectForm({ onCreated }: { onCreated: (project: Project) => void }) {
  const router = useRouter();
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  // The slug follows the name until someone types their own.
  const [slugEdited, setSlugEdited] = useState(false);
  const create = useMutation({
    mutationFn: createProject,
    onSuccess: async (project) => {
      const search = new URLSearchParams(window.location.search);
      search.set("project", project.id);
      const href = scopedHref("/settings/api-keys", search);
      try {
        setAuth({ mode: "session", session: await getSession() });
      } catch {
        // The project exists either way; a fresh page load reads the session again.
        loadPage(href);
        return;
      }
      onCreated(project);
      router.push(href);
    },
  });
  const busy = create.isPending || create.isSuccess;
  const failure = create.isError ? asUiError(create.error) : null;
  const slugTaken = failure?.code === "conflict";

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    create.mutate({ name: name.trim(), slug });
  };

  return (
    <>
      <div className="flex flex-col gap-2">
        <DialogTitle>New project</DialogTitle>
        <DialogDescription>
          A project keeps its own traces, API keys and settings. Its API keys page opens next.
        </DialogDescription>
      </div>
      <form
        className="flex flex-col gap-4"
        onSubmit={submit}
        onInput={() => {
          if (failure) create.reset();
        }}
      >
        <Field name="name" invalid={fieldFailed(failure, "name") || undefined}>
          <FieldLabel>Name</FieldLabel>
          <Input
            required
            maxLength={200}
            autoComplete="off"
            value={name}
            onValueChange={(next) => {
              setName(next);
              if (!slugEdited) setSlug(slugFromName(next));
            }}
          />
          <FieldError match="valueMissing">Enter a name for the project.</FieldError>
          <FieldError match={fieldFailed(failure, "name")}>Use 1 to 200 characters.</FieldError>
        </Field>
        <Field name="slug" invalid={slugTaken || fieldFailed(failure, "slug") || undefined}>
          <FieldLabel>Slug</FieldLabel>
          <Input
            required
            pattern={SLUG_PATTERN}
            maxLength={64}
            autoComplete="off"
            spellCheck={false}
            className="font-mono"
            value={slug}
            onValueChange={(next) => {
              setSlug(next.toLowerCase().replace(/\s/g, "-"));
              setSlugEdited(next !== "");
            }}
          />
          <FieldDescription>
            Lowercase letters, digits and hyphens, unique in the organization.
          </FieldDescription>
          <FieldError match="valueMissing">Enter a slug, like payments-api.</FieldError>
          <FieldError match="patternMismatch">
            Use only lowercase letters, digits and hyphens, like payments-api.
          </FieldError>
          <FieldError match={slugTaken}>Choose a slug no other project uses.</FieldError>
          <FieldError match={fieldFailed(failure, "slug")}>
            Use 1 to 64 lowercase letters, digits and hyphens.
          </FieldError>
        </Field>
        {failure ? (
          <FormAlert copy={failureCopy(failure, "Couldn't create the project.", PROJECT_COPY)} />
        ) : null}
        <div className="flex justify-end gap-2">
          <DialogClose>Cancel</DialogClose>
          <Button type="submit" data-variant="solid" disabled={busy} aria-busy={busy}>
            {busy ? "Creating the project" : "Create project"}
          </Button>
        </div>
      </form>
    </>
  );
}
