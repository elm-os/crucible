"use client";

import { useMutation } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import type { FormEvent } from "react";
import { acceptInvitation } from "@/api/client";
import { asUiError } from "@/api/errors";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { loadPage } from "@/lib/navigate";
import { AuthHeading } from "./auth-heading";
import { NewPasswordField } from "./new-password-field";

const TITLE = "Join Crucible";

const INVITE_COPY: CopyByCode = {
  token_invalid: {
    title: "This invitation can't be used.",
    detail:
      "It may have expired, been revoked or been used already. Ask the person who invited you for a new link.",
  },
  conflict: {
    title: "You already have an account.",
    detail: "Sign in with the email address this invitation was sent to.",
  },
};

export function InviteForm() {
  const token = useSearchParams().get("token");
  const accept = useMutation({ mutationFn: acceptInvitation, onSuccess: () => loadPage("/") });
  const busy = accept.isPending || accept.isSuccess;
  const failure = accept.isError ? asUiError(accept.error) : null;

  if (!token) {
    return (
      <>
        <AuthHeading title={TITLE} />
        <EmptyState kind="unavailable" title="This invitation link is incomplete.">
          Open the whole link from your invitation, or ask the person who invited you to send it
          again.
        </EmptyState>
      </>
    );
  }

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    accept.mutate({
      token,
      name: String(form.get("name")).trim(),
      password: String(form.get("password")),
    });
  };

  return (
    <>
      <AuthHeading title={TITLE}>
        You&apos;re invited to an organization on this Crucible server. Choose your name and a
        password to join.
      </AuthHeading>
      <form
        className="flex flex-col gap-4"
        onSubmit={submit}
        onInput={() => {
          if (failure) accept.reset();
        }}
      >
        <Field name="name" invalid={fieldFailed(failure, "name") || undefined}>
          <FieldLabel>Your name</FieldLabel>
          <Input required maxLength={200} autoComplete="name" />
          <FieldError match="valueMissing">Enter your name.</FieldError>
          <FieldError match={fieldFailed(failure, "name")}>Use 1 to 200 characters.</FieldError>
        </Field>
        <NewPasswordField
          name="password"
          label="Password"
          invalid={fieldFailed(failure, "password")}
        />
        {failure ? (
          <FormAlert
            copy={failureCopy(failure, "Couldn't accept the invitation.", INVITE_COPY)}
            requestId={failure.requestId}
            action={failure.code === "conflict" ? <Link href="/login">Sign in</Link> : null}
          />
        ) : null}
        <Button type="submit" data-variant="solid" disabled={busy} aria-busy={busy}>
          {busy ? "Joining" : "Accept invitation"}
        </Button>
      </form>
    </>
  );
}
