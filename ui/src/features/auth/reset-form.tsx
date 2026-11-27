"use client";

import { useMutation } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import type { FormEvent } from "react";
import { confirmPasswordReset, requestPasswordReset } from "@/api/client";
import { asUiError } from "@/api/errors";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { AuthHeading } from "./auth-heading";
import { NewPasswordField } from "./new-password-field";

const RESET_COPY: CopyByCode = {
  token_invalid: {
    title: "This reset link can't be used.",
    detail: "A reset link works once, for 30 minutes. Ask an administrator for a new one.",
  },
};

function AdminCommand({ email }: { email: string }) {
  return (
    <code className="block rounded-md bg-warm-sand px-3 py-2 font-mono text-data break-all">
      crucible admin reset-password {email}
    </code>
  );
}

export function ResetForm() {
  const token = useSearchParams().get("token");
  return token ? <ChooseNewPassword token={token} /> : <RequestReset />;
}

// The server sends no email yet (contract 12.7, `features.smtp`), and the sign-in pages can't read
// that flag before a session exists, so the page always names the administrator's command.
function RequestReset() {
  const request = useMutation({ mutationFn: requestPasswordReset });
  const failure = request.isError ? asUiError(request.error) : null;

  if (request.isSuccess) {
    return (
      <div className="flex flex-col gap-4">
        <AuthHeading title="Request received" focus>
          Crucible doesn&apos;t email reset links. Ask an administrator to run this command and send
          you the link it prints:
        </AuthHeading>
        <AdminCommand email={request.variables.email} />
        <Link href="/login" className="self-start text-data">
          Back to sign in
        </Link>
      </div>
    );
  }

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    request.mutate({ email: String(new FormData(event.currentTarget).get("email")).trim() });
  };

  return (
    <>
      <AuthHeading title="Reset your password">
        Crucible doesn&apos;t email reset links. An administrator makes one for you by running this
        command on the server:
      </AuthHeading>
      <AdminCommand email="<your email>" />
      <form
        className="flex flex-col gap-4"
        onSubmit={submit}
        onInput={() => {
          if (failure) request.reset();
        }}
      >
        <Field name="email">
          <FieldLabel>Email</FieldLabel>
          <Input type="email" required autoComplete="username" spellCheck={false} />
          <FieldError match="valueMissing">Enter your email address.</FieldError>
          <FieldError match="typeMismatch">Enter an email address like ada@example.com.</FieldError>
        </Field>
        {failure ? <FormAlert copy={failureCopy(failure, "Couldn't send the request.")} /> : null}
        <Button
          type="submit"
          data-variant="solid"
          disabled={request.isPending}
          aria-busy={request.isPending}
        >
          {request.isPending ? "Requesting" : "Request a reset link"}
        </Button>
        <Link href="/login" className="self-start text-data">
          Back to sign in
        </Link>
      </form>
    </>
  );
}

function ChooseNewPassword({ token }: { token: string }) {
  const reset = useMutation({ mutationFn: confirmPasswordReset });
  const failure = reset.isError ? asUiError(reset.error) : null;

  if (reset.isSuccess) {
    return (
      <div className="flex flex-col gap-4">
        <AuthHeading title="Password changed" focus>
          Sign in with your new password. Every other session of your account has ended.
        </AuthHeading>
        <Link href="/login" className="self-start text-data">
          Sign in
        </Link>
      </div>
    );
  }

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    reset.mutate({ token, new_password: String(form.get("new_password")) });
  };

  return (
    <>
      <AuthHeading title="Choose a new password" />
      <form
        className="flex flex-col gap-4"
        onSubmit={submit}
        onInput={() => {
          if (failure) reset.reset();
        }}
      >
        <NewPasswordField
          name="new_password"
          label="New password"
          invalid={fieldFailed(failure, "new_password")}
        />
        {failure ? (
          <FormAlert copy={failureCopy(failure, "Couldn't change your password.", RESET_COPY)} />
        ) : null}
        <Button
          type="submit"
          data-variant="solid"
          disabled={reset.isPending}
          aria-busy={reset.isPending}
        >
          {reset.isPending ? "Changing the password" : "Change password"}
        </Button>
      </form>
    </>
  );
}
