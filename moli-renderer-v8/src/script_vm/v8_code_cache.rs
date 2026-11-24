//! Two-tier (in-memory LRU + disk) V8 bytecode code cache for fast script compilation and hydration (方案 3).

use indexmap::IndexMap;
use parking_lot::RwLock;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
};
use url::Url;

const MAX_IN_MEMORY_ENTRIES: usize = 512;
const MAX_IN_MEMORY_BYTES: usize = 64 * 1024 * 1024; // 64 MB
const DISK_WRITER_QUEUE_CAPACITY: usize = 256;

enum DiskCacheOp {
    Write { path: PathBuf, data: Arc<[u8]> },
    Remove { path: PathBuf },
}

static DISK_WRITER_TX: OnceLock<SyncSender<DiskCacheOp>> = OnceLock::new();
static TMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

fn disk_writer() -> &'static SyncSender<DiskCacheOp> {
    DISK_WRITER_TX.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<DiskCacheOp>(DISK_WRITER_QUEUE_CAPACITY);
        let _ = std::thread::Builder::new()
            .name("moli-v8-code-cache".to_owned())
            .spawn(move || {
                let mut ensured_dir: Option<PathBuf> = None;
                let pid = std::process::id();
                while let Ok(op) = rx.recv() {
                    match op {
                        DiskCacheOp::Write { path, data } => {
                            if let Some(parent) = path.parent()
                                && ensured_dir.as_deref() != Some(parent)
                                && std::fs::create_dir_all(parent).is_ok()
                            {
                                ensured_dir = Some(parent.to_path_buf());
                            }
                            let seq = TMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
                            let tmp_path = path.with_extension(format!("v8code.tmp.{pid}.{seq}"));
                            if std::fs::write(&tmp_path, &*data).is_ok() {
                                if std::fs::rename(&tmp_path, &path).is_err() {
                                    let _ = std::fs::remove_file(&tmp_path);
                                }
                            } else {
                                let _ = std::fs::remove_file(&tmp_path);
                            }
                        }
                        DiskCacheOp::Remove { path } => {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
            });
        tx
    })
}

struct MemoryCodeCache {
    entries: IndexMap<PathBuf, Arc<[u8]>>,
    total_bytes: usize,
}

impl MemoryCodeCache {
    fn new() -> Self {
        Self {
            entries: IndexMap::new(),
            total_bytes: 0,
        }
    }

    fn get(&self, path: &Path) -> Option<Arc<[u8]>> {
        self.entries.get(path).cloned()
    }

    fn insert(&mut self, path: PathBuf, data: Arc<[u8]>) {
        let data_len = data.len();
        if let Some(old) = self.entries.insert(path, data) {
            self.total_bytes = self.total_bytes.saturating_sub(old.len());
        }
        self.total_bytes = self.total_bytes.saturating_add(data_len);

        while self.entries.len() > MAX_IN_MEMORY_ENTRIES || self.total_bytes > MAX_IN_MEMORY_BYTES {
            if let Some((_, removed)) = self.entries.shift_remove_index(0) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.len());
            } else {
                break;
            }
        }
    }

    fn remove(&mut self, path: &Path) {
        if let Some(removed) = self.entries.shift_remove(path) {
            self.total_bytes = self.total_bytes.saturating_sub(removed.len());
        }
    }
}

static IN_MEMORY_CACHE: OnceLock<RwLock<MemoryCodeCache>> = OnceLock::new();

fn in_memory_cache() -> &'static RwLock<MemoryCodeCache> {
    IN_MEMORY_CACHE.get_or_init(|| RwLock::new(MemoryCodeCache::new()))
}

/// Default directory for persistent V8 bytecode code cache.
#[allow(dead_code)]
pub fn default_v8_code_cache_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return PathBuf::from(home)
                .join("Library")
                .join("Caches")
                .join("moli")
                .join("v8-code-cache");
        }
    }
    #[cfg(windows)]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA")
            && !local_app_data.is_empty()
        {
            return PathBuf::from(local_app_data)
                .join("moli")
                .join("v8-code-cache");
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("moli").join("v8-code-cache");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home)
            .join(".cache")
            .join("moli")
            .join("v8-code-cache");
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

/// Get bytecode from cache (Tier 1: in-memory cache with read-lock fast path, Tier 2: disk).
pub fn get_code_cache(path: &Path) -> Option<Arc<[u8]>> {
    // 1. Check in-memory tier first using a shared read lock
    if let Some(data) = in_memory_cache().read().get(path) {
        return Some(data);
    }

    // 2. Fall back to disk tier
    if let Ok(bytes) = std::fs::read(path) {
        let arc_data: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
        in_memory_cache()
            .write()
            .insert(path.to_path_buf(), arc_data.clone());
        Some(arc_data)
    } else {
        None
    }
}

/// Invalidate bytecode cache entry from both in-memory and disk tiers.
pub fn invalidate_code_cache(path: &Path) {
    in_memory_cache().write().remove(path);
    let _ = disk_writer().try_send(DiskCacheOp::Remove {
        path: path.to_path_buf(),
    });
}

fn persist_code_cache_bytes(path: &Path, data: Arc<[u8]>) {
    in_memory_cache()
        .write()
        .insert(path.to_path_buf(), data.clone());
    let _ = disk_writer().try_send(DiskCacheOp::Write {
        path: path.to_path_buf(),
        data,
    });
}

/// Save compiled script bytecode to two-tier cache (memory + async atomic disk write).
pub fn save_script_code_cache(
    scope: &v8::PinScope<'_, '_>,
    script: v8::Local<'_, v8::Script>,
    path: &Path,
) {
    let unbound = script.get_unbound_script(scope);
    if let Some(code_cache) = unbound.create_code_cache() {
        let data: Arc<[u8]> = Arc::from(&code_cache[..]);
        persist_code_cache_bytes(path, data);
    }
}

/// Save compiled module bytecode to two-tier cache (memory + async atomic disk write).
pub fn save_module_code_cache(
    scope: &v8::PinScope<'_, '_>,
    module: v8::Local<'_, v8::Module>,
    path: &Path,
) {
    let unbound = module.get_unbound_module_script(scope);
    if let Some(code_cache) = unbound.create_code_cache() {
        let data: Arc<[u8]> = Arc::from(&code_cache[..]);
        persist_code_cache_bytes(path, data);
    }
}
