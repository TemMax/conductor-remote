//! Real HTTP acceptance through the production router, concrete service and preview listener.
//! Fetching HTML/JS/CSS proves transport and wiring, not browser execution or rendering.
#[path = "support/artifacts.rs"]
mod artifacts;

use artifacts::{document, Fixture, COLLECTION, INJECTION, RESOURCES, TOKEN};
use conductor_remote::artifacts::ArtifactPreviewSource;
use reqwest::Method;
use serde_json::{json, Value};

#[tokio::test]
async fn production_controls_bootstrap_full_document_dependencies_and_lease_lifecycle() {
    let f = Fixture::new().await;
    let original = document();
    f.write("index.htm", &original);
    for (path, _, content) in RESOURCES {
        f.write(path.trim_start_matches('/'), content);
    }
    f.write(".env.test", "synthetic protected fixture");
    f.write("../outside.json", "synthetic outside selected directory");

    let unauthorized = f
        .client
        .post(format!("{}{COLLECTION}", f.parent))
        .json(&f.file_request())
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let rejected_parent = f.control(Method::POST, COLLECTION)
        .json(&json!({"kind":"file", "reference":f.root.join("index.htm"), "parentOrigin":f.parent.replace("127.0.0.1", "localhost")}))
        .send().await.unwrap();
    assert_eq!(rejected_parent.status(), 403);

    let session = f.create(f.file_request()).await;
    assert_ne!(session.origin, f.parent);
    assert!(session.origin.starts_with("http://127.0.0.1:"));
    assert_eq!(session.entry_path, "/index.htm");
    assert_eq!(session.path.as_deref(), f.root.join("index.htm").to_str());
    assert!(!session.bootstrap_url.contains(&session.grant));
    assert!(!session.bootstrap_url.contains(TOKEN));

    let source = f.source(&session).await;
    assert_eq!(source.status(), 200);
    assert_eq!(source.headers()["cache-control"], "no-store");
    let source: ArtifactPreviewSource = source.json().await.unwrap();
    assert_eq!(source.content, original);
    assert_eq!(source.path, session.path);
    assert!(source.content.lines().count() > 700);
    assert!(!source.content.contains(INJECTION));

    let bootstrap = f.client.get(&session.bootstrap_url).send().await.unwrap();
    assert_eq!(bootstrap.status(), 200);
    let bootstrap = bootstrap.text().await.unwrap();
    for expected in [
        &session.id,
        &f.parent,
        "event.source!==parent",
        "event.origin!==parentOrigin",
        "__conductor_preview/authorize",
    ] {
        assert!(bootstrap.contains(expected));
    }
    assert!(!bootstrap.contains(&session.grant));
    assert!(!bootstrap.contains(TOKEN));
    // A main Bearer cannot unlock the artifact listener, and a grant is not a cookie.
    for request in [
        f.client
            .get(format!("{}{}", session.origin, session.entry_path)),
        f.client
            .get(format!("{}{}", session.origin, session.entry_path))
            .bearer_auth(TOKEN),
        f.client
            .get(format!("{}{}", session.origin, session.entry_path))
            .header("Cookie", format!("grant={}", session.grant)),
    ] {
        assert_eq!(request.send().await.unwrap().status(), 403);
    }
    let wrong_origin = f
        .client
        .post(format!("{}/__conductor_preview/authorize", session.origin))
        .header("Origin", &f.parent)
        .json(&json!({"sessionId":session.id,"grant":session.grant}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_origin.status(), 403);
    let cookie = f.cookie(&session).await;
    assert_eq!(f.authorize(&session).await.status(), 403); // one-use grant

    // Browser cookies shared across ports never authorize any main control operation.
    for (method, path, body) in [
        (Method::POST, COLLECTION.to_owned(), f.file_request()),
        (
            Method::GET,
            format!("{COLLECTION}/{}/source", session.id),
            Value::Null,
        ),
        (
            Method::DELETE,
            format!("{COLLECTION}/{}", session.id),
            Value::Null,
        ),
    ] {
        let response = f
            .client
            .request(method, format!("{}{path}", f.parent))
            .header("Cookie", &cookie)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }

    let page = f.resource(&session, &session.entry_path, &cookie).await;
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers()["content-type"], "text/html");
    assert_eq!(page.headers()["cache-control"], "no-store");
    assert_eq!(page.headers()["permissions-policy"], "document-domain=()");
    let policy = page.headers()["content-security-policy"].to_str().unwrap();
    assert!(policy.contains(&f.parent));
    assert!(policy.contains("sandbox allow-scripts allow-same-origin"));
    let page = page.text().await.unwrap();
    assert!(page.starts_with("<!DOCTYPE html>\r\n"));
    assert!(page.contains("id=\"final-marker\""));
    assert_eq!(page.replacen(INJECTION, "", 1), original);
    assert!(page.find(INJECTION).unwrap() < page.find("type=\"module\"").unwrap());
    assert!(!page.contains(TOKEN));
    assert!(!page.contains(&session.grant));

    for (path, mime, bytes) in RESOURCES {
        let response = f
            .resource(&session, path, &format!("unrelated=ignored; {cookie}"))
            .await;
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()["content-type"], *mime, "{path}");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.headers()["cross-origin-resource-policy"],
            "same-origin"
        );
        assert!(!response
            .headers()
            .contains_key("access-control-allow-origin"));
        assert_eq!(response.bytes().await.unwrap().as_ref(), *bytes, "{path}");
    }
    for (path, status) in [
        ("/missing.png", 404),
        ("/.env.test", 403),
        ("/css%2f..%2f..%2f..%2foutside.json", 403),
        ("/api/state", 404),
        ("/api/artifact-previews", 404),
        ("/css/", 403),
    ] {
        assert_eq!(
            f.resource(&session, path, &cookie).await.status(),
            status,
            "{path}"
        );
    }
    let failures = format!("/__conductor_preview/failures?session={}", session.id);
    let errors: Vec<Value> = f
        .resource(&session, &failures, &cookie)
        .await
        .json()
        .await
        .unwrap();
    assert!(errors.iter().any(|error| error["kind"] == "http"
        && error["source"] == "/missing.png"
        && error["message"] == "Static resource returned 404"));
    assert!(errors.len() <= 100);
    assert!(f
        .resource(&session, &failures, &cookie)
        .await
        .json::<Vec<Value>>()
        .await
        .unwrap()
        .is_empty());
    let diagnostics = f
        .resource(&session, "/__conductor_preview/diagnostics.js", &cookie)
        .await;
    assert_eq!(diagnostics.status(), 200);
    let diagnostics = diagnostics.text().await.unwrap();
    assert!(diagnostics.contains("unhandledrejection"));
    assert!(diagnostics.contains("resource"));
    assert!(!diagnostics.contains(TOKEN));
    let worker = f
        .client
        .get(format!("{}/worker.js", session.origin))
        .header("Cookie", &cookie)
        .header("Service-Worker", "script")
        .send()
        .await
        .unwrap();
    assert_eq!(worker.status(), 403);

    // Same artifact, two control sessions: closing one must preserve the other's reload.
    let second = f.create(f.file_request()).await;
    assert_ne!(second.id, session.id);
    assert_eq!(second.origin, session.origin);
    assert_eq!(f.cookie(&second).await, cookie);
    f.close(&session).await;
    assert_eq!(f.source(&session).await.status(), 404);
    assert_eq!(
        f.client
            .get(&second.bootstrap_url)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        f.resource(&second, &second.entry_path, &cookie)
            .await
            .status(),
        200
    );

    let refreshed = original.replace("final marker", "updated marker");
    f.write("index.htm", &refreshed);
    assert_eq!(
        f.source(&second)
            .await
            .json::<ArtifactPreviewSource>()
            .await
            .unwrap()
            .content,
        refreshed
    );
    assert_eq!(
        f.resource(&second, &second.entry_path, &cookie)
            .await
            .text()
            .await
            .unwrap()
            .replacen(INJECTION, "", 1),
        refreshed
    );
    f.close(&second).await;
    assert_eq!(f.source(&second).await.status(), 404);

    // Persisted identity keeps its origin; the new lease rotates credentials.
    let reopened = f.create(f.file_request()).await;
    assert_eq!(reopened.origin, session.origin);
    assert_ne!(reopened.id, session.id);
    assert_eq!(
        f.resource(&reopened, &reopened.entry_path, &cookie)
            .await
            .status(),
        403
    );
    let new_cookie = f.cookie(&reopened).await;
    assert_ne!(new_cookie, cookie);
    assert_eq!(new_cookie.split('=').next(), cookie.split('=').next());

    // A different inline identity cannot reuse the saved file's cookie or grant.
    let inline = f.create(json!({"kind":"html", "html":original, "baseDirectory":f.root, "parentOrigin":f.parent})).await;
    assert_ne!(inline.origin, reopened.origin);
    assert_eq!(
        f.source(&inline)
            .await
            .json::<ArtifactPreviewSource>()
            .await
            .unwrap()
            .content,
        original
    );
    assert_eq!(
        f.resource(&inline, &inline.entry_path, &new_cookie)
            .await
            .status(),
        403
    );
    let cross_grant = f
        .client
        .post(format!("{}/__conductor_preview/authorize", inline.origin))
        .header("Origin", &inline.origin)
        .json(&json!({"sessionId":reopened.id,"grant":reopened.grant}))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_grant.status(), 403);
    let inline_cookie = f.cookie(&inline).await;
    assert_eq!(
        f.resource(&inline, "/css/main.css", &inline_cookie)
            .await
            .status(),
        200
    );

    let expired_grant = f.create(f.file_request()).await;
    f.clock.advance(30);
    assert_eq!(f.authorize(&expired_grant).await.status(), 403);
    f.close(&expired_grant).await;
    f.clock.advance(1800);
    f.manager.sweep().await.unwrap();
    assert_eq!(f.source(&reopened).await.status(), 404);
    assert_eq!(f.source(&inline).await.status(), 404);
    // Reopening after expiry allows a reliable HTTP rejection of the expired lease's cookie.
    let after_expiry = f.create(f.file_request()).await;
    assert_eq!(after_expiry.origin, reopened.origin);
    assert_eq!(
        f.resource(&after_expiry, &after_expiry.entry_path, &new_cookie)
            .await
            .status(),
        403
    );
    let last_cookie = f.cookie(&after_expiry).await;
    assert_ne!(last_cookie, new_cookie);
    assert_eq!(
        f.resource(&after_expiry, &after_expiry.entry_path, &last_cookie)
            .await
            .status(),
        200
    );
    f.close(&after_expiry).await;
    f.stop().await;
}
