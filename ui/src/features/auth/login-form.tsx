"use client";

import { useMutation, useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  PasswordInput,
  PasswordInputControl,
  PasswordInputToggle,
} from "@ui-sdk/react/password-input";
import { Separator } from "@ui-sdk/react/separator";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import type { FormEvent, ReactNode } from "react";
import { getSignInMode, login } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { SignInOptions } from "@/api/generated/SignInOptions";
import { EmptyState } from "@/components/empty-state";
import { type Copy, type CopyByCode, FormAlert, failureCopy } from "@/components/form-alert";
import { useLoadingPhase } from "@/components/section";
import { loadPage } from "@/lib/navigate";
import { safeNext } from "@/lib/next-path";
import { AuthHeading } from "./auth-heading";

const TITLE = "Sign in to Crucible";

// Where a provider sign-in sends the browser back when it fails (contract 11.4).
const FLOW_ERRORS: CopyByCode = {
  oauth_denied: {
    title: "Sign-in was cancelled.",
    detail:
      "The sign-in provider didn't grant access. Try again, or choose another way to sign in.",
  },
  oauth_state_invalid: {
    title: "That sign-in attempt expired.",
    detail: "Start again from this page. A sign-in has 10 minutes to finish.",
  },
  oauth_exchange_failed: {
    title: "The sign-in provider didn't confirm who you are.",
    detail: "Try again in a moment, or choose another way to sign in.",
  },
  email_unverified: {
    title: "Your email isn't verified with the sign-in provider.",
    detail: "Verify your email address with the provider, then sign in again.",
  },
  not_invited: {
    title: "This account hasn't been invited.",
    detail: "Ask an administrator of this Crucible server for an invitation.",
  },
  account_disabled: {
    title: "This account is disabled.",
    detail: "Ask an administrator of this Crucible server to enable it again.",
  },
  provider_disabled: {
    title: "That sign-in option is turned off.",
    detail: "Choose another way to sign in, or ask an administrator.",
  },
  sso_required: {
    title: "Your organization signs in with its own provider.",
    detail: "Choose your organization's sign-in option below.",
  },
};

const FLOW_ERROR_UNKNOWN: Copy = {
  title: "Couldn't sign you in.",
  detail: "Try again, or choose another way to sign in.",
};

const LOGIN_COPY: CopyByCode = {
  invalid_credentials: {
    title: "That email and password don't match an account.",
    detail: "Check both and try again. Passwords are case-sensitive.",
  },
  forbidden: {
    title: "This account can't sign in.",
    detail: "Ask an administrator of this Crucible server to check your account.",
  },
};

// A provider that lets only some email domains sign in names them in `domains`; an entry that isn't
// a domain is left out, since anyone can write this address.
const DOMAIN = /^[a-z0-9-]+(\.[a-z0-9-]+)*$/;

function allowedDomains(text: string | null): string[] {
  return (text ?? "")
    .split(",")
    .map((entry) => entry.trim().toLowerCase())
    .filter((entry) => DOMAIN.test(entry))
    .slice(0, 10);
}

function either(items: string[]): string {
  return items.length < 2 ? items.join("") : `${items.slice(0, -1).join(", ")} or ${items.at(-1)}`;
}

function flowErrorCopy(code: string, domains: string[]): Copy {
  if (code === "not_invited" && domains.length > 0) {
    return {
      title: "This provider doesn't accept your email address.",
      detail: `Only addresses at ${either(domains)} can sign in with it. Sign in with an account at one of those domains, or choose another way to sign in.`,
    };
  }
  return Object.hasOwn(FLOW_ERRORS, code) ? FLOW_ERRORS[code] : FLOW_ERROR_UNKNOWN;
}

function providerHref(providerId: string, next: string): string {
  return `/api/v1/auth/oauth/${encodeURIComponent(providerId)}/start?${new URLSearchParams({ next })}`;
}

export function LoginForm() {
  const params = useSearchParams();
  const next = safeNext(params.get("next"));
  const flowError = params.get("error");
  const domains = allowedDomains(params.get("domains"));
  const mode = useQuery({
    queryKey: ["sign-in-mode"],
    queryFn: ({ signal }) => getSignInMode(signal),
  });
  const phase = useLoadingPhase(mode.isPending);

  let body: ReactNode;
  if (mode.isPending) {
    body = (
      <>
        <output className="sr-only">
          {phase === "hidden" ? "" : "Loading the ways to sign in"}
        </output>
        <div
          aria-hidden="true"
          className={`flex flex-col gap-4 ${phase === "hidden" ? "invisible" : ""}`}
        >
          <div className="skeleton h-14" />
          <div className="skeleton h-14" />
          <div className="skeleton h-8" />
        </div>
      </>
    );
  } else if (mode.isError) {
    const error = asUiError(mode.error);
    body = (
      <FormAlert
        copy={failureCopy(error, "Couldn't load the ways to sign in.")}
        requestId={error.requestId}
        action={<Button onClick={() => mode.refetch()}>Try again</Button>}
      />
    );
  } else if (mode.data.mode === "keys") {
    body = (
      <EmptyState
        kind="unavailable"
        title="This server has no sign-in."
        action={<Link href="/home">Open Crucible</Link>}
      >
        It runs without a control plane, so it has no accounts. Open Crucible and connect with a
        project id and its secret key.
      </EmptyState>
    );
  } else if (mode.data.options.setup_required) {
    body = (
      <EmptyState
        kind="first-use"
        title="Crucible isn't set up yet."
        action={<Link href="/setup">Set up Crucible</Link>}
      >
        Create the owner account first, with the setup token from the server log.
      </EmptyState>
    );
  } else {
    body = <SignInChoices options={mode.data.options} next={next} />;
  }

  return (
    <>
      <AuthHeading title={TITLE} />
      {flowError === null ? null : <FormAlert copy={flowErrorCopy(flowError, domains)} />}
      {body}
    </>
  );
}

function SignInChoices({ options, next }: { options: SignInOptions; next: string }) {
  const { providers, password_login_enabled: password } = options;
  if (!password && providers.length === 0) {
    return (
      <EmptyState kind="unavailable" title="No way to sign in is turned on.">
        Ask an administrator of this server to turn on password sign-in or add a sign-in provider.
      </EmptyState>
    );
  }
  return (
    <div className="flex flex-col gap-6">
      {providers.length > 0 ? (
        <ul className="flex flex-col gap-2">
          {providers.map((provider) => (
            <li key={provider.id}>
              {/* A full-page navigation: the server redirects to the provider. */}
              <Button
                className="w-full"
                render={(props) => (
                  <a {...props} href={providerHref(provider.id, next)}>
                    {provider.display_name}
                  </a>
                )}
              />
            </li>
          ))}
        </ul>
      ) : null}
      {providers.length > 0 && password ? <Separator /> : null}
      {password ? <PasswordSignIn next={next} /> : null}
    </div>
  );
}

function PasswordSignIn({ next }: { next: string }) {
  const signIn = useMutation({ mutationFn: login, onSuccess: () => loadPage(next) });
  const busy = signIn.isPending || signIn.isSuccess;
  const failure = signIn.isError ? asUiError(signIn.error) : null;

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    signIn.mutate({
      email: String(form.get("email")).trim(),
      password: String(form.get("password")),
    });
  };

  return (
    <form
      className="flex flex-col gap-4"
      onSubmit={submit}
      onInput={() => {
        if (failure) signIn.reset();
      }}
    >
      <Field name="email">
        <FieldLabel>Email</FieldLabel>
        <Input type="email" required autoComplete="username" spellCheck={false} />
        <FieldError match="valueMissing">Enter your email address.</FieldError>
        <FieldError match="typeMismatch">Enter an email address like ada@example.com.</FieldError>
      </Field>
      <Field name="password">
        <FieldLabel>Password</FieldLabel>
        <PasswordInput required>
          <PasswordInputControl />
          <PasswordInputToggle aria-label="Show password" />
        </PasswordInput>
        <FieldError match="valueMissing">Enter your password.</FieldError>
      </Field>
      {failure ? (
        <FormAlert
          copy={failureCopy(failure, "Couldn't sign you in.", LOGIN_COPY)}
          requestId={failure.requestId}
        />
      ) : null}
      <Button type="submit" data-variant="solid" disabled={busy} aria-busy={busy}>
        {busy ? "Signing in" : "Sign in"}
      </Button>
      <Link href="/reset" className="self-start text-data">
        Forgot your password?
      </Link>
    </form>
  );
}
