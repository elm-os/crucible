//! The object store adapter against a real S3-compatible server (contracts 2.10 and 14.2): `crucible-media`'s
//! `S3Store` presigns, a client uploads and downloads with the presigned URLs, and the store confirms by HEAD,
//! deletes, lists and joins multipart uploads, which export files use. Each row says what the server enforced,
//! so a server that does not enforce a signed header shows up here before media relies on it.

use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use bytes::Bytes;
use crucible_env::{Disposition, Env, MIN_PART_BYTES, ObjectStore, TlsRoots};
use crucible_media::{S3Config, S3Store, fetch};
use futures::StreamExt as _;
use http::{Method, StatusCode};
use sha2::{Digest, Sha256};

use crate::Outcome;

/// Where every key of a run lives, so that runs do not meet.
fn root(env: &Env) -> String {
    format!("conformance/{}", env.new_id())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// What a client does with a presigned URL, and the status it gets.
async fn call(
    env: &Env,
    method: Method,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(StatusCode, http::HeaderMap, Bytes)> {
    fetch(
        env,
        &TlsRoots::WebPki,
        method,
        url,
        headers,
        Bytes::copy_from_slice(body),
    )
    .await
    .context("the request did not get an answer")
}

/// The headers of a presigned PUT with one of them replaced.
fn with(headers: &[(String, String)], name: &str, value: &str) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(n, v)| {
            if n == name {
                (n.clone(), value.to_owned())
            } else {
                (n.clone(), v.clone())
            }
        })
        .collect()
}

const BODY: &[u8] = b"\x89PNG\r\n\x1a\nnot really an image, but its bytes are what is checked";

async fn upload(env: &Env, store: &S3Store, root: &str) -> Result<String> {
    let key = format!("{root}/upload");
    let signed = store.presign_put(
        &key,
        "image/png",
        &sha256(BODY),
        BODY.len() as u64,
        Duration::from_secs(3_600),
    )?;
    let (status, _, answer) = call(env, Method::PUT, &signed.url, &signed.headers, BODY).await?;
    ensure!(
        status.is_success(),
        "the presigned PUT answered {status}: {}",
        String::from_utf8_lossy(&answer)
    );
    let head = store.head(&key).await?.context("HEAD found no object")?;
    ensure!(
        head.len == BODY.len() as u64,
        "HEAD reports {} bytes",
        head.len
    );
    Ok(format!(
        "PUT {status}; HEAD {} bytes, checksum {}",
        head.len,
        match head.sha256 {
            Some(sha) if sha == sha256(BODY) => "returned and equal",
            Some(_) => "returned and DIFFERENT",
            None => "not returned",
        }
    ))
}

async fn refused(env: &Env, store: &S3Store, root: &str) -> Result<String> {
    let key = format!("{root}/refused");
    let signed = store.presign_put(
        &key,
        "image/png",
        &sha256(BODY),
        BODY.len() as u64,
        Duration::from_secs(3_600),
    )?;
    let mut seen = Vec::new();
    // Another content type than the signed one.
    let other_type = with(&signed.headers, "content-type", "text/html");
    let (status, _, _) = call(env, Method::PUT, &signed.url, &other_type, BODY).await?;
    ensure!(
        !status.is_success(),
        "a PUT with another content type answered {status}"
    );
    seen.push(format!("other type {status}"));
    // Other bytes of the signed length.
    let mut other = BODY.to_vec();
    other[0] ^= 0xff;
    let (status, _, _) = call(env, Method::PUT, &signed.url, &signed.headers, &other).await?;
    let stored = store.head(&key).await?;
    match (status.is_success(), stored) {
        (false, None) => seen.push(format!("other bytes {status}")),
        (true, Some(meta)) if meta.sha256.is_some_and(|sha| sha != sha256(BODY)) => {
            seen.push(format!(
                "other bytes {status} but HEAD shows the other checksum"
            ));
        }
        (accepted, stored) => bail!(
            "a PUT of other bytes answered {status} (accepted {accepted}) and HEAD shows {stored:?}: the store \
             neither checks the signed checksum nor reports the stored one"
        ),
    }
    store.delete(&key).await?;
    // A body of another length, sent with its own length, as any client sends it.
    let longer = [BODY, b"!"].concat();
    let length = with(&signed.headers, "content-length", &longer.len().to_string());
    let (status, _, _) = call(env, Method::PUT, &signed.url, &length, &longer).await?;
    ensure!(
        !status.is_success(),
        "a PUT of another length answered {status}"
    );
    seen.push(format!("other length {status}"));
    ensure!(
        store.head(&key).await?.is_none(),
        "a refused PUT stored an object"
    );
    Ok(seen.join(", "))
}

async fn download(env: &Env, store: &S3Store, root: &str) -> Result<String> {
    let key = format!("{root}/download");
    store
        .put_small(&key, "image/png", Bytes::from_static(BODY))
        .await?;
    let mut seen = Vec::new();
    for (disposition, expected) in [
        (Disposition::Inline, "inline"),
        (Disposition::Attachment, "attachment"),
    ] {
        let signed = store.presign_get(&key, disposition, Duration::from_secs(300))?;
        let (status, headers, body) = call(env, Method::GET, &signed.url, &[], b"").await?;
        ensure!(status == StatusCode::OK, "GET answered {status}");
        ensure!(body.as_ref() == BODY, "GET answered other bytes");
        let served = headers
            .get(http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        ensure!(
            served.starts_with(expected),
            "GET served Content-Disposition {served:?}, not {expected}"
        );
        let content_type = headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        ensure!(
            content_type == "image/png",
            "GET served Content-Type {content_type:?}, not the stored image/png"
        );
        seen.push(served);
    }
    let mut streamed = Vec::new();
    let mut stream = store.get_stream(&key).await?;
    while let Some(chunk) = stream.next().await {
        streamed.extend_from_slice(&chunk?);
    }
    ensure!(streamed == BODY, "the stream answered other bytes");
    Ok(format!(
        "{}; stream {} bytes",
        seen.join(", "),
        streamed.len()
    ))
}

async fn expiry(env: &Env, store: &S3Store, root: &str) -> Result<String> {
    let key = format!("{root}/expiry");
    store
        .put_small(&key, "image/png", Bytes::from_static(BODY))
        .await?;
    let signed = store.presign_get(&key, Disposition::Inline, Duration::from_secs(1))?;
    env.clock.sleep(Duration::from_secs(3)).await;
    let (status, _, _) = call(env, Method::GET, &signed.url, &[], b"").await?;
    ensure!(!status.is_success(), "an expired GET answered {status}");
    Ok(format!("expired GET {status}"))
}

async fn multipart(env: &Env, store: &S3Store, root: &str) -> Result<String> {
    let key = format!("{root}/multipart");
    let first = vec![0x5a_u8; MIN_PART_BYTES];
    let last = b"the short last part".to_vec();
    let upload = store.start_upload(&key, "application/gzip").await?;
    let etags = vec![
        store
            .upload_part(&key, &upload, 1, Bytes::from(first.clone()))
            .await?,
        store
            .upload_part(&key, &upload, 2, Bytes::from(last.clone()))
            .await?,
    ];
    ensure!(
        store.head(&key).await?.is_none(),
        "an unfinished upload is already an object"
    );
    store.finish_upload(&key, &upload, &etags).await?;
    let whole = [first.as_slice(), last.as_slice()].concat();
    let head = store
        .head(&key)
        .await?
        .context("HEAD found no finished upload")?;
    ensure!(
        head.len == whole.len() as u64,
        "HEAD reports {} bytes",
        head.len
    );
    let signed = store.presign_get(&key, Disposition::Attachment, Duration::from_secs(300))?;
    let (status, headers, body) = call(env, Method::GET, &signed.url, &[], b"").await?;
    ensure!(status == StatusCode::OK, "GET answered {status}");
    ensure!(body.as_ref() == whole.as_slice(), "GET answered other bytes");
    let served = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    ensure!(
        served == "application/gzip",
        "GET served Content-Type {served:?}, not the upload's application/gzip"
    );
    store.delete(&key).await?;

    let abandoned = format!("{root}/abandoned");
    let upload = store.start_upload(&abandoned, "application/gzip").await?;
    store
        .upload_part(&abandoned, &upload, 1, Bytes::from_static(b"never finished"))
        .await?;
    store.abort_upload(&abandoned, &upload).await?;
    store.abort_upload(&abandoned, &upload).await?;
    ensure!(
        store.head(&abandoned).await?.is_none(),
        "an abandoned upload stored an object"
    );
    Ok(format!(
        "2 parts joined into {} bytes and served as {served}; an abandoned upload stores nothing, and \
         abandoning it again succeeds",
        whole.len()
    ))
}

async fn delete_and_list(store: &S3Store, root: &str) -> Result<String> {
    let dir = format!("{root}/list");
    let keys: Vec<String> = (0..5).map(|n| format!("{dir}/{n}")).collect();
    for key in &keys {
        store
            .put_small(key, "text/plain", Bytes::from_static(b"x"))
            .await?;
    }
    let all = store.list(&format!("{dir}/"), None).await?;
    let listed: Vec<&str> = all.objects.iter().map(|o| o.key.as_str()).collect();
    ensure!(listed == keys, "the listing is {listed:?}");
    ensure!(all.next.is_none(), "a short listing has a next page");
    let after = store.list(&format!("{dir}/"), Some(&keys[2])).await?;
    let listed: Vec<&str> = after.objects.iter().map(|o| o.key.as_str()).collect();
    ensure!(
        listed == keys[3..],
        "the listing after {} is {listed:?}",
        keys[2]
    );
    store.delete(&keys[0]).await?;
    store.delete(&keys[0]).await?;
    store
        .delete_batch(&[keys[1].clone(), keys[2].clone(), format!("{dir}/missing")])
        .await?;
    for key in &keys[..3] {
        ensure!(store.head(key).await?.is_none(), "{key} is still there");
    }
    let rest = store.list(&format!("{dir}/"), None).await?;
    let listed: Vec<&str> = rest.objects.iter().map(|o| o.key.as_str()).collect();
    ensure!(
        listed == keys[3..],
        "after the deletes the listing is {listed:?}"
    );
    store.delete_batch(&keys[3..]).await?;
    Ok("5 listed in key order, 2 after the third; a missing key deletes; a batch with a missing key deletes"
        .to_owned())
}

/// Runs every row against the bucket of `config`.
pub async fn run(env: &Env, config: &S3Config) -> Vec<Outcome> {
    let store = match S3Store::new(env, config, TlsRoots::WebPki) {
        Ok(store) => store,
        Err(error) => {
            return vec![Outcome {
                row: "S3 the configuration makes a store",
                result: Err(error.to_string()),
            }];
        }
    };
    let root = root(env);
    let mut outcomes = vec![Outcome {
        row: "S3 1 the bucket answers with these credentials",
        result: store
            .check()
            .await
            .map(|()| "HEAD bucket succeeded".to_owned())
            .map_err(|e| e.to_string()),
    }];
    let rows: [(&'static str, Result<String>); 6] = [
        (
            "S3 2 a presigned PUT stores the bytes and HEAD confirms them",
            upload(env, &store, &root).await,
        ),
        (
            "S3 3 a PUT with another type, other bytes or another length is refused",
            refused(env, &store, &root).await,
        ),
        (
            "S3 4 a presigned GET serves the bytes with its disposition",
            download(env, &store, &root).await,
        ),
        (
            "S3 5 an expired presigned GET is refused",
            expiry(env, &store, &root).await,
        ),
        (
            "S3 6 delete, batch delete and list",
            delete_and_list(&store, &root).await,
        ),
        (
            "S3 7 a multipart upload is served whole with its content type; an abandoned one stores nothing",
            multipart(env, &store, &root).await,
        ),
    ];
    for (row, result) in rows {
        outcomes.push(Outcome {
            row,
            result: result.map_err(|error| format!("{error:#}")),
        });
    }
    let _ = store.delete(&format!("{root}/upload")).await;
    let _ = store.delete(&format!("{root}/download")).await;
    let _ = store.delete(&format!("{root}/expiry")).await;
    outcomes
}
