//! Finds the pages Servo loaded but could not show.
//!
//! Servo renders HTML, XML, text, images and media inline. Anything else, a
//! PDF or an archive, it replaces with a one-line document saying the content
//! type is unknown, and it has no downloads at all, so the reader is left
//! with nothing to do. The embedder is never told: the replacement is an
//! ordinary load that completes.
//!
//! So every completed load is asked two questions in the page: the response's
//! content type, and the document's origin. Servo gives the replacement an
//! opaque origin, which an HTTP or file document only otherwise has when its
//! server sandboxed it, and a sandboxed document is still of a type Servo
//! renders. Both answers together single out the replacement, and the content
//! type is classified with Servo's own [`MimeClassifier`], so what counts as
//! renderable here is exactly what Servo renders.
//!
//! A page can only lie about itself to its own disadvantage: the answer
//! decides whether the application offers to fetch the page's own URL again
//! as a file, and the fetched bytes are checked before a viewer opens them.

use std::{cell::RefCell, fmt};

use net_traits::mime_classifier::{MediaType, MimeClassifier};
use url::Url;

/// What a completed load's probe found, before the cookies are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unrenderable {
    pub url: Url,
    pub content_type: String,
}

/// Where the probe's asynchronous answer waits for the engine to report it.
pub(crate) type Slot = RefCell<Option<Unrenderable>>;

/// The script the probe evaluates: the response's type and the document's
/// origin, which is the string `"null"` for an opaque one.
const PROBE: &str = "[document.contentType, self.origin]";

/// Asks the page just loaded in `webview` whether Servo could render it.
pub(crate) fn probe(webview: &servo::WebView, slot: &std::rc::Rc<Slot>) {
    let Some(url) = webview.url() else {
        return;
    };
    if !matches!(url.scheme(), "http" | "https" | "file") {
        return;
    }
    let slot = slot.clone();
    let probed = webview.clone();
    webview.evaluate_javascript(PROBE, move |result| {
        let Ok(servo::JSValue::Array(values)) = result else {
            return;
        };
        let [
            servo::JSValue::String(content_type),
            servo::JSValue::String(origin),
        ] = values.as_slice()
        else {
            return;
        };
        // The reader may have moved on while the answer was on its way, and
        // a document they have left is not one to offer.
        if probed.url().as_ref() != Some(&url) {
            return;
        }
        if is_replacement(content_type, origin) {
            *slot.borrow_mut() = Some(Unrenderable {
                url,
                content_type: content_type.clone(),
            });
        }
    });
}

/// Whether a document with this content type and origin is Servo's stand-in
/// for a response it could not render.
fn is_replacement(content_type: &str, origin: &str) -> bool {
    origin == "null" && !renderable(content_type)
}

/// Whether Servo renders a response of `content_type` inline.
///
/// Servo also replaces a font, which it recognizes but does not display.
fn renderable(content_type: &str) -> bool {
    let Ok(mime) = content_type.parse::<mime::Mime>() else {
        return false;
    };
    !matches!(
        MimeClassifier::get_media_type(&mime),
        None | Some(MediaType::Font)
    )
}

impl Unrenderable {
    /// Completes the finding with what a fetch of the same URL needs to be
    /// treated as the browser's own: its cookies and user agent.
    pub(crate) fn into_document(self, servo: &servo::Servo) -> UnrenderableDocument {
        let cookie = (self.url.scheme() != "file")
            .then(|| {
                servo
                    .site_data_manager()
                    .cookies_for_url(self.url.clone(), net_traits::CookieSource::HTTP)
            })
            .map(|cookies| {
                cookies
                    .iter()
                    .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .filter(|header| !header.is_empty());
        UnrenderableDocument {
            url: self.url,
            content_type: self.content_type,
            fetch: FetchRequest {
                cookie,
                user_agent: servo::pref!(user_agent),
            },
        }
    }
}

/// A page the browser loaded and could not show.
#[derive(Debug, Clone)]
pub struct UnrenderableDocument {
    pub url: Url,
    /// The response's type as its server sent it, such as `application/pdf`.
    pub content_type: String,
    pub fetch: FetchRequest,
}

impl UnrenderableDocument {
    /// Whether this is a PDF, by the type its server gave it. The bytes are
    /// checked again once fetched.
    pub fn is_pdf(&self) -> bool {
        is_pdf_type(&self.content_type)
    }
}

pub fn is_pdf_type(content_type: &str) -> bool {
    content_type.parse::<mime::Mime>().is_ok_and(|mime| {
        mime.essence_str() == "application/pdf" || mime.essence_str() == "application/x-pdf"
    })
}

/// What a fetch of a page needs to be made as the browser made it.
#[derive(Clone)]
pub struct FetchRequest {
    /// The `Cookie` header Servo would have sent, if any.
    pub cookie: Option<String>,
    pub user_agent: String,
}

/// Cookies are credentials, so a log line never carries them.
impl fmt::Debug for FetchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FetchRequest")
            .field("cookie", &self.cookie.as_ref().map(|_| "<redacted>"))
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_opaque_document_of_a_type_servo_cannot_show_is_a_replacement() {
        assert!(is_replacement("application/pdf", "null"));
        assert!(is_replacement("application/zip", "null"));
        assert!(is_replacement("application/octet-stream", "null"));
        assert!(
            is_replacement("font/woff2", "null"),
            "Servo replaces fonts too"
        );
        // A sandboxed page is opaque and still rendered.
        assert!(!is_replacement("text/plain", "null"));
        assert!(!is_replacement("text/html; charset=utf-8", "null"));
        assert!(!is_replacement("image/png", "null"));
        assert!(!is_replacement("application/json", "null"));
        // A page with a real origin was rendered, whatever it says.
        assert!(!is_replacement("application/pdf", "https://arxiv.org"));
    }

    #[test]
    fn a_pdf_is_known_by_its_type_with_or_without_parameters() {
        assert!(is_pdf_type("application/pdf"));
        assert!(is_pdf_type("application/pdf; qs=0.001"));
        assert!(is_pdf_type("Application/PDF"));
        assert!(is_pdf_type("application/x-pdf"));
        assert!(!is_pdf_type("application/zip"));
        assert!(!is_pdf_type("not a type"));
    }

    #[test]
    fn a_fetch_request_never_prints_its_cookies() {
        let request = FetchRequest {
            cookie: Some("session=secret".into()),
            user_agent: "Servo".into(),
        };
        let printed = format!("{request:?}");
        assert!(!printed.contains("secret"), "{printed}");
    }
}
