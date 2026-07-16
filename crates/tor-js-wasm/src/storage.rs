//! JavaScript storage adapter for tor-js.
//!
//! This module provides [`CachedJsStorage`], a [`KeyValueStore`] implementation
//! that bridges async JavaScript storage APIs with Rust's sync storage traits
//! using a pre-load + cache + async write-back pattern:
//!
//! 1. During client creation (async), all data is loaded from JS storage
//! 2. Sync reads hit the in-memory cache
//! 3. Writes update the cache and schedule async persistence via spawn_local()
//!
//! [`KeyValueStore`]: tor_persist::KeyValueStore

use tor_persist::{KeyValueStore, StorageError};
use futures::FutureExt as _;
use js_sys::Promise;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

// ============================================================================
// JS Storage Interface
// ============================================================================

/// JavaScript storage interface.
///
/// This is the interface that JavaScript code must implement to provide
/// custom storage. All methods return Promises.
/// (Return values are wrapped in Result to handle the possibility that JS
/// throws synchronously.)
#[wasm_bindgen]
extern "C" {
    /// The JavaScript storage interface type.
    #[wasm_bindgen(typescript_type = "TorStorage")]
    pub type JsStorageInterface;

    /// Get a value by key. Returns null if not found.
    #[wasm_bindgen(method, catch)]
    fn get(this: &JsStorageInterface, key: &str) -> Result<Promise, JsValue>;

    /// Set a value by key.
    #[wasm_bindgen(method, catch)]
    fn set(this: &JsStorageInterface, key: &str, value: &str) -> Result<Promise, JsValue>;

    /// Delete a value by key.
    #[wasm_bindgen(method, catch)]
    fn delete(this: &JsStorageInterface, key: &str) -> Result<Promise, JsValue>;

    /// List all keys with a given prefix.
    #[wasm_bindgen(method, catch)]
    fn keys(this: &JsStorageInterface, prefix: &str) -> Result<Promise, JsValue>;

    /// Get all key-value pairs matching a prefix.
    /// Returns an array of [key, value] pairs.
    #[wasm_bindgen(method, catch, js_name = "getAll")]
    fn get_all(this: &JsStorageInterface, prefix: &str) -> Result<Promise, JsValue>;

    /// Try to acquire an exclusive write lock.
    /// Returns a boolean: true if newly acquired, false if already held.
    #[wasm_bindgen(method, catch, js_name = "tryLock")]
    fn try_lock(this: &JsStorageInterface) -> Result<Promise, JsValue>;

    /// Release the write lock.
    #[wasm_bindgen(method, catch)]
    fn unlock(this: &JsStorageInterface) -> Result<Promise, JsValue>;
}

// ============================================================================
// JsStorage Wrapper
// ============================================================================

/// Rust wrapper around the JavaScript storage interface.
///
/// Provides async methods that convert JS Promises to Rust Futures.
pub struct JsStorage {
    inner: send_wrapper::SendWrapper<JsStorageInterface>,
}

impl Clone for JsStorage {
    fn clone(&self) -> Self {
        let inner_clone: JsStorageInterface = (*self.inner).clone().unchecked_into();
        Self { inner: send_wrapper::SendWrapper::new(inner_clone) }
    }
}

impl JsStorage {
    /// Create a new JsStorage from a JavaScript storage interface.
    pub fn new(interface: JsStorageInterface) -> Self {
        Self { inner: send_wrapper::SendWrapper::new(interface) }
    }

    /// Get a value by key.
    pub async fn get(&self, key: &str) -> Result<Option<String>, JsValue> {
        let promise = self.inner.get(key)?;
        let result = JsFuture::from(promise).await?;
        if result.is_null() || result.is_undefined() {
            Ok(None)
        } else {
            Ok(result.as_string())
        }
    }

    /// Set a value by key.
    pub async fn set(&self, key: &str, value: &str) -> Result<(), JsValue> {
        let promise = self.inner.set(key, value)?;
        JsFuture::from(promise).await?;
        Ok(())
    }

    /// Delete a value by key.
    pub async fn delete(&self, key: &str) -> Result<(), JsValue> {
        let promise = self.inner.delete(key)?;
        JsFuture::from(promise).await?;
        Ok(())
    }

    /// List all keys with a given prefix.
    pub async fn keys(&self, prefix: &str) -> Result<Vec<String>, JsValue> {
        let promise = self.inner.keys(prefix)?;
        let result = JsFuture::from(promise).await?;

        // Convert JS array to Vec<String>
        let array = js_sys::Array::from(&result);
        let mut keys = Vec::with_capacity(array.length() as usize);
        for i in 0..array.length() {
            if let Some(key) = array.get(i).as_string() {
                keys.push(key);
            }
        }
        Ok(keys)
    }

    /// Get all key-value pairs matching a prefix.
    pub async fn get_all(&self, prefix: &str) -> Result<Vec<(String, String)>, JsValue> {
        let promise = self.inner.get_all(prefix)?;
        let result = JsFuture::from(promise).await?;

        // Convert JS array of [key, value] pairs to Vec<(String, String)>
        let array = js_sys::Array::from(&result);
        let mut entries = Vec::with_capacity(array.length() as usize);
        for i in 0..array.length() {
            let pair = js_sys::Array::from(&array.get(i));
            if let (Some(key), Some(value)) = (pair.get(0).as_string(), pair.get(1).as_string()) {
                entries.push((key, value));
            }
        }
        Ok(entries)
    }

    /// Try to acquire an exclusive write lock.
    pub async fn try_lock(&self) -> Result<bool, JsValue> {
        let promise = self.inner.try_lock()?;
        let result = JsFuture::from(promise).await?;
        Ok(result.as_bool().unwrap_or(false))
    }

    /// Release the write lock.
    pub async fn unlock(&self) -> Result<(), JsValue> {
        let promise = self.inner.unlock()?;
        JsFuture::from(promise).await?;
        Ok(())
    }
}

// ============================================================================
// CachedJsStorage - KeyValueStore backed by JS storage with cache
// ============================================================================

/// Cached JavaScript storage implementing [`KeyValueStore`].
///
/// Bridges async JavaScript storage APIs with the sync [`KeyValueStore`] trait
/// using a pre-load + cache + async write-back pattern. All keys from JS
/// storage are loaded into an in-memory cache during construction, then:
///
/// - **Reads** hit the cache directly (sync)
/// - **Writes** update the cache and schedule async persistence via `spawn_local()`
/// - **Locking**: the JS-side lock is acquired once during construction and
///   held for the client's lifetime. The sync `try_lock()`/`unlock()` methods
///   only manage local state for arti's internal protocol.
///
/// # Locking limitations
///
/// Because [`KeyValueStore`] lock methods are sync but JS locking is async,
/// we can't cycle locks at runtime. The JS lock is acquired once in [`new()`]
/// and never released (until the tab/worker closes).
///
/// This is eased in JS by using an in-memory overlay when needed so locks can
/// always be granted. See addLocking.
pub struct CachedJsStorage {
    /// The underlying JS storage (for async write-back).
    js_storage: JsStorage,
    /// In-memory cache for sync reads.
    cache: Arc<RwLock<HashMap<String, String>>>,
    /// Whether we currently hold the write lock.
    locked: Arc<RwLock<bool>>,
    /// Fires after the JS lock is released in `Drop`. `Option` so `Drop` can take it.
    drop_tx: Option<tor_async_utils::oneshot::Sender<()>>,
    /// Cloneable handle that resolves when the JS lock is released.
    drop_rx: futures::future::Shared<tor_async_utils::oneshot::Receiver<()>>,
}

// CachedJsStorage is Send/Sync because JsStorage uses SendWrapper internally.

impl CachedJsStorage {
    /// Create a new CachedJsStorage, preloading all data from JS storage.
    ///
    /// Acquires the JS-side lock and holds it for the client's lifetime.
    /// This validates that `tryLock()` and `unlock()` exist on the JS object,
    /// then preloads all `"state:"` and `"dir:"` prefixed keys into the
    /// in-memory cache (necessary because [`KeyValueStore`] is sync but JS
    /// storage APIs are async).
    pub async fn new(js_storage: JsStorage) -> Result<Self, JsValue> {
        // Acquire the JS-side lock. This is held for the client's lifetime.
        // Also validates that tryLock() exists on the JS object.
        js_storage.try_lock().await.map_err(|e| {
            JsValue::from_str(&format!(
                "Storage must implement tryLock(): {:?}",
                e
            ))
        })?;

        // Validate that unlock() exists (we'll need it for shutdown).
        // Call it, then re-acquire immediately.
        js_storage.unlock().await.map_err(|e| {
            JsValue::from_str(&format!(
                "Storage must implement unlock(): {:?}",
                e
            ))
        })?;
        js_storage.try_lock().await.map_err(|e| {
            JsValue::from_str(&format!(
                "Storage.tryLock() failed on re-acquire: {:?}",
                e
            ))
        })?;

        let (drop_tx, drop_rx) = tor_async_utils::oneshot::channel();
        let drop_rx = drop_rx.shared();
        let storage = Self {
            js_storage,
            cache: Arc::new(RwLock::new(HashMap::new())),
            locked: Arc::new(RwLock::new(true)),
            drop_tx: Some(drop_tx),
            drop_rx,
        };

        storage.preload_all().await?;

        Ok(storage)
    }

    /// Pre-load all data from JS storage into the cache.
    ///
    /// Uses bulk `getAll()` to fetch all key-value pairs per prefix in a single
    /// JS call, avoiding thousands of sequential WASM-JS round-trips.
    async fn preload_all(&self) -> Result<(), JsValue> {
        let state_entries = self.js_storage.get_all("state:").await?;
        let dir_entries = self.js_storage.get_all("dir:").await?;

        let mut cache = self
            .cache
            .write()
            .map_err(|_| JsValue::from_str("cache lock poisoned"))?;
        for (key, value) in state_entries {
            cache.insert(key, value);
        }
        for (key, value) in dir_entries {
            cache.insert(key, value);
        }

        tracing::debug!("CachedJsStorage: preloaded {} entries", cache.len());
        Ok(())
    }

    /// Schedule an async write to JS storage.
    fn schedule_persist(&self, key: String, value: String) {
        let js_storage = self.js_storage.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = js_storage.set(&key, &value).await {
                tracing::warn!("CachedJsStorage: failed to persist key {}: {:?}", key, e);
            }
        });
    }

    /// Insert many key-value pairs into the cache and schedule a single async
    /// batch persist to JS storage.  Much faster than calling `set()` in a loop
    /// because it acquires the RwLock once and spawns one `spawn_local()` task.
    pub fn set_many(&self, entries: Vec<(String, String)>) -> Result<(), StorageError> {
        {
            let mut cache = self
                .cache
                .write()
                .map_err(|_| -> StorageError { "cache lock poisoned".into() })?;
            for (k, v) in &entries {
                cache.insert(k.clone(), v.clone());
            }
        }

        let js_storage = self.js_storage.clone();
        wasm_bindgen_futures::spawn_local(async move {
            for (key, value) in entries {
                if let Err(e) = js_storage.set(&key, &value).await {
                    tracing::warn!("CachedJsStorage: failed to persist key {}: {:?}", key, e);
                }
            }
        });
        Ok(())
    }

    /// Schedule an async delete from JS storage.
    fn schedule_delete(&self, key: String) {
        let js_storage = self.js_storage.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = js_storage.delete(&key).await {
                tracing::warn!("CachedJsStorage: failed to delete key {}: {:?}", key, e);
            }
        });
    }
}

impl Drop for CachedJsStorage {
    fn drop(&mut self) {
        let js_storage = self.js_storage.clone();
        let drop_tx = self.drop_tx.take();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = js_storage.unlock().await {
                tracing::warn!("CachedJsStorage: failed to release JS lock on drop: {:?}", e);
            }
            // Signal waiters *after* the JS lock is released.
            drop(drop_tx);
        });
    }
}

impl KeyValueStore for CachedJsStorage {
    fn get(&self, key: &str) -> Result<Option<String>, StorageError> {
        let cache = self
            .cache
            .read()
            .map_err(|_| -> StorageError { "cache lock poisoned".into() })?;
        Ok(cache.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), StorageError> {
        // Update cache
        {
            let mut cache = self
                .cache
                .write()
                .map_err(|_| -> StorageError { "cache lock poisoned".into() })?;
            cache.insert(key.to_string(), value.to_string());
        }

        // Schedule async write to JS storage
        self.schedule_persist(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StorageError> {
        // Update cache
        {
            let mut cache = self
                .cache
                .write()
                .map_err(|_| -> StorageError { "cache lock poisoned".into() })?;
            cache.remove(key);
        }

        // Schedule async delete from JS storage
        self.schedule_delete(key.to_string());
        Ok(())
    }

    fn keys(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        let cache = self
            .cache
            .read()
            .map_err(|_| -> StorageError { "cache lock poisoned".into() })?;
        Ok(cache
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn try_lock(&self) -> Result<bool, StorageError> {
        // The JS-side lock is acquired once in CachedJsStorage::new() and held
        // for the client's lifetime. These sync methods only track local state
        // for arti's internal lock protocol — no JS calls are made here.
        let mut locked = self
            .locked
            .write()
            .map_err(|_| -> StorageError { "lock state poisoned".into() })?;
        if *locked {
            return Ok(false);
        }
        *locked = true;
        Ok(true)
    }

    fn can_store(&self) -> Result<bool, StorageError> {
        Ok(*self.locked.read().map_err(|e| e.to_string())?)
    }

    fn unlock(&self) -> Result<(), StorageError> {
        // Local-only: doesn't release the JS-side lock.
        // The JS lock is held for the client's lifetime.
        let mut locked = self
            .locked
            .write()
            .map_err(|_| -> StorageError { "lock state poisoned".into() })?;
        *locked = false;
        Ok(())
    }

    fn wait_for_unlock(
        &self,
    ) -> Pin<Box<dyn futures::Future<Output = ()> + Send + Sync + 'static>> {
        Box::pin(self.drop_rx.clone().map(|_| ()))
    }
}
