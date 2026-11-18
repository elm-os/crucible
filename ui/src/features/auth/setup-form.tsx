"use client";

import { useMutation } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import Link from "next/link";
import type { FormEvent } from "react";
import { setupOwner } from "@/api/client";
import { asUiError } from "@/api/errors";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { loadPage } from "@/lib/navigate";
import { AuthHeading } from "./auth-heading";
import { NewPasswordField } from "./new-password-field";

const SETUP_COPY: CopyByCode = {
  token_invalid: {
    title: "The setup token isn't valid.",
    detail: "Copy the newest token from the server log. Each token works once.",
  },
  conflict: {
    title: "Crucible already has an owner.",
    detail: "Sign in instead, or ask the owner for an invitation.",
  },
};

export function SetupForm() {
  const setup = useMutation({ mutationFn: setupOwner, onSuccess: () => loadPage("/") });
  const busy = setup.isPending || setup.isSuccess;
  const failure = setup.isError ? asUiError(setup.error) : null;
  const badToken = failure?.code === "token_invalid";

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    setup.mutate({
      token: String(form.get("token")).trim(),
      org_name: String(form.get("org_name")).trim(),
      name: String(form.get("name")).trim(),
      email: String(form.get("email")).trim(),
      password: String(form.get("password")),
    });
  };

  return (
    <>
      <AuthHeading title="Set up Crucible">
        Create your organization and its owner account. You can invite others afterwards.
      </AuthHeading>
      <form
        className="flex flex-col gap-4"
        onSubmit={submit}
        onInput={() => {
          if (failure) setup.reset();
        }}
      >
        <Field name="token" invalid={badToken || undefined}>
          <FieldLabel>Setup token</FieldLabel>
          <Input required autoComplete="off" spellCheck={false} className="font-mono" />
          <FieldDescription>
            Crucible prints it in the server log when it starts without an owner.
          </FieldDescription>
          <FieldError match="valueMissing">Enter the setup token from the server log.</FieldError>
          <FieldError match={badToken}>Copy the newest token from the server log.</FieldError>
        </Field>
        <Field name="org_name" invalid={fieldFailed(failure, "org_name") || undefined}>
          <FieldLabel>Organization name</FieldLabel>
          <Input required maxLength={200} autoComplete="organization" />
          <FieldError match="valueMissing">Enter your organization&apos;s name.</FieldError>
          <FieldError match={fieldFailed(failure, "org_name")}>Use 1 to 200 characters.</FieldError>
        </Field>
        <Field name="name" invalid={fieldFailed(failure, "name") || undefined}>
          <FieldLabel>Your name</FieldLabel>
          <Input required maxLength={200} autoComplete="name" />
          <FieldError match="valueMissing">Enter your name.</FieldError>
          <FieldError match={fieldFailed(failure, "name")}>Use 1 to 200 characters.</FieldError>
        </Field>
        <Field name="email" invalid={fieldFailed(failure, "email") || undefined}>
          <FieldLabel>Email</FieldLabel>
          <Input type="email" required autoComplete="email" spellCheck={false} />
          <FieldError match="valueMissing">Enter your email address.</FieldError>
          <FieldError match="typeMismatch">Enter an email address like ada@example.com.</FieldError>
          <FieldError match={fieldFailed(failure, "email")}>
            Enter an email address like ada@example.com.
          </FieldError>
        </Field>
        <NewPasswordField
          name="password"
          label="Password"
          invalid={fieldFailed(failure, "password")}
        />
        {failure ? (
          <FormAlert
            copy={failureCopy(failure, "Couldn't set up Crucible.", SETUP_COPY)}
            requestId={failure.requestId}
            action={failure.code === "conflict" ? <Link href="/login">Sign in</Link> : null}
          />
        ) : null}
        <Button type="submit" data-variant="solid" disabled={busy} aria-busy={busy}>
          {busy ? "Creating the owner account" : "Create owner account"}
        </Button>
      </form>
    </>
  );
}
