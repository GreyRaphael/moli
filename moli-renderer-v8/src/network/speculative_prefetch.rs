//! Speculative prefetching for dynamic imports and code-split module chunks.
//!
//! When modern web applications (e.g. Vite, Rolldown, Webpack, Rspack) load their
//! main bundle, subsequent routes and components are loaded via dynamic `import(...)`
//! or bundler chunk dependency maps. By proactively scanning script source for these
//! specifiers as soon as the parent script finishes downloading, Moli initiates
//! background prefetch requests before the JavaScript VM evaluates the router/import sites.
//!
//! Because `ResourceRequestClient` manages memory cache entries using SingleFlight
//! (`PendingWaiter` / `Owner`), in-flight prefetches automatically join any incoming
//! dynamic import requests without duplicate network roundtrips.

use moli_fetch::{BrowserRequestMetadata, Request, Response};
use std::collections::HashSet;
use url::Url;

use super::{ResourceRequestClient, backend::script_text_cache_key};

/// Maximum number of speculative prefetches launched from a single script.
const MAX_PREFETCHES_PER_SCRIPT: usize = 20;

/// Fast scan of script text to extract candidate module specifiers.
///
/// Looks for:
/// 1. Dynamic imports: `import("./...")`, `import('../...')`, `import('...')`
/// 2. Relative JS chunk specifiers: `"./foo.js"`, `'./bar-abc.js'`, `"../chunk.mjs"`
pub fn extract_speculative_script_specifiers(source: &str) -> Vec<String> {
    if !source.contains("import(") && !source.contains("./") && !source.contains("../") {
        return Vec::new();
    }

    let bytes = source.as_bytes();
    let n = bytes.len();
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    let mut i = 0;
    while i < n && results.len() < MAX_PREFETCHES_PER_SCRIPT {
        // Match `import(`
        if i + 7 <= n && &bytes[i..i + 7] == b"import(" {
            let mut j = i + 7;
            while j < n
                && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r' || bytes[j] == b'\n')
            {
                j += 1;
            }
            if j < n && (bytes[j] == b'"' || bytes[j] == b'\'' || bytes[j] == b'`') {
                let quote = bytes[j];
                let start = j + 1;
                let mut k = start;
                while k < n && bytes[k] != quote && bytes[k] != b'\n' && bytes[k] != b'\r' {
                    k += 1;
                }
                if k < n && bytes[k] == quote && k > start && k - start < 300 {
                    if let Ok(spec) = std::str::from_utf8(&bytes[start..k]) {
                        let spec = spec.trim();
                        if !spec.is_empty()
                            && is_valid_specifier_path(spec)
                            && seen.insert(spec.to_owned())
                        {
                            results.push(spec.to_owned());
                        }
                    }
                }
                i = k + 1;
                continue;
            }
        }

        // Match relative chunks: `"./`, `'./`, `"../`, `'../`
        if i + 3 <= n {
            let prefix3 = &bytes[i..i + 3];
            let is_dot_slash = prefix3 == b"\"./" || prefix3 == b"'./";
            let is_dot_dot_slash =
                i + 4 <= n && (&bytes[i..i + 4] == b"\"../" || &bytes[i..i + 4] == b"'../");

            if is_dot_slash || is_dot_dot_slash {
                let quote = bytes[i];
                let start = i + 1;
                let mut k = start;
                while k < n
                    && bytes[k] != quote
                    && bytes[k] != b'\n'
                    && bytes[k] != b'\r'
                    && bytes[k] != b' '
                {
                    k += 1;
                }
                if k < n && bytes[k] == quote && k > start && k - start < 300 {
                    if let Ok(spec) = std::str::from_utf8(&bytes[start..k]) {
                        if (spec.ends_with(".js") || spec.ends_with(".mjs"))
                            && is_valid_specifier_path(spec)
                            && seen.insert(spec.to_owned())
                        {
                            results.push(spec.to_owned());
                        }
                    }
                }
                i = k + 1;
                continue;
            }
        }

        i += 1;
    }

    results
}

fn is_valid_specifier_path(s: &str) -> bool {
    // Only accept relative paths or valid web module specifiers
    if s.is_empty() || s.len() > 256 {
        return false;
    }
    s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || c == '.'
            || c == '/'
            || c == '-'
            || c == '_'
            || c == '@'
            || c == '~'
            || c == '+'
            || c == '?'
            || c == '&'
            || c == '='
            || c == '%'
    })
}

impl ResourceRequestClient {
    pub(crate) fn speculatively_prefetch_script_dependencies(
        &self,
        parent_request: &Request,
        response: &Response,
    ) {
        if response.status < 200 || response.status > 299 {
            return;
        }
        self.speculatively_prefetch_script_source_text(parent_request, response.body_text());
    }

    pub(crate) fn speculatively_prefetch_script_source_text(
        &self,
        parent_request: &Request,
        source_text: &str,
    ) {
        // Only scan JavaScript or module resources
        if let Some(ref metadata) = parent_request.browser_request_metadata {
            if *metadata != BrowserRequestMetadata::Script {
                return;
            }
        }

        if source_text.len() < 10 {
            return;
        }

        let specifiers = extract_speculative_script_specifiers(source_text);
        if specifiers.is_empty() {
            return;
        }

        for specifier in specifiers {
            let resolved_url = match parent_request.url.join(&specifier) {
                Ok(url) => url,
                Err(_) => continue,
            };

            if resolved_url.scheme() != "http" && resolved_url.scheme() != "https" {
                continue;
            }

            // Don't prefetch self
            if resolved_url == parent_request.url {
                continue;
            }

            self.prefetch_single_speculative_script(parent_request, resolved_url);
        }
    }

    fn prefetch_single_speculative_script(&self, parent_request: &Request, url: Url) {
        let mut prefetch_req = parent_request.clone();
        prefetch_req.url = url.clone();
        prefetch_req.browser_request_metadata = Some(BrowserRequestMetadata::Script);
        let prefetch_req = prefetch_req.with_initiator_url(&parent_request.url);

        let page_cache_partition_id = self.page_network_policy.memory_cache_partition_id();
        let key =
            script_text_cache_key(&prefetch_req).for_page_cache_partition(page_cache_partition_id);

        let reserved = if prefetch_req.cache_mode().allows_memory_cache_lookup() {
            let mut cache = self.resource_runtime.memory_cache().lock();
            cache.try_reserve_speculative_prefetch(&key)
        } else {
            true
        };

        if !reserved {
            return;
        }

        let client = self.clone();
        let finish_key = key.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = client
                    .fetch_cacheable_script_text_stream(prefetch_req)
                    .await;
                client
                    .resource_runtime
                    .memory_cache()
                    .lock()
                    .finish_speculative_prefetch(&finish_key);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_dynamic_imports_with_different_quotes() {
        let source = r#"
            const a = import("./first-chunk.js");
            const b = import('./second-chunk.mjs');
            const c = import(`./third-chunk.js`);
            const ignore = 123;
        "#;
        let specs = extract_speculative_script_specifiers(source);
        assert_eq!(
            specs,
            vec!["./first-chunk.js", "./second-chunk.mjs", "./third-chunk.js"]
        );
    }

    #[test]
    fn extracts_vite_dep_array_chunks() {
        let source = r#"
            const __vite__mapDeps=(i,m=__vite__mapDeps,d=(m.f||(m.f=["./home-CWMse6cs.js","./rolldown-runtime.js","./theme.js"])))=>i.map(i=>d[i]);
        "#;
        let specs = extract_speculative_script_specifiers(source);
        assert_eq!(
            specs,
            vec!["./home-CWMse6cs.js", "./rolldown-runtime.js", "./theme.js"]
        );
    }

    #[test]
    fn deduplicates_extracted_specifiers() {
        let source = r#"
            import("./shared.js");
            import("./shared.js");
            const arr = ["./shared.js", "./other.js"];
        "#;
        let specs = extract_speculative_script_specifiers(source);
        assert_eq!(specs, vec!["./shared.js", "./other.js"]);
    }

    #[test]
    fn speculative_prefetch_source_text_skips_non_script() {
        let owner = ResourceRequestClient::new(&moli_fetch::FetchConfig::default()).unwrap();
        let mut req = Request::new("GET", "https://example.test/image.png", None, vec![]).unwrap();
        req.browser_request_metadata = Some(moli_fetch::BrowserRequestMetadata::Image);
        owner.speculatively_prefetch_script_source_text(&req, "import('./chunk.js')");
    }

    #[test]
    fn speculative_prefetch_source_text_handles_prefix_chunks() {
        let owner = ResourceRequestClient::new(&moli_fetch::FetchConfig::default()).unwrap();
        let mut req =
            Request::new("GET", "https://example.test/assets/app.js", None, vec![]).unwrap();
        req.browser_request_metadata = Some(moli_fetch::BrowserRequestMetadata::Script);
        let chunk0 = r#"const __vite__mapDeps=(i,d=["./home-chunk.js"])=>d[i];"#;
        owner.speculatively_prefetch_script_source_text(&req, chunk0);
    }
}
