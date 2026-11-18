//! The identity service in simulation over `PgModel`: setup, password sign-in, session expiry and the outage
//! grace, invitations, member rules, keys, password changes and resets, and provider sign-in against the fake
//! provider.

use std::sync::Arc;
use std::time::Duration;

use crucible_control::{OrgRole, ProjectRole, ProviderKind, Repos as _};
use crucible_env::Env;
use crucible_sim::pg_model::{PgFaults, PgModel};

use super::*;
use crate::csrf::Origins;
use crate::db_keys::{DbKeys, KeyVerdict, OutageMode};
use crate::password::HashCost;
use crate::test_support::idp::{CLIENT_ID, CLIENT_SECRET, FakeIdp, FakeUser, ISSUER};

const PASSWORD: &str = "correct horse battery";

fn identity(env: &Env, model: &PgModel, grace: Duration) -> Identity {
    let mut config =
        IdentityConfig::new(Origins::default(), Some("http://crucible.test".to_owned()));
    config.hash_cost = HashCost {
        memory_kib: 64,
        iterations: 1,
        lanes: 1,
    };
    config.session_outage_grace = grace;
    let lookup = LookupKey::new([5; 32]);
    let keys = Arc::new(DbKeys::new(env, lookup.clone(), OutageMode::default()));
    keys.load(&crucible_control::KeySnapshot {
        max_seq: 0,
        keys: Vec::new(),
    });
    Identity::new(
        env,
        Arc::new(model.clone()),
        Sealer::new("master-0", [6; 32]),
        lookup,
        keys,
        config,
    )
    .unwrap()
}

fn sim<T: 'static>(seed: u64, body: impl AsyncFnOnce(Env, PgModel) -> T + 'static) -> T {
    crucible_sim::run(seed, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let model = PgModel::new(
            host.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        body(host.env.clone(), model).await
    })
}

fn valid(verdict: SessionVerdict) -> Arc<SessionPrincipal> {
    match verdict {
        SessionVerdict::Valid(principal) => principal,
        other => panic!("expected a live session, got {other:?}"),
    }
}

/// Sets up the organization with its owner and answers the owner's session.
async fn owner(identity: &Identity) -> NewSession {
    let link = identity.issue_setup_token().await.unwrap().unwrap();
    identity
        .setup(
            link.token.as_str(),
            "Acme",
            "Owner@Example.com",
            "Olivia",
            PASSWORD,
        )
        .await
        .unwrap()
}

#[test]
fn first_owner_setup_then_password_sign_in() {
    sim(1, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        assert!(identity.setup_required().await.unwrap());
        assert!(matches!(
            identity.login("owner@example.com", PASSWORD).await,
            Err(AuthError::Forbidden(_))
        ));
        let link = identity.issue_setup_token().await.unwrap().unwrap();
        // A weak password and a wrong token are refused before anything is created.
        assert!(matches!(
            identity
                .setup(link.token.as_str(), "Acme", "o@example.com", "O", "short")
                .await,
            Err(AuthError::Invalid {
                field: "password",
                ..
            })
        ));
        assert_eq!(
            identity
                .setup(&"x".repeat(43), "Acme", "o@example.com", "O", PASSWORD)
                .await
                .unwrap_err(),
            AuthError::TokenInvalid
        );
        let session = identity
            .setup(
                link.token.as_str(),
                "Acme",
                "Owner@Example.com",
                "Olivia",
                PASSWORD,
            )
            .await
            .unwrap();
        assert_eq!(session.principal.org_role, OrgRole::Owner);
        assert_eq!(session.principal.user.email.as_str(), "owner@example.com");
        // The owner starts with a project to make keys for.
        let projects: Vec<_> = session.principal.projects.values().collect();
        assert_eq!(projects.len(), 1);
        assert_eq!(
            (projects[0].0.slug.as_str(), projects[0].1),
            ("default", ProjectRole::Admin)
        );
        assert!(!identity.setup_required().await.unwrap());
        assert!(identity.issue_setup_token().await.unwrap().is_none());
        // The setup token works once.
        assert_eq!(
            identity
                .setup(link.token.as_str(), "Acme", "x@example.com", "X", PASSWORD)
                .await
                .unwrap_err(),
            AuthError::TokenInvalid
        );
        // `crucible admin init` refuses once an owner exists.
        assert!(matches!(
            identity
                .init_owner("Acme", "x@example.com", "X", PASSWORD)
                .await,
            Err(AuthError::Conflict(_))
        ));

        let signed_in = identity
            .login(" OWNER@example.com ", PASSWORD)
            .await
            .unwrap();
        assert_ne!(signed_in.token, session.token, "a new token at every login");
        let principal = valid(identity.session(signed_in.token.as_str()).await);
        assert_eq!(
            principal.session.auth_method,
            crucible_control::AuthMethod::Password
        );
        for (email, password) in [
            ("owner@example.com", "wrong password!!"),
            ("nobody@example.com", PASSWORD),
            ("not an email", PASSWORD),
        ] {
            assert_eq!(
                identity.login(email, password).await.unwrap_err(),
                AuthError::InvalidCredentials
            );
        }
        identity.logout(&principal).await.unwrap();
        assert!(matches!(
            identity.session(signed_in.token.as_str()).await,
            SessionVerdict::Invalid
        ));
        assert!(matches!(
            identity.session("garbage").await,
            SessionVerdict::Invalid
        ));
    });
}

#[test]
fn sessions_expire_idle_and_absolute_and_survive_an_outage_within_the_grace() {
    sim(2, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let session = owner(&identity).await;
        let token = session.token.as_str().to_owned();
        valid(identity.session(&token).await);

        // Postgres goes away: the cached session keeps working for the grace, then the node says 503.
        model.set_faults(PgFaults {
            unavailable_ppm: 1_000_000,
            ..PgFaults::default()
        });
        env.clock.sleep(Duration::from_secs(60)).await;
        valid(identity.session(&token).await);
        assert!(matches!(
            identity.login("owner@example.com", PASSWORD).await,
            Err(AuthError::Unavailable)
        ));
        env.clock.sleep(OUTAGE_GRACE).await;
        assert!(matches!(
            identity.session(&token).await,
            SessionVerdict::Unavailable
        ));
        model.set_faults(PgFaults::default());
        valid(identity.session(&token).await);

        // A revocation made while Postgres is up applies within the 5 s cache, also through an outage after.
        let other = identity.login("owner@example.com", PASSWORD).await.unwrap();
        let other_token = other.token.as_str().to_owned();
        valid(identity.session(&other_token).await);
        model
            .sessions()
            .revoke(&other.principal.token_hash, env.clock.wall_now())
            .await
            .unwrap();
        env.clock.sleep(CACHE_FOR).await;
        assert!(matches!(
            identity.session(&other_token).await,
            SessionVerdict::Invalid
        ));

        // Idle expiry: 7 days without a request.
        env.clock.sleep(SESSION_IDLE + Duration::from_secs(1)).await;
        assert!(matches!(
            identity.session(&token).await,
            SessionVerdict::Invalid
        ));

        // Absolute expiry: 30 days even with daily use.
        let busy = identity.login("owner@example.com", PASSWORD).await.unwrap();
        let busy = busy.token.as_str().to_owned();
        for _ in 0..29 {
            env.clock.sleep(Duration::from_secs(86_400)).await;
            valid(identity.session(&busy).await);
        }
        env.clock.sleep(Duration::from_secs(86_400)).await;
        assert!(matches!(
            identity.session(&busy).await,
            SessionVerdict::Invalid
        ));
    });
}

#[test]
fn require_https_refuses_sign_in_over_plain_http() {
    sim(8, async |env, model| {
        let mut plain = identity(&env, &model, OUTAGE_GRACE);
        plain.config.require_https = true;
        let link = plain.issue_setup_token().await.unwrap().unwrap();
        assert!(matches!(
            plain
                .setup(link.token.as_str(), "Acme", "o@example.com", "O", PASSWORD)
                .await,
            Err(AuthError::Forbidden(_))
        ));
        assert!(matches!(
            plain.login("o@example.com", PASSWORD).await,
            Err(AuthError::Forbidden(_))
        ));
        // Nothing was created by the refused setup.
        assert!(plain.setup_required().await.unwrap());
    });
}

#[test]
fn a_zero_grace_fails_closed() {
    sim(3, async |env, model| {
        let identity = identity(&env, &model, Duration::ZERO);
        let session = owner(&identity).await;
        model.set_faults(PgFaults {
            unavailable_ppm: 1_000_000,
            ..PgFaults::default()
        });
        env.clock.sleep(CACHE_FOR).await;
        assert!(matches!(
            identity.session(session.token.as_str()).await,
            SessionVerdict::Unavailable
        ));
    });
}

#[test]
fn invitations_members_and_ownership() {
    sim(4, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let owner = owner(&identity).await.principal;
        let project = identity.create_project("Agents", "agents").await.unwrap();
        identity.forget_sessions();
        let owner = valid(identity.session_for_test(&owner).await);
        let invited = identity
            .invite(
                &owner,
                "Ada@Example.com",
                OrgRole::Admin,
                vec![(project.id, ProjectRole::Admin)],
            )
            .await
            .unwrap();
        let ada = identity
            .accept_invitation(invited.token.as_str(), "Ada", PASSWORD)
            .await
            .unwrap()
            .principal;
        assert_eq!(ada.org_role, OrgRole::Admin);
        assert_eq!(ada.project_role(project.id), Some(ProjectRole::Admin));
        assert_eq!(
            identity
                .accept_invitation(invited.token.as_str(), "Ada", PASSWORD)
                .await
                .unwrap_err(),
            AuthError::TokenInvalid
        );
        // An admin can neither change the owner nor make one.
        assert!(matches!(
            identity
                .set_org_role(&ada, owner.user.id, OrgRole::Viewer)
                .await,
            Err(AuthError::Forbidden(_))
        ));
        assert!(matches!(
            identity
                .invite(&ada, "bo@example.com", OrgRole::Owner, Vec::new())
                .await,
            Err(AuthError::Forbidden(_))
        ));
        // A revoked invitation's link no longer works.
        let bo = identity
            .invite(&ada, "bo@example.com", OrgRole::Viewer, Vec::new())
            .await
            .unwrap();
        identity.revoke_invitation(bo.invitation.id).await.unwrap();
        assert_eq!(
            identity
                .accept_invitation(bo.token.as_str(), "Bo", PASSWORD)
                .await
                .unwrap_err(),
            AuthError::TokenInvalid
        );
        // Ownership moves by transfer only; the old owner becomes an admin.
        identity
            .transfer_ownership(&owner, ada.user.id)
            .await
            .unwrap();
        let roles = identity.org_members().await.unwrap();
        assert_eq!(
            roles
                .iter()
                .map(|(user, role)| (user.email.as_str().to_owned(), *role))
                .collect::<Vec<_>>(),
            [
                ("owner@example.com".to_owned(), OrgRole::Admin),
                ("ada@example.com".to_owned(), OrgRole::Owner)
            ]
        );
    });
}

#[test]
fn keys_work_at_once_and_stop_at_once() {
    sim(5, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        owner(&identity).await;
        let project = identity.create_project("Agents", "agents").await.unwrap();
        let created = identity
            .create_key(project.id, None, Some("ci".to_owned()), None)
            .await
            .unwrap();
        assert!(matches!(
            identity.keys().check_secret(&created.secret),
            KeyVerdict::Valid(_)
        ));
        assert_eq!(
            identity
                .revoke_key(project.id, created.key.id, created.key.version + 1)
                .await
                .unwrap_err(),
            AuthError::PreconditionFailed
        );
        identity
            .revoke_key(project.id, created.key.id, created.key.version)
            .await
            .unwrap();
        assert_eq!(
            identity.keys().check_secret(&created.secret),
            KeyVerdict::Invalid
        );
        assert!(
            identity.api_keys(project.id).await.unwrap()[0]
                .revoked_at
                .is_some()
        );
        // A taken slug is a conflict that says what to do.
        assert!(matches!(
            identity.create_project("Again", "agents").await,
            Err(AuthError::Conflict(_))
        ));
    });
}

#[test]
fn password_changes_and_resets_revoke_sessions() {
    sim(6, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let first = owner(&identity).await;
        let second = identity.login("owner@example.com", PASSWORD).await.unwrap();
        assert_eq!(
            identity
                .change_password(&second.principal, "wrong password!!", "new password 123")
                .await
                .unwrap_err(),
            AuthError::InvalidCredentials
        );
        identity
            .change_password(&second.principal, PASSWORD, "new password 123")
            .await
            .unwrap();
        // The other session is revoked; the one that changed the password stays.
        assert!(matches!(
            identity.session(first.token.as_str()).await,
            SessionVerdict::Invalid
        ));
        valid(identity.session(second.token.as_str()).await);

        let link = identity.issue_reset("owner@example.com").await.unwrap();
        assert_eq!(
            identity
                .issue_reset("nobody@example.com")
                .await
                .unwrap_err(),
            AuthError::NotFound
        );
        identity
            .reset_password(link.token.as_str(), "after the reset 1")
            .await
            .unwrap();
        assert!(matches!(
            identity.session(second.token.as_str()).await,
            SessionVerdict::Invalid
        ));
        assert_eq!(
            identity
                .reset_password(link.token.as_str(), "after the reset 2")
                .await
                .unwrap_err(),
            AuthError::TokenInvalid
        );
        identity
            .login("owner@example.com", "after the reset 1")
            .await
            .unwrap();
    });
}

fn person(email: &str, verified: bool) -> FakeUser {
    FakeUser {
        subject: "7".to_owned(),
        email: email.to_owned(),
        email_verified: verified,
        name: "Grace".to_owned(),
    }
}

/// Runs a provider sign-in through the fake provider and answers the outcome.
async fn provider_sign_in(
    identity: &Identity,
    idp: &FakeIdp,
    provider: crucible_records::Uuid,
) -> Result<SignedIn, FlowFailure> {
    let start = identity.start_flow(provider, Some("/traces/")).await?;
    let (code, state) = idp
        .authorize(&start.location)
        .map_err(|_| FlowFailure::ExchangeFailed)?;
    let query = CallbackQuery {
        code: Some(code),
        state: Some(state),
        error: None,
    };
    identity
        .finish_flow(provider, &query, Some(start.flow_cookie.as_str()))
        .await
}

#[test]
fn provider_sign_in_links_invited_known_and_allowed_domain_users_only() {
    sim(7, async |env, model| {
        let idp = FakeIdp::new(&env, person("grace@example.com", true));
        let env = idp.env(&env);
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let owner = owner(&identity).await.principal;
        let provider = identity
            .add_provider(ProviderInput {
                kind: ProviderKind::Oidc,
                display_name: "Company SSO".to_owned(),
                issuer: Some(ISSUER.to_owned()),
                client_id: CLIENT_ID.to_owned(),
                client_secret: CLIENT_SECRET.to_owned(),
                scopes: Vec::new(),
                allowed_domains: Vec::new(),
            })
            .await
            .unwrap();
        // Not invited, domain not allowed.
        assert_eq!(
            provider_sign_in(&identity, &idp, provider.id)
                .await
                .unwrap_err(),
            FlowFailure::NotInvited
        );
        // Invited: the user is created with the invitation's role and the next path is kept.
        identity
            .invite(&owner, "grace@example.com", OrgRole::Member, Vec::new())
            .await
            .unwrap();
        let signed_in = provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        assert_eq!(signed_in.next, "/traces/");
        assert_eq!(signed_in.session.principal.org_role, OrgRole::Member);
        assert_eq!(
            signed_in.session.principal.session.auth_method,
            crucible_control::AuthMethod::Oidc
        );
        // Linked: the same subject signs in again.
        provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        // An existing user with a verified email is linked.
        idp.set_user(FakeUser {
            subject: "8".to_owned(),
            ..person("owner@example.com", true)
        });
        let linked = provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        assert_eq!(linked.session.principal.user.id, owner.user.id);
        // An unverified email is refused, even for an existing user.
        idp.set_user(FakeUser {
            subject: "9".to_owned(),
            ..person("owner@example.com", false)
        });
        assert_eq!(
            provider_sign_in(&identity, &idp, provider.id)
                .await
                .unwrap_err(),
            FlowFailure::EmailUnverified
        );
        // An allowed domain joins as a viewer.
        let org = identity.org().await.unwrap().unwrap();
        identity
            .update_org(
                org.version,
                OrgChange {
                    allowed_email_domains: Some(vec!["partner.example".to_owned()]),
                    ..OrgChange::default()
                },
            )
            .await
            .unwrap();
        idp.set_user(FakeUser {
            subject: "10".to_owned(),
            ..person("lin@partner.example", true)
        });
        let joined = provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        assert_eq!(joined.session.principal.org_role, OrgRole::Viewer);

        // The state must match the flow cookie and works once; a decline is reported as such.
        let start = identity.start_flow(provider.id, None).await.unwrap();
        let (code, state) = idp.authorize(&start.location).unwrap();
        let query = CallbackQuery {
            code: Some(code),
            state: Some(state),
            error: None,
        };
        assert_eq!(
            identity
                .finish_flow(provider.id, &query, Some(&"y".repeat(43)))
                .await
                .unwrap_err(),
            FlowFailure::StateInvalid
        );
        identity
            .finish_flow(provider.id, &query, Some(start.flow_cookie.as_str()))
            .await
            .unwrap();
        assert_eq!(
            identity
                .finish_flow(provider.id, &query, Some(start.flow_cookie.as_str()))
                .await
                .unwrap_err(),
            FlowFailure::StateInvalid
        );
        let declined = CallbackQuery {
            error: Some("access_denied".to_owned()),
            ..CallbackQuery::default()
        };
        assert_eq!(
            identity
                .finish_flow(provider.id, &declined, None)
                .await
                .unwrap_err(),
            FlowFailure::Denied
        );
        // A disabled provider is refused at the start.
        identity
            .update_provider(
                provider.id,
                ProviderChange {
                    enabled: Some(false),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            identity.start_flow(provider.id, None).await.unwrap_err(),
            FlowFailure::ProviderDisabled
        );
    });
}

impl Identity {
    /// The principal of an existing session, read again (tests only).
    async fn session_for_test(&self, principal: &SessionPrincipal) -> SessionVerdict {
        match self.load(principal.token_hash).await {
            Ok(Some(p)) => SessionVerdict::Valid(Arc::new(p)),
            _ => SessionVerdict::Invalid,
        }
    }
}
