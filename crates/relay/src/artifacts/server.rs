//! A preview-only listener: no main API, SPA fallback, filesystem pathname or proxy routes.
use std::sync::Arc;

use axum::body::{to_bytes, Body, HttpBody};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::controller::{denied, original_source, Inner, Runtime, GRANT_LIFE, IDLE};
use super::files::RESOURCE_HEADERS;
use super::ArtifactError;

const PREFIX: &str = "/__conductor_preview/";

#[derive(Clone)]
struct ListenerState {
    runtime: Runtime,
    binding: String,
}

pub(crate) fn start(listener: TcpListener, runtime: Runtime, binding: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let app = Router::new()
            .fallback(route)
            .with_state(ListenerState { runtime, binding });
        // Request authorization always checks the live registry, including keep-alive connections.
        let _ = axum::serve(listener, app).await;
    })
}

async fn route(State(listener): State<ListenerState>, request: Request) -> Response {
    let Some(inner) = listener.runtime.upgrade() else {
        return error(denied());
    };
    let method = request.method().clone();
    let head = method == Method::HEAD;
    let renewal = if !request.uri().path().starts_with(PREFIX) {
        let state = inner.state.lock().await;
        state
            .leases
            .get(&listener.binding)
            .filter(|lease| {
                cookie_matches(request.headers(), &lease.binding.cookie_name, &lease.cookie)
            })
            .map(|lease| {
                (
                    lease.binding.cookie_name.clone(),
                    lease.cookie.clone(),
                    lease.receipt.is_some(),
                )
            })
    } else {
        None
    };
    let result = dispatch(&inner, &listener.binding, request).await;
    let mut response = result.unwrap_or_else(error);
    if response.status().is_success() {
        if let Some((name, value, secure)) = renewal {
            let state = inner.state.lock().await;
            let live = state
                .leases
                .get(&listener.binding)
                .is_some_and(|lease| equal(&lease.cookie, &value))
                && state
                    .sessions
                    .values()
                    .any(|session| session.binding_id == listener.binding && session.authorized);
            // A close during a blocking read cannot restore the retired lease's cookie.
            if live {
                let _ = set_cookie(&mut response, &name, &value, IDLE.as_secs(), secure);
            } else {
                response = error(denied());
            }
        }
    }
    for (name, value) in RESOURCE_HEADERS {
        if !response.headers().contains_key(name) {
            response
                .headers_mut()
                .insert(name, HeaderValue::from_static(value));
        }
    }
    response.headers_mut().insert(
        "permissions-policy",
        HeaderValue::from_static("document-domain=()"),
    );
    let parents = {
        let state = inner.state.lock().await;
        let mut parents = inner.config.allowed_parent_origins.clone();
        // Every session parent passed canonical parsing and exact host admission at create.
        // Include only live viewers of this binding, never request Host/forwarded headers.
        parents.extend(
            state
                .sessions
                .values()
                .filter(|session| session.binding_id == listener.binding)
                .map(|session| session.parent.clone()),
        );
        parents.into_iter().collect::<Vec<_>>().join(" ")
    };
    // This sandbox also protects direct navigation, outside the main app's iframe attributes.
    let policy = format!("frame-ancestors {parents}; sandbox allow-scripts allow-same-origin");
    if let Ok(value) = HeaderValue::from_str(&policy) {
        response
            .headers_mut()
            .append("content-security-policy", value);
    }
    if head {
        *response.body_mut() = Body::empty();
    }
    response
}

async fn dispatch(
    inner: &Arc<Inner>,
    binding_id: &str,
    request: Request,
) -> Result<Response, ArtifactError> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let mut state = inner.state.lock().await;
    let _ = inner.prune(&mut state).await;
    let lease = state.leases.get(binding_id).ok_or_else(denied)?;
    let origin: axum::http::Uri = lease.binding.origin.parse().map_err(|_| denied())?;
    // Host is compared to the persisted authority, never used to construct a trusted origin.
    if request.headers().get("host").and_then(|v| v.to_str().ok())
        != origin.authority().map(|a| a.as_str())
    {
        return Err(denied());
    }
    let secure = lease.receipt.is_some();
    let document = lease.document.clone();
    let cookie_name = lease.binding.cookie_name.clone();
    let cookie_value = lease.cookie.clone();
    if request.headers().get("service-worker").is_some()
        || request
            .headers()
            .get("sec-fetch-dest")
            .is_some_and(|v| v == "serviceworker")
    {
        return Err(ArtifactError::Forbidden(
            "persistent service workers are disabled".into(),
        ));
    }

    if path == format!("{PREFIX}bootstrap") {
        if method != Method::GET && method != Method::HEAD {
            return Ok(method_error());
        }
        let id = session_query(&query)?;
        let session = state
            .sessions
            .get(id)
            .filter(|s| s.binding_id == binding_id)
            .ok_or_else(denied)?;
        let now = inner.config.clock.monotonic();
        if !session.authorized && now.saturating_sub(session.created) >= GRANT_LIFE {
            return Err(denied());
        }
        let parent = session.parent.clone();
        let mut response = body(
            "text/html; charset=utf-8",
            bootstrap(id, &parent, &document.entry_path),
        );
        // Cookies are shared across ports. Retire only known inactive bindings on this hostname;
        // active leases (including pending viewers) retain their cookie. Deleting a pending
        // lease's old cookie here could race another viewer's authorization response.
        let store = inner.store.clone();
        let snapshot = tokio::task::spawn_blocking(move || store.snapshot())
            .await
            .map_err(|_| denied())??;
        for binding in snapshot.bindings {
            let same_host = binding
                .origin
                .parse::<axum::http::Uri>()
                .is_ok_and(|uri| uri.host() == origin.host());
            let active = state.leases.contains_key(&binding.binding_id);
            if same_host && !active && has_cookie(request.headers(), &binding.cookie_name) {
                set_cookie(&mut response, &binding.cookie_name, "", 0, secure)?;
            }
        }
        return Ok(response);
    }
    if path == format!("{PREFIX}authorize") {
        if method != Method::POST {
            return Ok(method_error());
        }
        if request
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
            != Some(lease.binding.origin.as_str())
        {
            return Err(denied());
        }
        // Do not hold the registry while waiting on an untrusted request body.
        drop(state);
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            to_bytes(request.into_body(), 2048),
        )
        .await
        .map_err(|_| denied())?
        .map_err(|_| ArtifactError::TooLarge("authorization request exceeds limit".into()))?;
        let auth: Authorize = serde_json::from_slice(&bytes)
            .map_err(|_| ArtifactError::Invalid("invalid preview authorization".into()))?;
        let mut state = inner.state.lock().await;
        let _ = inner.prune(&mut state).await;
        let now = inner.config.clock.monotonic();
        let session = state
            .sessions
            .get_mut(&auth.session_id)
            .filter(|s| s.binding_id == binding_id)
            .ok_or_else(denied)?;
        if now.saturating_sub(session.created) >= GRANT_LIFE
            || !session
                .grant
                .as_ref()
                .is_some_and(|grant| equal(grant, &auth.grant))
        {
            return Err(denied());
        }
        session.grant = None;
        session.authorized = true;
        session.touched = now;
        let lease = state.leases.get(binding_id).ok_or_else(denied)?;
        let mut response = body("application/json", "{\"ok\":true}");
        set_cookie(
            &mut response,
            &lease.binding.cookie_name,
            &lease.cookie,
            IDLE.as_secs(),
            secure,
        )?;
        return Ok(response);
    }
    if method != Method::GET && method != Method::HEAD {
        return Ok(method_error());
    }
    if !cookie_matches(request.headers(), &cookie_name, &cookie_value)
        || !state
            .sessions
            .values()
            .any(|s| s.binding_id == binding_id && s.authorized)
    {
        return Err(denied());
    }
    let now = inner.config.clock.monotonic();
    // Subresources carry the shared lease cookie, not a per-tab identifier. Their activity keeps
    // authorized viewers of this lease alive; pending grants never gain authority this way.
    if !path.starts_with(PREFIX) {
        for session in state
            .sessions
            .values_mut()
            .filter(|s| s.binding_id == binding_id && s.authorized)
        {
            session.touched = now;
        }
    }
    if path == format!("{PREFIX}context") {
        let id = session_query(&query)?;
        let session = state
            .sessions
            .get(id)
            .filter(|s| s.binding_id == binding_id && s.authorized)
            .ok_or_else(denied)?;
        return Ok(body(
            "application/json",
            serde_json::json!({"sessionId": id, "parentOrigin": session.parent}).to_string(),
        ));
    }
    if path == format!("{PREFIX}diagnostics.js") {
        return Ok(body("text/javascript", DIAGNOSTICS));
    }
    if path == format!("{PREFIX}failures") {
        let id = session_query(&query)?;
        let session = state
            .sessions
            .get_mut(id)
            .filter(|s| s.binding_id == binding_id && s.authorized)
            .ok_or_else(denied)?;
        let failures: Vec<_> = session.failures.drain(..).collect();
        return Ok(body(
            "application/json",
            serde_json::to_string(&failures).map_err(|_| denied())?,
        ));
    }
    if path.starts_with(PREFIX) || path == "/__conductor_preview" {
        return Err(ArtifactError::Missing("unknown preview resource".into()));
    }
    drop(state);
    if path == document.entry_path {
        let source = original_source(&document).await?;
        let html = instrument(&source.content);
        let mut response = body("text/html; charset=utf-8", html);
        response
            .headers_mut()
            .insert("accept-ranges", HeaderValue::from_static("none"));
        return Ok(response);
    }
    let range = request
        .headers()
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let resource = document
        .root
        .clone()
        .read_resource(path.clone(), range, method == Method::HEAD)
        .await;
    let status = match &resource {
        Ok(r) => r.status,
        Err(error) => error.status_code(),
    };
    if status >= 400 {
        let mut state = inner.state.lock().await;
        for session in state
            .sessions
            .values_mut()
            .filter(|s| s.binding_id == binding_id && s.authorized)
        {
            if session.failures.len() < 100 {
                session.failures.push_back(serde_json::json!({"kind":"http", "message":format!("Static resource returned {status}"), "source":path.chars().take(2048).collect::<String>()}));
            }
        }
    }
    let resource = resource?;
    let mut response = body(&resource.mime, resource.body);
    *response.status_mut() = StatusCode::from_u16(resource.status).map_err(|_| denied())?;
    response.headers_mut().insert(
        "content-length",
        HeaderValue::from_str(&resource.content_length.to_string()).map_err(|_| denied())?,
    );
    if let Some(range) = resource.content_range {
        response.headers_mut().insert(
            "content-range",
            HeaderValue::from_str(&range).map_err(|_| denied())?,
        );
    }
    Ok(response)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Authorize {
    session_id: String,
    grant: String,
}

fn session_query(query: &str) -> Result<&str, ArtifactError> {
    query
        .strip_prefix("session=")
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .ok_or_else(denied)
}

fn equal(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

fn has_cookie(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .any(|part| {
            part.trim()
                .split_once('=')
                .is_some_and(|(key, _)| key == name)
        })
}

fn cookie_matches(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    let mut found = None;
    for header in headers.get_all("cookie") {
        let Ok(header) = header.to_str() else {
            return false;
        };
        for part in header.split(';') {
            if let Some((key, value)) = part.trim().split_once('=') {
                if key == name {
                    if found.is_some() {
                        return false;
                    }
                    found = Some(value);
                }
            }
        }
    }
    found.is_some_and(|value| equal(value, expected))
}

fn set_cookie(
    response: &mut Response,
    name: &str,
    value: &str,
    age: u64,
    secure: bool,
) -> Result<(), ArtifactError> {
    let value = format!(
        "{name}={value}; HttpOnly; Path=/; SameSite=Strict; Max-Age={age}{}",
        if secure { "; Secure" } else { "" }
    );
    response.headers_mut().append(
        "set-cookie",
        HeaderValue::from_str(&value).map_err(|_| denied())?,
    );
    Ok(())
}

fn body(mime: &str, content: impl Into<Body>) -> Response {
    let content = content.into();
    let length = content.size_hint().exact();
    let mut response = Response::new(content);
    if let Some(length) = length {
        if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
            response.headers_mut().insert("content-length", value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(mime) {
        response.headers_mut().insert("content-type", value);
    }
    response
}
fn error(error: ArtifactError) -> Response {
    let mut response = body("text/plain; charset=utf-8", error.to_string());
    *response.status_mut() =
        StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    response
}
fn method_error() -> Response {
    let mut response = body("text/plain", "method not allowed");
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    response
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value)
        .expect("string serialization")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

fn bootstrap(id: &str, parent: &str, entry: &str) -> String {
    let script = format!(
        r#"(()=>{{'use strict';const sessionId={},parentOrigin={},entry={};
const envelope=type=>({{channel:'conductor-artifact-preview',version:1,sessionId,type}});
let busy=false;window.addEventListener('message',async event=>{{const m=event.data;
if(event.source!==parent||event.origin!==parentOrigin||!m||m.channel!=='conductor-artifact-preview'||m.version!==1||m.sessionId!==sessionId||m.type!=='authorize'||typeof m.grant!=='string'||m.grant.length>128||busy)return;
busy=true;try{{const response=await fetch('/__conductor_preview/authorize',{{method:'POST',credentials:'same-origin',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{sessionId,grant:m.grant}})}});
if(!response.ok)throw new Error('Preview authorization failed');window.name=JSON.stringify({{channel:'conductor-artifact-preview',sessionId}});location.replace(entry);
}}catch(_){{parent.postMessage({{...envelope('diagnostic'),kind:'http',message:'Preview authorization failed'}},parentOrigin);}} }});
parent.postMessage(envelope('ready'),parentOrigin);}})();"#,
        js_string(id),
        js_string(parent),
        js_string(entry)
    );
    format!("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Opening preview</title></head><body><script>{script}</script></body></html>")
}

/// Insert a parser-blocking external script before artifact scripts, without rewriting a byte
/// of the source. In particular, preceding charset/CSP metadata and the DOCTYPE stay in place.
/// Comments and quoted attributes are skipped so embedded markup cannot move the insertion.
fn instrument(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut insertion = source.len();
    while cursor < bytes.len() {
        let Some(offset) = source[cursor..].find('<') else {
            break;
        };
        let start = cursor + offset;
        if source[start..].starts_with("<!--") {
            let Some(end) = source[start + 4..].find("-->") else {
                break;
            };
            cursor = start + 4 + end + 3;
            continue;
        }
        let mut end = start + 1;
        let mut quote = 0;
        while end < bytes.len() {
            let byte = bytes[end];
            if quote != 0 {
                if byte == quote {
                    quote = 0;
                }
            } else if byte == b'\'' || byte == b'"' {
                quote = byte;
            } else if byte == b'>' {
                break;
            }
            end += 1;
        }
        if end == bytes.len() {
            break;
        }
        let text = &source[start + 1..end];
        let closing = text.starts_with('/');
        let text = text.strip_prefix('/').unwrap_or(text);
        let name_end = text
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(text.len());
        let tag = format!(
            "{}{}",
            if closing { "/" } else { "" },
            text[..name_end].to_ascii_lowercase()
        );
        if matches!(tag.as_str(), "script" | "/head" | "body") {
            insertion = start;
            break;
        }
        // Raw-text containers can contain tag-shaped text; conservatively leave these
        // unusual documents uninstrumented rather than change their parsing or CSP semantics.
        if matches!(
            tag.as_str(),
            "style"
                | "textarea"
                | "title"
                | "xmp"
                | "iframe"
                | "noembed"
                | "noframes"
                | "plaintext"
                | "noscript"
                | "template"
        ) {
            let closing = format!("</{tag}");
            let lower = source[end + 1..].to_ascii_lowercase();
            let Some(close) = lower.match_indices(&closing).find_map(|(at, _)| {
                lower
                    .as_bytes()
                    .get(at + closing.len())
                    .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
                    .then_some(at)
            }) else {
                return source.into();
            };
            let Some(tail) = lower[close..].find('>') else {
                return source.into();
            };
            cursor = end + 1 + close + tail + 1;
        } else {
            cursor = end + 1;
        }
    }
    if source[insertion..]
        .to_ascii_lowercase()
        .contains("content-security-policy")
    {
        return source.into();
    }
    format!(
        "{}<script src=\"/__conductor_preview/diagnostics.js\"></script>{}",
        &source[..insertion],
        &source[insertion..]
    )
}

const DIAGNOSTICS: &str = r#"(()=>{'use strict';
let context;try{context=JSON.parse(window.name)}catch(_){return}
if(!context||context.channel!=='conductor-artifact-preview'||typeof context.sessionId!=='string')return;
const sessionId=context.sessionId;let parentOrigin=null,queue=[],count=0;
const bound=(value,max=1024)=>String(value??'').slice(0,max);
const send=(type,data={})=>{if(count>=100)return;const m={channel:'conductor-artifact-preview',version:1,sessionId,type,...data};if(parentOrigin){count++;parent.postMessage(m,parentOrigin)}else if(queue.length<100)queue.push(m)};
const report=(kind,message,source,line,column)=>send('diagnostic',{kind,message:bound(message),source:bound(source,2048),...(Number.isInteger(line)&&line>=0&&line<=4294967295?{line}:{}),...(Number.isInteger(column)&&column>=0&&column<=4294967295?{column}:{})});
window.addEventListener('error',event=>{if(event.target&&event.target!==window){const el=event.target;report('resource','Resource failed to load',el.currentSrc||el.src||el.href||'')}else report('javascript',event.message,event.filename,event.lineno,event.colno)},true);
window.addEventListener('unhandledrejection',event=>report('promise',event.reason instanceof Error?event.reason.message:bound(event.reason)));
const originalFetch=window.fetch;window.fetch=function(...args){return originalFetch.apply(this,args).then(response=>{if(!response.ok)report('http','HTTP '+response.status,response.url);return response},error=>{report('http','Fetch failed');throw error})};
const sendXHR=XMLHttpRequest.prototype.send;
XMLHttpRequest.prototype.send=function(...args){this.addEventListener('loadend',()=>{if(this.status===0||this.status>=400)report('http','HTTP '+this.status,this.responseURL)},{once:true});return sendXHR.apply(this,args)};
originalFetch('/__conductor_preview/context?session='+encodeURIComponent(sessionId),{credentials:'same-origin'}).then(r=>{if(!r.ok)throw new Error();return r.json()}).then(c=>{if(c.sessionId!==sessionId||typeof c.parentOrigin!=='string')return;parentOrigin=c.parentOrigin;const pending=queue;queue=[];for(const m of pending)send(m.type,m);send('loaded');const failures=()=>originalFetch('/__conductor_preview/failures?session='+encodeURIComponent(sessionId),{credentials:'same-origin'}).then(r=>{if(!r.ok)throw new Error();return r.json()}).then(items=>{for(const item of items.slice(0,100))report('http',item.message,item.source);if(count<100)setTimeout(failures,2000)}).catch(()=>{});failures()}).catch(()=>{});
})();"#;
