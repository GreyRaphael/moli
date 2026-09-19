use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moli_fetch::RequestCredentialsMode;
use parking_lot::Mutex;
use tokio::sync::watch;
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PreflightCacheKey {
    pub origin: String,
    pub target_origin: String,
    pub target_path: String,
    pub credentials_mode: RequestCredentialsMode,
}

impl PreflightCacheKey {
    pub fn new(origin: &str, url: &Url, credentials_mode: RequestCredentialsMode) -> Self {
        Self {
            origin: origin.to_owned(),
            target_origin: url.origin().ascii_serialization(),
            target_path: url.path().to_owned(),
            credentials_mode,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CorsPreflightEntry {
    pub expires_at: Instant,
    pub allow_methods: HashSet<String>,
    pub allow_all_methods: bool,
    pub allow_headers: HashSet<String>,
    pub allow_all_headers: bool,
}

impl CorsPreflightEntry {
    pub fn allows_method(&self, method: &str) -> bool {
        if moli_fetch::is_cors_safelisted_method(method) {
            return true;
        }
        if self.allow_all_methods {
            return true;
        }
        self.allow_methods.contains(&method.to_ascii_uppercase())
    }

    pub fn allows_headers(&self, unsafe_headers: &[String]) -> bool {
        if unsafe_headers.is_empty() {
            return true;
        }
        if self.allow_all_headers {
            return true;
        }
        unsafe_headers
            .iter()
            .all(|h| self.allow_headers.contains(&h.to_ascii_lowercase()))
    }
}

#[derive(Default)]
pub(crate) struct CorsPreflightCache {
    entries: Mutex<HashMap<PreflightCacheKey, CorsPreflightEntry>>,
    in_flight: Mutex<HashMap<PreflightCacheKey, (Arc<watch::Sender<bool>>, watch::Receiver<bool>)>>,
}

pub(crate) enum InFlightAction {
    Execute,
    Wait(watch::Receiver<bool>),
}

impl CorsPreflightCache {
    pub fn check(
        &self,
        origin: &str,
        url: &Url,
        credentials_mode: RequestCredentialsMode,
        method: &str,
        request_headers: &[(String, String)],
    ) -> bool {
        let key = PreflightCacheKey::new(origin, url, credentials_mode);
        let now = Instant::now();
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.get(&key) {
            if now < entry.expires_at {
                let unsafe_headers = moli_fetch::cors_unsafe_request_header_names(request_headers);
                if entry.allows_method(method) && entry.allows_headers(&unsafe_headers) {
                    return true;
                }
            } else {
                entries.remove(&key);
            }
        }
        false
    }

    pub fn register_or_wait(
        &self,
        origin: &str,
        url: &Url,
        credentials_mode: RequestCredentialsMode,
    ) -> InFlightAction {
        let key = PreflightCacheKey::new(origin, url, credentials_mode);
        let mut in_flight = self.in_flight.lock();
        if let Some((_, rx)) = in_flight.get(&key) {
            InFlightAction::Wait(rx.clone())
        } else {
            let (tx, rx) = watch::channel(false);
            in_flight.insert(key, (Arc::new(tx), rx));
            InFlightAction::Execute
        }
    }

    pub fn finish_in_flight(
        &self,
        origin: &str,
        url: &Url,
        credentials_mode: RequestCredentialsMode,
    ) {
        let key = PreflightCacheKey::new(origin, url, credentials_mode);
        if let Some((tx, _)) = self.in_flight.lock().remove(&key) {
            let _ = tx.send(true);
        }
    }

    pub fn record(
        &self,
        origin: &str,
        url: &Url,
        credentials_mode: RequestCredentialsMode,
        response_headers: &[(String, String)],
    ) {
        let key = PreflightCacheKey::new(origin, url, credentials_mode);

        // Parse Access-Control-Max-Age
        let max_age_secs =
            moli_web_mime::response_header_value(response_headers, "access-control-max-age")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(5);
        let max_age = max_age_secs.clamp(1, 7200);

        // Parse Access-Control-Allow-Methods
        let allow_methods_val =
            moli_web_mime::response_header_value(response_headers, "access-control-allow-methods")
                .unwrap_or_default();
        let allow_all_methods =
            allow_methods_val.trim() == "*" && credentials_mode != RequestCredentialsMode::Include;
        let allow_methods: HashSet<String> = allow_methods_val
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect();

        // Parse Access-Control-Allow-Headers
        let allow_headers_val =
            moli_web_mime::response_header_value(response_headers, "access-control-allow-headers")
                .unwrap_or_default();
        let allow_all_headers =
            allow_headers_val.trim() == "*" && credentials_mode != RequestCredentialsMode::Include;
        let allow_headers: HashSet<String> = allow_headers_val
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let entry = CorsPreflightEntry {
            expires_at: Instant::now() + Duration::from_secs(max_age),
            allow_methods,
            allow_all_methods,
            allow_headers,
            allow_all_headers,
        };

        self.entries.lock().insert(key, entry);
    }

    #[allow(dead_code)]
    #[cfg(test)]
    pub fn clear(&self) {
        self.entries.lock().clear();
        self.in_flight.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cors_preflight_cache_record_and_check() {
        let cache = CorsPreflightCache::default();
        let origin = "https://bailian.console.aliyun.com";
        let url =
            Url::parse("https://bailian-cs.console.aliyun.com/data/api.json?action=list").unwrap();
        let credentials = RequestCredentialsMode::Include;

        let response_headers = vec![
            ("access-control-allow-origin".to_owned(), origin.to_owned()),
            (
                "access-control-allow-credentials".to_owned(),
                "true".to_owned(),
            ),
            (
                "access-control-allow-methods".to_owned(),
                "GET, POST, OPTIONS".to_owned(),
            ),
            (
                "access-control-allow-headers".to_owned(),
                "content-type, x-csrf-token".to_owned(),
            ),
            ("access-control-max-age".to_owned(), "3600".to_owned()),
        ];

        cache.record(origin, &url, credentials, &response_headers);

        // Matching request with same endpoint and allowed header
        let req_headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("x-csrf-token".to_owned(), "token123".to_owned()),
        ];
        assert!(cache.check(origin, &url, credentials, "POST", &req_headers));

        // Different query param on same path should also match
        let url2 =
            Url::parse("https://bailian-cs.console.aliyun.com/data/api.json?action=other").unwrap();
        assert!(cache.check(origin, &url2, credentials, "POST", &req_headers));

        // Disallowed method should fail
        assert!(!cache.check(origin, &url, credentials, "DELETE", &req_headers));

        // Disallowed header should fail
        let bad_headers = vec![("x-unauthorized-header".to_owned(), "val".to_owned())];
        assert!(!cache.check(origin, &url, credentials, "POST", &bad_headers));
    }

    #[tokio::test]
    async fn test_in_flight_coalescing() {
        let cache = Arc::new(CorsPreflightCache::default());
        let origin = "https://example.com";
        let url = Url::parse("https://api.example.com/data").unwrap();
        let credentials = RequestCredentialsMode::SameOrigin;

        // First caller registers and executes
        match cache.register_or_wait(origin, &url, credentials) {
            InFlightAction::Execute => {}
            InFlightAction::Wait(_) => panic!("Expected Execute"),
        }

        // Second caller registers while first is still in flight -> must get Wait!
        let mut rx = match cache.register_or_wait(origin, &url, credentials) {
            InFlightAction::Execute => panic!("Expected Wait"),
            InFlightAction::Wait(rx) => rx,
        };

        // First task finishes and records
        let response_headers = vec![
            ("access-control-allow-methods".to_owned(), "POST".to_owned()),
            ("access-control-max-age".to_owned(), "60".to_owned()),
        ];
        cache.record(origin, &url, credentials, &response_headers);
        cache.finish_in_flight(origin, &url, credentials);

        // Second caller waits and sees completion
        let _ = rx.wait_for(|&done| done).await;
        assert!(cache.check(origin, &url, credentials, "POST", &[]));
    }
}
