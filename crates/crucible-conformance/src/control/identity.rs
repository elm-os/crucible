//! The identity repositories of migration V2 (contract 11.1): sign-in providers, linked provider accounts, sign-in
//! flows, one-time tokens and invitations. Every instant here is chosen by the script, so the transcript holds no
//! database time.

use anyhow::{Result, ensure};
use crucible_control::{
    AuthFlow, AuthProvider, Email, Invitation, OauthAccount, OneTimeToken, OrgRole, ProjectRole,
    ProviderKind, TokenKind,
};
use crucible_records::{ProjectId, UserId};

use super::{Ctl, Transcript, at, id, project, user};

fn provider(n: u64, kind: ProviderKind, seeded: bool) -> AuthProvider {
    AuthProvider {
        id: id(n),
        kind,
        display_name: format!("Provider {n}"),
        issuer: if kind == ProviderKind::Github {
            String::new()
        } else {
            format!("https://issuer-{n}.example.com")
        },
        client_id: format!("client-{n}"),
        secret_id: None,
        scopes: vec!["openid".to_owned(), "email".to_owned()],
        allowed_domains: Vec::new(),
        enabled: true,
        seeded,
    }
}

fn flow(n: u8, provider_id: crucible_records::Uuid, expires_in: i64) -> AuthFlow {
    AuthFlow {
        state_hash: [n; 32],
        provider_id,
        sealed_verifier: vec![n; 40],
        nonce: format!("nonce-{n}"),
        next_path: format!("/traces/?n={n}"),
        expires_at: at(expires_in),
        consumed_at: None,
    }
}

fn token(n: u8, kind: TokenKind, subject: &str, expires_in: i64) -> OneTimeToken {
    OneTimeToken {
        token_hash: [n; 32],
        kind,
        subject: subject.to_owned(),
        expires_at: at(expires_in),
        consumed_at: None,
    }
}

fn invitation(n: u64, email: &str, roles: Vec<(ProjectId, ProjectRole)>) -> Result<Invitation> {
    Ok(Invitation {
        id: id(n),
        email: Email::new(email).ok_or_else(|| anyhow::anyhow!("the fixture email"))?,
        org_role: OrgRole::Member,
        project_roles: roles,
        expires_at: at(7 * 86_400),
        accepted_at: None,
        revoked_at: None,
    })
}

/// What a consumed row looks like in the transcript: whether it came back, and when it was consumed.
fn consumed<T>(
    row: Option<T>,
    at_of: impl Fn(&T) -> Option<crucible_records::Timestamp>,
) -> String {
    match row {
        Some(row) => format!("consumed at {:?}", at_of(&row)),
        None => "refused".to_owned(),
    }
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    for n in [1, 2] {
        s.users().insert(&user(n)?).await?;
    }
    let p = project(10);
    s.projects().insert(&p).await?;

    // Providers: configuration seeds at most one per kind.
    let github = provider(20, ProviderKind::Github, true);
    let oidc = provider(21, ProviderKind::Oidc, false);
    t.outcome(
        "insert seeded github",
        s.auth_providers().insert(&github).await,
    );
    t.outcome(
        "insert a second seeded github",
        s.auth_providers()
            .insert(&provider(22, ProviderKind::Github, true))
            .await,
    );
    t.outcome(
        "insert unseeded github",
        s.auth_providers()
            .insert(&provider(23, ProviderKind::Github, false))
            .await,
    );
    t.outcome("insert oidc", s.auth_providers().insert(&oidc).await);
    t.outcome("insert oidc again", s.auth_providers().insert(&oidc).await);
    let mut renamed = oidc.clone();
    renamed.display_name = "Company SSO".to_owned();
    renamed.allowed_domains = vec!["example.com".to_owned()];
    renamed.secret_id = Some(id(99));
    t.outcome("update oidc", s.auth_providers().update(&renamed).await);
    t.outcome(
        "update a missing provider",
        s.auth_providers()
            .update(&provider(98, ProviderKind::Google, false))
            .await,
    );
    let listed = s.auth_providers().list().await?;
    t.note(
        "providers",
        listed
            .iter()
            .map(|p| (p.kind, p.display_name.clone(), p.seeded))
            .collect::<Vec<_>>(),
    );
    ensure!(
        s.auth_providers().get(oidc.id).await? == Some(renamed.clone()),
        "a provider reads back as written"
    );

    // Linked accounts: unique by (provider, subject); both parents must exist.
    let account = |provider_id, subject: &str, user_n: u64| OauthAccount {
        provider_id,
        subject: subject.to_owned(),
        user_id: UserId::from_uuid(id(user_n)),
    };
    t.outcome(
        "link github 1001 to user 1",
        s.oauth_accounts()
            .insert(&account(github.id, "1001", 1))
            .await,
    );
    t.outcome(
        "link github 1001 again",
        s.oauth_accounts()
            .insert(&account(github.id, "1001", 2))
            .await,
    );
    t.outcome(
        "link oidc sub-a to user 1",
        s.oauth_accounts()
            .insert(&account(oidc.id, "sub-a", 1))
            .await,
    );
    t.outcome(
        "link to a missing user",
        s.oauth_accounts()
            .insert(&account(oidc.id, "sub-b", 77))
            .await,
    );
    t.outcome(
        "link to a missing provider",
        s.oauth_accounts()
            .insert(&account(id(88), "sub-c", 2))
            .await,
    );
    t.outcome(
        "get github 1001",
        s.oauth_accounts().get(github.id, "1001").await,
    );
    t.outcome(
        "get github 1002",
        s.oauth_accounts().get(github.id, "1002").await,
    );
    t.note(
        "accounts of user 1",
        s.oauth_accounts()
            .list_for_user(UserId::from_uuid(id(1)))
            .await?
            .len(),
    );
    // Deleting a provider deletes its linked accounts; deleting a user deletes theirs.
    t.outcome("delete oidc", s.auth_providers().delete(oidc.id).await);
    t.outcome(
        "delete oidc again",
        s.auth_providers().delete(oidc.id).await,
    );
    t.note(
        "accounts of user 1 after the provider went",
        s.oauth_accounts()
            .list_for_user(UserId::from_uuid(id(1)))
            .await?
            .len(),
    );
    t.outcome(
        "delete user 1",
        s.users().delete(UserId::from_uuid(id(1))).await,
    );
    t.outcome(
        "get github 1001 after the user went",
        s.oauth_accounts().get(github.id, "1001").await,
    );

    // Sign-in flows work once and only before they expire.
    let flows = s.auth_flows();
    for f in [flow(1, github.id, 600), flow(2, github.id, 60)] {
        t.outcome("insert flow", flows.insert(&f).await);
    }
    t.outcome(
        "insert flow 1 again",
        flows.insert(&flow(1, github.id, 600)).await,
    );
    let first = flows.consume(&[1; 32], at(30)).await?;
    ensure!(
        first.as_ref().map(|f| f.next_path.as_str()) == Some("/traces/?n=1"),
        "a consumed flow comes back whole"
    );
    t.note("consume flow 1", consumed(first, |f| f.consumed_at));
    t.note(
        "consume flow 1 again",
        consumed(flows.consume(&[1; 32], at(31)).await?, |f| f.consumed_at),
    );
    t.note(
        "consume flow 2 at its expiry",
        consumed(flows.consume(&[2; 32], at(60)).await?, |f| f.consumed_at),
    );
    t.note(
        "consume a missing flow",
        consumed(flows.consume(&[3; 32], at(1)).await?, |f| f.consumed_at),
    );
    t.outcome("delete expired flows", flows.delete_expired(at(601)).await);

    // One-time tokens: the kind must match, they work once, and only before they expire.
    let tokens = s.one_time_tokens();
    let user_two = id(2).to_string();
    for row in [
        token(1, TokenKind::PasswordReset, &user_two, 1_800),
        token(2, TokenKind::PasswordReset, &user_two, 1_800),
        token(3, TokenKind::Setup, "", 86_400),
        token(4, TokenKind::Invite, "invitation", 10),
    ] {
        t.outcome("insert token", tokens.insert(&row).await);
    }
    t.outcome(
        "insert token 1 again",
        tokens.insert(&token(1, TokenKind::Setup, "", 1)).await,
    );
    t.note(
        "consume token 1 as setup",
        consumed(
            tokens.consume(&[1; 32], TokenKind::Setup, at(5)).await?,
            |r| r.consumed_at,
        ),
    );
    t.note(
        "consume token 1 as reset",
        consumed(
            tokens
                .consume(&[1; 32], TokenKind::PasswordReset, at(5))
                .await?,
            |r| r.consumed_at,
        ),
    );
    t.note(
        "consume token 1 again",
        consumed(
            tokens
                .consume(&[1; 32], TokenKind::PasswordReset, at(6))
                .await?,
            |r| r.consumed_at,
        ),
    );
    t.note(
        "consume token 4 after it expired",
        consumed(
            tokens.consume(&[4; 32], TokenKind::Invite, at(11)).await?,
            |r| r.consumed_at,
        ),
    );
    t.note(
        "get token 1",
        tokens.get(&[1; 32]).await?.map(|r| (r.kind, r.consumed_at)),
    );
    t.outcome(
        "an open setup token exists",
        tokens.any_open(TokenKind::Setup, at(10)).await,
    );
    t.outcome(
        "an open setup token exists after it expired",
        tokens.any_open(TokenKind::Setup, at(86_400)).await,
    );
    t.outcome(
        "an open invite token exists",
        tokens.any_open(TokenKind::Invite, at(10)).await,
    );
    // The used reset token stays; only the unused one goes.
    t.outcome(
        "delete unused reset tokens of user 2",
        tokens
            .delete_unused(TokenKind::PasswordReset, &user_two)
            .await,
    );
    t.note(
        "token 2 after the delete",
        tokens.get(&[2; 32]).await?.is_some(),
    );
    t.outcome(
        "delete expired tokens",
        tokens.delete_expired(at(86_401)).await,
    );
    t.note(
        "token 1 after expiry",
        tokens.get(&[1; 32]).await?.is_some(),
    );

    // Invitations are accepted once, only while open, and revoked only while open.
    let invitations = s.invitations();
    let roles = vec![(p.id, ProjectRole::Admin)];
    let open = invitation(30, "ada@example.com", roles.clone())?;
    let other = invitation(31, "Ada@Example.com", Vec::new())?;
    let mut short = invitation(32, "bo@example.com", roles)?;
    short.expires_at = at(10);
    for row in [&open, &other, &short] {
        t.outcome("insert invitation", invitations.insert(row).await);
    }
    t.outcome(
        "insert invitation 30 again",
        invitations.insert(&open).await,
    );
    ensure!(
        invitations.get(open.id).await? == Some(open.clone()),
        "an invitation reads back as written, project roles included"
    );
    t.note(
        "invitations of ada",
        invitations
            .list_for_email(&open.email)
            .await?
            .iter()
            .map(|i| i.id)
            .collect::<Vec<_>>(),
    );
    t.outcome("accept 30", invitations.accept(open.id, at(60)).await);
    t.outcome("accept 30 again", invitations.accept(open.id, at(61)).await);
    t.outcome(
        "revoke 30 after acceptance",
        invitations.revoke(open.id, at(62)).await,
    );
    t.outcome("revoke 31", invitations.revoke(other.id, at(63)).await);
    t.outcome(
        "accept 31 after revocation",
        invitations.accept(other.id, at(64)).await,
    );
    t.outcome(
        "accept 32 at its expiry",
        invitations.accept(short.id, at(10)).await,
    );
    t.outcome(
        "revoke a missing invitation",
        invitations.revoke(id(97), at(1)).await,
    );
    t.note(
        "invitations",
        invitations
            .list()
            .await?
            .iter()
            .map(|i| (i.email.to_string(), i.accepted_at, i.revoked_at))
            .collect::<Vec<_>>(),
    );
    Ok(())
}
