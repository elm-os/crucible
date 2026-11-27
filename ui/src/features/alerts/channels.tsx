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
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { type FormEvent, useState } from "react";
import {
  createChannel,
  deleteChannel,
  listChannels,
  patchChannel,
  testChannel,
} from "@/api/client";
import { asUiError, type UiError } from "@/api/errors";
import type { Channel } from "@/api/generated/Channel";
import type { ChannelCreate } from "@/api/generated/ChannelCreate";
import type { ChannelKind } from "@/api/generated/ChannelKind";
import type { ChannelList } from "@/api/generated/ChannelList";
import type { SessionProject } from "@/api/generated/SessionProject";
import { DialogBody } from "@/components/dialog-body";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { AlertIcon, CheckIcon } from "@/components/icons";
import { Section, SectionFrame } from "@/components/section";
import { ConfirmDialog } from "@/features/settings/confirm-dialog";
import { RowsSkeleton } from "@/features/settings/rows-skeleton";
import { ShowOnce } from "@/features/settings/show-once";
import { formatTime } from "@/lib/format";
import { KIND_LABEL, testFailure, urlRefusal } from "./format";
import { AlertsAccess, alertKeys } from "./shared";

const TITLE = "Channels";

const KINDS: { value: ChannelKind; label: string }[] = [
  { value: "webhook", label: "Webhook" },
  { value: "slack", label: "Slack" },
  { value: "email", label: "Email" },
];

const CHANGED: CopyByCode = {
  precondition_failed: {
    title: "This channel changed since the list loaded.",
    detail: "The list is up to date now. Check the channel, then try again.",
  },
};

const SAVE_COPY: CopyByCode = {
  ...CHANGED,
  ssrf_blocked: {
    title: "Crucible won't call that address.",
    detail: "Check the URL field for what to change.",
  },
};

export function ChannelsSettings() {
  return (
    <AlertsAccess title={TITLE}>
      {(project) =>
        // Listing channels needs the permission to edit rules (contract 12.6), so nothing is fetched.
        project.permissions.includes("edit_content") || project.permissions.includes("channels") ? (
          <ChannelsSection project={project} />
        ) : (
          <SectionFrame title={`Channels for ${project.name}`}>
            <EmptyState kind="unavailable" title="Only project members and admins see channels.">
              Channels hold secrets. Ask an admin of {project.name} for the member role to pick
              channels for a rule, or the admin role to manage them.
            </EmptyState>
          </SectionFrame>
        )
      }
    </AlertsAccess>
  );
}

function ChannelsSection({ project }: { project: SessionProject }) {
  const canManage = project.permissions.includes("channels");
  const query = useQuery({
    queryKey: alertKeys.channels(project.id),
    queryFn: ({ signal }) => listChannels(project.id, signal),
  });
  return (
    <Section
      title={`Channels for ${project.name}`}
      label="channels"
      query={query}
      actions={canManage ? <CreateChannelDialog project={project} /> : undefined}
      skeleton={<RowsSkeleton />}
      isEmpty={(list) => list.data.length === 0}
      empty={
        <EmptyState kind="first-use" title="No channels yet.">
          {canManage
            ? "A channel is where alert rules send notifications: a Slack incoming webhook, email, or a signed webhook. Create one with New channel."
            : "A channel is where alert rules send notifications. Project admins create channels, because they hold secrets."}
        </EmptyState>
      }
    >
      {(list) => <ChannelsTable channels={list.data} project={project} canManage={canManage} />}
    </Section>
  );
}

function target(channel: Channel): string {
  if (channel.kind === "slack") return channel.url_hint ?? "Slack incoming webhook";
  if (channel.kind === "email") return channel.to.length > 0 ? channel.to.join(", ") : "Email";
  return channel.url ?? "Webhook";
}

function ChannelsTable({
  channels,
  project,
  canManage,
}: {
  channels: Channel[];
  project: SessionProject;
  canManage: boolean;
}) {
  const queryClient = useQueryClient();
  const [remove] = useState(() => createAlertDialogController<Channel>());
  const refresh = () => queryClient.invalidateQueries({ queryKey: alertKeys.channels(project.id) });

  const removeOne = async (channel: Channel) => {
    try {
      await deleteChannel(project.id, channel);
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
      <ul className="flex flex-col">
        {channels.map((channel) => (
          <li
            key={channel.id}
            data-channel={channel.name}
            className="flex flex-col gap-2 border-t border-ash-border py-3 first:border-t-0"
          >
            <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
              <div className="flex min-w-0 flex-col gap-0.5">
                <p className="text-data font-medium">
                  {channel.name}{" "}
                  <span className="font-normal text-ink-soft">{KIND_LABEL[channel.kind]}</span>
                </p>
                {canManage ? (
                  <p className="truncate font-mono text-label text-ink-soft">{target(channel)}</p>
                ) : null}
              </div>
              <ChannelState channel={channel} />
            </div>
            {canManage ? (
              <ChannelActions
                channel={channel}
                project={project}
                remove={
                  <AlertDialogTrigger controller={remove} payload={channel} className="text-data">
                    Delete <span className="sr-only">{channel.name}</span>
                  </AlertDialogTrigger>
                }
              />
            ) : null}
          </li>
        ))}
      </ul>
      {canManage ? (
        <ConfirmDialog
          controller={remove}
          question={(channel) =>
            channel ? `Delete the channel "${channel.name}"?` : "Delete this channel?"
          }
          description="Rules that notify it stop notifying it, and its secret is deleted. This can't be undone."
          confirm="Delete channel"
          pending="Deleting"
          agentConfirm="Deletes the notification channel and its secret"
          failed="Couldn't delete the channel."
          copy={CHANGED}
          run={removeOne}
        />
      ) : null}
    </>
  );
}

function ChannelState({ channel }: { channel: Channel }) {
  if (channel.enabled) {
    return (
      <span data-status="on" className="flex items-center gap-1.5 text-data">
        <CheckIcon className="shrink-0" />
        {channel.failures > 0 ? `On, last ${channel.failures} deliveries failed` : "On"}
      </span>
    );
  }
  return (
    <span data-status="off" className="flex items-center gap-1.5 text-data">
      <AlertIcon className="shrink-0" />
      {channel.disabled_reason === undefined
        ? "Turned off"
        : "Turned off by Crucible: 50 deliveries in a row failed"}
    </span>
  );
}

function ChannelActions({
  channel,
  project,
  remove,
}: {
  channel: Channel;
  project: SessionProject;
  remove: React.ReactNode;
}) {
  const queryClient = useQueryClient();
  const replace = (updated: Channel) =>
    queryClient.setQueryData<ChannelList>(alertKeys.channels(project.id), (list) =>
      list
        ? {
            data: list.data.map((row) =>
              row.id === updated.id ? { ...updated, signing_secret: undefined } : row,
            ),
          }
        : list,
    );
  const refresh = () => queryClient.invalidateQueries({ queryKey: alertKeys.channels(project.id) });
  const onError = (error: unknown) => {
    if (asUiError(error).code === "precondition_failed") return refresh();
  };
  const test = useMutation({
    mutationFn: () => testChannel(project.id, channel.id),
    onSettled: refresh,
  });
  const toggle = useMutation({
    mutationFn: () => patchChannel(project.id, channel, { enabled: !channel.enabled }),
    onSuccess: replace,
    onError,
  });
  const rotate = useMutation({
    mutationFn: () => patchChannel(project.id, channel, { rotate_secret: true }),
    onSuccess: replace,
    onError,
  });
  const failure: UiError | null = toggle.isError
    ? asUiError(toggle.error)
    : rotate.isError
      ? asUiError(rotate.error)
      : null;
  const testError = test.isError ? asUiError(test.error) : null;

  return (
    <div className="flex flex-col items-start gap-2">
      <div className="flex flex-wrap items-center gap-2">
        <Button
          className="text-data"
          onClick={() => test.mutate()}
          disabled={test.isPending}
          aria-busy={test.isPending}
        >
          {test.isPending ? "Sending a test" : "Send test"}
          <span className="sr-only"> to {channel.name}</span>
        </Button>
        <Button
          className="text-data"
          onClick={() => toggle.mutate()}
          disabled={toggle.isPending}
          aria-busy={toggle.isPending}
        >
          {channel.enabled ? "Turn off" : "Turn on"}
          <span className="sr-only"> {channel.name}</span>
        </Button>
        {channel.kind === "webhook" ? (
          <Button
            className="text-data"
            onClick={() => rotate.mutate()}
            disabled={rotate.isPending}
            aria-busy={rotate.isPending}
            data-agent-confirm="Replaces the webhook signing secret; the receiver must be given the new one"
          >
            {rotate.isPending ? "Rotating the secret" : "Rotate secret"}
            <span className="sr-only"> of {channel.name}</span>
          </Button>
        ) : null}
        {remove}
      </div>
      {test.isSuccess ? (
        test.data.ok ? (
          <output data-test="ok" className="flex items-center gap-1.5 text-data">
            <CheckIcon className="shrink-0" />
            The test was delivered in {test.data.latency_ms} ms.
          </output>
        ) : (
          <p role="alert" data-test="failed" className="flex items-start gap-1.5 text-data">
            <AlertIcon className="mt-0.5 shrink-0" />
            {testFailure(test.data.error_code)}
          </p>
        )
      ) : null}
      {testError ? (
        <FormAlert
          copy={failureCopy(testError, "Couldn't send the test.", {
            rate_limited: {
              title: "Too many tests.",
              detail: "A channel takes 6 tests a minute. Wait a minute, then send another.",
            },
          })}
          requestId={testError.requestId}
        />
      ) : null}
      {!test.isSuccess && channel.last_test?.at ? (
        <p className="text-label text-ink-soft">
          Last test {formatTime(channel.last_test.at)}:{" "}
          {channel.last_test.ok ? "delivered" : "not delivered"}
        </p>
      ) : null}
      {failure ? (
        <FormAlert
          copy={failureCopy(failure, "Couldn't change the channel.", CHANGED)}
          requestId={failure.requestId}
        />
      ) : null}
      {rotate.isSuccess && rotate.data.signing_secret ? (
        <div className="flex w-full max-w-xl flex-col gap-2">
          <ShowOnce
            label="New signing secret"
            value={rotate.data.signing_secret}
            copyLabel="Copy signing secret"
          />
          <p className="text-label text-ink-soft">
            You won&apos;t see it again. Give it to the receiver now; deliveries are signed with it
            from this moment.
          </p>
          <Button className="self-start text-data" onClick={() => rotate.reset()}>
            Done
          </Button>
        </div>
      ) : null}
    </div>
  );
}

function CreateChannelDialog({ project }: { project: SessionProject }) {
  const queryClient = useQueryClient();
  const [kind, setKind] = useState<ChannelKind>("webhook");
  const create = useMutation({
    mutationFn: (body: ChannelCreate) => createChannel(project.id, body),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: alertKeys.channels(project.id) }),
  });
  const failure = create.isError ? asUiError(create.error) : null;
  const urlError = failure?.fieldErrors.find((field) => field.pointer === "/url");
  const noMail = failure?.fieldErrors.some((field) => field.code === "smtp_not_configured");

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const name = String(form.get("name")).trim();
    if (kind === "email") {
      const to = String(form.get("to"))
        .split(/[\s,;]+/)
        .filter((address) => address !== "");
      create.mutate({ name, kind, to });
    } else {
      create.mutate({ name, kind, url: String(form.get("url")).trim() });
    }
  };

  return (
    <Dialog
      onOpenChangeComplete={(open) => {
        // The signing secret shows once: closing forgets it.
        if (!open) {
          create.reset();
          setKind("webhook");
        }
      }}
    >
      <DialogTrigger className="text-data">New channel</DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        {create.isSuccess ? (
          <NewChannel channel={create.data} />
        ) : (
          <>
            <div className="flex flex-col gap-2">
              <DialogTitle>New channel for {project.name}</DialogTitle>
              <DialogDescription>
                Where alert rules send notifications. Send a test after you create it.
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
                <Field name="name" invalid={fieldFailed(failure, "name") || undefined}>
                  <FieldLabel>Name</FieldLabel>
                  <Input required maxLength={80} autoComplete="off" />
                  <FieldError match="valueMissing">Enter a name, like On call.</FieldError>
                  <FieldError match={fieldFailed(failure, "name")}>
                    Use 1 to 80 characters.
                  </FieldError>
                </Field>
                <Field>
                  <FieldLabel>Kind</FieldLabel>
                  <Select
                    items={KINDS}
                    value={kind}
                    onValueChange={(next) => {
                      if (next === "webhook" || next === "slack" || next === "email") {
                        setKind(next);
                        create.reset();
                      }
                    }}
                  >
                    <SelectTrigger className="text-data">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {KINDS.map((item) => (
                        <SelectItem key={item.value} value={item.value}>
                          {item.label}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </Field>
                {kind === "email" ? (
                  <Field name="to" invalid={fieldFailed(failure, "to") || noMail || undefined}>
                    <FieldLabel>Recipients</FieldLabel>
                    <Input required autoComplete="off" spellCheck={false} />
                    <FieldDescription>
                      Up to 20 addresses, separated by commas or spaces.
                    </FieldDescription>
                    <FieldError match="valueMissing">Enter at least one email address.</FieldError>
                    <FieldError match={fieldFailed(failure, "to")}>
                      Enter 1 to 20 email addresses, like team@example.com.
                    </FieldError>
                    <FieldError match={noMail === true}>
                      This server has no mail server yet. Ask an operator to set smtp.host and
                      smtp.from, then create the channel.
                    </FieldError>
                  </Field>
                ) : (
                  <Field name="url" invalid={urlError !== undefined || undefined}>
                    <FieldLabel>{kind === "slack" ? "Slack webhook URL" : "URL"}</FieldLabel>
                    <Input
                      type="url"
                      required
                      maxLength={2048}
                      autoComplete="off"
                      spellCheck={false}
                      className="font-mono"
                    />
                    <FieldDescription>
                      {kind === "slack"
                        ? "An incoming-webhook URL that starts with https://hooks.slack.com/. It is stored sealed and never shown again."
                        : "Crucible posts JSON here and signs it with a secret you get next. Addresses on your own network work."}
                    </FieldDescription>
                    <FieldError match="valueMissing">Enter the URL to call.</FieldError>
                    <FieldError match="typeMismatch">
                      Enter a full URL, like https://example.com/hooks/crucible.
                    </FieldError>
                    <FieldError match={urlError !== undefined}>
                      {kind === "slack" && failure?.code !== "ssrf_blocked"
                        ? "Use a Slack incoming-webhook URL that starts with https://hooks.slack.com/. For any other receiver choose Webhook."
                        : urlRefusal(failure?.code === "ssrf_blocked" ? urlError?.code : undefined)}
                    </FieldError>
                  </Field>
                )}
              </DialogBody>
              {failure ? (
                <FormAlert
                  copy={failureCopy(failure, "Couldn't create the channel.", SAVE_COPY)}
                  requestId={failure.requestId}
                />
              ) : null}
              <div className="flex justify-end gap-2">
                <DialogClose>Cancel</DialogClose>
                <Button
                  type="submit"
                  data-variant="solid"
                  disabled={create.isPending}
                  aria-busy={create.isPending}
                >
                  {create.isPending ? "Creating the channel" : "Create channel"}
                </Button>
              </div>
            </form>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function NewChannel({ channel }: { channel: Channel }) {
  return (
    <>
      <div className="flex flex-col gap-2">
        <DialogTitle>
          {channel.signing_secret ? "Copy the signing secret" : `${channel.name} is ready`}
        </DialogTitle>
        <DialogDescription>
          {channel.signing_secret
            ? "You won't see this secret again: Crucible keeps it sealed. The receiver uses it to check the X-Crucible-Signature header of each delivery."
            : "Close this, then use Send test to check that notifications arrive."}
        </DialogDescription>
      </div>
      {channel.signing_secret ? (
        <ShowOnce
          label="Signing secret"
          value={channel.signing_secret}
          copyLabel="Copy signing secret"
        />
      ) : null}
      <div className="flex justify-end">
        <DialogClose data-variant="solid">Done</DialogClose>
      </div>
    </>
  );
}
