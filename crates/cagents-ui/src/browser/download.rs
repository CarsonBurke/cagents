//! Fetches a page Servo could not render as a file.
//!
//! Servo has no downloads, and the document it shows in place of such a page
//! has an opaque origin, so not even the page itself can read the response
//! again. The application fetches the URL once more instead, through Servo's
//! own HTTP client: its TLS verification, proxy settings and header casing,
//! with the cookies and user agent Servo sent. The response goes to a fresh
//! directory under the application's cache, named as the server or the URL
//! names it, so a viewer or another application shows a recognizable name.

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use cosmic::iced::futures::{SinkExt, Stream, channel::mpsc::Sender};
use http_body_util::{BodyExt, Empty};
use hyper::{
    StatusCode,
    body::Bytes,
    header::{self, HeaderMap},
};
use tokio::io::AsyncWriteExt;
use url::Url;

use super::content::{UnrenderableDocument, is_pdf_type};

/// A server may redirect this many times before the fetch stops following.
const MAX_REDIRECTS: usize = 10;

/// How often a running fetch reports how far it has got.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Cached fetches older than this are removed when the next one starts.
const CACHE_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest file name a fetch writes, in bytes; most file systems allow 255.
const MAX_FILE_NAME: usize = 200;

/// How far into a file a PDF's signature may start.
const PDF_SIGNATURE_WINDOW: usize = 1024;

/// How a fetch is getting on.
#[derive(Debug, Clone)]
pub enum FetchProgress {
    /// `received` bytes so far, of `total` when the server said.
    Receiving {
        received: u64,
        total: Option<u64>,
    },
    Finished(Result<Fetched, String>),
}

/// A page fetched as a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub path: PathBuf,
    pub bytes: u64,
    /// Whether the file starts like a PDF, whatever its server called it.
    pub is_pdf: bool,
    /// Whether `path` is the reader's own file, as for a `file:` page, rather
    /// than a copy the application made and may remove.
    pub original: bool,
}

impl Fetched {
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Fetches `document` into a new directory under `cache`, reporting progress.
///
/// Dropping the stream stops the fetch; the partial file is left for the
/// next fetch's cleanup.
pub fn fetch_document(
    document: UnrenderableDocument,
    cache: PathBuf,
) -> impl Stream<Item = FetchProgress> {
    cosmic::iced::stream::channel(4, async move |mut output| {
        let result = fetch(&document, &cache, &mut output).await;
        let _ = output.send(FetchProgress::Finished(result)).await;
    })
}

async fn fetch(
    document: &UnrenderableDocument,
    cache: &Path,
    output: &mut Sender<FetchProgress>,
) -> Result<Fetched, String> {
    if document.url.scheme() == "file" {
        return local_file(&document.url).await;
    }
    let pruned = cache.to_owned();
    let _ = tokio::task::spawn_blocking(move || prune(&pruned, SystemTime::now())).await;

    let client = http_client()?;
    let mut url = document.url.clone();
    let mut cookie = document.fetch.cookie.clone();
    let mut redirects = 0;
    let response = loop {
        let mut request = hyper::Request::get(url.as_str())
            .header(header::USER_AGENT, &document.fetch.user_agent)
            .header(header::ACCEPT, "*/*")
            // The bytes are saved as they are, so they must come as they are.
            .header(header::ACCEPT_ENCODING, "identity");
        if let Some(cookie) = &cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let request = request
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed(),
            )
            .map_err(|_| "This address can't be fetched.".to_owned())?;
        let response = client
            .request(request)
            .await
            .map_err(|error| format!("The connection failed: {}", root_cause(&error)))?;
        if !response.status().is_redirection() {
            break response;
        }
        let Some(next) = redirect_target(&url, response.headers()) else {
            break response;
        };
        redirects += 1;
        if redirects > MAX_REDIRECTS {
            return Err("The server redirected too many times.".into());
        }
        // Cookies were read for the page's own site, so they go no further.
        if next.origin() != url.origin() {
            cookie = None;
        }
        url = next;
    };
    let status = response.status();
    if !status.is_success() {
        return Err(status_message(status));
    }
    let headers = response.headers();
    if headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|encoding| !encoding.trim().eq_ignore_ascii_case("identity"))
    {
        return Err("The server sent the file compressed, and it can't be saved as sent.".into());
    }
    let total = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(&document.content_type)
        .to_owned();
    let name = file_name(
        headers
            .get(header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok()),
        &url,
        is_pdf_type(&content_type),
    );

    let directory = cache.join(uuid::Uuid::new_v4().simple().to_string());
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| write_failure(&error))?;
    let partial = directory.join(".partial");
    let mut file = tokio::io::BufWriter::new(
        tokio::fs::File::create(&partial)
            .await
            .map_err(|error| write_failure(&error))?,
    );
    let mut body = response.into_body();
    let mut received = 0u64;
    let mut head = Vec::with_capacity(PDF_SIGNATURE_WINDOW);
    let mut reported = Instant::now();
    let _ = output
        .send(FetchProgress::Receiving { received, total })
        .await;
    while let Some(frame) = body.frame().await {
        let frame =
            frame.map_err(|error| format!("The download stopped: {}", root_cause(&error)))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if head.len() < PDF_SIGNATURE_WINDOW {
            let wanted = (PDF_SIGNATURE_WINDOW - head.len()).min(data.len());
            head.extend_from_slice(&data[..wanted]);
        }
        file.write_all(&data)
            .await
            .map_err(|error| write_failure(&error))?;
        received += data.len() as u64;
        if reported.elapsed() >= PROGRESS_INTERVAL {
            reported = Instant::now();
            let _ = output
                .send(FetchProgress::Receiving { received, total })
                .await;
        }
    }
    if total.is_some_and(|total| received < total) {
        return Err("The download stopped before the whole file arrived.".into());
    }
    file.flush().await.map_err(|error| write_failure(&error))?;
    drop(file);
    let path = directory.join(&name);
    tokio::fs::rename(&partial, &path)
        .await
        .map_err(|error| write_failure(&error))?;
    Ok(Fetched {
        path,
        bytes: received,
        is_pdf: is_pdf(&head),
        original: false,
    })
}

/// A client that connects as Servo's does: HTTP/1.1 or 2 over rustls, with
/// the platform's certificate verifier and the same cryptography provider.
fn http_client() -> Result<
    hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>,
    >,
    String,
> {
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
        .map_err(|error| format!("Secure connections aren't available: {error}"))?;
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("Secure connections aren't available: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(verifier))
        .with_no_client_auth();
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Ok(
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(connector),
    )
}

/// A `file:` page is already a file, so it is used where it is.
async fn local_file(url: &Url) -> Result<Fetched, String> {
    let path = url
        .to_file_path()
        .map_err(|()| "This file's location can't be read.".to_owned())?;
    let missing = |_| "The file is no longer there.".to_owned();
    let metadata = tokio::fs::metadata(&path).await.map_err(missing)?;
    let mut head = vec![0; PDF_SIGNATURE_WINDOW];
    let read = {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&path).await.map_err(missing)?;
        file.read(&mut head).await.map_err(missing)?
    };
    head.truncate(read);
    Ok(Fetched {
        path,
        bytes: metadata.len(),
        is_pdf: is_pdf(&head),
        original: true,
    })
}

/// Moves or copies a fetched file into `directory` under its own name, or the
/// first free variant of it, and returns where it went.
///
/// A copy the application made is moved, so that saving a large file does
/// not need twice its space, unless `keep` says something still reads it
/// where it is; the reader's own file is always copied.
pub fn save_into(fetched: &Fetched, directory: &Path, keep: bool) -> io::Result<PathBuf> {
    std::fs::create_dir_all(directory)?;
    let name = fetched.file_name();
    let (stem, extension) = split_extension(&name);
    for attempt in 0..1000 {
        let candidate = directory.join(if attempt == 0 {
            name.clone()
        } else {
            format!("{stem} ({attempt}){extension}")
        });
        if candidate.exists() {
            continue;
        }
        if !fetched.original && !keep && std::fs::rename(&fetched.path, &candidate).is_ok() {
            if let Some(parent) = fetched.path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
            return Ok(candidate);
        }
        // Another file system, or the reader's own file: copy without
        // replacing anything that appeared meanwhile.
        let mut source = std::fs::File::open(&fetched.path)?;
        let mut target = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = io::copy(&mut source, &mut target) {
            drop(target);
            let _ = std::fs::remove_file(&candidate);
            return Err(error);
        }
        return Ok(candidate);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "every name for this file is taken",
    ))
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

/// Removes cached fetches older than [`CACHE_LIFETIME`].
fn prune(cache: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > CACHE_LIFETIME);
        if old && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn redirect_target(from: &Url, headers: &HeaderMap) -> Option<Url> {
    let location = headers.get(header::LOCATION)?.to_str().ok()?;
    let next = from.join(location).ok()?;
    matches!(next.scheme(), "http" | "https").then_some(next)
}

fn status_message(status: StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("The server answered {} {reason}.", status.as_u16()),
        None => format!("The server answered {}.", status.as_u16()),
    }
}

fn write_failure(error: &io::Error) -> String {
    format!("The file couldn't be saved: {error}")
}

/// The innermost error, which says what actually went wrong; hyper's own
/// messages only say which layer noticed.
fn root_cause(error: &(dyn std::error::Error + 'static)) -> String {
    let mut cause = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

/// Whether `head`, the start of a file, is the start of a PDF.
fn is_pdf(head: &[u8]) -> bool {
    head.windows(5).any(|window| window == b"%PDF-")
}

/// The name a fetched file is saved under: the server's, the URL's last path
/// segment, or the host, made safe for a file system.
fn file_name(disposition: Option<&str>, url: &Url, pdf: bool) -> String {
    let segment = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(percent_decode);
    let mut name = [
        disposition.and_then(disposition_file_name),
        segment,
        url.host_str().map(str::to_owned),
    ]
    .into_iter()
    .flatten()
    .map(|name| sanitize(&name))
    .find(|name| !name.is_empty())
    .unwrap_or_else(|| "download".to_owned());
    if pdf && !name.to_ascii_lowercase().ends_with(".pdf") {
        name.push_str(".pdf");
    }
    name
}

/// The `filename*` or `filename` parameter of a `Content-Disposition` header.
fn disposition_file_name(disposition: &str) -> Option<String> {
    let mut plain = None;
    for parameter in disposition.split(';').skip(1) {
        let Some((key, value)) = parameter.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "filename*" {
            // RFC 8187: charset'language'percent-encoded
            let mut parts = value.splitn(3, '\'');
            let charset = parts.next()?;
            let _language = parts.next()?;
            let encoded = parts.next()?;
            if charset.eq_ignore_ascii_case("utf-8") {
                return Some(percent_decode(encoded));
            }
        } else if key == "filename" {
            plain = Some(value.trim_matches('"').to_owned());
        }
    }
    plain
}

/// Decodes `%XX` escapes, keeping any that are malformed as written.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Keeps a name to one path component a file system and a file manager both
/// show plainly: no separators, control characters or leading dots, and no
/// more than [`MAX_FILE_NAME`] bytes.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    let mut end = trimmed.len().min(MAX_FILE_NAME);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn a_pdf_is_named_after_its_url_and_given_its_extension() {
        assert_eq!(
            file_name(None, &url("https://arxiv.org/pdf/2605.30154"), true),
            "2605.30154.pdf"
        );
        assert_eq!(
            file_name(None, &url("https://example.test/papers/Report.PDF"), true),
            "Report.PDF"
        );
        assert_eq!(
            file_name(None, &url("https://example.test/files/archive.zip"), false),
            "archive.zip"
        );
        assert_eq!(
            file_name(None, &url("https://example.test/a/b/"), false),
            "b",
            "a trailing slash names the last segment"
        );
        assert_eq!(
            file_name(None, &url("https://example.test/"), false),
            "example.test"
        );
        assert_eq!(
            file_name(None, &url("https://example.test/My%20Paper.pdf"), true),
            "My Paper.pdf"
        );
    }

    #[test]
    fn the_server_s_name_wins_and_is_made_safe() {
        let page = url("https://example.test/download?id=7");
        assert_eq!(
            file_name(Some("attachment; filename=\"notes.txt\""), &page, false),
            "notes.txt"
        );
        assert_eq!(
            file_name(
                Some(
                    "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"
                ),
                &page,
                true
            ),
            "résumé.pdf"
        );
        assert_eq!(
            file_name(Some("attachment; filename=\"../../.bashrc\""), &page, false),
            "_.._.bashrc",
            "no separator or leading dot survives"
        );
        assert_eq!(
            file_name(Some("attachment; filename=\"...\""), &page, false),
            "download",
            "a name with nothing left falls back to the URL"
        );
    }

    #[test]
    fn a_long_name_is_cut_on_a_character_boundary() {
        let long = "é".repeat(MAX_FILE_NAME);
        let name = sanitize(&long);
        assert!(name.len() <= MAX_FILE_NAME);
        assert!(name.chars().all(|character| character == 'é'));
    }

    #[test]
    fn a_pdf_is_known_by_its_signature_near_the_start() {
        assert!(is_pdf(b"%PDF-1.7\n"));
        assert!(
            is_pdf(b"\xef\xbb\xbf%PDF-1.4"),
            "a prefix before the signature is allowed"
        );
        assert!(!is_pdf(b"<html>"));
        assert!(!is_pdf(b""));
    }

    #[test]
    fn a_redirect_resolves_against_the_page_and_stays_on_the_web() {
        let mut headers = HeaderMap::new();
        headers.insert(header::LOCATION, "/pdf/2605.30154v2".parse().unwrap());
        assert_eq!(
            redirect_target(&url("https://arxiv.org/pdf/2605.30154"), &headers),
            Some(url("https://arxiv.org/pdf/2605.30154v2"))
        );
        headers.insert(header::LOCATION, "file:///etc/passwd".parse().unwrap());
        assert_eq!(
            redirect_target(&url("https://arxiv.org/pdf/1"), &headers),
            None
        );
    }

    #[test]
    fn saving_takes_a_free_name_and_moves_the_application_s_copy() {
        let root = tempfile::tempdir().unwrap();
        let cached = root.path().join("cache/abc");
        std::fs::create_dir_all(&cached).unwrap();
        let downloads = root.path().join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::write(downloads.join("paper.pdf"), b"older").unwrap();
        let fetched = Fetched {
            path: cached.join("paper.pdf"),
            bytes: 5,
            is_pdf: true,
            original: false,
        };
        std::fs::write(&fetched.path, b"%PDF-").unwrap();

        let saved = save_into(&fetched, &downloads, false).unwrap();

        assert_eq!(saved, downloads.join("paper (1).pdf"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"%PDF-");
        assert_eq!(
            std::fs::read(downloads.join("paper.pdf")).unwrap(),
            b"older"
        );
        assert!(!cached.exists(), "the emptied cache directory goes too");
    }

    #[test]
    fn saving_the_reader_s_own_file_copies_it() {
        let root = tempfile::tempdir().unwrap();
        let own = root.path().join("own.pdf");
        std::fs::write(&own, b"%PDF-").unwrap();
        let fetched = Fetched {
            path: own.clone(),
            bytes: 5,
            is_pdf: true,
            original: true,
        };
        let saved = save_into(&fetched, &root.path().join("Downloads"), false).unwrap();
        assert!(own.exists());
        assert_eq!(std::fs::read(saved).unwrap(), b"%PDF-");
    }

    /// Serves `responses` in order, one per connection, and returns each
    /// request's head as it arrived.
    async fn serve(responses: Vec<String>) -> (Url, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let served = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                let mut buffer = [0; 1024];
                while !head.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert!(read > 0, "the client hung up mid-request");
                    head.extend_from_slice(&buffer[..read]);
                }
                requests.push(String::from_utf8(head).unwrap());
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
            requests
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), served)
    }

    fn document(url: Url, cookie: Option<&str>) -> UnrenderableDocument {
        UnrenderableDocument {
            url,
            content_type: "application/pdf".into(),
            fetch: super::super::FetchRequest {
                cookie: cookie.map(str::to_owned),
                user_agent: "TestAgent/1".into(),
            },
        }
    }

    async fn finish(stream: impl Stream<Item = FetchProgress>) -> Result<Fetched, String> {
        use cosmic::iced::futures::StreamExt;
        let mut stream = std::pin::pin!(stream);
        while let Some(progress) = stream.next().await {
            if let FetchProgress::Finished(result) = progress {
                return result;
            }
        }
        panic!("the fetch ended without finishing");
    }

    #[tokio::test]
    async fn a_pdf_is_fetched_through_a_redirect_as_the_browser_asked_for_it() {
        let body = "%PDF-1.4\n%%EOF\n";
        let (base, served) = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: /files/paper\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        ])
        .await;
        let cache = tempfile::tempdir().unwrap();

        let fetched = finish(fetch_document(
            document(base.join("pdf/paper").unwrap(), Some("session=1")),
            cache.path().to_owned(),
        ))
        .await
        .unwrap();

        assert_eq!(fetched.file_name(), "paper.pdf");
        assert!(fetched.is_pdf);
        assert!(!fetched.original);
        assert!(fetched.path.starts_with(cache.path()));
        assert_eq!(std::fs::read_to_string(&fetched.path).unwrap(), body);
        let requests = served.await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            let request = request.to_ascii_lowercase();
            assert!(request.contains("user-agent: testagent/1"), "{request}");
            assert!(
                request.contains("cookie: session=1"),
                "same-site keeps cookies: {request}"
            );
            assert!(request.contains("accept-encoding: identity"), "{request}");
        }
    }

    #[tokio::test]
    async fn cookies_stay_with_the_site_they_were_read_for() {
        let (elsewhere, elsewhere_served) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: 2\r\nConnection: close\r\n\r\nPK".into(),
        ])
        .await;
        let (base, served) = serve(vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            elsewhere.join("archive.zip").unwrap()
        )])
        .await;
        let cache = tempfile::tempdir().unwrap();

        let fetched = finish(fetch_document(
            document(base.join("get").unwrap(), Some("session=1")),
            cache.path().to_owned(),
        ))
        .await
        .unwrap();

        assert_eq!(fetched.file_name(), "archive.zip");
        assert!(!fetched.is_pdf);
        assert!(served.await.unwrap()[0].contains("session=1"));
        let followed = elsewhere_served.await.unwrap();
        assert!(
            !followed[0].to_ascii_lowercase().contains("cookie:"),
            "{}",
            followed[0]
        );
    }

    #[tokio::test]
    async fn a_refused_fetch_says_what_the_server_answered() {
        let (base, served) = serve(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ])
        .await;
        let cache = tempfile::tempdir().unwrap();
        let result = finish(fetch_document(
            document(base.join("missing.pdf").unwrap(), None),
            cache.path().to_owned(),
        ))
        .await;
        assert_eq!(result, Err("The server answered 404 Not Found.".to_owned()));
        let requests = served.await.unwrap();
        assert!(!requests[0].to_ascii_lowercase().contains("cookie:"));
    }

    #[test]
    fn pruning_removes_only_old_fetches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("recent")).unwrap();
        prune(root.path(), SystemTime::now() + CACHE_LIFETIME / 2);
        assert!(root.path().join("recent").exists());
        prune(root.path(), SystemTime::now() + CACHE_LIFETIME * 2);
        assert!(!root.path().join("recent").exists());
    }
}
