"use client";

import {
  AlertDialog,
  AlertDialogBackdrop,
  AlertDialogClose,
  AlertDialogContent,
  type AlertDialogController,
  AlertDialogDescription,
  AlertDialogTitle,
} from "@ui-sdk/react/alert-dialog";
import type { ReactNode } from "react";
import { asUiError } from "@/api/errors";
import { type CopyByCode, FormAlert, failureCopy } from "@/components/form-alert";

// One confirmation per table: each row's trigger opens it with that row as the payload. The dialog
// stays open until `run` settles, so a failure shows here and a success closes it.
export function ConfirmDialog<Row>({
  controller,
  question,
  description,
  confirm,
  pending,
  agentConfirm,
  failed,
  copy,
  run,
}: {
  controller: AlertDialogController<Row>;
  question: (row: Row | undefined) => string;
  description: ReactNode;
  confirm: string;
  pending: string;
  agentConfirm: string;
  failed: string;
  copy?: CopyByCode;
  run: (row: Row) => Promise<void>;
}) {
  return (
    <AlertDialog controller={controller}>
      <AlertDialogBackdrop />
      <AlertDialogContent<Row> keepMounted>
        {(state) => (
          <>
            <div className="flex flex-col gap-2">
              <AlertDialogTitle>{question(state.payload)}</AlertDialogTitle>
              <AlertDialogDescription>{description}</AlertDialogDescription>
            </div>
            <AlertDialogClose
              value="confirm"
              data-variant="solid"
              data-agent-confirm={agentConfirm}
              action={() => (state.payload === undefined ? undefined : run(state.payload))}
              render={(props, close) => {
                const error = close.error ? asUiError(close.error) : null;
                return (
                  <div className="flex flex-col gap-3">
                    {error ? (
                      <FormAlert
                        copy={failureCopy(error, failed, copy)}
                        requestId={error.requestId}
                      />
                    ) : null}
                    <div className="flex justify-end gap-2">
                      <AlertDialogClose>Cancel</AlertDialogClose>
                      <button {...props}>{close.pending ? pending : confirm}</button>
                    </div>
                  </div>
                );
              }}
            />
          </>
        )}
      </AlertDialogContent>
    </AlertDialog>
  );
}
