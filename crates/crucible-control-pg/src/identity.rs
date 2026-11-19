//! The identity repositories over Postgres (migration V2): sign-in providers, linked provider accounts, sign-in
//! flows, one-time tokens and invitations.

use async_trait::async_trait;
use crucible_control::{
    AuthFlow, AuthFlowRepo, AuthProvider, AuthProviderRepo, Email, Invitation, InvitationRepo,
    OauthAccount, OauthAccountRepo, OneTimeToken, OneTimeTokenRepo, OrgRole, ProjectRole,
    ProviderKind, TokenHash, TokenKind, TxError,
};
use crucible_records::{ProjectId, Timestamp, UserId, Uuid};

use crate::sql::{Cols, P};
use crate::store::{Handle, every, first};

const PROVIDER: &str = "id, kind, display_name, issuer, client_id, secret_id, scopes, allowed_domains, enabled, seeded";

fn provider(c: &mut Cols<'_>) -> Result<AuthProvider, TxError> {
    Ok(AuthProvider {
        id: c.uuid()?,
        kind: c.parsed(ProviderKind::parse)?,
        display_name: c.text()?,
        issuer: c.text()?,
        client_id: c.text()?,
        secret_id: c.opt_uuid()?,
        scopes: c.texts()?,
        allowed_domains: c.texts()?,
        enabled: c.bool()?,
        seeded: c.bool()?,
    })
}

fn texts(items: &[String]) -> P<'_> {
    P::TextArray(items.iter().map(String::as_str).collect())
}

fn provider_params(provider: &AuthProvider) -> [P<'_>; 10] {
    [
        P::Uuid(provider.id),
        P::Text(provider.kind.as_str()),
        P::Text(&provider.display_name),
        P::Text(&provider.issuer),
        P::Text(&provider.client_id),
        P::OptUuid(provider.secret_id),
        texts(&provider.scopes),
        texts(&provider.allowed_domains),
        P::Bool(provider.enabled),
        P::Bool(provider.seeded),
    ]
}

#[async_trait]
impl AuthProviderRepo for Handle {
    async fn get(&self, id: Uuid) -> Result<Option<AuthProvider>, TxError> {
        let sql = format!("SELECT {PROVIDER} FROM auth_providers WHERE id = $1");
        first(&self.read(&sql, &[P::Uuid(id)]).await?, provider)
    }

    async fn list(&self) -> Result<Vec<AuthProvider>, TxError> {
        let sql = format!("SELECT {PROVIDER} FROM auth_providers ORDER BY id");
        every(&self.read(&sql, &[]).await?, provider)
    }

    async fn insert(&self, provider: &AuthProvider) -> Result<(), TxError> {
        let sql = format!(
            "INSERT INTO auth_providers ({PROVIDER}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             RETURNING 1"
        );
        self.write(&sql, &provider_params(provider)).await?;
        Ok(())
    }

    async fn update(&self, provider: &AuthProvider) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE auth_providers SET kind = $2, display_name = $3, issuer = $4, client_id = $5, \
                 secret_id = $6, scopes = $7, allowed_domains = $8, enabled = $9, seeded = $10 \
                 WHERE id = $1 RETURNING 1",
                &provider_params(provider),
            )
            .await?;
        Ok(changed == 1)
    }

    async fn delete(&self, id: Uuid) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM auth_providers WHERE id = $1 RETURNING 1",
                &[P::Uuid(id)],
            )
            .await?;
        Ok(deleted == 1)
    }
}

fn account(c: &mut Cols<'_>) -> Result<OauthAccount, TxError> {
    Ok(OauthAccount {
        provider_id: c.uuid()?,
        subject: c.text()?,
        user_id: UserId::from_uuid(c.uuid()?),
    })
}

#[async_trait]
impl OauthAccountRepo for Handle {
    async fn get(&self, provider_id: Uuid, subject: &str) -> Result<Option<OauthAccount>, TxError> {
        let rows = self
            .read(
                "SELECT provider_id, subject, user_id FROM oauth_accounts \
                 WHERE provider_id = $1 AND subject = $2",
                &[P::Uuid(provider_id), P::Text(subject)],
            )
            .await?;
        first(&rows, account)
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<OauthAccount>, TxError> {
        let rows = self
            .read(
                "SELECT provider_id, subject, user_id FROM oauth_accounts WHERE user_id = $1 \
                 ORDER BY provider_id, subject",
                &[P::Uuid(user_id.uuid())],
            )
            .await?;
        every(&rows, account)
    }

    async fn insert(&self, account: &OauthAccount) -> Result<(), TxError> {
        self.write(
            "INSERT INTO oauth_accounts (provider_id, subject, user_id) VALUES ($1, $2, $3) RETURNING 1",
            &[
                P::Uuid(account.provider_id),
                P::Text(&account.subject),
                P::Uuid(account.user_id.uuid()),
            ],
        )
        .await?;
        Ok(())
    }
}

const FLOW: &str =
    "state_hash, provider_id, sealed_verifier, nonce, next_path, expires_at, consumed_at";

fn flow(c: &mut Cols<'_>) -> Result<AuthFlow, TxError> {
    Ok(AuthFlow {
        state_hash: c.array()?,
        provider_id: c.uuid()?,
        sealed_verifier: c.bytes()?,
        nonce: c.text()?,
        next_path: c.text()?,
        expires_at: c.ts()?,
        consumed_at: c.opt_ts()?,
    })
}

#[async_trait]
impl AuthFlowRepo for Handle {
    async fn insert(&self, flow: &AuthFlow) -> Result<(), TxError> {
        let sql = format!(
            "INSERT INTO auth_flows ({FLOW}) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING 1"
        );
        self.write(
            &sql,
            &[
                P::Bytes(&flow.state_hash),
                P::Uuid(flow.provider_id),
                P::Bytes(&flow.sealed_verifier),
                P::Text(&flow.nonce),
                P::Text(&flow.next_path),
                P::Ts(flow.expires_at),
                P::OptTs(flow.consumed_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn consume(
        &self,
        state_hash: &TokenHash,
        now: Timestamp,
    ) -> Result<Option<AuthFlow>, TxError> {
        let sql = format!(
            "UPDATE auth_flows SET consumed_at = $2 \
             WHERE state_hash = $1 AND consumed_at IS NULL AND expires_at > $2 RETURNING {FLOW}"
        );
        let rows = self
            .query_main(true, &sql, &[P::Bytes(state_hash), P::Ts(now)])
            .await?;
        first(&rows, flow)
    }

    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError> {
        self.write(
            "DELETE FROM auth_flows WHERE expires_at < $1 RETURNING 1",
            &[P::Ts(now)],
        )
        .await
    }
}

const TOKEN: &str = "token_hash, kind, subject, expires_at, consumed_at";

fn token(c: &mut Cols<'_>) -> Result<OneTimeToken, TxError> {
    Ok(OneTimeToken {
        token_hash: c.array()?,
        kind: c.parsed(TokenKind::parse)?,
        subject: c.text()?,
        expires_at: c.ts()?,
        consumed_at: c.opt_ts()?,
    })
}

#[async_trait]
impl OneTimeTokenRepo for Handle {
    async fn get(&self, token_hash: &TokenHash) -> Result<Option<OneTimeToken>, TxError> {
        let sql = format!("SELECT {TOKEN} FROM one_time_tokens WHERE token_hash = $1");
        first(&self.read(&sql, &[P::Bytes(token_hash)]).await?, token)
    }

    async fn insert(&self, token: &OneTimeToken) -> Result<(), TxError> {
        let sql = format!(
            "INSERT INTO one_time_tokens ({TOKEN}) VALUES ($1, $2, $3, $4, $5) RETURNING 1"
        );
        self.write(
            &sql,
            &[
                P::Bytes(&token.token_hash),
                P::Text(token.kind.as_str()),
                P::Text(&token.subject),
                P::Ts(token.expires_at),
                P::OptTs(token.consumed_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn consume(
        &self,
        token_hash: &TokenHash,
        kind: TokenKind,
        now: Timestamp,
    ) -> Result<Option<OneTimeToken>, TxError> {
        let sql = format!(
            "UPDATE one_time_tokens SET consumed_at = $3 \
             WHERE token_hash = $1 AND kind = $2 AND consumed_at IS NULL AND expires_at > $3 \
             RETURNING {TOKEN}"
        );
        let rows = self
            .query_main(
                true,
                &sql,
                &[P::Bytes(token_hash), P::Text(kind.as_str()), P::Ts(now)],
            )
            .await?;
        first(&rows, token)
    }

    async fn delete_unused(&self, kind: TokenKind, subject: &str) -> Result<u64, TxError> {
        self.write(
            "DELETE FROM one_time_tokens WHERE kind = $1 AND subject = $2 AND consumed_at IS NULL \
             RETURNING 1",
            &[P::Text(kind.as_str()), P::Text(subject)],
        )
        .await
    }

    async fn any_open(&self, kind: TokenKind, now: Timestamp) -> Result<bool, TxError> {
        let rows = self
            .read(
                "SELECT 1 FROM one_time_tokens WHERE kind = $1 AND consumed_at IS NULL \
                 AND expires_at > $2 LIMIT 1",
                &[P::Text(kind.as_str()), P::Ts(now)],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError> {
        self.write(
            "DELETE FROM one_time_tokens WHERE expires_at < $1 RETURNING 1",
            &[P::Ts(now)],
        )
        .await
    }
}

const INVITATION: &str = "id, email, org_role, project_roles, expires_at, accepted_at, revoked_at";

/// One `<project id>:<role>` entry of an invitation.
fn project_role(entry: &str) -> Option<(ProjectId, ProjectRole)> {
    let (project, role) = entry.split_once(':')?;
    Some((ProjectId::parse(project).ok()?, ProjectRole::parse(role)?))
}

fn invitation(c: &mut Cols<'_>) -> Result<Invitation, TxError> {
    Ok(Invitation {
        id: c.uuid()?,
        email: c.parsed(Email::new)?,
        org_role: c.parsed(OrgRole::parse)?,
        project_roles: c
            .texts()?
            .iter()
            .map(|entry| {
                project_role(entry).ok_or_else(|| {
                    TxError::Invalid(format!("unknown invitation project role {entry:?}"))
                })
            })
            .collect::<Result<_, _>>()?,
        expires_at: c.ts()?,
        accepted_at: c.opt_ts()?,
        revoked_at: c.opt_ts()?,
    })
}

#[async_trait]
impl InvitationRepo for Handle {
    async fn get(&self, id: Uuid) -> Result<Option<Invitation>, TxError> {
        let sql = format!("SELECT {INVITATION} FROM invitations WHERE id = $1");
        first(&self.read(&sql, &[P::Uuid(id)]).await?, invitation)
    }

    async fn list(&self) -> Result<Vec<Invitation>, TxError> {
        let sql = format!("SELECT {INVITATION} FROM invitations ORDER BY id");
        every(&self.read(&sql, &[]).await?, invitation)
    }

    async fn list_for_email(&self, email: &Email) -> Result<Vec<Invitation>, TxError> {
        let sql = format!("SELECT {INVITATION} FROM invitations WHERE email = $1 ORDER BY id");
        every(
            &self.read(&sql, &[P::Text(email.as_str())]).await?,
            invitation,
        )
    }

    async fn insert(&self, invitation: &Invitation) -> Result<(), TxError> {
        let sql = format!(
            "INSERT INTO invitations ({INVITATION}) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING 1"
        );
        let roles: Vec<String> = invitation
            .project_roles
            .iter()
            .map(|(project, role)| format!("{project}:{role}"))
            .collect();
        self.write(
            &sql,
            &[
                P::Uuid(invitation.id),
                P::Text(invitation.email.as_str()),
                P::Text(invitation.org_role.as_str()),
                texts(&roles),
                P::Ts(invitation.expires_at),
                P::OptTs(invitation.accepted_at),
                P::OptTs(invitation.revoked_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn accept(&self, id: Uuid, now: Timestamp) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE invitations SET accepted_at = $2 WHERE id = $1 AND accepted_at IS NULL \
                 AND revoked_at IS NULL AND expires_at > $2 RETURNING 1",
                &[P::Uuid(id), P::Ts(now)],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn revoke(&self, id: Uuid, now: Timestamp) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE invitations SET revoked_at = $2 WHERE id = $1 AND accepted_at IS NULL \
                 AND revoked_at IS NULL RETURNING 1",
                &[P::Uuid(id), P::Ts(now)],
            )
            .await?;
        Ok(changed == 1)
    }
}
