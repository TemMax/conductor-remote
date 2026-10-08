//! Sends one push request through curl.
//!
//! The request is described to curl by three private files (headers, body, response) so that
//! neither the `Authorization` value nor the encrypted body ever appears in an argument list.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::reads::extras::commands::{Commands, Limits};

const CURL: &str = "/usr/bin/curl";
const FILE_PREFIX: &str = "push-";
/// How many characters of the push service's answer go into an error.
const ERROR_TEXT_CHARS: usize = 200;
/// How much of the response file is read at most.
const RESPONSE_READ_LIMIT: u64 = 64 * 1024;

/// One push: the endpoint, the `Authorization` value, the encrypted body and its TTL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushRequest {
    pub endpoint: String,
    pub authorization: String,
    pub body: Vec<u8>,
    pub ttl_secs: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushResult {
    pub ok: bool,
    /// 0 when no HTTP answer came back.
    pub status: u16,
    pub error: Option<String>,
    /// 404 or 410: the subscription is gone.
    pub gone: bool,
}

impl PushResult {
    fn failed(status: u16, error: String) -> Self {
        Self {
            ok: false,
            status,
            error: Some(error),
            gone: matches!(status, 404 | 410),
        }
    }
}

/// Blocking: call it from a blocking thread.
pub trait PushSender: Send + Sync + 'static {
    fn send(&self, request: &PushRequest) -> PushResult;
}

/// Pushes through the system's `curl`.
pub struct CurlSender {
    commands: Arc<dyn Commands>,
    tmp_dir: PathBuf,
}

impl CurlSender {
    /// `tmp_dir` is created (0700) on first use. Files a crash left there are removed now.
    pub fn new(commands: Arc<dyn Commands>, tmp_dir: PathBuf) -> CurlSender {
        remove_leftovers(&tmp_dir);
        CurlSender { commands, tmp_dir }
    }

    /// Creates the three files; the guard removes whichever exist when it is dropped.
    fn prepare(&self, request: &PushRequest) -> std::io::Result<(TempFiles, [String; 3])> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.tmp_dir)?;
        let mut id = [0u8; 8];
        getrandom::fill(&mut id).map_err(|e| std::io::Error::other(e.to_string()))?;
        let id: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = |extension: &str| self.tmp_dir.join(format!("{FILE_PREFIX}{id}.{extension}"));

        let mut guard = TempFiles(Vec::new());
        let headers = path("headers");
        let mut file = create_private(&headers)?;
        guard.0.push(headers.clone());
        write!(
            file,
            "authorization: {}\ncontent-encoding: aes128gcm\ncontent-type: application/octet-stream\nttl: {}\nurgency: normal\n",
            request.authorization, request.ttl_secs
        )?;

        let body = path("body");
        let mut file = create_private(&body)?;
        guard.0.push(body.clone());
        file.write_all(&request.body)?;

        let response = path("response");
        create_private(&response)?;
        guard.0.push(response.clone());

        let text = |p: &Path| p.to_string_lossy().into_owned();
        Ok((guard, [text(&headers), text(&body), text(&response)]))
    }
}

impl PushSender for CurlSender {
    fn send(&self, request: &PushRequest) -> PushResult {
        if !is_https(&request.endpoint) {
            return PushResult::failed(0, "the push endpoint is not an https URL".to_owned());
        }
        let Ok((_guard, [headers, body, response])) = self.prepare(request) else {
            return PushResult::failed(0, "could not prepare the push request".to_owned());
        };
        let headers_arg = format!("@{headers}");
        let body_arg = format!("@{body}");
        let args = [
            "-sS",
            "--proto",
            "=https",
            "--max-time",
            "10",
            "-X",
            "POST",
            "-H",
            &headers_arg,
            "--data-binary",
            &body_arg,
            "-o",
            &response,
            "-w",
            "%{http_code}",
            "--",
            &request.endpoint,
        ];
        let limits = Limits {
            timeout: Duration::from_secs(15),
            max_stdout: 64,
        };
        let output = match self.commands.run(CURL, &args, None, limits) {
            Ok(output) => output,
            Err(error) => return PushResult::failed(0, error.to_string()),
        };

        let status = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u16>()
            .unwrap_or(0);
        match status {
            200..=299 => PushResult {
                ok: true,
                status,
                error: None,
                gone: false,
            },
            0 => {
                let code = output
                    .code
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string());
                PushResult::failed(
                    0,
                    format!("could not reach the push service (curl exit {code})"),
                )
            }
            _ => {
                let text = read_response_text(Path::new(&response));
                let error = if text.is_empty() {
                    format!("HTTP {status}")
                } else {
                    format!("HTTP {status}: {text}")
                };
                PushResult::failed(status, error)
            }
        }
    }
}

fn is_https(endpoint: &str) -> bool {
    endpoint
        .as_bytes()
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
}

fn create_private(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// The first characters of the response file, trimmed; empty when it cannot be read.
fn read_response_text(path: &Path) -> String {
    let mut bytes = Vec::new();
    if File::open(path)
        .and_then(|file| file.take(RESPONSE_READ_LIMIT).read_to_end(&mut bytes))
        .is_err()
    {
        return String::new();
    }
    String::from_utf8_lossy(&bytes)
        .trim()
        .chars()
        .take(ERROR_TEXT_CHARS)
        .collect()
}

/// Removes `push-*` files a crash left in `dir`. A missing directory is fine.
fn remove_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_push_file = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(FILE_PREFIX));
        if is_push_file && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Removes the files it holds when dropped, whichever way `send` returns.
struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}
