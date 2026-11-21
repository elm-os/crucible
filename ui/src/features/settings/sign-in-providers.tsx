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
  PasswordInput,
  PasswordInputControl,
  PasswordInputToggle,
} from "@ui-sdk/react/password-input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { type FormEvent, type ReactNode, useState } from "react";
import {
  createAuthProvider,
  deleteAuthProvider,
  listAuthProviders,
  patchAuthProvider,
} from "@/api/client";
import { asUiError } from "@/api/errors";
import type { AuthProvider } from "@/api/generated/AuthProvider";
import type { AuthProviderCreate } from "@/api/generated/AuthProviderCreate";
import type { AuthProviderList } from "@/api/generated/AuthProviderList";
import type { AuthProviderPatch } from "@/api/generated/AuthProviderPatch";
import type { ProviderKind } from "@/api/generated/ProviderKind";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { AlertIcon, CheckIcon, UnsetIcon } from "@/components/icons";
import { Section, SectionFrame } from "@/components/section";
import { useWide } from "@/features/dashboards/shared";
import { useAuth } from "@/lib/auth";
import { ConfirmDialog } from "./confirm-dialog";
import { parseDomains, sameDomains } from "./domains";
import { RowsSkeleton } from "./rows-skeleton";

const TITLE = "Sign-in providers";
export const PROVIDERS_KEY = ["org", "auth-providers"];
// At least one character that isn't a space: the server trims these before it checks them.
const TEXT_PATTERN = ".*\\S.*";

const KIND_LABEL: Record<ProviderKind, string> = {
  github: "GitHub",
  google: "Google",
  oidc: "OpenID Connect",
};

const KINDS = (["github", "google", "oidc"] as const).map((value) => ({
  value,
  label: KIND_LABEL[value],
}));

const isKind = (value: string | null): value is ProviderKind =>
  value !== null && Object.hasOwn(KIND_LABEL, value);

// What the sign-in button says until the person writes their own.
const BUTTON_TEXT: Record<ProviderKind, string> = {
  github: "Continue with GitHub",
  google: "Continue with Google",
  oidc: "",
};

// Where each kind of provider takes the callback URL.
const CALLBACK_HELP: Record<ProviderKind, string> = {
  github: "In the GitHub OAuth app, set the authorization callback URL to this address.",
  google: "In the Google OAuth client, add this address to the authorized redirect URIs.",
  oidc: "Register this address with the provider as a redirect URI.",
};

export type ProviderState = "on" | "needs_public_url" | "off";

// Whether the sign-in page offers the provider: it must be on, and its callback URL is built from
// server.public_url, without which the server offers no provider at all.
export function providerState(
  provider: Pick<AuthProvider, "enabled" | "redirect_uri">,
): ProviderState {
  if (!provider.enabled) return "off";
  return provider.redirect_uri === "" ? "needs_public_url" : "on";
}

const CHANGE_COPY: CopyByCode = {
  forbidden: {
    title: "Crucible refused to change this provider.",
    detail:
      "Only organization owners and admins change sign-in providers, and one from the server configuration changes only there.",
  },
  not_found: {
    title: "This provider isn't there anymore.",
    detail: "Someone may have deleted it. The list shows the current providers.",
  },
};

const ADD_COPY: CopyByCode = {
  forbidden: {
    title: "Only organization owners and admins add sign-in providers.",
    detail: "Ask an owner or admin of the organization.",
  },
};

export function SignInSettings() {
  const auth = useAuth();
  const wide = useWide();
  if (auth.mode !== "session") {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="Sign-in providers need a control plane.">
          This server runs without one, so it has no sign-in. Set postgres.url
          (CRUCIBLE_POSTGRES_URL) to turn sign-in on.
        </EmptyState>
      </SectionFrame>
    );
  }
  // Reading providers needs the org settings permission (contract 11.6), so nothing is fetched.
  const { role } = auth.session.org;
  if (role !== "owner" && role !== "admin") {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState
          kind="unavailable"
          title="Only organization owners and admins manage sign-in providers."
        >
          Ask an owner or admin of the organization to add or change one.
        </EmptyState>
      </SectionFrame>
    );
  }
  return <ProvidersSection editable={wide} />;
}

function ProvidersSection({ editable }: { editable: boolean }) {
  const query = useQuery({
    queryKey: PROVIDERS_KEY,
    queryFn: ({ signal }) => listAuthProviders(signal),
  });
  return (
    <Section
      title={TITLE}
      label="sign-in providers"
      query={query}
      actions={editable ? <AddProviderDialog /> : undefined}
      skeleton={<RowsSkeleton />}
      isEmpty={(list) => list.data.length === 0}
      empty={
        <EmptyState kind="first-use" title="No sign-in providers yet.">
          Add GitHub, Google or an OpenID Connect provider, and people sign in with the account they
          already have there.
          {editable ? null : " Add one on a screen at least 768 pixels wide."}
        </EmptyState>
      }
    >
      {(list) => <ProviderList providers={list.data} editable={editable} />}
    </Section>
  );
}

function ProviderList({ providers, editable }: { providers: AuthProvider[]; editable: boolean }) {
  const queryClient = useQueryClient();
  const [remove] = useState(() => createAlertDialogController<AuthProvider>());

  // A provider someone else already deleted needs nothing more.
  const removeOne = async (provider: AuthProvider) => {
    try {
      await deleteAuthProvider(provider.id);
    } catch (error) {
      if (asUiError(error).kind !== "not_found") throw error;
    }
    await queryClient.invalidateQueries({ queryKey: PROVIDERS_KEY });
  };

  return (
    <div className="flex flex-col gap-3">
      {providers.some((provider) => provider.redirect_uri === "") ? (
        <div className="flex items-start gap-2 text-data">
          <AlertIcon className="mt-0.5 shrink-0" />
          <div className="flex flex-col gap-0.5">
            <p className="font-medium">
              The sign-in page offers no provider until server.public_url is set.
            </p>
            <p className="text-ink-soft">
              Set server.public_url (CRUCIBLE_SERVER_PUBLIC_URL) to the address people open Crucible
              at, such as https://crucible.example.com, then restart Crucible. Each provider&apos;s
              callback URL is built from it.
            </p>
          </div>
        </div>
      ) : null}
      <ul className="flex flex-col">
        {providers.map((provider) => (
          <ProviderItem
            key={provider.id}
            provider={provider}
            editable={editable}
            remove={
              <AlertDialogTrigger controller={remove} payload={provider} className="text-data">
                Delete <span className="sr-only">{provider.display_name}</span>
              </AlertDialogTrigger>
            }
          />
        ))}
      </ul>
      {editable ? (
        <ConfirmDialog
          controller={remove}
          question={(provider) =>
            provider ? `Delete ${provider.display_name}?` : "Delete this provider?"
          }
          description="It leaves the sign-in page and its client secret is deleted. Accounts that signed in with it stay, and can sign in through another provider that verifies the same email address. This can't be undone."
          confirm="Delete provider"
          pending="Deleting"
          agentConfirm="Deletes the sign-in provider and its client secret"
          failed="Couldn't delete the provider."
          copy={CHANGE_COPY}
          run={removeOne}
        />
      ) : (
        <p className="text-label text-ink-soft">Editing needs a wider screen.</p>
      )}
    </div>
  );
}

const STATE: Record<ProviderState, { label: string; icon: ReactNode }> = {
  on: { label: "On", icon: <CheckIcon className="shrink-0" /> },
  needs_public_url: {
    label: "Not offered: server.public_url isn't set",
    icon: <AlertIcon className="shrink-0" />,
  },
  off: { label: "Turned off", icon: <UnsetIcon className="shrink-0" /> },
};

function ProviderItem({
  provider,
  editable,
  remove,
}: {
  provider: AuthProvider;
  editable: boolean;
  remove: ReactNode;
}) {
  const state = providerState(provider);
  return (
    <li
      data-provider={provider.display_name}
      className="flex flex-col gap-3 border-t border-ash-border py-3 first:border-t-0"
    >
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <p className="min-w-0 text-data font-medium break-words">
          {provider.display_name}{" "}
          <span className="font-normal text-ink-soft">{KIND_LABEL[provider.kind]}</span>
        </p>
        <span data-status={state} className="flex items-center gap-1.5 text-data">
          {STATE[state].icon}
          {STATE[state].label}
        </span>
      </div>
      <dl className="grid min-w-0 gap-x-4 gap-y-1 text-data sm:grid-cols-[10rem_minmax(0,1fr)]">
        {provider.kind === "oidc" ? (
          <>
            <dt className="text-ink-soft">Issuer</dt>
            <dd className="font-mono wrap-anywhere">{provider.issuer}</dd>
          </>
        ) : null}
        <dt className="text-ink-soft">Client ID</dt>
        <dd className="font-mono wrap-anywhere">{provider.client_id}</dd>
        <dt className="text-ink-soft">Client secret</dt>
        <dd>Stored sealed; never shown</dd>
        {provider.scopes.length > 0 ? (
          <>
            <dt className="text-ink-soft">Extra scopes</dt>
            <dd className="font-mono wrap-anywhere">{provider.scopes.join(" ")}</dd>
          </>
        ) : null}
        <dt className="text-ink-soft">Allowed email domains</dt>
        <dd className="wrap-anywhere">
          {provider.allowed_domains.length > 0
            ? provider.allowed_domains.join(", ")
            : "Any address the provider verifies"}
        </dd>
        <dt className="text-ink-soft">Callback URL</dt>
        <dd className="flex min-w-0 flex-col items-start gap-1">
          {provider.redirect_uri === "" ? (
            "Shows here once server.public_url is set."
          ) : (
            <>
              <code className="font-mono wrap-anywhere select-all">{provider.redirect_uri}</code>
              <CopyButton
                value={provider.redirect_uri}
                label="Copy callback URL"
                name={provider.display_name}
              />
            </>
          )}
        </dd>
      </dl>
      {provider.seeded ? (
        <p className="text-label text-ink-soft">
          From the server configuration, so it changes only there.
        </p>
      ) : editable ? (
        <ProviderActions provider={provider} remove={remove} />
      ) : null}
    </li>
  );
}

function useReplaceRow() {
  const queryClient = useQueryClient();
  return (updated: AuthProvider) =>
    queryClient.setQueryData<AuthProviderList>(PROVIDERS_KEY, (list) =>
      list ? { data: list.data.map((row) => (row.id === updated.id ? updated : row)) } : list,
    );
}

// A provider someone else deleted leaves the list; anything else stays where it failed.
function useRefreshWhenGone() {
  const queryClient = useQueryClient();
  return (error: unknown) => {
    if (asUiError(error).kind === "not_found") {
      return queryClient.invalidateQueries({ queryKey: PROVIDERS_KEY });
    }
  };
}

function ProviderActions({ provider, remove }: { provider: AuthProvider; remove: ReactNode }) {
  const replaceRow = useReplaceRow();
  const refreshWhenGone = useRefreshWhenGone();
  const toggle = useMutation({
    mutationFn: () => patchAuthProvider(provider.id, { enabled: !provider.enabled }),
    onSuccess: replaceRow,
    onError: refreshWhenGone,
  });
  const failure = toggle.isError ? asUiError(toggle.error) : null;
  return (
    <div className="flex flex-col items-start gap-2">
      <div className="flex flex-wrap items-center gap-2">
        <EditProviderDialog provider={provider} />
        <ReplaceSecretDialog provider={provider} />
        <Button
          className="text-data"
          onClick={() => toggle.mutate()}
          disabled={toggle.isPending}
          aria-busy={toggle.isPending}
          data-agent-confirm={
            provider.enabled
              ? "Turns off the sign-in provider, so nobody can sign in with it"
              : "Turns on the sign-in provider"
          }
        >
          {provider.enabled ? "Turn off" : "Turn on"}{" "}
          <span className="sr-only">{provider.display_name}</span>
        </Button>
        {remove}
      </div>
      {failure ? (
        <FormAlert
          copy={failureCopy(failure, "Couldn't change the provider.", CHANGE_COPY)}
          requestId={failure.requestId}
        />
      ) : null}
    </div>
  );
}

function EditProviderDialog({ provider }: { provider: AuthProvider }) {
  const replaceRow = useReplaceRow();
  const refreshWhenGone = useRefreshWhenGone();
  const [open, setOpen] = useState(false);
  const [name, setName] = useState(provider.display_name);
  const [domainText, setDomainText] = useState(provider.allowed_domains.join(", "));
  const domains = parseDomains(domainText);
  const save = useMutation({
    mutationFn: (body: AuthProviderPatch) => patchAuthProvider(provider.id, body),
    onSuccess: (updated) => {
      replaceRow(updated);
      setOpen(false);
    },
    onError: refreshWhenGone,
  });
  const failure = save.isError ? asUiError(save.error) : null;
  // Only what changed is sent, and never a client secret: Replace secret is the one way to set it.
  const patch: AuthProviderPatch = {};
  if (name.trim() !== provider.display_name) patch.display_name = name.trim();
  if (domains !== "invalid" && !sameDomains(domains, provider.allowed_domains)) {
    patch.allowed_domains = domains;
  }
  const changed = domains === "invalid" || Object.keys(patch).length > 0;

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (domains === "invalid" || !changed || save.isPending) return;
    save.mutate(patch);
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        // Each opening starts from the provider as it is now.
        if (next) {
          setName(provider.display_name);
          setDomainText(provider.allowed_domains.join(", "));
          save.reset();
        }
        setOpen(next);
      }}
    >
      <DialogTrigger className="text-data">
        Edit <span className="sr-only">{provider.display_name}</span>
      </DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        <div className="flex flex-col gap-2">
          <DialogTitle>Change {provider.display_name}</DialogTitle>
          <DialogDescription>
            To use another client ID or issuer, add the provider again, then delete this one.
          </DialogDescription>
        </div>
        <form
          className="flex flex-col gap-4"
          onSubmit={submit}
          onInput={() => {
            if (failure) save.reset();
          }}
        >
          <Field name="display_name" invalid={fieldFailed(failure, "display_name") || undefined}>
            <FieldLabel>Button text</FieldLabel>
            <Input
              required
              maxLength={200}
              pattern={TEXT_PATTERN}
              autoComplete="off"
              value={name}
              onValueChange={setName}
            />
            <FieldError match="valueMissing">Enter the text for the sign-in button.</FieldError>
            <FieldError match="patternMismatch">Enter the text for the sign-in button.</FieldError>
            <FieldError match={fieldFailed(failure, "display_name")}>
              Use 1 to 200 characters.
            </FieldError>
          </Field>
          <DomainsField
            value={domainText}
            onChange={setDomainText}
            invalid={domains === "invalid"}
            refused={fieldFailed(failure, "allowed_domains")}
          />
          {failure ? (
            <FormAlert
              copy={failureCopy(failure, "Couldn't save the provider.", CHANGE_COPY)}
              requestId={failure.requestId}
            />
          ) : null}
          <div className="flex justify-end gap-2">
            <DialogClose>Cancel</DialogClose>
            <Button
              type="submit"
              data-variant="solid"
              disabled={!changed || save.isPending}
              focusableWhenDisabled
              aria-busy={save.isPending}
              data-agent-confirm="Changes the sign-in provider's button text or allowed email domains"
            >
              {save.isPending ? "Saving" : "Save changes"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function DomainsField({
  value,
  onChange,
  invalid,
  refused,
}: {
  value: string;
  onChange: (text: string) => void;
  invalid: boolean;
  refused: boolean;
}) {
  return (
    <Field name="allowed_domains" invalid={invalid || refused || undefined}>
      <FieldLabel>Allowed email domains</FieldLabel>
      <Input
        autoComplete="off"
        spellCheck={false}
        placeholder="Any"
        value={value}
        onValueChange={onChange}
      />
      <FieldDescription>
        Optional. Only people with an email address at these domains sign in with this provider.
        Separate domains with commas.
      </FieldDescription>
      <FieldError match={invalid}>
        Enter domains only, like example.com, separated by commas.
      </FieldError>
      <FieldError match={!invalid && refused}>
        The server refused these domains. Enter bare domains, like example.com.
      </FieldError>
    </Field>
  );
}

function SecretField({ label }: { label: string }) {
  return (
    <Field name="client_secret">
      <FieldLabel>{label}</FieldLabel>
      <PasswordInput name="client_secret" required autoComplete="off">
        <PasswordInputControl spellCheck={false} pattern={TEXT_PATTERN} />
        <PasswordInputToggle aria-label="Show client secret" />
      </PasswordInput>
      <FieldDescription>Crucible stores it sealed and never shows it again.</FieldDescription>
      <FieldError match="valueMissing">Enter the client secret the provider issued.</FieldError>
      <FieldError match="patternMismatch">Enter the client secret the provider issued.</FieldError>
    </Field>
  );
}

function ReplaceSecretDialog({ provider }: { provider: AuthProvider }) {
  const replaceRow = useReplaceRow();
  const refreshWhenGone = useRefreshWhenGone();
  const replace = useMutation({
    mutationFn: (secret: string) => patchAuthProvider(provider.id, { client_secret: secret }),
    onSuccess: replaceRow,
    onError: refreshWhenGone,
  });
  const failure = replace.isError ? asUiError(replace.error) : null;

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const secret = String(new FormData(event.currentTarget).get("client_secret")).trim();
    if (secret !== "" && !replace.isPending) replace.mutate(secret);
  };

  return (
    <Dialog
      onOpenChangeComplete={(open) => {
        // The form forgets the secret once the dialog closes.
        if (!open) replace.reset();
      }}
    >
      <DialogTrigger className="text-data">
        Replace secret <span className="sr-only">of {provider.display_name}</span>
      </DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        {replace.isSuccess ? (
          <>
            <div className="flex flex-col gap-2">
              <DialogTitle>Client secret replaced</DialogTitle>
              <DialogDescription>
                {provider.display_name} uses the new secret from the next sign-in.
              </DialogDescription>
            </div>
            <div className="flex justify-end">
              <DialogClose data-variant="solid">Done</DialogClose>
            </div>
          </>
        ) : (
          <>
            <div className="flex flex-col gap-2">
              <DialogTitle>New client secret for {provider.display_name}</DialogTitle>
              <DialogDescription>
                Paste the secret the provider issued. Sign-ins use it from the moment you save it.
              </DialogDescription>
            </div>
            <form
              className="flex flex-col gap-4"
              onSubmit={submit}
              onInput={() => {
                if (failure) replace.reset();
              }}
            >
              <SecretField label="New client secret" />
              {failure ? (
                <FormAlert
                  copy={failureCopy(failure, "Couldn't replace the secret.", CHANGE_COPY)}
                  requestId={failure.requestId}
                />
              ) : null}
              <div className="flex justify-end gap-2">
                <DialogClose>Cancel</DialogClose>
                <Button
                  type="submit"
                  data-variant="solid"
                  disabled={replace.isPending}
                  aria-busy={replace.isPending}
                  data-agent-confirm="Replaces the client secret Crucible sends to the sign-in provider"
                >
                  {replace.isPending ? "Saving" : "Save secret"}
                </Button>
              </div>
            </form>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function AddProviderDialog() {
  const queryClient = useQueryClient();
  const [kind, setKind] = useState<ProviderKind>("github");
  // What the person wrote on the button; until then it follows the kind.
  const [buttonText, setButtonText] = useState<string | null>(null);
  const [domainText, setDomainText] = useState("");
  const domains = parseDomains(domainText);
  const create = useMutation({
    mutationFn: createAuthProvider,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: PROVIDERS_KEY }),
  });
  const failure = create.isError ? asUiError(create.error) : null;
  const text = buttonText ?? BUTTON_TEXT[kind];

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (domains === "invalid" || create.isPending) return;
    const form = new FormData(event.currentTarget);
    const field = (name: string) => String(form.get(name) ?? "").trim();
    const scopes = field("scopes")
      .split(/[\s,]+/)
      .filter((scope) => scope !== "");
    const body: AuthProviderCreate = {
      kind,
      display_name: text.trim(),
      client_id: field("client_id"),
      client_secret: field("client_secret"),
    };
    if (kind === "oidc") body.issuer = field("issuer");
    if (kind === "oidc" && scopes.length > 0) body.scopes = scopes;
    if (domains.length > 0) body.allowed_domains = domains;
    create.mutate(body);
  };

  return (
    <Dialog
      onOpenChangeComplete={(open) => {
        // The secret is forgotten once the dialog closes.
        if (!open) {
          create.reset();
          setKind("github");
          setButtonText(null);
          setDomainText("");
        }
      }}
    >
      <DialogTrigger className="text-data">Add provider</DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        {create.isSuccess ? (
          <ProviderAdded provider={create.data} />
        ) : (
          <>
            <div className="flex flex-col gap-2">
              <DialogTitle>Add a sign-in provider</DialogTitle>
              <DialogDescription>
                Register Crucible with the provider first, to get a client ID and secret. The
                callback URL to give the provider shows once you add it here.
              </DialogDescription>
            </div>
            <form
              className="flex flex-col gap-4"
              onSubmit={submit}
              onInput={() => {
                if (failure) create.reset();
              }}
            >
              <Field>
                <FieldLabel>Provider</FieldLabel>
                <Select
                  items={KINDS}
                  value={kind}
                  onValueChange={(next) => {
                    if (!isKind(next)) return;
                    setKind(next);
                    create.reset();
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
              <Field
                name="display_name"
                invalid={fieldFailed(failure, "display_name") || undefined}
              >
                <FieldLabel>Button text</FieldLabel>
                <Input
                  required
                  maxLength={200}
                  pattern={TEXT_PATTERN}
                  autoComplete="off"
                  placeholder="Continue with Acme SSO"
                  value={text}
                  onValueChange={setButtonText}
                />
                <FieldDescription>What the button on the sign-in page says.</FieldDescription>
                <FieldError match="valueMissing">Enter the text for the sign-in button.</FieldError>
                <FieldError match="patternMismatch">
                  Enter the text for the sign-in button.
                </FieldError>
                <FieldError match={fieldFailed(failure, "display_name")}>
                  Use 1 to 200 characters.
                </FieldError>
              </Field>
              {kind === "oidc" ? (
                <Field name="issuer" invalid={fieldFailed(failure, "issuer") || undefined}>
                  <FieldLabel>Issuer URL</FieldLabel>
                  <Input
                    type="url"
                    required
                    autoComplete="off"
                    spellCheck={false}
                    placeholder="https://login.example.com"
                    className="font-mono"
                  />
                  <FieldDescription>
                    Crucible reads the provider&apos;s settings from this address followed by
                    /.well-known/openid-configuration.
                  </FieldDescription>
                  <FieldError match="valueMissing">Enter the issuer URL.</FieldError>
                  <FieldError match="typeMismatch">
                    Enter a full URL, like https://login.example.com.
                  </FieldError>
                  <FieldError match={fieldFailed(failure, "issuer")}>
                    Enter the issuer URL, starting with https:// or http://.
                  </FieldError>
                </Field>
              ) : null}
              <Field name="client_id" invalid={fieldFailed(failure, "client_id") || undefined}>
                <FieldLabel>Client ID</FieldLabel>
                <Input
                  required
                  pattern={TEXT_PATTERN}
                  autoComplete="off"
                  spellCheck={false}
                  className="font-mono"
                />
                <FieldError match="valueMissing">
                  Enter the client ID the provider issued.
                </FieldError>
                <FieldError match="patternMismatch">
                  Enter the client ID the provider issued.
                </FieldError>
                <FieldError match={fieldFailed(failure, "client_id")}>
                  Enter the client ID and secret the provider issued.
                </FieldError>
              </Field>
              <SecretField label="Client secret" />
              {kind === "oidc" ? (
                <Field name="scopes">
                  <FieldLabel>Extra scopes</FieldLabel>
                  <Input
                    autoComplete="off"
                    spellCheck={false}
                    placeholder="None"
                    className="font-mono"
                  />
                  <FieldDescription>
                    Optional, separated by spaces. Crucible always asks for openid, email and
                    profile.
                  </FieldDescription>
                </Field>
              ) : null}
              <DomainsField
                value={domainText}
                onChange={setDomainText}
                invalid={domains === "invalid"}
                refused={fieldFailed(failure, "allowed_domains")}
              />
              {failure ? (
                <FormAlert
                  copy={failureCopy(failure, "Couldn't add the provider.", ADD_COPY)}
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
                  data-agent-confirm="Adds a sign-in provider, so people can sign in with it"
                >
                  {create.isPending ? "Adding" : `Add ${KIND_LABEL[kind]}`}
                </Button>
              </div>
            </form>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function ProviderAdded({ provider }: { provider: AuthProvider }) {
  return (
    <>
      <div className="flex flex-col gap-2">
        <DialogTitle>{provider.display_name} is added</DialogTitle>
        <DialogDescription>
          Crucible stores the client secret sealed and never shows it again.
        </DialogDescription>
      </div>
      {provider.redirect_uri === "" ? (
        <p className="text-data">
          The sign-in page offers it once server.public_url (CRUCIBLE_SERVER_PUBLIC_URL) is set.
          Then give the provider the callback URL that the list shows.
        </p>
      ) : (
        <div className="flex flex-col gap-2">
          <p className="text-label font-medium">Callback URL</p>
          <code className="block rounded-md border border-ash-border bg-warm-sand px-3 py-2 font-mono text-data break-all select-all">
            {provider.redirect_uri}
          </code>
          <CopyButton
            value={provider.redirect_uri}
            label="Copy callback URL"
            name={provider.display_name}
          />
          <p className="text-data text-ink-soft">{CALLBACK_HELP[provider.kind]}</p>
        </div>
      )}
      <div className="flex justify-end">
        <DialogClose data-variant="solid">Done</DialogClose>
      </div>
    </>
  );
}

// The browser has no clipboard API on plain-HTTP origins, so the value itself selects with one
// click and the button says when copying fails.
function CopyButton({ value, label, name }: { value: string; label: string; name: string }) {
  const [copied, setCopied] = useState<boolean | null>(null);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };
  return (
    <span className="flex flex-wrap items-center gap-3">
      <Button className="text-data" onClick={copy}>
        {label} <span className="sr-only">of {name}</span>
      </Button>
      <output className="text-label text-ink-soft">{copied ? "Copied." : ""}</output>
      {copied === false ? (
        <span role="alert" className="text-label">
          Couldn&apos;t copy here. Select the URL and copy it yourself.
        </span>
      ) : null}
    </span>
  );
}
