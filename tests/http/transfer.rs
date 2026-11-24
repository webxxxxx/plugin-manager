//! transfer http tests.

use super::*;

#[test]
fn blobs_upload_and_download_round_trip() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        b"blob body",
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let meta = json_body(&put);
    let id = meta["id"].as_str().expect("blob id").to_string();
    assert_eq!(meta["size"], 9);

    let get = raw_request(&address, "GET", &format!("/api/v1/blobs/{id}"), "", b"");
    assert_eq!(get.status, 200);
    assert_eq!(get.body, b"blob body");

    let missing = raw_request(&address, "GET", "/api/v1/blobs/sha256:deadbeef", "", b"");
    assert_eq!(missing.status, 404);
}

/// #291: a transfer artifact can be released on purpose instead of waiting for
/// `temp_retention_secs`. The store calls this once the browser has the bytes,
/// so an install's disk peak is one package, not two.
#[test]
fn a_blob_can_be_released_with_delete() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        b"throwaway package",
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let id = json_body(&put)["id"].as_str().expect("blob id").to_string();

    let removed = raw_request(&address, "DELETE", &format!("/api/v1/blobs/{id}"), "", b"");
    assert_eq!(removed.status, 200, "{:?}", removed.body);
    assert_eq!(json_body(&removed)["removed"], true);

    let gone = raw_request(&address, "GET", &format!("/api/v1/blobs/{id}"), "", b"");
    assert_eq!(gone.status, 404, "a released blob must not be downloadable");

    let again = raw_request(&address, "DELETE", &format!("/api/v1/blobs/{id}"), "", b"");
    assert_eq!(again.status, 404, "a second release is not an error to invent");
}

/// The upgrade pre-check reads every plugin root plus the temp volume. The
/// rows are a list so a multi-root install can ask "where can this go" (#291).
#[test]
fn the_disk_route_lists_roots_and_the_temp_volume() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let disk = raw_request(&address, "GET", "/api/v1/disk", "", b"");
    assert_eq!(disk.status, 200, "{:?}", disk.body);
    let body = json_body(&disk);
    let roots = body["roots"].as_array().expect("roots is a list");
    assert_eq!(roots.len(), 1, "{body}");
    assert_eq!(roots[0]["default"], true);
    assert!(
        roots[0]["root"]
            .as_str()
            .is_some_and(|root| root.ends_with("plugins")),
        "{body}"
    );
    assert!(
        roots[0]["freeBytes"].as_u64().unwrap_or(0) > 0,
        "a root the daemon can write must report bytes: {body}"
    );
    assert!(
        roots[0]["plugin"].is_null(),
        "without ?id there is no plugin column: {body}"
    );
    assert!(
        body["tempFreeBytes"].as_u64().unwrap_or(0) > 0,
        "the temp volume is reported beside the roots: {body}"
    );
}

/// `?id=` marks the root that already holds that plugin, which is what tells
/// a caller to upgrade in place instead of picking a fresh root.
#[test]
fn the_disk_route_marks_the_root_that_holds_the_asked_plugin() {
    let mut sandbox = Sandbox::new();
    let plugin = sandbox.data.join("plugins/demo");
    std::fs::create_dir_all(plugin.join("1.0.0")).expect("version dir");
    std::fs::write(plugin.join("current"), "1.0.0").expect("current");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let disk = raw_request(&address, "GET", "/api/v1/disk?id=demo", "", b"");
    assert_eq!(disk.status, 200, "{:?}", disk.body);
    let body = json_body(&disk);
    let row = &body["roots"][0];
    assert_eq!(row["plugin"]["id"], "demo", "{body}");
    assert_eq!(row["plugin"]["version"], "1.0.0", "{body}");

    let ghost = raw_request(&address, "GET", "/api/v1/disk?id=ghost", "", b"");
    assert_eq!(ghost.status, 200, "{:?}", ghost.body);
    assert!(
        json_body(&ghost)["roots"][0]["plugin"].is_null(),
        "an unknown id is not installed in any root"
    );

    let invalid = raw_request(&address, "GET", "/api/v1/disk?id=bad%2Fid", "", b"");
    assert_eq!(invalid.status, 400, "{:?}", invalid.body);
}

#[test]
fn blob_upload_accepts_chunked_and_multipart_bodies() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let body = b"chunked blob body";
    let chunked = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        body,
        &[],
    );
    assert_eq!(chunked.status, 200, "{:?}", chunked.body);
    let meta = json_body(&chunked);
    let expected = format!("sha256:{}", hex_sha256(body));
    assert_eq!(meta["id"], expected);
    assert_eq!(meta["size"], body.len());
    assert_eq!(
        raw_header_values(&chunked, "etag"),
        vec![format!("\"{}\"", expected).as_str()]
    );

    let boundary = "svcd-test-boundary";
    let multipart = multipart_body(boundary, "payload.bin", b"multipart blob body");
    let response = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        &format!("multipart/form-data; boundary={}", boundary),
        &multipart,
    );
    assert_eq!(response.status, 200, "{:?}", response.body);
    let meta = json_body(&response);
    let id = meta["id"].as_str().expect("blob id");
    let download = raw_request(&address, "GET", &format!("/api/v1/blobs/{}", id), "", b"");
    assert_eq!(download.status, 200);
    assert_eq!(download.body, b"multipart blob body");

    // The same multipart body also works when the outer request is chunked.
    let chunked_multipart = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/blobs",
        &format!("multipart/form-data; boundary={}", boundary),
        &multipart,
        &[],
    );
    assert_eq!(
        chunked_multipart.status, 200,
        "{:?}",
        chunked_multipart.body
    );
    let id = json_body(&chunked_multipart)["id"]
        .as_str()
        .expect("blob id")
        .to_string();
    assert_eq!(id, meta["id"].as_str().expect("blob id"));
}

#[test]
fn file_bundle_upload_download_and_cross_part_range() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let created = request(&address, "POST", "/api/v1/file/uploads", "", b"");
    assert_eq!(created.status, 200, "{}", created.body);
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();

    let first = request(
        &address,
        "PUT",
        &format!("/api/v1/file/uploads/{}/parts/1", upload_id),
        "application/octet-stream",
        b"hello ",
    );
    assert_eq!(first.status, 200, "{}", first.body);
    let first_etag = first.body["etag"].as_str().expect("part etag").to_string();

    let second = raw_request_chunked(
        &address,
        "PUT",
        &format!("/api/v1/file/uploads/{}/parts/2", upload_id),
        "application/octet-stream",
        b"world",
        &[],
    );
    assert_eq!(second.status, 200, "{:?}", second.body);
    let second_etag = json_body(&second)["etag"]
        .as_str()
        .expect("part etag")
        .to_string();

    let third = request(
        &address,
        "PUT",
        &format!("/api/v1/files/uploads/{}/parts/3", upload_id),
        "application/octet-stream",
        b"!",
    );
    assert_eq!(third.status, 200, "{}", third.body);
    let third_etag = third.body["etag"].as_str().expect("part etag").to_string();

    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", upload_id),
        "application/json",
        serde_json::to_string(&json!({
            "name": "greeting.txt",
            "mimeType": "text/plain",
            "parts": [
                {"partNumber": 1, "etag": first_etag},
                {"partNumber": 2, "etag": second_etag},
                {"partNumber": 3, "etag": third_etag},
            ]
        }))
        .expect("complete body")
        .as_bytes(),
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("file id").to_string();
    assert_eq!(complete.body["size"], 12);
    assert_eq!(id, format!("sha256:{}", hex_sha256(b"hello world!")));

    let download = raw_request(&address, "GET", &format!("/api/v1/file/{}", id), "", b"");
    assert_eq!(download.status, 200);
    assert_eq!(download.body, b"hello world!");
    assert_eq!(
        raw_header_values(&download, "content-type"),
        vec!["text/plain"]
    );
    assert_eq!(
        raw_header_values(&download, "etag"),
        vec![format!("\"{}\"", id).as_str()]
    );
    assert_eq!(
        raw_header_values(&download, "content-disposition"),
        vec!["attachment; filename=\"greeting.txt\""]
    );

    let range = raw_request_with(
        &address,
        "GET",
        &format!("/api/v1/files/{}", id),
        "",
        b"",
        &[("Range", "bytes=4-8")],
    );
    assert_eq!(range.status, 206);
    assert_eq!(range.body, b"o wor");
    assert_eq!(
        raw_header_values(&range, "content-range"),
        vec!["bytes 4-8/12"]
    );

    let head = raw_request(&address, "HEAD", &format!("/api/v1/file/{}", id), "", b"");
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(raw_header_values(&head, "content-length"), vec!["12"]);

    // A second session with the same content reuses the content-addressed file.
    let created = request(&address, "POST", "/api/v1/files/uploads", "", b"");
    let duplicate_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    for (number, content) in [
        (1, b"hello ".as_slice()),
        (2, b"world".as_slice()),
        (3, b"!".as_slice()),
    ] {
        let part = request(
            &address,
            "PUT",
            &format!("/api/v1/files/uploads/{}/parts/{}", duplicate_id, number),
            "application/octet-stream",
            content,
        );
        assert_eq!(part.status, 200, "{}", part.body);
    }
    let duplicate = request(
        &address,
        "POST",
        &format!("/api/v1/files/uploads/{}/complete", duplicate_id),
        "application/json",
        b"{\"parts\":[{\"partNumber\":1},{\"partNumber\":2},{\"partNumber\":3}]}",
    );
    assert_eq!(duplicate.status, 200, "{}", duplicate.body);
    assert_eq!(duplicate.body["id"].as_str(), Some(id.as_str()));
}

#[test]
fn blob_download_supports_range_etag_and_conditionals() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        b"0123456789",
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let id = json_body(&put)["id"].as_str().expect("blob id").to_string();
    let path = format!("/api/v1/blobs/{}", id);

    let full = raw_request(&address, "GET", &path, "", b"");
    assert_eq!(full.status, 200);
    assert_eq!(full.body, b"0123456789");
    let etag = raw_header_values(&full, "etag")
        .into_iter()
        .next()
        .expect("etag")
        .to_string();
    assert_eq!(etag, format!("\"{}\"", id));
    assert_eq!(raw_header_values(&full, "accept-ranges"), vec!["bytes"]);
    assert!(!raw_header_values(&full, "last-modified").is_empty());

    let range = raw_request_with(&address, "GET", &path, "", b"", &[("Range", "bytes=2-5")]);
    assert_eq!(range.status, 206);
    assert_eq!(range.body, b"2345");
    assert_eq!(
        raw_header_values(&range, "content-range"),
        vec!["bytes 2-5/10"]
    );

    let suffix = raw_request_with(&address, "GET", &path, "", b"", &[("Range", "bytes=-3")]);
    assert_eq!(suffix.status, 206);
    assert_eq!(suffix.body, b"789");

    let open = raw_request_with(&address, "GET", &path, "", b"", &[("Range", "bytes=7-")]);
    assert_eq!(open.status, 206);
    assert_eq!(open.body, b"789");

    let unsatisfiable =
        raw_request_with(&address, "GET", &path, "", b"", &[("Range", "bytes=99-")]);
    assert_eq!(unsatisfiable.status, 416);
    assert_eq!(
        raw_header_values(&unsatisfiable, "content-range"),
        vec!["bytes */10"]
    );

    let not_modified =
        raw_request_with(&address, "GET", &path, "", b"", &[("If-None-Match", &etag)]);
    assert_eq!(not_modified.status, 304);
    assert!(not_modified.body.is_empty());

    let future = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("If-Modified-Since", "Sun, 15 Nov 2099 00:00:00 GMT")],
    );
    assert_eq!(future.status, 304);

    let unmodified = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:00 GMT")],
    );
    assert_eq!(unmodified.status, 412);

    let failed = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("If-Match", "\"sha256:deadbeef\"")],
    );
    assert_eq!(failed.status, 412);

    let stale_range = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("Range", "bytes=2-5"), ("If-Range", "\"sha256:deadbeef\"")],
    );
    assert_eq!(stale_range.status, 200);
    assert_eq!(stale_range.body, b"0123456789");

    let fresh_range = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("Range", "bytes=2-5"), ("If-Range", &etag)],
    );
    assert_eq!(fresh_range.status, 206);
    assert_eq!(fresh_range.body, b"2345");

    let head = raw_request(&address, "HEAD", &path, "", b"");
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(raw_header_values(&head, "content-length"), vec!["10"]);

    let head_unsatisfiable =
        raw_request_with(&address, "HEAD", &path, "", b"", &[("Range", "bytes=99-")]);
    assert_eq!(head_unsatisfiable.status, 416);
    assert!(head_unsatisfiable.body.is_empty());

    let missing = raw_request(&address, "HEAD", "/api/v1/blobs/sha256:deadbeef", "", b"");
    assert_eq!(missing.status, 404);
    assert!(missing.body.is_empty());
}

/// An export used to take the connection out of the loop. It is a stream
/// handler now: a slow reader must hold one long-lived slot, leave the loop
/// free, and release both when it disconnects.
#[test]
fn a_slow_export_reader_drops_the_stream_cap_and_leaves_the_loop_free() {
    let payload = large_download_body();
    let (_sandbox, address) = export_sandbox(36, "max_streams = 1\n", &payload);
    let path = "/api/v1/export/demo/spk";

    let mut stalled = TcpStream::connect(&address).expect("stalled connect");
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stalled timeout");
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, address
    );
    stalled
        .write_all(request.as_bytes())
        .expect("stalled request");
    let (status, headers) = read_response_head(&mut stalled);
    assert_eq!(status, 200);
    let content_length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.as_str())
        .expect("an export advertises its length")
        .parse()
        .expect("content-length is a number");
    assert!(
        content_length > payload.len(),
        "the archive is larger than its payload: {content_length}"
    );

    // The loop still answers an ordinary request while the transfer is stalled.
    let version = raw_request(&address, "GET", "/api/v1/version", "", b"");
    assert_eq!(
        version.status, 200,
        "a slow export reader must not block the loop"
    );

    // The stalled export owns the only long-lived slot.
    let busy = raw_request(&address, "GET", path, "", b"");
    assert_eq!(busy.status, 503, "{:?}", busy.body);
    assert_eq!(
        raw_header_values(&busy, "retry-after"),
        vec!["1"],
        "the busy response keeps the stable Retry-After contract"
    );
    assert_eq!(json_body(&busy)["error"]["code"], "busy");

    // Dropping the connection releases the slot and the file state.
    drop(stalled);
    let mut released = None;
    for _ in 0..200 {
        let response = raw_request(&address, "GET", path, "", b"");
        if response.status == 200 {
            released = Some(response);
            break;
        }
        assert_eq!(response.status, 503, "{:?}", response.body);
        std::thread::sleep(Duration::from_millis(10));
    }
    let released = released.expect("the export cap must be released after a disconnect");
    assert_eq!(
        released.body.len(),
        content_length,
        "the export writes exactly the length it advertised"
    );
    let names = tar_names(&released.body);
    assert!(
        names.iter().any(|name| name == "payload/big.bin"),
        "the released export still carries the payload: {names:?}"
    );
}

/// An export used to take the connection out of the loop and spawn a
/// per-export thread. It is a stream handler now, so a stalled export must not
/// change the daemon thread count.
#[cfg(target_os = "linux")]
#[test]
fn unix_export_has_no_takeover_thread() {
    let payload = large_download_body();
    let (sandbox, address) = export_sandbox(37, "", &payload);
    let pid = sandbox.daemon.as_ref().expect("running daemon").id();
    let idle = stable_thread_count(pid);

    let mut stalled = TcpStream::connect(&address).expect("stalled connect");
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stalled timeout");
    let request = format!(
        "GET /api/v1/export/demo HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    stalled
        .write_all(request.as_bytes())
        .expect("stalled request");
    let (status, _) = read_response_head(&mut stalled);
    assert_eq!(status, 200);

    assert_eq!(
        stable_thread_count(pid),
        idle,
        "an export must not add a takeover thread"
    );
    drop(stalled);
}

#[test]
fn a_slow_blob_reader_drops_the_stream_cap_and_leaves_the_loop_free() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("max_streams = 1\n");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let content = large_download_body();
    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        &content,
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let id = json_body(&put)["id"].as_str().expect("blob id").to_string();
    assert_slow_download_holds_the_cap(&address, &format!("/api/v1/blobs/{}", id), &content);
}

#[test]
fn a_slow_file_bundle_reader_drops_the_stream_cap_and_leaves_the_loop_free() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("max_streams = 1\n");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let created = request(&address, "POST", "/api/v1/file/uploads", "", b"");
    assert_eq!(created.status, 200, "{}", created.body);
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    let first = vec![0x11; 16 * 1024 * 1024];
    let second = vec![0x22; 16 * 1024 * 1024];
    for (number, part) in [(1, first.as_slice()), (2, second.as_slice())] {
        let response = raw_request(
            &address,
            "PUT",
            &format!("/api/v1/file/uploads/{}/parts/{}", upload_id, number),
            "application/octet-stream",
            part,
        );
        assert_eq!(response.status, 200, "{:?}", response.body);
    }
    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", upload_id),
        "application/json",
        b"{\"parts\":[{\"partNumber\":1},{\"partNumber\":2}]}",
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("file id");
    let mut content = first;
    content.extend_from_slice(&second);
    assert_eq!(complete.body["size"], content.len() as u64);
    assert_slow_download_holds_the_cap(&address, &format!("/api/v1/file/{}", id), &content);
}

/// A stalled upload holds one long-lived max_streams slot, keeps the loop
/// serving other requests, and releases its staging file on disconnect.
#[test]
fn a_slow_upload_holds_the_stream_cap_and_leaves_no_staging_file() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("max_streams = 1\n");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let temp = sandbox.data.join("temp");

    let initial = vec![0x5a; 64 * 1024];
    let stalled = start_stalled_upload(
        &address,
        "POST",
        "/api/v1/blobs",
        32 * 1024 * 1024,
        &initial,
    );
    wait_for_staging(&sandbox, &temp);

    let version = raw_request(&address, "GET", "/api/v1/version", "", b"");
    assert_eq!(version.status, 200, "an upload must not block the loop");

    let (status, headers) = second_upload_status(&address);
    assert_eq!(status, 503, "the stalled upload owns the only stream slot");
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("retry-after") && value == "1"),
        "busy upload keeps Retry-After: 1: {:?}",
        headers
    );

    drop(stalled);
    wait_for_no_staging(&sandbox, &temp);

    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        b"after release",
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
}

#[test]
fn a_disconnected_part_upload_removes_its_staging_file() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    assert_eq!(created.status, 200, "{}", created.body);
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    let path = format!("/api/v1/blobs/uploads/{}/parts/1", upload_id);
    let temp = sandbox.data.join("temp");

    let initial = vec![0x5a; 64 * 1024];
    let stalled = start_stalled_upload(&address, "PUT", &path, 32 * 1024 * 1024, &initial);
    wait_for_staging(&sandbox, &temp);
    drop(stalled);
    wait_for_no_staging(&sandbox, &temp);
}

/// The upload used to take the connection out of the loop and spawn a
/// `svcd-blob-http` thread. It is a stream handler now, so a stalled upload must
/// not add a daemon thread.
#[cfg(target_os = "linux")]
#[test]
fn unix_upload_has_no_takeover_thread() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let pid = sandbox.daemon.as_ref().expect("running daemon").id();
    let idle = stable_thread_count(pid);

    let initial = vec![0x5a; 64 * 1024];
    let stalled = start_stalled_upload(
        &address,
        "POST",
        "/api/v1/blobs",
        32 * 1024 * 1024,
        &initial,
    );
    wait_for_staging(&sandbox, &sandbox.data.join("temp"));

    assert_eq!(
        stable_thread_count(pid),
        idle,
        "a blob upload must not add a takeover thread"
    );
    drop(stalled);
}

/// A package upload is a long-lived stream: it draws from the same
/// `max_streams` budget as downloads and is refused with the stable 503 before
/// its body is read.
#[test]
fn a_stalled_download_refuses_a_package_upload_with_503() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("max_streams = 1\n");
    let key = SigningKey::from_bytes(&[48u8; 32]);
    let package = plugin_bundle(&key, "demo", "1.0.0", None, false);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let content = large_download_body();
    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        &content,
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let id = json_body(&put)["id"].as_str().expect("blob id").to_string();
    let mut stalled = TcpStream::connect(&address).expect("stalled connect");
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    stalled
        .write_all(
            format!(
                "GET /api/v1/blobs/{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                id, address
            )
            .as_bytes(),
        )
        .expect("stalled request");
    assert_eq!(read_response_head(&mut stalled).0, 200);

    let mut upload = TcpStream::connect(&address).expect("upload connect");
    upload
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    upload
        .write_all(
            format!(
                "POST /api/v1/plugins HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                address,
                package.len()
            )
            .as_bytes(),
        )
        .expect("upload head");
    let (status, headers) = read_response_head(&mut upload);
    assert_eq!(
        status, 503,
        "a full stream budget refuses the package upload"
    );
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("retry-after") && value == "1"),
        "busy upload keeps Retry-After: 1: {headers:?}"
    );
    drop(upload);
    drop(stalled);
}

#[test]
fn file_part_2mib_upload_round_trip() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let created = request(&address, "POST", "/api/v1/file/uploads", "", b"");
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    let content = vec![0x5a; 2 * 1024 * 1024];
    let part = raw_request(
        &address,
        "PUT",
        &format!("/api/v1/file/uploads/{}/parts/1", upload_id),
        "application/octet-stream",
        &content,
    );
    assert_eq!(part.status, 200, "{:?}", part.body);
    let etag = json_body(&part)["etag"]
        .as_str()
        .expect("part etag")
        .to_string();
    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", upload_id),
        "application/json",
        serde_json::to_string(&json!({"parts": [{"partNumber": 1, "etag": etag}]}))
            .expect("complete body")
            .as_bytes(),
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("file id");
    let expected = format!("sha256:{}", hex_sha256(&content));
    assert_eq!(id, expected);
    let download = raw_request(&address, "GET", &format!("/api/v1/file/{}", id), "", b"");
    assert_eq!(download.status, 200);
    assert_eq!(download.body.len(), content.len());
    assert_eq!(download.body, content);
}

#[test]
fn blob_2mib_upload_and_multipart_complete_round_trip() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let direct = vec![0x33; 2 * 1024 * 1024];
    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        &direct,
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let direct_id = format!("sha256:{}", hex_sha256(&direct));
    assert_eq!(json_body(&put)["id"].as_str(), Some(direct_id.as_str()));
    let ranged = raw_request_with(
        &address,
        "GET",
        &format!("/api/v1/blobs/{}", direct_id),
        "",
        b"",
        &[("Range", "bytes=1048576-2097151")],
    );
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.body, direct[1024 * 1024..2 * 1024 * 1024]);

    let created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    let first = vec![0x11; 2 * 1024 * 1024];
    let second = vec![0x22; 2 * 1024 * 1024];
    for (number, content) in [(1, first.as_slice()), (2, second.as_slice())] {
        let part = raw_request(
            &address,
            "PUT",
            &format!("/api/v1/blobs/uploads/{}/parts/{}", upload_id, number),
            "application/octet-stream",
            content,
        );
        assert_eq!(part.status, 200, "{:?}", part.body);
    }
    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", upload_id),
        "application/json",
        b"{\"parts\":[{\"partNumber\":1},{\"partNumber\":2}]}",
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let mut expected = first.clone();
    expected.extend_from_slice(&second);
    let expected_id = format!("sha256:{}", hex_sha256(&expected));
    assert_eq!(complete.body["id"].as_str(), Some(expected_id.as_str()));

    let downloaded = raw_request(
        &address,
        "GET",
        &format!("/api/v1/blobs/{}", expected_id),
        "",
        b"",
    );
    assert_eq!(downloaded.status, 200);
    assert_eq!(downloaded.body, expected);
}

/// A well-formed but unknown upload id is a 404 on every session endpoint: the
/// storage kind decides the status, never the message text (#151).
#[test]
fn an_unknown_upload_session_is_not_found() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let unknown = "0".repeat(32);

    for (method, path, body) in [
        ("GET", format!("/api/v1/blobs/uploads/{}", unknown), ""),
        (
            "POST",
            format!("/api/v1/blobs/uploads/{}/complete", unknown),
            "",
        ),
        ("DELETE", format!("/api/v1/blobs/uploads/{}", unknown), ""),
        ("GET", format!("/api/v1/file/uploads/{}", unknown), ""),
    ] {
        let response = request(&address, method, &path, "", body.as_bytes());
        assert_eq!(response.status, 404, "{method} {path}: {:?}", response.body);
        assert_eq!(response.body["error"]["code"], "not_found", "{path}");
    }
}

#[test]
fn upload_sessions_reject_cross_kind_requests() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let file_created = request(&address, "POST", "/api/v1/file/uploads", "", b"");
    assert_eq!(file_created.status, 200);
    let file_id = file_created.body["uploadId"]
        .as_str()
        .expect("file upload id")
        .to_string();

    let blob_status = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", file_id),
        "",
        b"",
    );
    assert_eq!(blob_status.status, 409, "{:?}", blob_status.body);
    let blob_abort = request(
        &address,
        "DELETE",
        &format!("/api/v1/blobs/uploads/{}", file_id),
        "",
        b"",
    );
    assert_eq!(blob_abort.status, 409, "{:?}", blob_abort.body);
    let blob_complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", file_id),
        "application/json",
        b"",
    );
    assert_eq!(blob_complete.status, 409, "{:?}", blob_complete.body);
    let file_status = request(
        &address,
        "GET",
        &format!("/api/v1/file/uploads/{}", file_id),
        "",
        b"",
    );
    assert_eq!(file_status.status, 200);

    let file_part = raw_request(
        &address,
        "PUT",
        &format!("/api/v1/file/uploads/{}/parts/1", file_id),
        "application/octet-stream",
        b"file",
    );
    assert_eq!(file_part.status, 200);
    let file_complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", file_id),
        "application/json",
        b"",
    );
    assert_eq!(file_complete.status, 200, "{}", file_complete.body);

    let blob_created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    assert_eq!(blob_created.status, 200);
    let blob_id = blob_created.body["uploadId"]
        .as_str()
        .expect("blob upload id")
        .to_string();
    let file_status = request(
        &address,
        "GET",
        &format!("/api/v1/file/uploads/{}", blob_id),
        "",
        b"",
    );
    assert_eq!(file_status.status, 409, "{:?}", file_status.body);
    let file_abort = request(
        &address,
        "DELETE",
        &format!("/api/v1/file/uploads/{}", blob_id),
        "",
        b"",
    );
    assert_eq!(file_abort.status, 409, "{:?}", file_abort.body);
    let file_complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", blob_id),
        "application/json",
        b"",
    );
    assert_eq!(file_complete.status, 409, "{:?}", file_complete.body);
    let blob_part = raw_request(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/1", blob_id),
        "application/octet-stream",
        b"blob",
    );
    assert_eq!(blob_part.status, 200);
    let blob_complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", blob_id),
        "application/json",
        b"{\"parts\":[{\"partNumber\":1}]}",
    );
    assert_eq!(blob_complete.status, 200, "{}", blob_complete.body);
}

#[test]
fn blob_upload_honours_expect_100_continue() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let body = b"continue body";
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "POST /api/v1/blobs HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let mut interim = Vec::new();
    let mut byte = [0u8; 1];
    while !interim.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("100 continue");
        interim.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&interim).starts_with("HTTP/1.1 100"),
        "{:?}",
        String::from_utf8_lossy(&interim)
    );
    stream.write_all(body).expect("write body");
    let response = read_raw_response(&mut stream);
    assert_eq!(response.status, 200, "{:?}", response.body);
}

#[test]
fn blob_resumable_upload_supports_parts_retries_and_parallel_uploads() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    assert_eq!(created.status, 200, "{}", created.body);
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();

    let first = raw_request_chunked(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/1", upload_id),
        "application/octet-stream",
        b"hello ",
        &[],
    );
    assert_eq!(first.status, 200, "{:?}", first.body);
    let first_etag = json_body(&first)["etag"]
        .as_str()
        .expect("part etag")
        .to_string();

    let second = request(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/2", upload_id),
        "application/octet-stream",
        b"world",
    );
    assert_eq!(second.status, 200, "{}", second.body);
    let second_etag = second.body["etag"].as_str().expect("part etag").to_string();

    let status = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", upload_id),
        "",
        b"",
    );
    assert_eq!(status.status, 200, "{}", status.body);
    assert_eq!(status.body["totalSize"], 11);
    assert_eq!(status.body["parts"].as_array().expect("parts").len(), 2);

    // Re-uploading the same part number replaces it.
    let replaced = request(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/2", upload_id),
        "application/octet-stream",
        b"there",
    );
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    let replaced_etag = replaced.body["etag"]
        .as_str()
        .expect("part etag")
        .to_string();
    let status = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", upload_id),
        "",
        b"",
    );
    assert_eq!(status.body["totalSize"], 11);

    // Replace it back so the assembled content is predictable.
    let restored = request(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/2", upload_id),
        "application/octet-stream",
        b"world",
    );
    assert_eq!(restored.status, 200, "{}", restored.body);
    let restored_etag = restored.body["etag"]
        .as_str()
        .expect("part etag")
        .to_string();
    assert_eq!(restored_etag, second_etag);
    assert_ne!(replaced_etag, second_etag);

    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", upload_id),
        "application/json",
        serde_json::to_string(&json!({
            "parts": [
                {"partNumber": 1, "etag": first_etag},
                {"partNumber": 2, "etag": restored_etag},
            ]
        }))
        .expect("complete body")
        .as_bytes(),
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("blob id");
    let expected = format!("sha256:{}", hex_sha256(b"hello world"));
    assert_eq!(id, expected);
    let download = raw_request(&address, "GET", &format!("/api/v1/blobs/{}", id), "", b"");
    assert_eq!(download.status, 200);
    assert_eq!(download.body, b"hello world");

    let after = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", upload_id),
        "",
        b"",
    );
    assert_eq!(after.status, 404, "{}", after.body);

    // Two independent upload sessions can write parts concurrently.
    let parallel = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    let parallel_id = parallel.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    let left = {
        let address = address.clone();
        let id = parallel_id.clone();
        std::thread::spawn(move || {
            raw_request(
                &address,
                "PUT",
                &format!("/api/v1/blobs/uploads/{}/parts/1", id),
                "application/octet-stream",
                b"left-",
            )
        })
    };
    let right = {
        let address = address.clone();
        let id = parallel_id.clone();
        std::thread::spawn(move || {
            raw_request(
                &address,
                "PUT",
                &format!("/api/v1/blobs/uploads/{}/parts/2", id),
                "application/octet-stream",
                b"right",
            )
        })
    };
    assert_eq!(left.join().expect("left").status, 200);
    assert_eq!(right.join().expect("right").status, 200);
    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", parallel_id),
        "",
        b"",
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("blob id");
    let download = raw_request(&address, "GET", &format!("/api/v1/blobs/{}", id), "", b"");
    assert_eq!(download.body, b"left-right");

    let aborted = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    let abort_id = aborted.body["uploadId"].as_str().expect("upload id");
    let deleted = request(
        &address,
        "DELETE",
        &format!("/api/v1/blobs/uploads/{}", abort_id),
        "",
        b"",
    );
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    let gone = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", abort_id),
        "",
        b"",
    );
    assert_eq!(gone.status, 404);
}

/// A blob download used to take the connection out of the loop and spawn a
/// `svcd-blob-http` thread. It is a stream handler now, so a stalled download
/// must not change the daemon thread count.
#[cfg(target_os = "linux")]
#[test]
fn unix_blob_download_has_no_takeover_thread() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let pid = sandbox.daemon.as_ref().expect("running daemon").id();

    // The takeovers that remain (uploads) are transient; wait for the idle
    // count after the upload has closed its connection.
    let content = vec![0x5a; 32 * 1024 * 1024];
    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        &content,
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let id = json_body(&put)["id"].as_str().expect("blob id").to_string();
    let idle = stable_thread_count(pid);

    let mut stalled = TcpStream::connect(&address).expect("stalled connect");
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stalled timeout");
    let request = format!(
        "GET /api/v1/blobs/{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        id, address
    );
    stalled
        .write_all(request.as_bytes())
        .expect("stalled request");
    let (status, _) = read_response_head(&mut stalled);
    assert_eq!(status, 200);

    assert_eq!(
        stable_thread_count(pid),
        idle,
        "a blob download must not add a takeover thread"
    );
    drop(stalled);
}

/// Both complete endpoints decode the same `parts[]` list, so the same
/// non-empty body must produce the same status, error code and message. Empty
/// bodies are the deliberate exception: they keep their pre-existing paths
/// (blob 400 when it has no parts, file 200 after a part upload), and an
/// explicit body without `parts` follows the same optional/required split.
#[test]
fn blob_and_file_complete_share_the_parts_input_table() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let blob_with_part = |content: &[u8]| -> String {
        let created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
        assert_eq!(created.status, 200, "{}", created.body);
        let id = created.body["uploadId"]
            .as_str()
            .expect("blob upload id")
            .to_string();
        let part = raw_request(
            &address,
            "PUT",
            &format!("/api/v1/blobs/uploads/{}/parts/1", id),
            "application/octet-stream",
            content,
        );
        assert_eq!(part.status, 200, "{:?}", part.body);
        id
    };
    let blob_without_parts = || -> String {
        let created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
        assert_eq!(created.status, 200, "{}", created.body);
        created.body["uploadId"]
            .as_str()
            .expect("blob upload id")
            .to_string()
    };
    let file_with_part = |content: &[u8]| -> String {
        let created = request(&address, "POST", "/api/v1/file/uploads", "", b"");
        assert_eq!(created.status, 200, "{}", created.body);
        let id = created.body["uploadId"]
            .as_str()
            .expect("file upload id")
            .to_string();
        let part = raw_request(
            &address,
            "PUT",
            &format!("/api/v1/file/uploads/{}/parts/1", id),
            "application/octet-stream",
            content,
        );
        assert_eq!(part.status, 200, "{:?}", part.body);
        id
    };

    let malformed: &[(&str, &[u8], &str)] = &[
        ("not JSON", b"not json", "body must be JSON"),
        (
            "parts is not an array",
            br#"{"parts":7}"#,
            "parts must be an array",
        ),
        (
            "partNumber missing",
            br#"{"parts":[{"etag":"x"}]}"#,
            "partNumber is required",
        ),
        (
            "partNumber zero",
            br#"{"parts":[{"partNumber":0}]}"#,
            "partNumber must be between 1 and 10000",
        ),
        (
            "partNumber above the range",
            br#"{"parts":[{"partNumber":10001}]}"#,
            "partNumber must be between 1 and 10000",
        ),
    ];
    for &(label, body, message) in malformed {
        let blob_id = blob_with_part(b"payload");
        let file_id = file_with_part(b"payload");
        let blob = request(
            &address,
            "POST",
            &format!("/api/v1/blobs/uploads/{}/complete", blob_id),
            "application/json",
            body,
        );
        let file = request(
            &address,
            "POST",
            &format!("/api/v1/file/uploads/{}/complete", file_id),
            "application/json",
            body,
        );
        for (endpoint, response) in [("blob", &blob), ("file", &file)] {
            assert_eq!(
                response.status, 400,
                "{label} ({endpoint}) status: {:?}",
                response.body
            );
            assert_eq!(
                response.body["error"]["code"], "bad_request",
                "{label} ({endpoint}) code"
            );
            assert_eq!(
                response.body["error"]["message"], message,
                "{label} ({endpoint}) message"
            );
        }
        assert_eq!(blob.body, file.body, "{label}: endpoints disagree");
    }

    // Empty body: the blob has no parts to select, while the file selects the
    // part that was uploaded. Both are today's paths, not parser behavior.
    let blob_id = blob_without_parts();
    let blob = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", blob_id),
        "application/json",
        b"",
    );
    assert_eq!(blob.status, 400, "{:?}", blob.body);
    assert_eq!(blob.body["error"]["code"], "bad_request");
    assert_eq!(blob.body["error"]["message"], "no parts uploaded");

    let file_id = file_with_part(b"empty-body-file");
    let file = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", file_id),
        "application/json",
        b"",
    );
    assert_eq!(file.status, 200, "{}", file.body);
    let expected = format!("sha256:{}", hex_sha256(b"empty-body-file"));
    assert_eq!(file.body["id"], expected);
    assert_eq!(
        header_values(&file, "etag"),
        vec![format!("\"{}\"", expected).as_str()]
    );
    assert_eq!(
        header_values(&file, "location"),
        vec![format!("/api/v1/file/{}", expected).as_str()]
    );

    // A non-empty body without `parts` is the required/optional split the
    // shared parser exists for.
    let blob_id = blob_with_part(b"missing-parts-blob");
    let blob = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", blob_id),
        "application/json",
        br#"{}"#,
    );
    assert_eq!(blob.status, 400, "{:?}", blob.body);
    assert_eq!(blob.body["error"]["code"], "bad_request");
    assert_eq!(blob.body["error"]["message"], "parts is required");

    let file_id = file_with_part(b"missing-parts-file");
    let file = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", file_id),
        "application/json",
        br#"{}"#,
    );
    assert_eq!(file.status, 200, "{}", file.body);

    // The valid list completes both endpoints with the same input, and the
    // success headers stay what they were before the refactor.
    let blob_id = blob_with_part(b"payload");
    let file_id = file_with_part(b"payload");
    let body = br#"{"parts":[{"partNumber":1}]}"#;
    let blob = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", blob_id),
        "application/json",
        body,
    );
    let file = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", file_id),
        "application/json",
        body,
    );
    let expected = format!("sha256:{}", hex_sha256(b"payload"));
    assert_eq!(blob.status, 200, "{}", blob.body);
    assert_eq!(file.status, 200, "{}", file.body);
    assert_eq!(blob.body["id"], expected);
    assert_eq!(blob.body["size"], 7);
    assert_eq!(file.body["id"], expected);
    assert_eq!(file.body["size"], 7);
    assert_eq!(file.body["name"], "file");
    assert_eq!(file.body["mimeType"], "application/octet-stream");
    assert_eq!(
        header_values(&blob, "etag"),
        vec![format!("\"{}\"", expected).as_str()]
    );
    assert_eq!(
        header_values(&blob, "location"),
        vec![format!("/api/v1/blobs/{}", expected).as_str()]
    );
    assert_eq!(
        header_values(&file, "etag"),
        vec![format!("\"{}\"", expected).as_str()]
    );
    assert_eq!(
        header_values(&file, "location"),
        vec![format!("/api/v1/file/{}", expected).as_str()]
    );
}
