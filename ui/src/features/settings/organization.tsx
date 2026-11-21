"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import {
  Field,
  FieldDescription,
  FieldError,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import { Radio, RadioGroup, RadioIndicator } from "@ui-sdk/react/radio";
import { type FormEvent, useState } from "react";
import { getOrg, patchOrg } from "@/api/client";
import { asUiError, type UiError } from "@/api/errors";
import type { Org } from "@/api/generated/Org";
import type { OrgPatch } from "@/api/generated/OrgPatch";
import type { ProjectAccess } from "@/api/generated/ProjectAccess";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { TickIcon } from "@/components/icons";
import { Section, SectionFrame } from "@/components/section";
import { useWide } from "@/features/dashboards/shared";
import { useAuth } from "@/lib/auth";
import { parseDomains, sameDomains } from "./domains";
import { SETTINGS_CHANGED } from "./retention";

const TITLE = "Organization";
const ORG_KEY = ["org", "settings"];
// At least one character that isn't a space: the server trims the name before it checks it.
const NAME_PATTERN = ".*\\S.*";

const ACCESS: { value: ProjectAccess; label: string; description: string }[] = [
  {
    value: "all",
    label: "Every project",
    description: "Members and viewers have their organization role on every project.",
  },
  {
    value: "explicit",
    label: "Only projects they're added to",
    description:
      "Members and viewers see a project only once they're added to it, with the role they get there.",
  },
];

const isAccess = (value: string | null): value is ProjectAccess =>
  ACCESS.some((option) => option.value === value);

// The fields that differ from the stored organization; only those are sent.
function orgChanges(
  org: Org,
  draft: { name: string; access: ProjectAccess; domains: string[] },
): OrgPatch {
  const patch: OrgPatch = {};
  if (draft.name.trim() !== org.name) patch.name = draft.name.trim();
  if (draft.access !== org.settings.project_access) patch.project_access = draft.access;
  if (!sameDomains(draft.domains, org.settings.allowed_email_domains)) {
    patch.allowed_email_domains = draft.domains;
  }
  return patch;
}

export function OrganizationSettings() {
  const auth = useAuth();
  const wide = useWide();
  if (auth.mode !== "session") {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="Organization settings need a control plane.">
          This server runs without one, so it has no organization, sign-in or members. Set
          postgres.url (CRUCIBLE_POSTGRES_URL) to turn them on.
        </EmptyState>
      </SectionFrame>
    );
  }
  const { role } = auth.session.org;
  // Every member reads the organization; owners and admins change it (contract 11.6), on a screen
  // wide enough for editing.
  const canEdit = role === "owner" || role === "admin";
  const readOnly = canEdit
    ? wide
      ? null
      : "Editing needs a wider screen."
    : "Only organization owners and admins change these settings.";
  return <OrganizationSection readOnly={readOnly} />;
}

interface Refusal {
  version: number;
  error: UiError;
}

function OrganizationSection({ readOnly }: { readOnly: string | null }) {
  const query = useQuery({ queryKey: ORG_KEY, queryFn: ({ signal }) => getOrg(signal) });
  // The version a save produced: the form of that version says so until it is edited.
  const [savedVersion, setSavedVersion] = useState<number | null>(null);
  // A save refused because the stored version moved on: the reloaded form says so until it is edited.
  const [refusal, setRefusal] = useState<Refusal | null>(null);
  return (
    <Section
      title={TITLE}
      label="organization settings"
      query={query}
      skeleton={<OrganizationSkeleton />}
      isEmpty={() => false}
      empty={null}
    >
      {(org) => (
        // A saved or reloaded version starts the form from the stored values.
        <OrganizationForm
          key={org.version}
          org={org}
          readOnly={readOnly}
          saved={savedVersion === org.version}
          onSaved={setSavedVersion}
          refused={refusal !== null && refusal.version !== org.version ? refusal.error : null}
          onRefused={setRefusal}
        />
      )}
    </Section>
  );
}

const SKELETON_ROWS = ["name", "access", "domains"];

function OrganizationSkeleton() {
  return (
    <div className="flex flex-col gap-6">
      {SKELETON_ROWS.map((row) => (
        <div key={row} className="flex flex-col gap-2">
          <div className="skeleton h-3 w-32" />
          <div className="skeleton h-8 w-80 max-w-full" />
        </div>
      ))}
    </div>
  );
}

const SAVE_COPY: CopyByCode = {
  precondition_failed: SETTINGS_CHANGED,
  forbidden: {
    title: "Only organization owners and admins change these settings.",
    detail: "Ask an owner or admin of the organization.",
  },
};

function OrganizationForm({
  org,
  readOnly,
  saved,
  onSaved,
  refused,
  onRefused,
}: {
  org: Org;
  readOnly: string | null;
  saved: boolean;
  onSaved: (version: number | null) => void;
  refused: UiError | null;
  onRefused: (refusal: Refusal | null) => void;
}) {
  const queryClient = useQueryClient();
  const editable = readOnly === null;
  const [name, setName] = useState(org.name);
  const [access, setAccess] = useState<ProjectAccess>(org.settings.project_access);
  const [domainText, setDomainText] = useState(org.settings.allowed_email_domains.join(", "));
  const domains = parseDomains(domainText);

  const save = useMutation({
    mutationFn: (patch: OrgPatch) => patchOrg(org, patch),
    onSuccess: (stored) => {
      onSaved(stored.version);
      queryClient.setQueryData(ORG_KEY, stored);
    },
    onError: async (error) => {
      // The stored version is what the next attempt must build on.
      const failed = asUiError(error);
      if (failed.code !== "precondition_failed") return;
      onRefused({ version: org.version, error: failed });
      await queryClient.invalidateQueries({ queryKey: ORG_KEY });
    },
  });
  const failure = save.isError ? asUiError(save.error) : refused;
  const patch = orgChanges(org, { name, access, domains: domains === "invalid" ? [] : domains });
  const changed = domains === "invalid" || Object.keys(patch).length > 0;

  // Any edit starts over: the last answer no longer describes the form.
  const edited = () => {
    if (saved) onSaved(null);
    if (refused) onRefused(null);
    if (save.isError) save.reset();
  };

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!editable || save.isPending || domains === "invalid" || !changed) return;
    save.mutate(patch);
  };

  return (
    <form className="flex max-w-prose min-w-0 flex-col gap-6" onSubmit={submit}>
      <Field name="name" invalid={fieldFailed(failure, "name") || undefined}>
        <FieldLabel>Organization name</FieldLabel>
        <Input
          required
          maxLength={200}
          pattern={NAME_PATTERN}
          autoComplete="off"
          readOnly={!editable}
          value={name}
          onValueChange={(next) => {
            setName(next);
            edited();
          }}
          className="max-w-sm"
        />
        <FieldError match="valueMissing">Enter a name for the organization.</FieldError>
        <FieldError match="patternMismatch">Enter a name for the organization.</FieldError>
        <FieldError match={fieldFailed(failure, "name")}>Use 1 to 200 characters.</FieldError>
      </Field>

      <Field>
        <FieldLabel>Project access</FieldLabel>
        <RadioGroup
          value={access}
          readOnly={!editable}
          onValueChange={(next) => {
            if (!isAccess(next)) return;
            setAccess(next);
            edited();
          }}
          className="flex flex-col gap-3"
        >
          {ACCESS.map((option) => (
            <Field key={option.value} className="flex-row items-start gap-2">
              <Radio value={option.value} className="mt-0.5 shrink-0">
                <RadioIndicator />
              </Radio>
              <div className="flex flex-col gap-0.5">
                <FieldLabel>{option.label}</FieldLabel>
                <FieldDescription>{option.description}</FieldDescription>
              </div>
            </Field>
          ))}
        </RadioGroup>
        <FieldDescription>
          Owners and admins are admins of every project either way.
          {access === "explicit" ? (
            <>
              {" "}
              Add people to a project with{" "}
              <code className="font-mono wrap-anywhere">
                {"POST /api/v1/projects/{project_id}/members"}
              </code>
              .
            </>
          ) : null}
        </FieldDescription>
      </Field>

      <FieldSet className="flex flex-col gap-3">
        <FieldLegend className="mb-0 text-data font-medium">Sign-up</FieldLegend>
        <FieldDescription>People join by invitation. There is no open sign-up.</FieldDescription>
        <Field
          name="allowed_email_domains"
          invalid={
            domains === "invalid" || fieldFailed(failure, "allowed_email_domains") || undefined
          }
        >
          <FieldLabel>Allowed email domains</FieldLabel>
          <Input
            autoComplete="off"
            spellCheck={false}
            placeholder="None"
            readOnly={!editable}
            value={domainText}
            onValueChange={(next) => {
              setDomainText(next);
              edited();
            }}
            className="max-w-md"
          />
          <FieldDescription>
            Someone with an email address at one of these domains can also join without an
            invitation, as a viewer, when the provider they sign in with says the address is
            verified. Separate domains with commas. Leave it empty to require an invitation for
            everyone.
          </FieldDescription>
          <FieldError match={domains === "invalid"}>
            Enter domains only, like example.com, separated by commas.
          </FieldError>
          <FieldError
            match={domains !== "invalid" && fieldFailed(failure, "allowed_email_domains")}
          >
            The server refused these domains. Enter bare domains, like example.com.
          </FieldError>
        </Field>
      </FieldSet>

      {failure ? (
        <FormAlert
          copy={failureCopy(failure, "Couldn't save the organization settings.", SAVE_COPY)}
          requestId={failure.requestId}
        />
      ) : null}
      {editable ? (
        <div className="flex flex-wrap items-center gap-3">
          <Button
            type="submit"
            data-variant="solid"
            disabled={!changed || save.isPending}
            focusableWhenDisabled
            aria-busy={save.isPending}
          >
            {save.isPending ? "Saving" : "Save settings"}
          </Button>
          <output className="flex items-center gap-1.5 text-data text-ink-soft">
            {saved ? (
              <>
                <TickIcon aria-hidden="true" /> Saved.
              </>
            ) : null}
          </output>
        </div>
      ) : (
        <p className="text-data text-ink-soft">{readOnly}</p>
      )}
    </form>
  );
}
