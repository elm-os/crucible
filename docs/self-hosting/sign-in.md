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
  added to it, with the role they get there. Owners and admins are project admins on every project either way.
- **Sign-up**. People join by invitation, from **Settings**, **Members**
  ([quickstart](quickstart.md#people-and-outages)); there is no open sign-up. Under **Allowed email domains**,
  someone with an email address at one of the listed domains can also join without an invitation, as a viewer,
  when the provider they sign in with says the address is verified. Leave it empty to require an invitation for
  everyone.

## Sign-in providers

A provider sends people back to Crucible after they sign in, so providers need `server.public_url`, the address
people reach the server at ([configuration](configuration.md#server)). Until it is set, no provider is offered on
the sign-in page, and each one shows **Not offered: server.public_url isn't set**.

**Settings**, **Sign-in**, **Add provider**:

1. Choose **GitHub**, **Google** or **OpenID Connect**, and enter the **Button text** for the sign-in page, and the
   **Client ID** and **Client secret** that the provider issued for Crucible. An OpenID Connect provider also takes
   its **Issuer URL** and, optionally, **Extra scopes**; Crucible always asks for `openid`, `email` and `profile`.
2. Optionally, list **Allowed email domains**. Someone signing in with this provider for the first time then needs
   an email address at one of these domains; people who already signed in with it keep their access.
3. The last step shows the **Callback URL**, which holds the new provider's id:
   `<server.public_url>/api/v1/auth/oauth/<provider id>/callback`. Register it with the provider: in a GitHub OAuth
   app as the authorization callback URL, in a Google OAuth client as an authorized redirect URI, and with an
   OpenID Connect provider as a redirect URI.

Each provider then shows whether it is **On** or **Turned off**, with these actions:

- **Edit** changes the button text and the allowed email domains. To use another client ID or issuer, add the
  provider again, then delete the old one.
- **Replace secret** stores a new client secret. Crucible keeps client secrets sealed with a key derived from the
  master key ([security](security.md#always-on)) and never shows them again.
- **Turn off** takes the provider off the sign-in page until **Turn on**.
- **Delete** removes the provider and its client secret. Accounts that signed in with it stay, and can sign in
  through another provider that verifies the same email address.

## How a provider sign-in finds the person

1. A person who signed in with the provider before is signed in.
2. Anyone else needs an email address that the provider says is verified, at one of the provider's allowed email
   domains when it lists any.
3. With such an address, the sign-in links an existing account with that address, or accepts an open invitation
   for it, with the roles the invitation gives. Failing both, an address at one of the organization's allowed
   email domains joins as a viewer. Anything else is refused.
