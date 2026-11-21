//! The identity service in simulation over `PgModel`: setup, password sign-in, session expiry and the outage
//! grace, invitations, member rules, keys, password changes and resets, and provider sign-in against the fake
//! provider.

use std::sync::Arc;
use std::time::Duration;

use crucible_control::{
    DeletionSelector, MediaObject, MediaRef, MediaState, OrgRole, ProjectRole, ProviderKind,
    Repos as _,
};
use crucible_env::Env;
use crucible_records::{DeletionState, MediaObjectId, ProjectId, Timestamp, TraceId};
use crucible_sim::pg_model::{PgFaults, PgModel};

use super::*;
use crate::csrf::Origins;
use crate::db_keys::{DbKeys, KeyVerdict, OutageMode};
use crate::password::HashCost;
use crate::seal::{Sealed, Unsealable};
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
        identity.sign_in_view().await.unwrap();

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
        // The sign-in view is served from the last read, so a reload during the outage still finds sign-in.
        assert!(!identity.sign_in_view().await.unwrap().setup_required);
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
        let by = owner(&identity).await.principal.user.id;
        let project = identity.create_project("Agents", "agents").await.unwrap();
        let created = identity
            .create_key(project.id, None, Some("ci".to_owned()), None, false)
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
        // Deleting a project revokes its keys at once.
        let other = identity
            .create_key(project.id, None, None, None, false)
            .await
            .unwrap();
        assert!(matches!(
            identity.keys().check_secret(&other.secret),
            KeyVerdict::Valid(_)
        ));
        identity
            .delete_project(project.id, project.version, by)
            .await
            .unwrap();
        assert_eq!(
            identity.keys().check_secret(&other.secret),
            KeyVerdict::Invalid
        );
        assert!(
            identity
                .api_keys(project.id)
                .await
                .unwrap()
                .iter()
                .all(|k| k.revoked_at.is_some())
        );
        // A taken slug is a conflict that says what to do.
        assert!(matches!(
            identity.create_project("Again", "default").await,
            Err(AuthError::Conflict(_))
        ));
    });
}

/// `crucible admin create-key`: the project by slug or id, and the import flag of contract 11.3.
#[test]
fn the_command_line_finds_projects_and_marks_import_keys() {
    sim(12, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let by = owner(&identity).await.principal.user.id;
        let project = identity.create_project("Agents", "agents").await.unwrap();
        let by_slug = identity.project_by_reference("agents").await.unwrap();
        let by_id = identity
            .project_by_reference(&project.id.to_string())
            .await
            .unwrap();
        assert_eq!((by_slug.id, by_id.id), (project.id, project.id));
        assert_eq!(
            identity.project_by_reference("nope").await.unwrap_err(),
            AuthError::NotFound
        );
        let import = identity
            .create_key(project.id, None, None, None, true)
            .await
            .unwrap();
        assert!(import.key.import);
        let KeyVerdict::Valid(grant) = identity.keys().check_secret(&import.secret) else {
            panic!("the import key does not verify");
        };
        assert!(grant.import);
        let plain = identity
            .create_key(project.id, None, None, None, false)
            .await
            .unwrap();
        assert!(!plain.key.import);
        identity
            .delete_project(project.id, project.version, by)
            .await
            .unwrap();
        assert_eq!(
            identity.project_by_reference("agents").await.unwrap_err(),
            AuthError::NotFound
        );
    });
}

/// Contract 13.4: a media use recorded while its project is deleted, after the deletion's snapshot, keeps its
/// object row, which the deletion cannot then delete. Nothing is applied and the deletion answers as a lost race,
/// which the caller retries; deleting again removes the use with the rest.
#[test]
fn a_media_use_that_races_the_deletion_of_its_project_makes_the_caller_retry() {
    sim(13, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let by = owner(&identity).await.principal.user.id;
        let project = identity.create_project("Agents", "agents").await.unwrap();
        let at = Timestamp::from_micros(1_800_000_000_000_000);
        let object = MediaObject {
            id: MediaObjectId::from_bytes([7; 16]),
            project_id: project.id,
            sha256: [7; 32],
            content_type: "image/png".to_owned(),
            len: 4,
            state: MediaState::Uploaded,
            created_at: at,
            uploaded_at: Some(at),
        };
        model.media().insert_object(&object).await.unwrap();
        let trace = TraceId::from_bytes([9; 16]);
        let reference = MediaRef {
            project_id: project.id,
            media_id: object.id,
            trace_id: trace,
            span_id: None,
            field: "input".to_owned(),
            target_time: at,
        };
        // Every statement answers after 10 ms. The deletion reads the project (10 ms) and its keys (20 ms), begins
        // (30 ms) and takes its snapshot with its first write (40 ms); it deletes the media rows at 60 ms at the
        // earliest. The use lands between, at 55 ms.
        model.set_faults(PgFaults {
            latency_micros: (10_000, 10_000),
            ..PgFaults::default()
        });
        let late_use = async {
            env.clock.sleep(Duration::from_millis(45)).await;
            model.media().add_ref(&reference).await
        };
        let (deleted, used) = futures::join!(
            identity.delete_project(project.id, project.version, by),
            late_use
        );
        assert_eq!(used, Ok(true));
        assert_eq!(deleted.unwrap_err(), AuthError::Unavailable);

        let uses = model.media().uses(project.id, trace, None).await.unwrap();
        assert_eq!(uses.len(), 1);
        assert!(identity.project_by_reference("agents").await.is_ok());
        identity
            .delete_project(project.id, project.version, by)
            .await
            .unwrap();
        let uses = model.media().uses(project.id, trace, None).await.unwrap();
        assert!(uses.is_empty());
        let row = model.media().object(project.id, object.id).await.unwrap();
        assert!(row.is_none());
    });
}

/// Contract 13.4: deleting a project makes, in the same transaction, the deletion request that purges its data:
/// pending, of kind project, at the instant the project was marked deleted, by the user who deleted it. A deletion
/// refused for a stale version makes none.
#[test]
fn deleting_a_project_starts_the_purge_of_its_data() {
    sim(41, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let by = owner(&identity).await.principal.user.id;
        let project = identity.create_project("Agents", "agents").await.unwrap();
        assert_eq!(
            identity
                .delete_project(project.id, project.version + 1, by)
                .await
                .unwrap_err(),
            AuthError::PreconditionFailed
        );
        let requests = |model: PgModel| async move {
            model
                .lifecycle()
                .list_requests(project.id, None, 10)
                .await
                .unwrap()
        };
        assert!(requests(model.clone()).await.is_empty());
        let deleted = identity
            .delete_project(project.id, project.version, by)
            .await
            .unwrap();
        let made = requests(model.clone()).await;
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].selector, DeletionSelector::Project);
        assert_eq!(made[0].state, DeletionState::Pending);
        assert_eq!(Some(made[0].requested_at), deleted.deleted_at);
        assert_eq!(made[0].requested_by, format!("user:{by}"));
        assert!(made[0].deletion_seq > 0);
    });
}

/// The salt of `project` as the control store holds it, opened as the sealer of [`identity`] opens it, with the
/// associated data that binds it to the project: `None` when the project has no salt row.
async fn stored_salt(model: &PgModel, project: ProjectId) -> Option<Result<Vec<u8>, Unsealable>> {
    let row = model.project_salts().get(project).await.unwrap()?;
    let sealed = Sealed {
        key_id: row.key_id,
        nonce: row.nonce,
        ciphertext: row.ciphertext,
    };
    let aad = format!("{}\0project_salt", project.uuid());
    Some(Sealer::new("master-0", [6; 32]).open(&sealed, aad.as_bytes()))
}

/// Contract 9.3: a project gets its salt in the transaction that creates it, on every path that creates one:
/// first-owner setup and `crucible admin init` (the default project) and project creation. Each salt is 32 bytes
/// of its own, sealed under the master key and bound to its project.
#[test]
fn projects_are_created_with_their_salt() {
    sim(9, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        owner(&identity).await;
        let default = identity.project_by_reference("default").await.unwrap();
        let first = stored_salt(&model, default.id)
            .await
            .expect("setup made the default project's salt")
            .expect("the salt opens");
        let project = identity.create_project("Agents", "agents").await.unwrap();
        let second = stored_salt(&model, project.id)
            .await
            .expect("the project's salt exists as soon as the project does")
            .expect("the salt opens");
        assert_eq!((first.len(), second.len()), (32, 32));
        assert_ne!(first, second, "each project has a salt of its own");
        // Bound to its project: one project's row does not open as another's.
        let row = model
            .project_salts()
            .get(project.id)
            .await
            .unwrap()
            .unwrap();
        let aad = format!("{}\0project_salt", default.id.uuid());
        let sealed = Sealed {
            key_id: row.key_id,
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        };
        assert_eq!(
            Sealer::new("master-0", [6; 32]).open(&sealed, aad.as_bytes()),
            Err(Unsealable)
        );
        // A taken slug is still a conflict that says what to do, and leaves no salt behind.
        assert!(matches!(
            identity.create_project("Again", "agents").await,
            Err(AuthError::Conflict(_))
        ));
        assert_eq!(model.project_salts().list().await.unwrap().len(), 2);
    });
    sim(10, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        identity
            .init_owner("Acme", "owner@example.com", "Olivia", PASSWORD)
            .await
            .unwrap();
        let default = identity.project_by_reference("default").await.unwrap();
        assert!(matches!(
            stored_salt(&model, default.id).await,
            Some(Ok(salt)) if salt.len() == 32
        ));
    });
}

/// A project and its salt commit together: while connections drop and commits end with an unknown outcome,
/// every project that exists has its salt.
#[test]
fn a_project_never_exists_without_its_salt() {
    sim(11, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        owner(&identity).await;
        model.set_faults(PgFaults {
            unavailable_ppm: 150_000,
            commit_unknown_ppm: 150_000,
            ..PgFaults::default()
        });
        for n in 0..60 {
            // A failed creation may or may not have committed; either way the pair is whole.
            let _ = identity.create_project("Project", &format!("p-{n}")).await;
        }
        model.set_faults(PgFaults::default());
        let projects = model.projects().list().await.unwrap();
        assert!(projects.len() > 20, "{} projects", projects.len());
        for project in projects {
            assert!(
                stored_salt(&model, project.id).await.is_some(),
                "project {} has no salt",
                project.slug
            );
        }
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
            .update_provider(provider.id, provider.version, toggle(false))
            .await
            .unwrap();
        assert_eq!(
            identity.start_flow(provider.id, None).await.unwrap_err(),
            FlowFailure::ProviderDisabled
        );
    });
}

/// The fake OpenID Connect provider as a provider to add, with the button text `name`.
fn oidc(name: &str) -> ProviderInput {
    ProviderInput {
        kind: ProviderKind::Oidc,
        display_name: name.to_owned(),
        issuer: Some(ISSUER.to_owned()),
        client_id: CLIENT_ID.to_owned(),
        client_secret: CLIENT_SECRET.to_owned(),
        scopes: Vec::new(),
        allowed_domains: Vec::new(),
    }
}

/// The fake GitHub as a provider to add.
fn github() -> ProviderInput {
    ProviderInput {
        kind: ProviderKind::Github,
        display_name: "Continue with GitHub".to_owned(),
        issuer: None,
        ..oidc("")
    }
}

fn toggle(enabled: bool) -> ProviderChange {
    ProviderChange {
        enabled: Some(enabled),
        ..ProviderChange::default()
    }
}

/// Turns password sign-in on or off at the organization's current version.
async fn set_password(identity: &Identity, on: bool) -> Result<crucible_control::Org, AuthError> {
    let org = identity.org().await.unwrap().unwrap();
    identity
        .update_org(
            org.version,
            OrgChange {
                password_login_enabled: Some(on),
                ..OrgChange::default()
            },
        )
        .await
}

/// Whether someone can sign in: password sign-in is on, or the sign-in page offers a provider.
async fn a_way_in(identity: &Identity) -> bool {
    let view = identity.sign_in_view().await.unwrap();
    view.password_login_enabled || !view.providers.is_empty()
}

/// The provider with this id as stored.
async fn stored(identity: &Identity, id: crucible_records::Uuid) -> crucible_control::AuthProvider {
    identity
        .providers()
        .await
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == id)
        .unwrap()
}

#[test]
fn a_change_that_leaves_no_way_to_sign_in_is_refused() {
    sim(13, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        owner(&identity).await;
        // Password sign-in is the only way in, so it stays on; the refusal says what to turn on first.
        let AuthError::Conflict(message) = set_password(&identity, false).await.unwrap_err() else {
            panic!("turning off the only way in is a conflict");
        };
        assert!(message.contains("turn on a sign-in provider"), "{message}");
        // A provider that is turned off is no way in either.
        let first = identity.add_provider(oidc("Company SSO")).await.unwrap();
        let first = identity
            .update_provider(first.id, first.version, toggle(false))
            .await
            .unwrap();
        assert!(matches!(
            set_password(&identity, false).await,
            Err(AuthError::Conflict(_))
        ));
        // With the provider on, password sign-in can go.
        let first = identity
            .update_provider(first.id, first.version, toggle(true))
            .await
            .unwrap();
        let org = set_password(&identity, false).await.unwrap();
        assert!(!org.settings.password_login_enabled);
        assert!(a_way_in(&identity).await);
        // The provider is now the only way in: turning it off or deleting it is refused, and the refusal says to
        // turn password sign-in on first.
        let AuthError::Conflict(message) = identity
            .update_provider(first.id, first.version, toggle(false))
            .await
            .unwrap_err()
        else {
            panic!("turning off the last way in is a conflict");
        };
        assert!(message.contains("turn password sign-in on"), "{message}");
        assert!(matches!(
            identity.delete_provider(first.id, first.version).await,
            Err(AuthError::Conflict(_))
        ));
        // A second provider that is on lets the first one go, and is then the last way in itself.
        let second = identity.add_provider(github()).await.unwrap();
        identity
            .delete_provider(first.id, first.version)
            .await
            .unwrap();
        assert!(matches!(
            identity
                .update_provider(second.id, second.version, toggle(false))
                .await,
            Err(AuthError::Conflict(_))
        ));
        // Turning password sign-in on is always allowed; then the last provider can go.
        set_password(&identity, true).await.unwrap();
        let second = stored(&identity, second.id).await;
        identity
            .delete_provider(second.id, second.version)
            .await
            .unwrap();
        assert!(a_way_in(&identity).await);
    });
    // Without server.public_url no provider is offered, so the refusal names that setting.
    sim(14, async |env, model| {
        let mut identity = identity(&env, &model, OUTAGE_GRACE);
        identity.config.public_url = None;
        owner(&identity).await;
        identity.add_provider(oidc("Company SSO")).await.unwrap();
        let AuthError::Conflict(message) = set_password(&identity, false).await.unwrap_err() else {
            panic!("no provider is offered without server.public_url");
        };
        assert!(message.contains("server.public_url"), "{message}");
    });
}

/// The guard reads two rows (the organization and the providers) and writes one, so two changes made at once could
/// each see the other's way in. Every change that removes a way in also moves the organization's version, so one
/// of two such changes fails instead.
#[test]
fn two_changes_made_at_once_never_leave_no_way_to_sign_in() {
    let slow = PgFaults {
        latency_micros: (0, 4_000),
        ..PgFaults::default()
    };
    for seed in 0..24 {
        let faults = slow.clone();
        sim(200 + seed, async move |env, model| {
            let identity = identity(&env, &model, OUTAGE_GRACE);
            owner(&identity).await;
            // Password sign-in goes while the only provider is turned off.
            let only = identity.add_provider(oidc("Only")).await.unwrap();
            model.set_faults(faults.clone());
            let _ = futures::join!(
                set_password(&identity, false),
                identity.update_provider(only.id, only.version, toggle(false)),
            );
            model.set_faults(PgFaults::default());
            assert!(
                a_way_in(&identity).await,
                "seed {seed}: password off and the provider off"
            );
            // With password sign-in off, two providers go at once.
            set_password(&identity, true).await.unwrap();
            let only = stored(&identity, only.id).await;
            if !only.enabled {
                identity
                    .update_provider(only.id, only.version, toggle(true))
                    .await
                    .unwrap();
            }
            let other = identity.add_provider(github()).await.unwrap();
            set_password(&identity, false).await.unwrap();
            let only = stored(&identity, only.id).await;
            model.set_faults(faults);
            let _ = futures::join!(
                identity.update_provider(only.id, only.version, toggle(false)),
                identity.delete_provider(other.id, other.version),
            );
            model.set_faults(PgFaults::default());
            assert!(a_way_in(&identity).await, "seed {seed}: both providers off");
        });
    }
}

/// A turn-off that loses to another change of its provider writes nothing, not even the organization's version.
#[test]
fn a_turn_off_that_loses_a_race_leaves_the_organization_as_it_was() {
    let slow = PgFaults {
        latency_micros: (0, 4_000),
        ..PgFaults::default()
    };
    let mut lost = 0;
    for seed in 0..24 {
        let faults = slow.clone();
        lost += sim(500 + seed, async move |env, model| {
            let identity = identity(&env, &model, OUTAGE_GRACE);
            owner(&identity).await;
            let provider = identity.add_provider(oidc("SSO")).await.unwrap();
            let before = identity.org().await.unwrap().unwrap().version;
            let rename = ProviderChange {
                display_name: Some("Renamed".to_owned()),
                ..ProviderChange::default()
            };
            model.set_faults(faults);
            let (_, off) = futures::join!(
                identity.update_provider(provider.id, provider.version, rename),
                identity.update_provider(provider.id, provider.version, toggle(false)),
            );
            model.set_faults(PgFaults::default());
            let after = identity.org().await.unwrap().unwrap().version;
            if off.is_ok() {
                assert_eq!(after, before + 1, "seed {seed}");
                0
            } else {
                assert_eq!(
                    after, before,
                    "seed {seed}: a refused turn-off moved the organization"
                );
                1
            }
        });
    }
    assert!(lost > 0, "no seed made the turn-off lose the race");
}

/// One step of [`no_sequence_of_changes_leaves_no_way_to_sign_in`]: providers are named by their place in the list.
#[derive(Clone, Debug)]
enum Step {
    Password(bool),
    Add(bool),
    Toggle(usize),
    Delete(usize),
}

fn step() -> impl proptest::strategy::Strategy<Value = Step> {
    use proptest::prelude::*;
    prop_oneof![
        any::<bool>().prop_map(Step::Password),
        any::<bool>().prop_map(Step::Add),
        (0..4usize).prop_map(Step::Toggle),
        (0..4usize).prop_map(Step::Delete),
    ]
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]
    /// Whatever an administrator asks, in any order, someone can still sign in afterwards.
    #[test]
    fn no_sequence_of_changes_leaves_no_way_to_sign_in(
        steps in proptest::collection::vec(step(), 1..14),
    ) {
        let left = sim(300, async move |env, model| {
            let identity = identity(&env, &model, OUTAGE_GRACE);
            owner(&identity).await;
            for step in steps {
                let providers = identity.providers().await.unwrap();
                let pick = |n: usize| providers.get(n % providers.len().max(1)).cloned();
                let _ = match step {
                    Step::Password(on) => set_password(&identity, on).await.map(drop),
                    Step::Add(oidc_kind) => {
                        let input = if oidc_kind { oidc("SSO") } else { github() };
                        identity.add_provider(input).await.map(drop)
                    }
                    Step::Toggle(n) => match pick(n) {
                        Some(p) => identity
                            .update_provider(p.id, p.version, toggle(!p.enabled))
                            .await
                            .map(drop),
                        None => Ok(()),
                    },
                    Step::Delete(n) => match pick(n) {
                        Some(p) => identity.delete_provider(p.id, p.version).await,
                        None => Ok(()),
                    },
                };
                if !a_way_in(&identity).await {
                    return false;
                }
            }
            true
        });
        proptest::prop_assert!(left, "a sequence of changes left no way to sign in");
    }
}

#[test]
fn provider_changes_name_the_version_they_read() {
    sim(15, async |env, model| {
        let identity = identity(&env, &model, OUTAGE_GRACE);
        owner(&identity).await;
        let provider = identity.add_provider(oidc("Company SSO")).await.unwrap();
        assert_eq!(provider.version, 1);
        let renamed = identity
            .update_provider(
                provider.id,
                provider.version,
                ProviderChange {
                    display_name: Some("Acme SSO".to_owned()),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (renamed.display_name.as_str(), renamed.version),
            ("Acme SSO", 2)
        );
        assert_eq!(stored(&identity, provider.id).await, renamed);
        // The version the provider was read at is stale now.
        assert_eq!(
            identity
                .update_provider(provider.id, provider.version, toggle(false))
                .await
                .unwrap_err(),
            AuthError::PreconditionFailed
        );
        assert_eq!(
            identity
                .delete_provider(provider.id, provider.version)
                .await
                .unwrap_err(),
            AuthError::PreconditionFailed
        );
        identity
            .delete_provider(renamed.id, renamed.version)
            .await
            .unwrap();
        assert_eq!(
            identity
                .delete_provider(renamed.id, renamed.version)
                .await
                .unwrap_err(),
            AuthError::NotFound
        );
    });
}

fn refused_field(result: Result<crucible_control::AuthProvider, AuthError>) -> &'static str {
    match result {
        Err(AuthError::Invalid { field, .. }) => field,
        other => panic!("expected a field error, got {other:?}"),
    }
}

#[test]
fn provider_edits_change_the_client_and_keep_the_secret_unless_a_new_one_is_given() {
    sim(16, async |env, model| {
        let idp = FakeIdp::new(&env, person("grace@example.com", true));
        let env = idp.env(&env);
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let owner = owner(&identity).await.principal;
        identity
            .invite(&owner, "grace@example.com", OrgRole::Member, Vec::new())
            .await
            .unwrap();
        let provider = identity
            .add_provider(ProviderInput {
                client_id: "an-old-client".to_owned(),
                ..oidc("Company SSO")
            })
            .await
            .unwrap();
        assert_eq!(
            provider_sign_in(&identity, &idp, provider.id)
                .await
                .unwrap_err(),
            FlowFailure::ExchangeFailed
        );
        // The client id, issuer and scopes change, checked as on create; an empty secret is refused.
        let change = |f: fn(&mut ProviderChange)| {
            let mut change = ProviderChange::default();
            f(&mut change);
            change
        };
        for (bad, field) in [
            (
                change(|c| c.issuer = Some("login.example.com".to_owned())),
                "issuer",
            ),
            (change(|c| c.client_id = Some("  ".to_owned())), "client_id"),
            (
                change(|c| c.client_secret = Some(String::new())),
                "client_secret",
            ),
            (
                change(|c| c.client_secret = Some("  ".to_owned())),
                "client_secret",
            ),
        ] {
            assert_eq!(
                refused_field(
                    identity
                        .update_provider(provider.id, provider.version, bad)
                        .await
                ),
                field
            );
        }
        let changed = identity
            .update_provider(
                provider.id,
                provider.version,
                ProviderChange {
                    client_id: Some(format!(" {CLIENT_ID} ")),
                    scopes: Some(vec!["groups".to_owned()]),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (changed.client_id.as_str(), changed.scopes.as_slice()),
            (CLIENT_ID, ["groups".to_owned()].as_slice())
        );
        assert_eq!(changed.secret_id, provider.secret_id);
        // Without a client secret the stored one stays, and sign-in uses it.
        let grace = provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap()
            .session
            .principal
            .user
            .id;
        assert_eq!(
            model
                .oauth_accounts()
                .list_for_user(grace)
                .await
                .unwrap()
                .len(),
            1
        );
        // A subject at another issuer is another person, so an issuer change unlinks the provider's accounts; people
        // link again by their verified email.
        let moved = identity
            .update_provider(
                provider.id,
                changed.version,
                ProviderChange {
                    issuer: Some("https://login.example.com".to_owned()),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(moved.issuer, "https://login.example.com");
        assert!(
            model
                .oauth_accounts()
                .list_for_user(grace)
                .await
                .unwrap()
                .is_empty()
        );
        let back = identity
            .update_provider(
                provider.id,
                moved.version,
                ProviderChange {
                    issuer: Some(ISSUER.to_owned()),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        let again = provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        assert_eq!(again.session.principal.user.id, grace);
        // A new secret replaces the stored one: the wrong one fails at the provider, the right one works.
        let wrong = identity
            .update_provider(
                provider.id,
                back.version,
                ProviderChange {
                    client_secret: Some("not-the-secret".to_owned()),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            provider_sign_in(&identity, &idp, provider.id)
                .await
                .unwrap_err(),
            FlowFailure::ExchangeFailed
        );
        identity
            .update_provider(
                provider.id,
                wrong.version,
                ProviderChange {
                    client_secret: Some(CLIENT_SECRET.to_owned()),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        // GitHub and Google have a fixed issuer, as on create.
        let hub = identity.add_provider(github()).await.unwrap();
        let hub = identity
            .update_provider(
                hub.id,
                hub.version,
                ProviderChange {
                    issuer: Some("https://login.example.com".to_owned()),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hub.issuer, "");
    });
}

#[test]
fn a_provider_checks_its_allowed_domains_at_every_sign_in() {
    sim(17, async |env, model| {
        let idp = FakeIdp::new(&env, person("grace@example.com", true));
        let env = idp.env(&env);
        let identity = identity(&env, &model, OUTAGE_GRACE);
        let owner = owner(&identity).await.principal;
        identity
            .invite(&owner, "grace@example.com", OrgRole::Member, Vec::new())
            .await
            .unwrap();
        let provider = identity.add_provider(oidc("Company SSO")).await.unwrap();
        provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        // The provider now allows other domains: the linked account is refused, and the refusal names them.
        let provider = identity
            .update_provider(
                provider.id,
                provider.version,
                ProviderChange {
                    allowed_domains: Some(vec!["acme.test".to_owned(), "@Partner.test".to_owned()]),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        let refused = provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap_err();
        assert_eq!(
            refused,
            FlowFailure::DomainNotAllowed(vec!["acme.test".to_owned(), "partner.test".to_owned()])
        );
        assert_eq!(refused.code(), "not_invited");
        assert_eq!(
            refused.login_query(),
            "error=not_invited&domains=acme.test%2Cpartner.test"
        );
        // An unverified email shows no domain, so a linked account is refused for that too.
        idp.set_user(person("grace@acme.test", false));
        assert_eq!(
            provider_sign_in(&identity, &idp, provider.id)
                .await
                .unwrap_err(),
            FlowFailure::EmailUnverified
        );
        // An allowed domain signs the linked account in.
        idp.set_user(person("grace@acme.test", true));
        let linked = provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        assert_eq!(
            linked.session.principal.user.email.as_str(),
            "grace@example.com"
        );
        // Without allowed domains, a linked account signs in as before, whatever its provider says of the email.
        identity
            .update_provider(
                provider.id,
                provider.version,
                ProviderChange {
                    allowed_domains: Some(Vec::new()),
                    ..ProviderChange::default()
                },
            )
            .await
            .unwrap();
        idp.set_user(person("grace@elsewhere.test", false));
        provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
    });
}

/// `crucible admin enable-password-login` for a deployment whose provider broke at the identity provider; a person
/// who joined through a provider then sets a password with a reset link.
#[test]
fn password_sign_in_comes_back_from_the_command_line() {
    sim(19, async |env, model| {
        let idp = FakeIdp::new(&env, person("grace@example.com", true));
        let env = idp.env(&env);
        let identity = identity(&env, &model, OUTAGE_GRACE);
        assert_eq!(
            identity.enable_password_login().await.unwrap_err(),
            AuthError::NotFound
        );
        let owner = owner(&identity).await.principal;
        assert!(
            !identity.enable_password_login().await.unwrap(),
            "already on"
        );
        identity
            .invite(&owner, "grace@example.com", OrgRole::Admin, Vec::new())
            .await
            .unwrap();
        let provider = identity.add_provider(oidc("Company SSO")).await.unwrap();
        provider_sign_in(&identity, &idp, provider.id)
            .await
            .unwrap();
        set_password(&identity, false).await.unwrap();
        assert!(matches!(
            identity.login("owner@example.com", PASSWORD).await,
            Err(AuthError::Forbidden(_))
        ));
        assert!(identity.enable_password_login().await.unwrap());
        identity.login("owner@example.com", PASSWORD).await.unwrap();
        // Grace joined through the provider and has no password yet; a reset link sets one.
        let link = identity.issue_reset("grace@example.com").await.unwrap();
        identity
            .reset_password(link.token.as_str(), "a new password 1")
            .await
            .unwrap();
        identity
            .login("grace@example.com", "a new password 1")
            .await
            .unwrap();
    });
}

#[test]
fn providers_from_the_configuration_are_seeded_read_only_and_turned_off_when_removed() {
    sim(18, async |env, model| {
        let idp = FakeIdp::new(&env, person("grace@example.com", true));
        let env = idp.env(&env);
        let mut identity = identity(&env, &model, OUTAGE_GRACE);
        identity.config.providers = vec![oidc("Company SSO")];
        let owner = owner(&identity).await.principal;
        identity.seed_providers().await.unwrap();
        let seeded = identity.providers().await.unwrap();
        assert_eq!(seeded.len(), 1);
        let row = seeded[0].clone();
        assert!(row.seeded && row.enabled, "{row:?}");
        assert_eq!(row.secret_id, None, "the secret stays in the configuration");
        // A restart with the same configuration writes nothing.
        identity.seed_providers().await.unwrap();
        assert_eq!(identity.providers().await.unwrap(), seeded);
        // Read-only through the API.
        assert!(matches!(
            identity
                .update_provider(row.id, row.version, toggle(false))
                .await,
            Err(AuthError::Forbidden(_))
        ));
        assert!(matches!(
            identity.delete_provider(row.id, row.version).await,
            Err(AuthError::Forbidden(_))
        ));
        // Sign-in uses the secret from the configuration.
        identity
            .invite(&owner, "grace@example.com", OrgRole::Member, Vec::new())
            .await
            .unwrap();
        provider_sign_in(&identity, &idp, row.id).await.unwrap();
        // A changed configuration changes the row in place: its id is in the callback URL.
        identity.config.providers[0].display_name = "Acme SSO".to_owned();
        identity.config.providers[0].allowed_domains = vec!["Example.com".to_owned()];
        identity.seed_providers().await.unwrap();
        let changed = stored(&identity, row.id).await;
        assert_eq!(
            (
                changed.display_name.as_str(),
                changed.allowed_domains.as_slice(),
                changed.version
            ),
            ("Acme SSO", ["example.com".to_owned()].as_slice(), 2)
        );
        // Removed from the configuration, the row stays, turned off, and the sign-in page no longer offers it.
        identity.config.providers.clear();
        identity.seed_providers().await.unwrap();
        let off = stored(&identity, row.id).await;
        assert!(!off.enabled);
        assert!(identity.sign_in_providers().await.unwrap().is_empty());
        assert!(!identity.nobody_can_sign_in().await.unwrap());
        // Back in the configuration, the same row is on again.
        identity.config.providers = vec![oidc("Acme SSO")];
        identity.seed_providers().await.unwrap();
        let on = identity.providers().await.unwrap();
        assert_eq!(on.len(), 1);
        assert!(on[0].enabled && on[0].id == row.id);
        assert!(!identity.nobody_can_sign_in().await.unwrap());
    });
}

/// Two nodes start at once with the same configuration: one row per kind, and neither start fails.
#[test]
fn nodes_that_start_together_seed_one_row_per_kind() {
    for seed in 0..16 {
        sim(400 + seed, async move |env, model| {
            let mut first = identity(&env, &model, OUTAGE_GRACE);
            let mut second = identity(&env, &model, OUTAGE_GRACE);
            let configured = vec![oidc("Company SSO"), github()];
            first.config.providers = configured.clone();
            second.config.providers = configured;
            model.set_faults(PgFaults {
                latency_micros: (0, 4_000),
                ..PgFaults::default()
            });
            let (a, b) = futures::join!(first.seed_providers(), second.seed_providers());
            model.set_faults(PgFaults::default());
            a.unwrap();
            b.unwrap();
            let rows = first.providers().await.unwrap();
            let mut kinds: Vec<_> = rows.iter().map(|p| (p.kind, p.seeded, p.enabled)).collect();
            kinds.sort();
            assert_eq!(
                kinds,
                [
                    (ProviderKind::Github, true, true),
                    (ProviderKind::Oidc, true, true)
                ],
                "seed {seed}"
            );
        });
    }
}

/// How many provider accounts `user` has linked.
async fn links(model: &PgModel, user: crucible_records::UserId) -> usize {
    model
        .oauth_accounts()
        .list_for_user(user)
        .await
        .unwrap()
        .len()
}

/// A configured OpenID Connect provider moved to another issuer unlinks its accounts, as an edit through the API
/// does: a subject at another issuer is another person. People link again through their verified email.
#[test]
fn a_configured_provider_at_another_issuer_unlinks_its_accounts() {
    sim(24, async |env, model| {
        let idp = FakeIdp::new(&env, person("grace@example.com", true));
        let env = idp.env(&env);
        let mut identity = identity(&env, &model, OUTAGE_GRACE);
        identity.config.providers = vec![oidc("Company SSO")];
        let owner = owner(&identity).await.principal;
        identity.seed_providers().await.unwrap();
        let row = identity.providers().await.unwrap()[0].clone();
        identity
            .invite(&owner, "grace@example.com", OrgRole::Member, Vec::new())
            .await
            .unwrap();
        let grace = provider_sign_in(&identity, &idp, row.id)
            .await
            .unwrap()
            .session
            .principal
            .user
            .id;
        assert_eq!(links(&model, grace).await, 1);
        // Another change keeps the links.
        identity.config.providers[0].display_name = "Acme SSO".to_owned();
        identity.seed_providers().await.unwrap();
        assert_eq!(links(&model, grace).await, 1);
        identity.config.providers[0].issuer = Some("https://login.example.com".to_owned());
        assert_eq!(
            identity.seed_providers().await.unwrap(),
            [(ProviderKind::Oidc, Seeded::Changed)]
        );
        assert_eq!(
            stored(&identity, row.id).await.issuer,
            "https://login.example.com"
        );
        assert_eq!(links(&model, grace).await, 0);
        // Back at the first issuer, Grace links again through her verified email.
        identity.config.providers[0].issuer = Some(ISSUER.to_owned());
        identity.seed_providers().await.unwrap();
        let again = provider_sign_in(&identity, &idp, row.id).await.unwrap();
        assert_eq!(again.session.principal.user.id, grace);
        assert_eq!(links(&model, grace).await, 1);
    });
}

/// A start whose configuration no longer declares the last way to sign in (password sign-in off, no other provider
/// offered) keeps that provider's row on, so the start leaves the organization as it was and the section can come
/// back. This server cannot sign anyone in through it, because its client secret was in the section. Once password
/// sign-in is on, as the start's message says, the next start turns the row off.
#[test]
fn a_configured_provider_that_is_the_last_way_in_stays_on_when_its_section_goes() {
    sim(25, async |env, model| {
        let mut identity = identity(&env, &model, OUTAGE_GRACE);
        identity.config.providers = vec![oidc("Company SSO")];
        owner(&identity).await;
        identity.seed_providers().await.unwrap();
        let row = identity.providers().await.unwrap()[0].clone();
        set_password(&identity, false).await.unwrap();
        let org = identity.org().await.unwrap().unwrap();
        identity.config.providers.clear();
        for _ in 0..2 {
            assert_eq!(
                identity.seed_providers().await.unwrap(),
                [(ProviderKind::Oidc, Seeded::KeptOn)]
            );
            assert_eq!(stored(&identity, row.id).await, row);
            assert_eq!(identity.org().await.unwrap().unwrap(), org);
        }
        assert!(identity.sign_in_providers().await.unwrap().is_empty());
        assert!(identity.nobody_can_sign_in().await.unwrap());
        assert!(identity.enable_password_login().await.unwrap());
        assert_eq!(
            identity.seed_providers().await.unwrap(),
            [(ProviderKind::Oidc, Seeded::TurnedOff)]
        );
        assert!(!stored(&identity, row.id).await.enabled);
    });
}

/// A start that swaps one configured provider for another adds the new one before it judges the old one, so the old
/// one is turned off rather than kept as the last way in.
#[test]
fn a_start_that_swaps_configured_providers_turns_the_old_one_off() {
    sim(26, async |env, model| {
        let mut identity = identity(&env, &model, OUTAGE_GRACE);
        identity.config.providers = vec![github()];
        owner(&identity).await;
        identity.seed_providers().await.unwrap();
        set_password(&identity, false).await.unwrap();
        identity.config.providers = vec![oidc("Company SSO")];
        let mut changed = identity.seed_providers().await.unwrap();
        changed.sort_by_key(|(kind, _)| *kind);
        assert_eq!(
            changed,
            [
                (ProviderKind::Github, Seeded::TurnedOff),
                (ProviderKind::Oidc, Seeded::Added)
            ]
        );
        let mut rows: Vec<_> = identity
            .providers()
            .await
            .unwrap()
            .iter()
            .map(|p| (p.kind, p.enabled))
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            [(ProviderKind::Github, false), (ProviderKind::Oidc, true)]
        );
        assert!(a_way_in(&identity).await);
    });
}

/// One node starts without a provider's section while another, which still declares it, turns password sign-in off.
/// The start's turn-off moves the organization's version as an API change does, so one of the two refuses or fails
/// and someone can still sign in.
#[test]
fn a_removal_at_start_and_a_password_turn_off_at_once_leave_a_way_in() {
    let slow = PgFaults {
        latency_micros: (0, 4_000),
        ..PgFaults::default()
    };
    for seed in 0..24 {
        let faults = slow.clone();
        sim(600 + seed, async move |env, model| {
            let mut declares = identity(&env, &model, OUTAGE_GRACE);
            declares.config.providers = vec![oidc("Company SSO")];
            owner(&declares).await;
            declares.seed_providers().await.unwrap();
            let dropped = identity(&env, &model, OUTAGE_GRACE);
            model.set_faults(faults);
            let _ = futures::join!(set_password(&declares, false), dropped.seed_providers());
            model.set_faults(PgFaults::default());
            assert!(
                a_way_in(&declares).await,
                "seed {seed}: password off and the provider off"
            );
        });
    }
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
