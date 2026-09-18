//! Persistent V8 bytecode code cache for fast script compilation and hydration (手段 3).

use std::path::{Path, PathBuf};
use url::Url;

/// Default directory for persistent V8 bytecode code cache.
pub fn default_v8_code_cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("moli").join("v8-code-cache");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".cache")
                .join("moli")
                .join("v8-code-cache");
        }
    }
    std::env::temp_dir().join("moli").join("v8-code-cache")
}

/// Compute the cache file path for a script URL and its source length.
pub fn code_cache_path_for_url(url: &Url, source_len: usize) -> Option<PathBuf> {
    #[cfg(test)]
    {
        let _ = (url, source_len);
        None
    }
    #[cfg(not(test))]
    {
        if url.scheme() != "http" && url.scheme() != "https" {
            return None;
        }
        let cache_dir = default_v8_code_cache_dir();
        let key = format!("{}:{}", url.as_str(), source_len);
        let hash = moli_crypto::sha256_hex(key.as_bytes());
        Some(cache_dir.join(format!("{hash}.v8code")))
    }
}

/// Save compiled script bytecode to cache file.
pub fn save_script_code_cache(
    scope: &v8::PinScope<'_, '_>,
    script: v8::Local<'_, v8::Script>,
    path: &Path,
) {
    let unbound = script.get_unbound_script(scope);
    if let Some(code_cache) = unbound.create_code_cache() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, &**code_cache);
    }
}

/// Save compiled module bytecode to cache file.
pub fn save_module_code_cache(
    scope: &v8::PinScope<'_, '_>,
    module: v8::Local<'_, v8::Module>,
    path: &Path,
) {
    let unbound = module.get_unbound_module_script(scope);
    if let Some(code_cache) = unbound.create_code_cache() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, &**code_cache);
    }
}
