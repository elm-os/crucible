"use client";

import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import {
  createDialogController,
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
import { type FormEvent, type ReactNode, useState } from "react";
import { type ConnectionCheck, checkConnection } from "@/api/client";
import { DialogBody } from "@/components/dialog-body";
import { AlertIcon, CheckIcon } from "@/components/icons";
import { clearConnection, saveConnection, useConnectedProject } from "@/lib/connection";

// A server without a control plane (keys-file mode) has no sign-in: a project id and its secret
// key stand in for it. With Postgres, browser sessions are used and this dialog is not shown.
const controller = createDialogController();

const FAILURES: Record<Exclude<ConnectionCheck, "ok">, string> = {
  rejected: "Crucible didn't accept this key. Check that you pasted the whole key.",
  wrong_project: "This key belongs to a different project. Check the project id.",
  unreachable: "Couldn't reach Crucible. Check your connection, then try again.",
};

const PROJECT_ID =
  "\\s*[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\\s*";
const SECRET_KEY = "\\s*csk_[0-9A-Za-z]{43}\\s*";

// Opens the Connect dialog from anywhere, such as a section that answered 401.
export function ConnectTrigger({ children }: { children: ReactNode }) {
  return <DialogTrigger controller={controller}>{children}</DialogTrigger>;
}

export function ConnectDialog() {
  const connected = useConnectedProject();
  const queryClient = useQueryClient();
  const [pending, setPending] = useState(false);
  const [failure, setFailure] = useState<Exclude<ConnectionCheck, "ok"> | null>(null);

  // The page's project comes from the URL first, so it follows the connection.
  const showProject = (project: string | null) => {
    const params = new URLSearchParams(window.location.search);
    if (project === null) params.delete("project");
    else params.set("project", project);
    window.history.replaceState(null, "", `?${params}`);
    void queryClient.invalidateQueries();
  };

  const connect = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const connection = {
      project: String(form.get("project")).trim().toLowerCase(),
      key: String(form.get("key")).trim(),
    };
    setPending(true);
    setFailure(null);
    const result = await checkConnection(connection);
    setPending(false);
    if (result !== "ok") {
      setFailure(result);
      return;
    }
    saveConnection(connection);
    showProject(connection.project);
    controller.close();
  };

  const disconnect = () => {
    clearConnection();
    showProject(null);
    controller.close();
  };

  return (
    <Dialog
      controller={controller}
      onOpenChange={(open) => {
        if (!open) setFailure(null);
      }}
    >
      <DialogTrigger controller={controller} className="text-data">
        {connected ? (
          <span data-connected className="flex items-center gap-1.5">
            <CheckIcon className="shrink-0" />
            Connected
          </span>
        ) : (
          "Connect"
        )}
      </DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        <div className="flex flex-col gap-2">
          <DialogTitle>Connect to a project</DialogTitle>
          <DialogDescription>
            This server runs without a control plane, so there is no sign-in. Paste a project id and
            its secret key to read that project&apos;s data. Both stay in this browser tab and are
            forgotten when you close it. With Postgres configured, people sign in instead.
          </DialogDescription>
        </div>
        <form className="flex min-h-0 flex-col gap-4" onSubmit={connect}>
          <DialogBody>
            <Field name="project">
              <FieldLabel>Project id</FieldLabel>
              <Input
                required
                pattern={PROJECT_ID}
                defaultValue={connected ?? ""}
                autoComplete="off"
                spellCheck={false}
                placeholder="0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
                className="font-mono"
              />
              <FieldError match="valueMissing">Enter the project id.</FieldError>
              <FieldError match="patternMismatch">
                A project id looks like 0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b.
              </FieldError>
            </Field>
            <Field name="key">
              <FieldLabel>Secret key</FieldLabel>
              <PasswordInput required autoComplete="off">
                <PasswordInputControl pattern={SECRET_KEY} className="font-mono" />
                <PasswordInputToggle aria-label="Show secret key" />
              </PasswordInput>
              <FieldDescription>
                The key from the server&apos;s keys file. It starts with csk_.
              </FieldDescription>
              <FieldError match="valueMissing">Enter the project&apos;s secret key.</FieldError>
              <FieldError match="patternMismatch">
                A secret key is csk_ followed by 43 letters and digits.
              </FieldError>
            </Field>
          </DialogBody>
          {failure ? (
            <p role="alert" data-error={failure} className="flex items-center gap-1.5 text-data">
              <AlertIcon className="shrink-0" />
              {FAILURES[failure]}
            </p>
          ) : null}
          <div className="flex justify-end gap-2">
            {connected ? <Button onClick={disconnect}>Disconnect</Button> : null}
            <DialogClose>Cancel</DialogClose>
            <Button type="submit" data-variant="solid" disabled={pending} aria-busy={pending}>
              {pending ? "Connecting" : "Connect"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
