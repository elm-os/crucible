//! The identity repositories of the model (migration V2): sign-in providers, linked provider accounts, sign-in
//! flows, one-time tokens and invitations.

use async_trait::async_trait;
use crucible_control::{
    AuthFlow, AuthFlowRepo, AuthProvider, AuthProviderRepo, Email, Invitation, InvitationRepo,
    OauthAccount, OauthAccountRepo, OneTimeToken, OneTimeTokenRepo, TokenHash, TokenKind, TxError,
};
use crucible_records::{Timestamp, UserId, Uuid};

use super::{Key, Pool, Row, Runner, Tbl, uuid_key};

pub(super) fn account_key(provider_id: Uuid, subject: &str) -> Key {
    [&provider_id.to_bytes()[..], subject.as_bytes()].concat()
}

#[async_trait]
impl AuthProviderRepo for Runner {
    async fn get(&self, id: Uuid) -> Result<Option<AuthProvider>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::AuthProviders,
                &uuid_key(id.to_bytes()),
                Row::AuthProvider
            ))
        })
        .await
    }

    async fn list(&self) -> Result<Vec<AuthProvider>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::AuthProviders, Row::AuthProvider))
        })
        .await
    }

    async fn insert(&self, provider: &AuthProvider) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::AuthProviders,
                uuid_key(provider.id.to_bytes()),
                Row::AuthProvider(provider.clone()),
                "auth_providers_pkey",
            )
        })
        .await
    }

    async fn update(&self, provider: &AuthProvider) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(provider.id.to_bytes());
            if cx.get(Tbl::AuthProviders, &key).is_none() {
                return Ok(false);
            }
            cx.put(
                Tbl::AuthProviders,
                key,
                Some(Row::AuthProvider(provider.clone())),
            )?;
            Ok(true)
        })
        .await
    }

    async fn delete(&self, id: Uuid) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            if cx.get(Tbl::AuthProviders, &key).is_none() {
                return Ok(false);
            }
            cx.put(Tbl::AuthProviders, key, None)?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl OauthAccountRepo for Runner {
    async fn get(&self, provider_id: Uuid, subject: &str) -> Result<Option<OauthAccount>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::OauthAccounts,
                &account_key(provider_id, subject),
                Row::OauthAccount
            ))
        })
        .await
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<OauthAccount>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut accounts = all!(cx, Tbl::OauthAccounts, Row::OauthAccount);
            accounts.retain(|account| account.user_id == user_id);
            Ok(accounts)
        })
        .await
    }

    async fn insert(&self, account: &OauthAccount) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::OauthAccounts,
                account_key(account.provider_id, &account.subject),
                Row::OauthAccount(account.clone()),
                "oauth_accounts_pkey",
            )
        })
        .await
    }
}

#[async_trait]
impl AuthFlowRepo for Runner {
    async fn insert(&self, flow: &AuthFlow) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::AuthFlows,
                flow.state_hash.to_vec(),
                Row::AuthFlow(flow.clone()),
                "auth_flows_pkey",
            )
        })
        .await
    }

    async fn consume(
        &self,
        state_hash: &TokenHash,
        now: Timestamp,
    ) -> Result<Option<AuthFlow>, TxError> {
        self.run(Pool::Main, true, |cx| {
            match one!(cx, Tbl::AuthFlows, state_hash, Row::AuthFlow) {
                Some(mut flow) if flow.consumed_at.is_none() && flow.expires_at > now => {
                    flow.consumed_at = Some(now);
                    cx.put(
                        Tbl::AuthFlows,
                        state_hash.to_vec(),
                        Some(Row::AuthFlow(flow.clone())),
                    )?;
                    Ok(Some(flow))
                }
                _ => Ok(None),
            }
        })
        .await
    }

    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut deleted = 0;
            for flow in all!(cx, Tbl::AuthFlows, Row::AuthFlow) {
                if flow.expires_at < now {
                    cx.put(Tbl::AuthFlows, flow.state_hash.to_vec(), None)?;
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await
    }
}

#[async_trait]
impl OneTimeTokenRepo for Runner {
    async fn get(&self, token_hash: &TokenHash) -> Result<Option<OneTimeToken>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(cx, Tbl::OneTimeTokens, token_hash, Row::OneTimeToken))
        })
        .await
    }

    async fn insert(&self, token: &OneTimeToken) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::OneTimeTokens,
                token.token_hash.to_vec(),
                Row::OneTimeToken(token.clone()),
                "one_time_tokens_pkey",
            )
        })
        .await
    }

    async fn consume(
        &self,
        token_hash: &TokenHash,
        kind: TokenKind,
        now: Timestamp,
    ) -> Result<Option<OneTimeToken>, TxError> {
        self.run(Pool::Main, true, |cx| {
            match one!(cx, Tbl::OneTimeTokens, token_hash, Row::OneTimeToken) {
                Some(mut token)
                    if token.kind == kind
                        && token.consumed_at.is_none()
                        && token.expires_at > now =>
                {
                    token.consumed_at = Some(now);
                    cx.put(
                        Tbl::OneTimeTokens,
                        token_hash.to_vec(),
                        Some(Row::OneTimeToken(token.clone())),
                    )?;
                    Ok(Some(token))
                }
                _ => Ok(None),
            }
        })
        .await
    }

    async fn delete_unused(&self, kind: TokenKind, subject: &str) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut deleted = 0;
            for token in all!(cx, Tbl::OneTimeTokens, Row::OneTimeToken) {
                if token.kind == kind && token.subject == subject && token.consumed_at.is_none() {
                    cx.put(Tbl::OneTimeTokens, token.token_hash.to_vec(), None)?;
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await
    }

    async fn any_open(&self, kind: TokenKind, now: Timestamp) -> Result<bool, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::OneTimeTokens, Row::OneTimeToken)
                .iter()
                .any(|token| {
                    token.kind == kind && token.consumed_at.is_none() && token.expires_at > now
                }))
        })
        .await
    }

    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut deleted = 0;
            for token in all!(cx, Tbl::OneTimeTokens, Row::OneTimeToken) {
                if token.expires_at < now {
                    cx.put(Tbl::OneTimeTokens, token.token_hash.to_vec(), None)?;
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await
    }
}

#[async_trait]
impl InvitationRepo for Runner {
    async fn get(&self, id: Uuid) -> Result<Option<Invitation>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::Invitations,
                &uuid_key(id.to_bytes()),
                Row::Invitation
            ))
        })
        .await
    }

    async fn list(&self) -> Result<Vec<Invitation>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::Invitations, Row::Invitation))
        })
        .await
    }

    async fn list_for_email(&self, email: &Email) -> Result<Vec<Invitation>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let every: Vec<Invitation> = all!(cx, Tbl::Invitations, Row::Invitation);
            Ok(every
                .into_iter()
                .filter(|invitation| invitation.email == *email)
                .collect())
        })
        .await
    }

    async fn insert(&self, invitation: &Invitation) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::Invitations,
                uuid_key(invitation.id.to_bytes()),
                Row::Invitation(invitation.clone()),
                "invitations_pkey",
            )
        })
        .await
    }

    async fn accept(&self, id: Uuid, now: Timestamp) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::Invitations, &key, Row::Invitation) {
                Some(mut invitation) if invitation.is_open(now) => {
                    invitation.accepted_at = Some(now);
                    cx.put(Tbl::Invitations, key, Some(Row::Invitation(invitation)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn revoke(&self, id: Uuid, now: Timestamp) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::Invitations, &key, Row::Invitation) {
                Some(mut invitation)
                    if invitation.accepted_at.is_none() && invitation.revoked_at.is_none() =>
                {
                    invitation.revoked_at = Some(now);
                    cx.put(Tbl::Invitations, key, Some(Row::Invitation(invitation)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }
}
