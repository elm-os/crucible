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
import { Field, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { type FormEvent, useState } from "react";
import {
  createInvitation,
  listInvitations,
  listOrgMembers,
  removeOrgMember,
  revokeInvitation,
  setOrgMemberRole,
} from "@/api/client";
import { asUiError } from "@/api/errors";
import type { Invitation } from "@/api/generated/Invitation";
import type { InvitationCreated } from "@/api/generated/InvitationCreated";
import type { InvitationList } from "@/api/generated/InvitationList";
import type { OrgMember } from "@/api/generated/OrgMember";
import type { OrgMemberList } from "@/api/generated/OrgMemberList";
import type { OrgRole } from "@/api/generated/OrgRole";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy } from "@/components/form-alert";
import { Section, SectionFrame } from "@/components/section";
import { useAuth } from "@/lib/auth";
import { formatTime } from "@/lib/format";
import { ConfirmDialog } from "./confirm-dialog";
import { RowsSkeleton } from "./rows-skeleton";
import { ShowOnce } from "./show-once";

const MEMBERS_KEY = ["org", "members"];
const INVITATIONS_KEY = ["org", "invitations"];

const ROLE_LABEL: Record<OrgRole, string> = {
  owner: "Owner",
  admin: "Admin",
  member: "Member",
  viewer: "Viewer",
};

// Ownership moves only by transferring it, so `owner` is never offered.
const ROLE_ITEMS = (["admin", "member", "viewer"] as const).map((role) => ({
  value: role,
  label: ROLE_LABEL[role],
}));

const isOrgRole = (value: string): value is OrgRole => Object.hasOwn(ROLE_LABEL, value);

const GONE: CopyByCode = {
  not_found: {
    title: "This person isn't a member anymore.",
    detail: "Someone may have removed them. The list shows the current members.",
  },
};

export function MembersSettings() {
  const auth = useAuth();
  if (auth.mode !== "session") {
    return (
      <SectionFrame title="Members">
        <EmptyState kind="unavailable" title="Members need a control plane.">
          This server runs without one, so it has no sign-in, members or invitations. Set
          postgres.url (CRUCIBLE_POSTGRES_URL) to turn them on.
        </EmptyState>
      </SectionFrame>
    );
  }
  const { org, user } = auth.session;
  // Owners and admins change members and invitations; everyone else can list them (contract 11.6).
  const canManage = org.role === "owner" || org.role === "admin";
  return (
    <div className="flex flex-col gap-6">
      <MembersSection canManage={canManage} self={user.id} />
      <InvitationsSection canManage={canManage} />
    </div>
  );
}

function MembersSection({ canManage, self }: { canManage: boolean; self: string }) {
  const query = useQuery({
    queryKey: MEMBERS_KEY,
    queryFn: ({ signal }) => listOrgMembers(signal),
  });
  return (
    <Section
      title="Members"
      query={query}
      skeleton={<RowsSkeleton />}
      isEmpty={(list) => list.data.length === 0}
      empty={<EmptyState kind="first-use" title="No members yet." />}
    >
      {(list) => <MembersTable members={list.data} canManage={canManage} self={self} />}
    </Section>
  );
}

function MembersTable({
  members,
  canManage,
  self,
}: {
  members: OrgMember[];
  canManage: boolean;
  self: string;
}) {
  const queryClient = useQueryClient();
  const [remove] = useState(() => createAlertDialogController<OrgMember>());
  // An owner's role moves only by transferring ownership, and nobody demotes or removes themselves
  // here, which could lock them out of this page.
  const editable = (member: OrgMember) =>
    canManage && member.role !== "owner" && member.user_id !== self;

  const removeMember = async (member: OrgMember) => {
    try {
      await removeOrgMember(member.user_id);
    } catch (error) {
      if (asUiError(error).kind !== "not_found") throw error;
    }
    await queryClient.invalidateQueries({ queryKey: MEMBERS_KEY });
  };

  return (
    <>
      {/* `relative` keeps the visually hidden labels in the rows inside the scroller; positioned
          against the page instead, they widen it on a phone. */}
      <div className="relative overflow-x-auto">
        <table className="w-full min-w-[36rem] text-start text-data">
          <thead>
            <tr className="text-label text-ink-soft">
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Name
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Email
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Role
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Status
              </th>
              {canManage ? (
                <th scope="col" className="h-8 text-start font-medium">
                  <span className="sr-only">Actions</span>
                </th>
              ) : null}
            </tr>
          </thead>
          <tbody>
            {members.map((member) => (
              <tr key={member.user_id} className="border-t border-ash-border">
                <td className="py-2 pe-4">
                  {member.name}
                  {member.user_id === self ? (
                    <span className="ms-1.5 text-ink-soft">(you)</span>
                  ) : null}
                </td>
                <td className="py-2 pe-4">{member.email}</td>
                <td className="py-2 pe-4">
                  {editable(member) ? <RoleSelect member={member} /> : ROLE_LABEL[member.role]}
                </td>
                <td className="py-2 pe-4">{member.status === "active" ? "Active" : "Disabled"}</td>
                {canManage ? (
                  <td className="py-2 text-end">
                    {editable(member) ? (
                      <AlertDialogTrigger
                        controller={remove}
                        payload={member}
                        className="text-data"
                      >
                        Remove <span className="sr-only">{member.name}</span>
                      </AlertDialogTrigger>
                    ) : null}
                  </td>
                ) : null}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {canManage ? (
        <ConfirmDialog
          controller={remove}
          question={(member) => (member ? `Remove ${member.name}?` : "Remove this member?")}
          description="They lose access to the organization and its projects at once. You can invite them again later."
          confirm="Remove member"
          pending="Removing"
          agentConfirm="Removes this person from the organization"
          failed="Couldn't remove the member."
          run={removeMember}
        />
      ) : null}
    </>
  );
}

function RoleSelect({ member }: { member: OrgMember }) {
  const queryClient = useQueryClient();
  const change = useMutation({
    mutationFn: (role: OrgRole) => setOrgMemberRole(member.user_id, { role }),
    onSuccess: (updated) =>
      queryClient.setQueryData<OrgMemberList>(MEMBERS_KEY, (list) =>
        list
          ? { data: list.data.map((row) => (row.user_id === updated.user_id ? updated : row)) }
          : list,
      ),
  });
  const failure = change.isError ? asUiError(change.error) : null;
  return (
    <div className="flex flex-col items-start gap-1">
      <Select
        items={ROLE_ITEMS}
        value={change.isPending ? change.variables : member.role}
        readOnly={change.isPending}
        onValueChange={(role) => {
          if (role !== null && isOrgRole(role) && role !== member.role) change.mutate(role);
        }}
      >
        <SelectLabel className="sr-only">Role for {member.name}</SelectLabel>
        <SelectTrigger className="text-data">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {ROLE_ITEMS.map((item) => (
            <SelectItem key={item.value} value={item.value}>
              {item.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      {failure ? (
        <FormAlert
          copy={failureCopy(failure, `Couldn't change the role of ${member.name}.`, GONE)}
          requestId={failure.requestId}
        />
      ) : null}
    </div>
  );
}

type InvitationStatus = "pending" | "accepted" | "revoked" | "expired";

const STATUS_LABEL: Record<InvitationStatus, string> = {
  pending: "Pending",
  accepted: "Accepted",
  revoked: "Revoked",
  expired: "Expired",
};

function invitationStatus(invitation: Invitation, asOf: number): InvitationStatus {
  if (invitation.accepted_at !== null) return "accepted";
  if (invitation.revoked_at !== null) return "revoked";
  return Date.parse(invitation.expires_at) <= asOf ? "expired" : "pending";
}

function InvitationsSection({ canManage }: { canManage: boolean }) {
  const query = useQuery({
    queryKey: INVITATIONS_KEY,
    queryFn: ({ signal }) => listInvitations(signal),
  });
  return (
    <Section
      title="Invitations"
      query={query}
      actions={canManage ? <InviteDialog /> : undefined}
      skeleton={<RowsSkeleton />}
      isEmpty={(list) => list.data.length === 0}
      empty={
        <EmptyState kind="first-use" title="No invitations yet.">
          {canManage
            ? "Invite someone by email. Crucible makes a link for you to send them."
            : "Organization owners and admins invite people."}
        </EmptyState>
      }
    >
      {(list) => <InvitationsTable list={list} canManage={canManage} asOf={query.dataUpdatedAt} />}
    </Section>
  );
}

function InvitationsTable({
  list,
  canManage,
  asOf,
}: {
  list: InvitationList;
  canManage: boolean;
  asOf: number;
}) {
  const queryClient = useQueryClient();
  const [revoke] = useState(() => createAlertDialogController<Invitation>());

  // An invitation that is no longer open (accepted, revoked or expired) needs nothing more.
  const revokeOne = async (invitation: Invitation) => {
    try {
      await revokeInvitation(invitation.id);
    } catch (error) {
      if (asUiError(error).kind !== "not_found") throw error;
    }
    await queryClient.invalidateQueries({ queryKey: INVITATIONS_KEY });
  };

  return (
    <>
      <div className="relative overflow-x-auto">
        <table className="w-full min-w-[36rem] text-start text-data">
          <thead>
            <tr className="text-label text-ink-soft">
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Email
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Role
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Status
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Expires
              </th>
              {canManage ? (
                <th scope="col" className="h-8 text-start font-medium">
                  <span className="sr-only">Actions</span>
                </th>
              ) : null}
            </tr>
          </thead>
          <tbody>
            {list.data.map((invitation) => {
              const status = invitationStatus(invitation, asOf);
              return (
                <tr key={invitation.id} className="h-10 border-t border-ash-border">
                  <td className="pe-4">{invitation.email}</td>
                  <td className="pe-4">{ROLE_LABEL[invitation.org_role]}</td>
                  <td className="pe-4">{STATUS_LABEL[status]}</td>
                  <td className="pe-4 whitespace-nowrap">{formatTime(invitation.expires_at)}</td>
                  {canManage ? (
                    <td className="text-end">
                      {status === "pending" ? (
                        <AlertDialogTrigger
                          controller={revoke}
                          payload={invitation}
                          className="text-data"
                        >
                          Revoke{" "}
                          <span className="sr-only">the invitation for {invitation.email}</span>
                        </AlertDialogTrigger>
                      ) : null}
                    </td>
                  ) : null}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {canManage ? (
        <ConfirmDialog
          controller={revoke}
          question={(invitation) =>
            invitation
              ? `Revoke the invitation for ${invitation.email}?`
              : "Revoke this invitation?"
          }
          description="Its link stops working at once. You can invite the same email again later."
          confirm="Revoke invitation"
          pending="Revoking"
          agentConfirm="Revokes the invitation so its link stops working"
          failed="Couldn't revoke the invitation."
          run={revokeOne}
        />
      ) : null}
    </>
  );
}

const INVITE_COPY: CopyByCode = {
  conflict: {
    title: "That person is already a member.",
    detail: "Find them in the members list, where you can change their role.",
  },
};

function InviteDialog() {
  const queryClient = useQueryClient();
  const invite = useMutation({
    mutationFn: createInvitation,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: INVITATIONS_KEY }),
  });
  const failure = invite.isError ? asUiError(invite.error) : null;

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const role = String(form.get("org_role"));
    invite.mutate({
      email: String(form.get("email")).trim(),
      org_role: isOrgRole(role) ? role : "member",
    });
  };

  return (
    <Dialog
      onOpenChangeComplete={(open) => {
        // The link shows once: closing forgets it.
        if (!open) invite.reset();
      }}
    >
      <DialogTrigger className="text-data">Invite member</DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        {invite.isSuccess ? (
          <InvitationLink created={invite.data} />
        ) : (
          <>
            <div className="flex flex-col gap-2">
              <DialogTitle>Invite a member</DialogTitle>
              <DialogDescription>
                Crucible makes a link for you to send them. It works for 7 days.
              </DialogDescription>
            </div>
            <form
              className="flex flex-col gap-4"
              onSubmit={submit}
              onInput={() => {
                if (failure) invite.reset();
              }}
            >
              <Field name="email">
                <FieldLabel>Email</FieldLabel>
                <Input type="email" required autoComplete="off" spellCheck={false} />
                <FieldError match="valueMissing">Enter their email address.</FieldError>
                <FieldError match="typeMismatch">
                  Enter an email address like ada@example.com.
                </FieldError>
              </Field>
              <Field>
                <FieldLabel>Role</FieldLabel>
                <Select name="org_role" items={ROLE_ITEMS} defaultValue="member" required>
                  <SelectTrigger className="text-data">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {ROLE_ITEMS.map((item) => (
                      <SelectItem key={item.value} value={item.value}>
                        {item.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
              {failure ? (
                <FormAlert
                  copy={failureCopy(failure, "Couldn't create the invitation.", INVITE_COPY)}
                  requestId={failure.requestId}
                />
              ) : null}
              <div className="flex justify-end gap-2">
                <DialogClose>Cancel</DialogClose>
                <Button
                  type="submit"
                  data-variant="solid"
                  disabled={invite.isPending}
                  aria-busy={invite.isPending}
                >
                  {invite.isPending ? "Creating the invitation" : "Create invitation"}
                </Button>
              </div>
            </form>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function InvitationLink({ created }: { created: InvitationCreated }) {
  return (
    <>
      <div className="flex flex-col gap-2">
        <DialogTitle>Send this link to {created.invitation.email}</DialogTitle>
        <DialogDescription>
          Crucible doesn&apos;t send email yet, so send the link yourself. It shows only now: copy
          it before you close this. It works until {formatTime(created.invitation.expires_at)}.
        </DialogDescription>
      </div>
      <ShowOnce
        label="Invitation link"
        value={`${window.location.origin}${created.accept_path}`}
        copyLabel="Copy link"
      />
      <div className="flex justify-end">
        <DialogClose data-variant="solid">Done</DialogClose>
      </div>
    </>
  );
}
