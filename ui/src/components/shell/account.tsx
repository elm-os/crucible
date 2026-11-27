"use client";

import {
  Menu,
  MenuContent,
  MenuGroup,
  MenuGroupLabel,
  MenuItem,
  MenuTrigger,
} from "@ui-sdk/react/menu";
import { useState } from "react";
import { logout } from "@/api/client";
import { asUiError, type UiError } from "@/api/errors";
import { failureCopy } from "@/components/form-alert";
import { AlertIcon, ChevronDownIcon } from "@/components/icons";
import { useAuth } from "@/lib/auth";
import { loadPage } from "@/lib/navigate";
import { ConnectDialog } from "./connect-dialog";

// Without a control plane, the Connect dialog stands in for signing in.
export function ConnectControl() {
  return useAuth().mode === "keys" ? <ConnectDialog /> : null;
}

export function AccountControls({ className }: { className?: string }) {
  const auth = useAuth();
  const [failure, setFailure] = useState<UiError | null>(null);
  if (auth.mode !== "session") return null;
  const { user } = auth.session;

  // The returned promise keeps the item pending until the server answers.
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
    <div className={`flex flex-wrap items-center gap-x-3 gap-y-1 ${className ?? ""}`}>
      <Menu pendingAnnouncement="Signing out">
        <MenuTrigger className="text-data">
          <span className="max-w-48 truncate">{user.name}</span>
          <ChevronDownIcon className="text-ink-soft" />
        </MenuTrigger>
        <MenuContent align="end">
          <MenuGroup>
            <MenuGroupLabel className="text-label">Signed in as {user.email}</MenuGroupLabel>
            <MenuItem className="text-data" onSelect={signOut}>
              Sign out
            </MenuItem>
          </MenuGroup>
        </MenuContent>
      </Menu>
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
