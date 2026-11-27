"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertDialogTrigger, createAlertDialogController } from "@ui-sdk/react/alert-dialog";
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
import { Field, FieldDescription, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import { type FormEvent, useState } from "react";
import { createApiKey, listApiKeys, revokeApiKey } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { ApiKey } from "@/api/generated/ApiKey";
import type { ApiKeyCreated } from "@/api/generated/ApiKeyCreated";
import type { SessionProject } from "@/api/generated/SessionProject";
import { DialogBody } from "@/components/dialog-body";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy } from "@/components/form-alert";
import { Section, SectionFrame } from "@/components/section";
import { useAuth } from "@/lib/auth";
import { formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { ConfirmDialog } from "./confirm-dialog";
import { RowsSkeleton } from "./rows-skeleton";
import { ShowOnce } from "./show-once";

const TITLE = "API keys";

const keysKey = (project: string) => [project, "api-keys"];

export function ApiKeysSettings() {
  const auth = useAuth();
  const project = useProject();
  if (auth.mode !== "session") {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="API keys need a control plane.">
          This server reads its keys from the keys file. Set postgres.url (CRUCIBLE_POSTGRES_URL) to
          create and revoke keys here.
        </EmptyState>
      </SectionFrame>
    );
  }
  const { org, projects } = auth.session;
  const access = projects.find((visible) => visible.id === project);
  if (projects.length === 0) {
    return (
      <SectionFrame title={TITLE}>
        {org.role === "owner" || org.role === "admin" ? (
          <EmptyState kind="first-use" title="This organization has no projects yet.">
            API keys belong to a project. Create one with New project, and its keys page opens here.
          </EmptyState>
        ) : (
          <EmptyState kind="unavailable" title="You aren't in any project yet.">
            Ask an organization owner or admin to add you to one.
          </EmptyState>
        )}
      </SectionFrame>
    );
  }
  if (access === undefined) {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="This project isn't available to you.">
          Choose one of your projects in the project menu.
        </EmptyState>
      </SectionFrame>
    );
  }
  // Listing keys needs the same permission as changing them (contract 12.6), so nothing is fetched.
  if (!access.permissions.includes("manage")) {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="Only project admins manage API keys.">
          Ask an admin of {access.name} for a key, or for the admin role on the project.
        </EmptyState>
      </SectionFrame>
    );
  }
  return <ApiKeysSection project={access} />;
}

function ApiKeysSection({ project }: { project: SessionProject }) {
  const query = useQuery({
    queryKey: keysKey(project.id),
    queryFn: ({ signal }) => listApiKeys(project.id, signal),
  });
  return (
    <Section
      title={`API keys for ${project.name}`}
      label="API keys"
      query={query}
      actions={<CreateKeyDialog project={project} />}
      skeleton={<RowsSkeleton />}
      isEmpty={(list) => list.data.length === 0}
      empty={
        <EmptyState kind="first-use" title="No API keys yet.">
          Create a key to send traces to {project.name}, or to read its data from scripts.
        </EmptyState>
      }
    >
      {(list) => <KeysTable keys={list.data} project={project} asOf={query.dataUpdatedAt} />}
    </Section>
  );
}

function keyStatus(key: ApiKey, asOf: number): "Active" | "Expired" | "Revoked" {
  if (key.revoked_at !== null) return "Revoked";
  if (key.expires_at !== null && Date.parse(key.expires_at) <= asOf) return "Expired";
  return "Active";
}

const REVOKE_COPY: CopyByCode = {
  precondition_failed: {
    title: "This key changed since the list loaded.",
    detail: "The list is up to date now. Close this, check the key, then revoke it again.",
  },
};

function KeysTable({
  keys,
  project,
  asOf,
}: {
  keys: ApiKey[];
  project: SessionProject;
  asOf: number;
}) {
  const queryClient = useQueryClient();
  const [revoke] = useState(() => createAlertDialogController<ApiKey>());
  const refresh = () => queryClient.invalidateQueries({ queryKey: keysKey(project.id) });

  // A key that is already gone needs nothing more; one that changed needs the fresh version first.
  const revokeOne = async (key: ApiKey) => {
    try {
      await revokeApiKey(project.id, key);
    } catch (error) {
      const failure = asUiError(error);
      if (failure.kind !== "not_found") {
        if (failure.code === "precondition_failed") await refresh();
        throw error;
      }
    }
    await refresh();
  };

  return (
    <>
      {/* `relative` keeps the visually hidden labels in the rows inside the scroller; positioned
          against the page instead, they widen it on a phone. */}
      <div className="relative overflow-x-auto">
        <table className="w-full min-w-[44rem] text-start text-data">
          <thead>
            <tr className="text-label text-ink-soft">
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Public key
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Note
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Scopes
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Last used
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Status
              </th>
              <th scope="col" className="h-8 text-start font-medium">
                <span className="sr-only">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {keys.map((key) => {
              const status = keyStatus(key, asOf);
              return (
                <tr key={key.id} className="h-10 border-t border-ash-border">
                  <td className="pe-4 font-mono">{key.public_key}</td>
                  <td className="max-w-60 truncate pe-4">{key.note}</td>
                  <td className="pe-4">{key.scopes.join(", ")}</td>
                  <td className="pe-4 whitespace-nowrap">
                    {key.last_used_at === null ? "Never" : formatTime(key.last_used_at)}
                  </td>
                  <td className="pe-4">{status}</td>
                  <td className="text-end">
                    {status === "Active" ? (
                      <AlertDialogTrigger controller={revoke} payload={key} className="text-data">
                        Revoke <span className="sr-only">{key.public_key}</span>
                      </AlertDialogTrigger>
                    ) : null}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        controller={revoke}
        question={(key) => (key?.note ? `Revoke the key "${key.note}"?` : "Revoke this key?")}
        description="Apps that use it stop being able to send or read data within seconds. This can't be undone."
        confirm="Revoke key"
        pending="Revoking"
        agentConfirm="Revokes the API key; apps that use it stop working"
        failed="Couldn't revoke the key."
        copy={REVOKE_COPY}
        run={revokeOne}
      />
    </>
  );
}

function CreateKeyDialog({ project }: { project: SessionProject }) {
  const queryClient = useQueryClient();
  const create = useMutation({
    mutationFn: (note: string) => createApiKey(project.id, note ? { note } : {}),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keysKey(project.id) }),
  });
  const failure = create.isError ? asUiError(create.error) : null;

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    create.mutate(String(new FormData(event.currentTarget).get("note")).trim());
  };

  return (
    <Dialog
      onOpenChangeComplete={(open) => {
        // The secret shows once: closing forgets it.
        if (!open) create.reset();
      }}
    >
      <DialogTrigger className="text-data">Create key</DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        {create.isSuccess ? (
          <NewKey created={create.data} />
        ) : (
          <>
            <div className="flex flex-col gap-2">
              <DialogTitle>Create an API key for {project.name}</DialogTitle>
              <DialogDescription>
                The key can send traces, read the project&apos;s data and write feedback, tags and
                comments.
              </DialogDescription>
            </div>
            <form
              className="flex min-h-0 flex-col gap-4"
              onSubmit={submit}
              onInput={() => {
                if (failure) create.reset();
              }}
            >
              <DialogBody>
                <Field name="note">
                  <FieldLabel>Note</FieldLabel>
                  <Input autoComplete="off" />
                  <FieldDescription>
                    Optional. What uses this key, like checkout-service.
                  </FieldDescription>
                </Field>
              </DialogBody>
              {failure ? (
                <FormAlert copy={failureCopy(failure, "Couldn't create the key.")} />
              ) : null}
              <div className="flex justify-end gap-2">
                <DialogClose>Cancel</DialogClose>
                <Button
                  type="submit"
                  data-variant="solid"
                  disabled={create.isPending}
                  aria-busy={create.isPending}
                >
                  {create.isPending ? "Creating the key" : "Create key"}
                </Button>
              </div>
            </form>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function NewKey({ created }: { created: ApiKeyCreated }) {
  return (
    <>
      <div className="flex flex-col gap-2">
        <DialogTitle>Copy your secret key</DialogTitle>
        <DialogDescription>
          You won&apos;t see this secret key again: Crucible keeps only a hash of it. Store it
          somewhere safe, like your app&apos;s secret store, before you close this.
        </DialogDescription>
      </div>
      <ShowOnce label="Secret key" value={created.secret_key} copyLabel="Copy secret key" />
      <ShowOnce label="Public key" value={created.key.public_key} copyLabel="Copy public key" />
      <div className="flex justify-end">
        <DialogClose data-variant="solid">Done</DialogClose>
      </div>
    </>
  );
}
