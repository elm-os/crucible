//! The signing inputs of the S3 adapter (contracts 2.10 and 14.2) as properties over arbitrary keys, content
//! types, checksums, lengths and lifetimes, on the simulation's clock, which does not move while a case runs: a
//! presigned upload names the public endpoint and signs exactly its content type, length and checksum, at the
//! `Env` clock's time and for the asked lifetime; a presigned download signs its disposition; each signed input
//! changes the signature; and no URL carries the secret key.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crucible_env::{Disposition, Env, ObjectStore, Presigned, TlsRoots};
use crucible_media::rules::CONTENT_TYPES;
use crucible_media::{S3Config, S3Store};
use proptest::prelude::*;

const SECRET: &str = "the-secret-access-key-stays-on-the-node";

fn config() -> S3Config {
    S3Config {
        endpoint: "http://objects.internal:7070".to_owned(),
        public_endpoint: Some("http://media.example.com:9000".to_owned()),
        region: "us-east-1".to_owned(),
        bucket: "media".to_owned(),
        access_key: "access-key".to_owned(),
        secret_key: SECRET.to_owned(),
        virtual_host: false,
    }
}

/// Runs `body` with a store for `config` on a simulated host.
fn with_store<T>(config: &S3Config, body: impl FnOnce(&Env, &S3Store) -> T) -> T {
    let config = config.clone();
    crucible_sim::run(5, |sim| async move {
        let host = sim.host("s3", &sim.disk("s3"), 0);
        let store = S3Store::new(&host.env, &config, TlsRoots::WebPki).unwrap();
        body(&host.env, &store)
    })
}

/// The decoded query of a URL.
fn query(url: &str) -> BTreeMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// The `X-Amz-Date` of a signature made now: the clock's time to the second, in the basic ISO 8601 form.
fn amz_date(env: &Env) -> String {
    let text = env.clock.wall_now().to_string();
    format!("{}Z", text[..19].replace(['-', ':'], ""))
}

/// Keys shaped like the ones `object_key` makes: segments of lowercase letters, digits and dashes.
fn key() -> impl Strategy<Value = String> {
    "[a-z0-9-]{1,16}(/[a-z0-9-]{1,36}){0,4}"
}

fn upload(store: &S3Store, key: &str, content_type: &str, sha: &[u8; 32], len: u64) -> Presigned {
    store
        .presign_put(key, content_type, sha, len, Duration::from_secs(3_600))
        .unwrap()
}

fn signature(signed: &Presigned) -> String {
    query(&signed.url)["X-Amz-Signature"].clone()
}

proptest! {
    #[test]
    fn a_presigned_upload_signs_its_type_length_and_checksum_at_the_clock_time(
        key in key(),
        type_at in 0..CONTENT_TYPES.len(),
        sha: [u8; 32],
        len in 1u64..=(64 << 20),
        ttl_secs in 1u64..=(7 * 24 * 3_600),
    ) {
        let content_type = CONTENT_TYPES[type_at];
        with_store(&config(), |env, store| {
            let ttl = Duration::from_secs(ttl_secs);
            let signed = store.presign_put(&key, content_type, &sha, len, ttl).unwrap();
            // The public endpoint, the bucket, then the key.
            let url = url::Url::parse(&signed.url).unwrap();
            assert_eq!((url.host_str(), url.port()), (Some("media.example.com"), Some(9000)));
            assert_eq!(url.path(), format!("/media/{key}"));
            let q = query(&signed.url);
            assert_eq!(q["X-Amz-SignedHeaders"], "content-length;content-type;host;x-amz-checksum-sha256");
            assert_eq!(q["X-Amz-Expires"], ttl_secs.to_string());
            assert_eq!(q["X-Amz-Date"], amz_date(env));
            assert!(q["X-Amz-Credential"].starts_with("access-key/"), "{q:?}");
            // The client is told to send exactly the signed values.
            let headers: BTreeMap<&str, &str> =
                signed.headers.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
            let length = len.to_string();
            let checksum = STANDARD.encode(sha);
            let expected = BTreeMap::from([
                ("content-length", length.as_str()),
                ("content-type", content_type),
                ("x-amz-checksum-sha256", checksum.as_str()),
            ]);
            assert_eq!(headers, expected);
            let micros = i64::try_from(ttl.as_micros()).unwrap();
            assert_eq!(signed.expires_at, env.clock.wall_now().saturating_add_micros(micros));
            assert!(!signed.url.contains(SECRET));
        });
    }

    #[test]
    fn each_signed_input_changes_the_signature_and_equal_inputs_sign_alike(
        key in key(),
        other_key in key(),
        sha: [u8; 32],
        other_sha: [u8; 32],
        len in 1u64..=(64 << 20),
    ) {
        with_store(&config(), |_, store| {
            let base = signature(&upload(store, &key, "image/png", &sha, len));
            assert_eq!(base, signature(&upload(store, &key, "image/png", &sha, len)));
            assert_ne!(base, signature(&upload(store, &key, "image/jpeg", &sha, len)));
            assert_ne!(base, signature(&upload(store, &key, "image/png", &sha, len + 1)));
            if other_sha != sha {
                assert_ne!(base, signature(&upload(store, &key, "image/png", &other_sha, len)));
            }
            if other_key != key {
                assert_ne!(base, signature(&upload(store, &other_key, "image/png", &sha, len)));
            }
        });
    }

    #[test]
    fn a_presigned_download_signs_its_disposition(key in key(), inline: bool, ttl_secs in 1u64..=3_600) {
        with_store(&config(), |env, store| {
            let disposition = if inline { Disposition::Inline } else { Disposition::Attachment };
            let signed = store
                .presign_get(&key, disposition, Duration::from_secs(ttl_secs))
                .unwrap();
            let url = url::Url::parse(&signed.url).unwrap();
            assert_eq!(url.host_str(), Some("media.example.com"));
            assert_eq!(url.path(), format!("/media/{key}"));
            let q = query(&signed.url);
            assert_eq!(q["response-content-disposition"], disposition.header());
            assert_eq!(q["X-Amz-SignedHeaders"], "host");
            assert_eq!(q["X-Amz-Expires"], ttl_secs.to_string());
            assert_eq!(q["X-Amz-Date"], amz_date(env));
            assert!(signed.headers.is_empty());
            assert!(!signed.url.contains(SECRET));
            // The disposition is signed: the other one makes another signature.
            let other = if inline { Disposition::Attachment } else { Disposition::Inline };
            let flipped = store.presign_get(&key, other, Duration::from_secs(ttl_secs)).unwrap();
            assert_ne!(signature(&signed), signature(&flipped));
        });
    }
}

#[test]
fn browsers_load_media_from_the_public_origin_or_the_internal_one_without_it() {
    with_store(&config(), |_, store| {
        assert_eq!(store.public_origin(), "http://media.example.com:9000");
    });
    let internal_only = S3Config {
        public_endpoint: None,
        ..config()
    };
    with_store(&internal_only, |_, store| {
        assert_eq!(store.public_origin(), "http://objects.internal:7070");
        let signed = upload(store, "k", "image/png", &[0; 32], 1);
        assert!(
            signed
                .url
                .starts_with("http://objects.internal:7070/media/k?"),
            "{}",
            signed.url
        );
    });
}

#[test]
fn endpoints_that_make_no_bucket_url_are_refused_with_the_setting_to_fix() {
    crucible_sim::run(5, |sim| async move {
        let host = sim.host("s3", &sim.disk("s3"), 0);
        for (endpoint, public, setting) in [
            ("objects.internal:7070", None, "object_store.endpoint"),
            ("ftp://objects.internal", None, "object_store.endpoint"),
            (
                "http://objects.internal:7070",
                Some("not a url"),
                "object_store.public_endpoint",
            ),
        ] {
            let bad = S3Config {
                endpoint: endpoint.to_owned(),
                public_endpoint: public.map(str::to_owned),
                ..config()
            };
            let error = S3Store::new(&host.env, &bad, TlsRoots::WebPki).unwrap_err();
            assert!(error.to_string().contains(setting), "{endpoint}: {error}");
        }
    });
}
