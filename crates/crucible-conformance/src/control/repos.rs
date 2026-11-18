//! Every repository once: reads, writes, optimistic versions, unique and foreign-key constraints, cascades.

use anyhow::{Result, ensure};
use crucible_control::{
    AuthMethod, Credential, LedgerEntry, MigrationKind, Org, OrgMembership, OrgRole, OrgSettings,
    ProjectAccess, ProjectMembership, ProjectRole, Secret, Session, SigningKey, SigningKeyState,
    SigningPurpose, UserStatus,
};
use crucible_records::{OrgId, UserId};

use super::{Ctl, Transcript, at, id, project, user};

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();

    // The organization is a singleton with an optimistic version.
    t.outcome("org before setup", s.org().get().await);
    let mut org = Org {
        id: OrgId::from_uuid(id(1)),
        name: "Acme".to_owned(),
        settings: OrgSettings::default(),
        version: 1,
    };
    t.outcome("org insert", s.org().insert(&org).await);
    let second = Org {
        id: OrgId::from_uuid(id(2)),
        ..org.clone()
    };
    t.outcome("second org", s.org().insert(&second).await);
    org.name = "Acme Corp".to_owned();
    org.settings.project_access = ProjectAccess::Explicit;
    org.settings.allowed_email_domains = vec!["example.com".to_owned()];
    t.outcome("org update at 1", s.org().update_if_version(&org, 1).await);
    t.outcome(
        "org update at 1 again",
        s.org().update_if_version(&org, 1).await,
    );
    let stored = s.org().get().await?;
    ensure!(
        stored.as_ref().map(|o| o.version) == Some(2),
        "an update raises the version by one"
    );
    t.note("org", stored);

    // Users: unique email, plain update, delete.
    let (ada, bob) = (user(10)?, user(11)?);
    t.outcome("user insert", s.users().insert(&ada).await);
    t.outcome("user insert 2", s.users().insert(&bob).await);
    t.outcome("user same id", s.users().insert(&ada).await);
    let mut clash = user(12)?;
    clash.email = ada.email.clone();
    t.outcome("user same email", s.users().insert(&clash).await);
    t.outcome("user get", s.users().get(ada.id).await);
    t.outcome(
        "user by email",
        s.users().get_by_email(bob.email.as_str()).await,
    );
    t.outcome(
        "user by unknown email",
        s.users().get_by_email("nobody@example.com").await,
    );
    let mut disabled = bob.clone();
    disabled.status = UserStatus::Disabled;
    disabled.email_verified_at = Some(at(5));
    t.outcome("user update", s.users().update(&disabled).await);
    t.outcome("user update missing", s.users().update(&user(13)?).await);
    t.outcome("user list", s.users().list().await);

    // Credentials: one per user, replaced by put; the user must exist.
    let mut credential = Credential {
        user_id: ada.id,
        password_hash: "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA".to_owned(),
        params_version: 1,
        failed_count: 0,
        locked_until: None,
    };
    t.outcome("credential put", s.credentials().put(&credential).await);
    credential.failed_count = 3;
    credential.locked_until = Some(at(60));
    t.outcome(
        "credential put again",
        s.credentials().put(&credential).await,
    );
    let stored = s.credentials().get(ada.id).await?;
    ensure!(
        stored.as_ref() == Some(&credential),
        "a credential reads back as written"
    );
    let orphan = Credential {
        user_id: UserId::from_uuid(id(99)),
        ..credential.clone()
    };
    t.outcome(
        "credential without user",
        s.credentials().put(&orphan).await,
    );
    t.outcome("credential delete", s.credentials().delete(ada.id).await);
    t.outcome(
        "credential delete again",
        s.credentials().delete(ada.id).await,
    );
    t.outcome("credential restore", s.credentials().put(&credential).await);

    // Sessions.
    let session = |n: u8, user: UserId| Session {
        token_hash: [n; 32],
        user_id: user,
        csrf_token: format!("csrf-{n}"),
        idle_expires_at: at(600),
        absolute_expires_at: at(3_600 * i64::from(n)),
        auth_method: AuthMethod::Password,
        revoked_at: None,
    };
    for n in 1..=3 {
        t.outcome(
            "session insert",
            s.sessions().insert(&session(n, ada.id)).await,
        );
    }
    t.outcome(
        "session same token",
        s.sessions().insert(&session(1, bob.id)).await,
    );
    t.outcome(
        "session without user",
        s.sessions()
            .insert(&session(9, UserId::from_uuid(id(99))))
            .await,
    );
    t.outcome(
        "session touch",
        s.sessions().touch(&[1; 32], at(1_200)).await,
    );
    t.outcome(
        "session touch missing",
        s.sessions().touch(&[8; 32], at(1_200)).await,
    );
    t.outcome("session revoke", s.sessions().revoke(&[1; 32], at(7)).await);
    t.outcome(
        "session revoke again",
        s.sessions().revoke(&[1; 32], at(8)).await,
    );
    t.outcome(
        "session revoke others",
        s.sessions()
            .revoke_user(ada.id, Some(&[3; 32]), at(9))
            .await,
    );
    let read = s.sessions().get(&[1; 32]).await?;
    ensure!(
        read.as_ref().and_then(|s| s.revoked_at) == Some(at(7)),
        "a revocation keeps its first time"
    );
    for n in 1..=3 {
        let read = s.sessions().get(&[n; 32]).await?;
        t.note(
            "session",
            read.map(|s| (s.csrf_token, s.idle_expires_at, s.revoked_at)),
        );
    }
    t.outcome(
        "sessions expired before hour 2",
        s.sessions().delete_expired(at(3_600 * 2)).await,
    );
    t.outcome(
        "session 1 after expiry sweep",
        s.sessions().get(&[1; 32]).await.map(|s| s.is_some()),
    );

    // Projects: unique slug, optimistic version, soft delete.
    let (mut web, api) = (project(20), project(21));
    t.outcome("project insert", s.projects().insert(&web).await);
    t.outcome("project insert 2", s.projects().insert(&api).await);
    let mut clash = project(22);
    clash.slug = web.slug.clone();
    t.outcome("project same slug", s.projects().insert(&clash).await);
    web.name = "Web".to_owned();
    web.retention_days = None;
    web.payload_retention_days = Some(7);
    web.config_version = 2;
    web.settings = crucible_control::JsonDoc::parse(r#"{"mask": ["email"], "sampling": 1}"#)?;
    t.outcome(
        "project update at 1",
        s.projects().update_if_version(&web, 1).await,
    );
    t.outcome(
        "project update at 1 again",
        s.projects().update_if_version(&web, 1).await,
    );
    t.outcome("project get", s.projects().get(web.id).await);
    t.outcome(
        "project by slug",
        s.projects()
            .get_by_slug("project-21")
            .await
            .map(|p| p.map(|p| p.id)),
    );
    let mut gone = api.clone();
    gone.deleted_at = Some(at(30));
    t.outcome(
        "project soft delete",
        s.projects().update_if_version(&gone, 1).await,
    );
    let listed = s.projects().list().await?;
    ensure!(
        listed.iter().all(|p| p.deleted_at.is_none()),
        "list leaves deleted projects out"
    );
    t.note(
        "project list",
        listed.iter().map(|p| p.id).collect::<Vec<_>>(),
    );
    t.outcome(
        "deleted project still reads",
        s.projects().get(api.id).await.map(|p| p.map(|p| p.version)),
    );

    // Memberships and their foreign keys.
    let m = s.memberships();
    t.outcome(
        "org member",
        m.put_org(OrgMembership {
            user_id: ada.id,
            role: OrgRole::Owner,
        })
        .await,
    );
    t.outcome(
        "org member 2",
        m.put_org(OrgMembership {
            user_id: bob.id,
            role: OrgRole::Viewer,
        })
        .await,
    );
    t.outcome(
        "org member role change",
        m.put_org(OrgMembership {
            user_id: bob.id,
            role: OrgRole::Member,
        })
        .await,
    );
    t.outcome(
        "org member without user",
        m.put_org(OrgMembership {
            user_id: UserId::from_uuid(id(99)),
            role: OrgRole::Viewer,
        })
        .await,
    );
    t.outcome("org role", m.org_role(bob.id).await);
    t.outcome("org list", m.list_org().await);
    for (project_id, user_id, role) in [
        (web.id, ada.id, ProjectRole::Admin),
        (web.id, bob.id, ProjectRole::Viewer),
        (api.id, bob.id, ProjectRole::Member),
        (web.id, bob.id, ProjectRole::Member),
    ] {
        let membership = ProjectMembership {
            project_id,
            user_id,
            role,
        };
        t.outcome("project member", m.put_project(membership).await);
    }
    t.outcome(
        "project member without project",
        m.put_project(ProjectMembership {
            project_id: project(98).id,
            user_id: ada.id,
            role: ProjectRole::Viewer,
        })
        .await,
    );
    t.outcome(
        "project member without user",
        m.put_project(ProjectMembership {
            project_id: web.id,
            user_id: UserId::from_uuid(id(99)),
            role: ProjectRole::Viewer,
        })
        .await,
    );
    t.outcome("members of web", m.list_project(web.id).await);
    t.outcome("projects of bob", m.list_for_user(bob.id).await);
    t.outcome(
        "project member delete",
        m.delete_project(api.id, bob.id).await,
    );
    t.outcome(
        "project member delete again",
        m.delete_project(api.id, bob.id).await,
    );
    t.outcome("org member delete", m.delete_org(bob.id).await);
    t.outcome("org member delete again", m.delete_org(bob.id).await);

    // Deleting a user takes the credential, the sessions and the memberships with it.
    t.outcome("user delete", s.users().delete(ada.id).await);
    t.outcome("user delete again", s.users().delete(ada.id).await);
    let left = (
        s.credentials().get(ada.id).await?.is_some(),
        s.sessions().get(&[3; 32]).await?.is_some(),
        m.org_role(ada.id).await?,
        m.list_project(web.id).await?,
    );
    ensure!(
        !left.0 && !left.1 && left.2.is_none(),
        "a deleted user leaves no credential, session or org membership"
    );
    t.note("after user delete", left);

    // Signing keys.
    let key = |kid: &str, purpose, secs| SigningKey {
        kid: kid.to_owned(),
        purpose,
        sealed_key: kid.as_bytes().to_vec(),
        state: SigningKeyState::Active,
        created_at: at(secs),
    };
    for signing in [
        key("cur-2", SigningPurpose::Cursor, 20),
        key("cur-1", SigningPurpose::Cursor, 10),
        key("dl-1", SigningPurpose::Download, 10),
    ] {
        t.outcome(
            "signing key insert",
            s.signing_keys().insert(&signing).await,
        );
    }
    t.outcome(
        "signing key same kid",
        s.signing_keys()
            .insert(&key("cur-1", SigningPurpose::MediaToken, 30))
            .await,
    );
    t.outcome(
        "signing key state",
        s.signing_keys()
            .set_state("cur-1", SigningKeyState::VerifyOnly)
            .await,
    );
    t.outcome(
        "signing key state missing",
        s.signing_keys()
            .set_state("cur-9", SigningKeyState::Retired)
            .await,
    );
    t.outcome(
        "cursor keys",
        s.signing_keys()
            .list(SigningPurpose::Cursor)
            .await
            .map(|keys| {
                keys.into_iter()
                    .map(|k| (k.kid, k.state, k.sealed_key, k.created_at))
                    .collect::<Vec<_>>()
            }),
    );
    t.outcome(
        "signing key get",
        s.signing_keys()
            .get("dl-1")
            .await
            .map(|k| k.map(|k| k.purpose)),
    );

    // Secrets.
    let secret = Secret {
        id: id(40),
        scope: format!("project:{}", web.id),
        kind: "channel_webhook".to_owned(),
        key_id: "k1".to_owned(),
        nonce: vec![7; 24],
        ciphertext: vec![0, 255, 1, 254],
    };
    t.outcome("secret insert", s.secrets().insert(&secret).await);
    t.outcome("secret same id", s.secrets().insert(&secret).await);
    let read = s.secrets().get(secret.id).await?;
    ensure!(
        read.as_ref() == Some(&secret),
        "a secret reads back as written"
    );
    t.outcome("secret delete", s.secrets().delete(secret.id).await);
    t.outcome("secret delete again", s.secrets().delete(secret.id).await);
    t.outcome(
        "secret after delete",
        s.secrets().get(secret.id).await.map(|s| s.is_some()),
    );

    // The migrations ledger is forward-only: a recorded step is never rewritten.
    let step = |step: &str, kind, checksum: &str| LedgerEntry {
        id: step.to_owned(),
        kind,
        applied_at: at(100),
        checksum: checksum.to_owned(),
    };
    let ledger = s.migrations();
    let before = ledger.applied().await?.len();
    t.outcome(
        "ledger apply schema",
        ledger
            .apply(&step(
                "0001_schema_base",
                MigrationKind::ClickhouseSchema,
                "aa",
            ))
            .await,
    );
    t.outcome(
        "ledger apply access",
        ledger
            .apply(&step(
                "0002_access_service",
                MigrationKind::ClickhouseAccess,
                "bb",
            ))
            .await,
    );
    t.outcome(
        "ledger apply schema again",
        ledger
            .apply(&step(
                "0001_schema_base",
                MigrationKind::ClickhouseSchema,
                "cc",
            ))
            .await,
    );
    let applied = ledger.applied().await?;
    ensure!(applied.len() == before + 2, "the ledger gained two steps");
    t.note(
        "ledger",
        applied
            .into_iter()
            .filter(|e| e.kind != MigrationKind::Postgres)
            .collect::<Vec<_>>(),
    );
    Ok(())
}
