mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::*;
use dav_server::{memls::MemLs, DavHandler};
use davlog::change::{Change, Op};
use davlog::davfs::GitFs;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;

struct Env {
    dir: tempfile::TempDir,
    handler: DavHandler,
    changes: Arc<RecordingChanges>,
}

fn env(max: Option<u64>) -> Env {
    let (dir, _repo) = temp_repo("main");
    let changes = RecordingChanges::new();
    let fs = GitFs::new(dir.path().to_path_buf(), max, changes.clone());
    let handler = DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(MemLs::new())
        .build_handler();
    Env {
        dir,
        handler,
        changes,
    }
}

type Reply = (StatusCode, HeaderMap, Vec<u8>);

async fn send(e: &Env, method: &str, uri: &str, body: &[u8], headers: &[(&str, &str)]) -> Reply {
    let mut b = Request::builder().method(method).uri(uri);
    if !body.is_empty() || method == "PUT" {
        b = b.header("content-length", body.len().to_string());
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = b.body(Full::new(Bytes::copy_from_slice(body))).unwrap();
    let res = e.handler.handle(req).await;
    let (parts, body) = res.into_parts();
    let bytes = body
        .collect()
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();
    (parts.status, parts.headers, bytes)
}

async fn put(e: &Env, uri: &str, body: &[u8]) -> StatusCode {
    send(e, "PUT", uri, body, &[]).await.0
}

/// PUT with a streamed body and no Content-Length (chunked).
async fn put_stream(
    e: &Env,
    uri: &str,
    items: Vec<Result<&'static [u8], std::io::Error>>,
) -> StatusCode {
    let frames = items
        .into_iter()
        .map(|r| r.map(|b| Frame::data(Bytes::from_static(b))));
    let body = StreamBody::new(futures_util::stream::iter(frames));
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .body(body)
        .unwrap();
    e.handler.handle(req).await.status()
}

fn tmp_entries(e: &Env) -> usize {
    std::fs::read_dir(e.dir.path().join(".git/davlog-tmp"))
        .map(|d| d.count())
        .unwrap_or(0)
}

#[tokio::test]
async fn dav_server_calls_flush_after_put_and_deletes_children_one_by_one() {
    let e = env(None);
    assert_eq!(put(&e, "/a.md", b"hello").await, StatusCode::CREATED);
    assert_eq!(e.changes.all(), vec![Change::new(Op::Put, "a.md")]);

    send(&e, "MKCOL", "/d/", b"", &[]).await;
    put(&e, "/d/x.md", b"x").await;
    put(&e, "/d/y.md", b"y").await;
    e.changes.changes.lock().unwrap().clear();

    let (status, _, _) = send(&e, "DELETE", "/d/", b"", &[]).await;
    assert!(status.is_success(), "{status}");
    let got = e.changes.all();
    assert!(got.contains(&Change::new(Op::Delete, "d/x.md")), "{got:?}");
    assert!(got.contains(&Change::new(Op::Delete, "d/y.md")), "{got:?}");
    assert_eq!(got.last().unwrap(), &Change::new(Op::Delete, "d"));
    assert!(!e.dir.path().join("d").exists());
}

#[tokio::test]
async fn put_then_get_roundtrip() {
    let e = env(None);
    put(&e, "/notes/a.md", b"hello").await;
    // parent missing: 409
    assert_eq!(
        put(&e, "/missing/dir/a.md", b"x").await,
        StatusCode::CONFLICT
    );
    send(&e, "MKCOL", "/notes/", b"", &[]).await;
    assert_eq!(put(&e, "/notes/a.md", b"hello").await, StatusCode::CREATED);
    let (status, _, body) = send(&e, "GET", "/notes/a.md", b"", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"hello");
    assert_eq!(
        std::fs::read(e.dir.path().join("notes/a.md")).unwrap(),
        b"hello"
    );
}

#[tokio::test]
async fn put_is_atomic_and_leaves_no_temp_files() {
    let e = env(None);
    assert_eq!(put(&e, "/a.md", b"complete").await, StatusCode::CREATED);
    assert_eq!(tmp_entries(&e), 0);
    assert_eq!(
        std::fs::read(e.dir.path().join("a.md")).unwrap(),
        b"complete"
    );
    // overwrite replaces in one step
    assert_eq!(put(&e, "/a.md", b"second").await, StatusCode::NO_CONTENT);
    assert_eq!(std::fs::read(e.dir.path().join("a.md")).unwrap(), b"second");
    assert_eq!(tmp_entries(&e), 0);
}

#[tokio::test]
async fn dot_git_is_hidden_and_protected_on_every_method() {
    let e = env(None);
    for uri in ["/.git/config", "/.git", "/.git/", "/sub/.git/x"] {
        for method in ["GET", "HEAD", "PROPFIND"] {
            let (status, _, body) = send(&e, method, uri, b"", &[("depth", "0")]).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
            assert!(!String::from_utf8_lossy(&body).contains("[core]"));
        }
        const PROPPATCH: &[u8] = br#"<?xml version="1.0"?><D:propertyupdate xmlns:D="DAV:"><D:set><D:prop><D:displayname>x</D:displayname></D:prop></D:set></D:propertyupdate>"#;
        const LOCK: &[u8] = br#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockinfo>"#;
        for (method, body) in [
            ("PUT", &b"x"[..]),
            ("DELETE", b""),
            ("MKCOL", b""),
            ("PROPPATCH", PROPPATCH),
            ("LOCK", LOCK),
        ] {
            let (status, _, _) = send(&e, method, uri, body, &[]).await;
            assert!(
                status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
                "{method} {uri} gave {status}"
            );
        }
    }
    assert!(
        e.dir.path().join(".git/config").exists(),
        "config untouched"
    );
    assert!(!e.dir.path().join("sub").exists());
}

#[tokio::test]
async fn move_and_copy_into_dot_git_are_refused() {
    let e = env(None);
    put(&e, "/a.md", b"data").await;
    for method in ["MOVE", "COPY"] {
        let (status, _, _) = send(
            &e,
            method,
            "/a.md",
            b"",
            &[("destination", "http://localhost/.git/stolen.md")],
        )
        .await;
        assert!(status.is_client_error(), "{method} gave {status}");
    }
    assert!(e.dir.path().join("a.md").exists());
    assert!(!e.dir.path().join(".git/stolen.md").exists());
}

#[tokio::test]
async fn directory_listing_hides_dot_git() {
    let e = env(None);
    put(&e, "/a.md", b"a").await;
    let (status, _, body) = send(&e, "PROPFIND", "/", b"", &[("depth", "1")]).await;
    assert_eq!(status.as_u16(), 207);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("a.md"));
    assert!(!text.contains(".git"), "{text}");
}

#[tokio::test]
async fn encoded_dotdot_cannot_escape_root() {
    let e = env(None);
    for uri in ["/%2e%2e/etc/passwd", "/a/..%2f..%2fx", "/..", "/a/../../x"] {
        let (status, _, _) = send(&e, "GET", uri, b"", &[]).await;
        assert!(status.is_client_error(), "{uri} gave {status}");
        let (status, _, _) = send(&e, "PUT", uri, b"x", &[]).await;
        assert!(status.is_client_error(), "PUT {uri} gave {status}");
    }
    assert!(e.changes.all().is_empty());
}

#[tokio::test]
async fn symlinks_are_never_followed() {
    let e = env(None);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "top secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), e.dir.path().join("link")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), e.dir.path().join("f")).unwrap();

    for uri in ["/f", "/link", "/link/secret"] {
        let (status, _, body) = send(&e, "GET", uri, b"", &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!String::from_utf8_lossy(&body).contains("top secret"));
    }
    let (_, _, listing) = send(&e, "PROPFIND", "/", b"", &[("depth", "1")]).await;
    let listing = String::from_utf8(listing).unwrap();
    assert!(!listing.contains("link") && !listing.contains(">f<") && !listing.contains("/f<"));

    let status = put(&e, "/link/new.md", b"x").await;
    assert!(status.is_client_error(), "{status}");
    assert!(!outside.path().join("new.md").exists());
}

#[tokio::test]
async fn invalid_utf8_path_is_rejected_without_panic() {
    let e = env(None);
    let (status, _, _) = send(&e, "GET", "/%ff%fe", b"", &[]).await;
    assert!(status.is_client_error(), "{status}");
    assert!(put(&e, "/%ff%fe", b"x").await.is_client_error());
    assert!(e.changes.all().is_empty());
    assert_eq!(
        std::fs::read_dir(e.dir.path()).unwrap().count(),
        1,
        "only .git"
    );
}

#[tokio::test]
async fn odd_names_roundtrip_and_are_recorded() {
    let e = env(None);
    assert_eq!(
        put(&e, "/a%20b%23c%25d%20%C3%A9.md", b"odd").await,
        StatusCode::CREATED
    );
    assert_eq!(
        std::fs::read(e.dir.path().join("a b#c%d é.md")).unwrap(),
        b"odd"
    );
    assert_eq!(e.changes.all(), vec![Change::new(Op::Put, "a b#c%d é.md")]);
    let (status, _, body) = send(&e, "GET", "/a%20b%23c%25d%20%C3%A9.md", b"", &[]).await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"odd"[..]));
}

#[tokio::test]
async fn oversize_put_with_content_length_is_413() {
    let e = env(Some(10));
    assert_eq!(
        put(&e, "/big.bin", &[0u8; 11]).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(!e.dir.path().join("big.bin").exists());
    assert_eq!(tmp_entries(&e), 0);
    assert_eq!(put(&e, "/ok.bin", &[0u8; 10]).await, StatusCode::CREATED);
    assert!(e.changes.all().iter().all(|c| c.path != "big.bin"));
}

#[tokio::test]
async fn oversize_chunked_put_is_cut_off_and_temp_removed() {
    let e = env(Some(10));
    let status = put_stream(
        &e,
        "/big.bin",
        vec![Ok(&b"123456"[..]), Ok(&b"123456"[..]), Ok(&b"123456"[..])],
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!e.dir.path().join("big.bin").exists());
    assert_eq!(tmp_entries(&e), 0);
    assert!(e.changes.all().is_empty());
}

#[tokio::test]
async fn client_disconnect_mid_upload_leaves_no_temp_and_no_change() {
    let e = env(None);
    put(&e, "/keep.md", b"original").await;
    e.changes.changes.lock().unwrap().clear();
    let status = put_stream(
        &e,
        "/keep.md",
        vec![
            Ok(&b"partial"[..]),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "gone",
            )),
        ],
    )
    .await;
    assert!(!status.is_success(), "{status}");
    assert_eq!(
        std::fs::read(e.dir.path().join("keep.md")).unwrap(),
        b"original"
    );
    assert_eq!(tmp_entries(&e), 0);
    assert!(e.changes.all().is_empty());
}

#[tokio::test]
async fn identical_content_put_is_2xx_and_recorded_each_time() {
    let e = env(None);
    assert_eq!(put(&e, "/a.md", b"same").await, StatusCode::CREATED);
    assert_eq!(put(&e, "/a.md", b"same").await, StatusCode::NO_CONTENT);
    assert_eq!(e.changes.all().len(), 2);
}

#[tokio::test]
async fn zero_byte_put_roundtrips() {
    let e = env(None);
    assert_eq!(put(&e, "/empty.md", b"").await, StatusCode::CREATED);
    assert_eq!(
        std::fs::metadata(e.dir.path().join("empty.md"))
            .unwrap()
            .len(),
        0
    );
    let (status, _, body) = send(&e, "GET", "/empty.md", b"", &[]).await;
    assert_eq!((status, body.len()), (StatusCode::OK, 0));
    assert_eq!(e.changes.all(), vec![Change::new(Op::Put, "empty.md")]);
}

#[tokio::test]
async fn shape_mismatches_return_4xx_without_damage() {
    let e = env(None);
    send(&e, "MKCOL", "/dir/", b"", &[]).await;
    put(&e, "/file.md", b"keep").await;
    put(&e, "/dir/inner.md", b"inner").await;

    assert!(
        put(&e, "/dir", b"x").await.is_client_error(),
        "PUT over directory"
    );
    let (status, _, _) = send(&e, "MKCOL", "/file.md/", b"", &[]).await;
    assert!(status.is_client_error(), "MKCOL over file: {status}");
    let (status, _, _) = send(&e, "MKCOL", "/dir/", b"", &[]).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    assert_eq!(
        std::fs::read(e.dir.path().join("file.md")).unwrap(),
        b"keep"
    );
    assert_eq!(
        std::fs::read(e.dir.path().join("dir/inner.md")).unwrap(),
        b"inner"
    );
}

#[tokio::test]
async fn move_onto_itself_does_not_lose_data() {
    let e = env(None);
    put(&e, "/a.md", b"precious").await;
    let (status, _, _) = send(
        &e,
        "MOVE",
        "/a.md",
        b"",
        &[("destination", "http://localhost/a.md")],
    )
    .await;
    assert!(status.is_client_error() || status.is_success(), "{status}");
    assert_eq!(
        std::fs::read(e.dir.path().join("a.md")).unwrap(),
        b"precious"
    );
}

#[tokio::test]
async fn move_copy_and_mkcol_are_recorded() {
    let e = env(None);
    put(&e, "/a.md", b"a").await;
    send(&e, "MKCOL", "/d/", b"", &[]).await;
    send(
        &e,
        "COPY",
        "/a.md",
        b"",
        &[("destination", "http://localhost/b.md")],
    )
    .await;
    send(
        &e,
        "MOVE",
        "/a.md",
        b"",
        &[("destination", "http://localhost/c.md")],
    )
    .await;
    let got = e.changes.all();
    assert!(got.contains(&Change::new(Op::Mkcol, "d")), "{got:?}");
    assert!(
        got.contains(&Change::with_target(Op::Copy, "a.md", "b.md")),
        "{got:?}"
    );
    assert!(
        got.contains(&Change::with_target(Op::Move, "a.md", "c.md")),
        "{got:?}"
    );
    assert!(e.dir.path().join("b.md").exists() && e.dir.path().join("c.md").exists());
    assert!(!e.dir.path().join("a.md").exists());
}

#[tokio::test]
async fn leftover_temp_files_are_removed_at_startup() {
    let (dir, _repo) = temp_repo("main");
    let tmp = dir.path().join(".git/davlog-tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("stale"), "x").unwrap();
    let _fs = GitFs::new(dir.path().to_path_buf(), None, RecordingChanges::new());
    assert_eq!(std::fs::read_dir(&tmp).map(|d| d.count()).unwrap_or(0), 0);
}

#[tokio::test]
async fn etag_is_stable_across_reads_and_changes_on_write() {
    let e = env(None);
    put(&e, "/a.md", b"one").await;
    let etag = |h: &HeaderMap| h.get("etag").map(|v| v.to_str().unwrap().to_string());
    let (_, h1, _) = send(&e, "GET", "/a.md", b"", &[]).await;
    let (_, h2, _) = send(&e, "GET", "/a.md", b"", &[]).await;
    assert!(etag(&h1).is_some());
    assert_eq!(etag(&h1), etag(&h2));
    put(&e, "/a.md", b"longer content").await;
    let (_, h3, _) = send(&e, "GET", "/a.md", b"", &[]).await;
    assert_ne!(etag(&h1), etag(&h3));
}
