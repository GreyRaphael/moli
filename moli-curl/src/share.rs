use curl::easy::{Easy2, Handler};
use std::ptr;

pub(crate) struct CurlShare {
    raw: *mut curl_sys::CURLSH,
}

unsafe impl Send for CurlShare {}

impl CurlShare {
    pub(crate) fn new() -> Option<Self> {
        let raw = unsafe { curl_sys::curl_share_init() };
        if raw.is_null() {
            tracing::warn!("failed to initialize curl share handle");
            return None;
        }

        unsafe {
            let res = curl_sys::curl_share_setopt(
                raw,
                curl_sys::CURLSHOPT_SHARE,
                curl_sys::CURL_LOCK_DATA_DNS,
            );
            if res != curl_sys::CURLSHE_OK {
                tracing::warn!("failed to share DNS on curl share handle: {res}");
            }

            let res = curl_sys::curl_share_setopt(
                raw,
                curl_sys::CURLSHOPT_SHARE,
                curl_sys::CURL_LOCK_DATA_SSL_SESSION,
            );
            if res != curl_sys::CURLSHE_OK {
                tracing::warn!("failed to share SSL session on curl share handle: {res}");
            }
        }

        Some(Self { raw })
    }

    pub(crate) fn attach<H: Handler>(&self, easy: &mut Easy2<H>) {
        unsafe {
            let code = curl_sys::curl_easy_setopt(easy.raw(), curl_sys::CURLOPT_SHARE, self.raw);
            if code != curl_sys::CURLE_OK {
                tracing::debug!("failed to attach CURLOPT_SHARE to easy handle: {code}");
            }
        }
    }

    pub(crate) fn detach<H: Handler>(easy: &mut Easy2<H>) {
        unsafe {
            let _ = curl_sys::curl_easy_setopt(
                easy.raw(),
                curl_sys::CURLOPT_SHARE,
                ptr::null_mut::<curl_sys::CURLSH>(),
            );
        }
    }
}

impl Drop for CurlShare {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                curl_sys::curl_share_cleanup(self.raw);
            }
        }
    }
}
