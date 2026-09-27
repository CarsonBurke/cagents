use cagents_browser_import::{Cookie, SameSite};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub cookies: usize,
    pub sites: usize,
    pub skipped: usize,
    pub cancelled: bool,
}

pub struct ImportRequest {
    pub cookies: Vec<Cookie>,
    pub cancel: Arc<AtomicBool>,
    pub result: crossbeam_channel::Sender<Result<ImportReport, String>>,
}

impl std::fmt::Debug for ImportRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportRequest")
            .field("count", &self.cookies.len())
            .finish_non_exhaustive()
    }
}

pub(super) struct Pending {
    cookies: VecDeque<Cookie>,
    cancel: Arc<AtomicBool>,
    result: crossbeam_channel::Sender<Result<ImportReport, String>>,
    report: ImportReport,
    sites: BTreeSet<String>,
}

impl From<ImportRequest> for Pending {
    fn from(request: ImportRequest) -> Self {
        Self {
            cookies: request.cookies.into(),
            cancel: request.cancel,
            result: request.result,
            report: ImportReport::default(),
            sites: BTreeSet::new(),
        }
    }
}

impl Pending {
    /// Bounded work per engine turn: input, close, and Servo wakeups continue
    /// while a large selected profile is installed in the shared cookie jar.
    pub fn step(&mut self, servo: &servo::Servo) -> bool {
        for _ in 0..32 {
            if self.cancel.load(Ordering::Relaxed) {
                self.report.cancelled = true;
                self.cookies.clear();
                break;
            }
            let Some(imported) = self.cookies.pop_front() else {
                break;
            };
            let scheme = if imported.secure { "https" } else { "http" };
            let Ok(mut origin) = url::Url::parse(&format!("{scheme}://{}/", imported.host)) else {
                self.report.skipped += 1;
                continue;
            };
            origin.set_path(&imported.path);
            let mut cookie = cookie::Cookie::new(imported.name.clone(), imported.value.to_string());
            cookie.set_path(imported.path.clone());
            if imported.domain {
                cookie.set_domain(imported.host.clone());
            }
            cookie.set_secure(imported.secure);
            cookie.set_http_only(imported.http_only);
            match imported.same_site {
                SameSite::None => cookie.set_same_site(cookie::SameSite::None),
                SameSite::Lax => cookie.set_same_site(cookie::SameSite::Lax),
                SameSite::Strict => cookie.set_same_site(cookie::SameSite::Strict),
                SameSite::Unspecified => {}
            }
            if let Some(expires) = imported.expires_unix {
                match cookie::time::OffsetDateTime::from_unix_timestamp(expires) {
                    Ok(expires) => cookie.set_expires(expires),
                    Err(_) => {
                        self.report.skipped += 1;
                        continue;
                    }
                }
            }
            let manager = servo.site_data_manager();
            manager.set_cookie_for_url(origin.clone(), cookie);
            // Servo applies prefix and public-suffix policy. Count
            // only accepted cookies, not merely messages sent to its jar.
            if manager
                .cookies_for_url(origin, net_traits::CookieSource::HTTP)
                .iter()
                .any(|cookie| {
                    cookie.name() == imported.name
                        && cookie.value() == imported.value.as_str()
                        && cookie.path() == Some(imported.path.as_str())
                })
            {
                self.sites.insert(imported.host);
                self.report.cookies += 1;
            } else {
                self.report.skipped += 1;
            }
        }
        if self.cookies.is_empty() {
            self.report.sites = self.sites.len();
            let _ = self.result.send(Ok(self.report.clone()));
            true
        } else {
            false
        }
    }
}
