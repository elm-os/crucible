# Sign-in and the organization

Crucible serves one organization. People sign in with an email address and a password, or through a sign-in
provider: GitHub, Google or any OpenID Connect provider. Organization owners and admins manage the organization
and its providers under **Settings**: members and viewers see the organization page without changing it, and the
sign-in page tells them to ask an owner or admin. Both pages need the control plane (Postgres), and editing needs
a wider screen than a phone's; on a narrow screen they are read-only.

## The organization

**Settings**, **Organization** holds:

- **Organization name**, 1 to 200 characters.
- **Project access**. With **Every project**, the default, members and viewers have their organization role on
  every project. With **Only projects they're added to**, members and viewers see a project only once they are
  added to it ([project members](#project-members)), with the role they get there. Owners and admins are project
  admins on every project either way.
- **Password sign-in**. On by default. Turned off, people sign in only through a provider. Crucible keeps it on
  while it is the only way to sign in: saving it off while the sign-in page offers no provider is refused, with what
  to turn on first.
- **Sign-up**. People join by invitation, from **Settings**, **Members**
  ([quickstart](quickstart.md#people-and-outages)); there is no open sign-up. Under **Allowed email domains**,
  someone with an email address at one of the listed domains can also join without an invitation, as a viewer,
  when the provider they sign in with says the address is verified. Leave it empty to require an invitation for
  everyone.

## Project members

A project admin adds someone in the organization to a project through the API, with a project role: `admin`,
`member` or `viewer`. With **Only projects they're added to**, that is how members and viewers reach a project.
With **Every project**, a membership can raise someone's role on one project, so that an organization member added
as `admin` is a project admin there; it never lowers a role.

Sign in, with `CRUCIBLE_URL` set to the address you open the UI at (`server.public_url` when it is set), then list
the organization's members to find the person's `user_id`:

```sh
CRUCIBLE_URL=http://127.0.0.1:8080
CSRF=$(curl -s -c cookies.txt -X POST "$CRUCIBLE_URL/api/v1/auth/login" \
  -H 'Content-Type: application/json' -H 'X-Crucible-CSRF: prelogin' -H "Origin: $CRUCIBLE_URL" \
  -d '{"email": "you@example.com", "password": "your password"}' | sed -n 's/.*"csrf_token":"\([^"]*\)".*/\1/p')
curl -s -b cookies.txt "$CRUCIBLE_URL/api/v1/org/members"
```

Then add them, with the project's id from **Settings**, **Project** (**Copy id**) and their `user_id` in the body:

```sh
PROJECT_ID=01a1f633-d59a-7014-8007-ae2dcfb94996
curl -s -b cookies.txt -X POST "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/members" \
  -H 'Content-Type: application/json' -H "X-Crucible-CSRF: $CSRF" -H "Origin: $CRUCIBLE_URL" \
  -d '{"user_id": "01a1f633-d59a-7014-8007-ae2dcfb94997", "role": "member"}'
```

The answer is 201 with the membership. Someone outside the organization is refused with 422 `validation_failed`:
invite them first, and add them once they have accepted. An invitation made through the API can carry project
memberships too: `POST /api/v1/org/invitations` with
`{"email": "ada@example.com", "org_role": "member", "project_roles": [{"project_id": "...", "role": "member"}]}`
adds them to those projects when they accept. `GET .../members` lists a project's members, and
`PATCH .../members/{user_id}` with `{"role": "viewer"}` and `DELETE .../members/{user_id}` change and remove one
([API](../api/README.md#projects-members-and-keys)).

## Sign-in providers

A provider sends people back to Crucible after they sign in, to the callback URL
`<server.public_url>/api/v1/auth/oauth/<provider id>/callback`, so set `server.public_url`, the address people reach
the server at ([configuration](configuration.md#server)), before you add one. Without it, no provider is offered on
the sign-in page, and each one shows **Not offered: server.public_url isn't set** and no callback URL. The provider
id is made when the provider is added, so the full callback URL shows once the provider exists.

**Settings**, **Sign-in**, **Add provider**:

1. Choose **GitHub**, **Google** or **OpenID Connect**, and enter the **Button text** for the sign-in page, and the
   **Client ID** and **Client secret** that the provider issued for Crucible. An OpenID Connect provider also takes
   its **Issuer URL** and, optionally, **Extra scopes**; Crucible always asks for `openid`, `email` and `profile`.
2. Optionally, list **Allowed email domains**. Only someone whose verified email address is at one of these domains
   then signs in with this provider, at every sign-in, also people who signed in with it before the list changed.
   The sign-in page names the allowed domains to someone it refuses.
3. The last step shows the **Callback URL**, with the new provider's id in it. Register it with the provider: in a
   GitHub OAuth app as the authorization callback URL, in a Google OAuth client as an authorized redirect URI, and
   with an OpenID Connect provider as a redirect URI.

Each provider then shows whether it is **On** or **Turned off**, with these actions:

- **Edit** changes the button text, the client ID, the allowed email domains and, for OpenID Connect, the issuer
  and the extra scopes. A new issuer unlinks the accounts that signed in through the provider, because a person at
  one issuer is not the same person at another; each person links again through their verified email at their next
  sign-in.
- **Replace secret** stores a new client secret. Crucible keeps client secrets sealed with a key derived from the
  master key ([security](security.md#always-on)) and never shows them again.
- **Turn off** takes the provider off the sign-in page until **Turn on**.
- **Delete** removes the provider and its client secret. Accounts that signed in with it stay, and can sign in
  through another provider that verifies the same email address.

With password sign-in off, Crucible refuses to turn off or delete the last provider the sign-in page offers, and says
to turn password sign-in on first. A provider can still stop working at the identity provider, for example when its
OAuth app is deleted or its client secret expires; `crucible admin enable-password-login` then turns password sign-in
back on from the shell ([troubleshooting](troubleshooting.md#signing-in)).

Each change sends the version the page read, so a change made meanwhile by someone else is not overwritten: the page
says so and shows the provider as it is now.

Providers can also be declared in the server configuration, one of each kind (`[auth_github]`, `[auth_google]`,
`[auth_oidc]`), with their client secrets in files or environment variables
([configuration](configuration.md#auth_github-auth_google-and-auth_oidc)). Such a provider is read-only here and
in the API: change it in the configuration and restart. A new issuer there unlinks its accounts, as an edit here
does. Removing its section turns it off at the next start, except while it is the last way to sign in: with password
sign-in off and no other provider offered, it stays on, and the server logs at start what to do.

## How a provider sign-in finds the person

1. When the provider lists allowed email domains, the person needs an email address that the provider says is
   verified, at one of them, at every sign-in.
2. A person who signed in with the provider before is signed in.
3. Anyone else needs an email address that the provider says is verified. With one, the sign-in links an existing
   account with that address, or accepts an open invitation for it, with the roles the invitation gives. Failing
   both, an address at one of the organization's allowed email domains joins as a viewer. Anything else is
   refused.
