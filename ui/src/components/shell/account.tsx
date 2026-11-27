"use client";

import { Button } from "@ui-sdk/react/button";
import { useState } from "react";
import { logout } from "@/api/client";
import { asUiError, type UiError } from "@/api/errors";
import { failureCopy } from "@/components/form-alert";
import { AlertIcon } from "@/components/icons";
import { useAuth } from "@/lib/auth";
import { loadPage } from "@/lib/navigate";
import { ConnectDialog } from "./connect-dialog";

// Without a control plane, the Connect dialog stands in for signing in.
export function ConnectControl() {
  return useAuth().mode === "keys" ? <ConnectDialog /> : null;
}

export function AccountControls() {
  const auth = useAuth();
  const [failure, setFailure] = useState<UiError | null>(null);
  if (auth.mode !== "session") return null;

  const signOut = async () => {
    setFailure(null);
    try {
      await logout();
    } catch (error) {
      // A session that had already ended sent the browser to sign in on its own.
      const failed = asUiError(error);
      if (failed.kind !== "signed_out") setFailure(failed);
      return;
    }
    loadPage("/login/");
  };

  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
      <span className="max-w-48 truncate text-data">{auth.session.user.name}</span>
      <Button onClick={signOut} className="text-data">
        Sign out
      </Button>
      {failure ? (
        <p role="alert" className="flex items-center gap-1.5 text-label">
          <AlertIcon className="shrink-0" />
          {/* The header has room for one line: the words, without a code. */}
          Couldn&apos;t sign out. {failureCopy(failure, "Couldn't sign out.").detail}
        </p>
      ) : null}
    </div>
  );
}
