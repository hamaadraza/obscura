use std::cell::RefCell;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use deno_core::error::ModuleLoaderError;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;
use deno_core::RequestedModuleType;

use crate::import_map::ImportMap;
use crate::ops::ObscuraState;

/// Observable network activity for ES-module graphs.
///
/// deno_core keeps dynamic-import state inside its private module map. The
/// browser lifecycle still needs to distinguish a genuinely idle page from a
/// graph whose fetch future is being advanced in short event-loop slices. A
/// loader-owned counter provides that signal without treating unrelated
/// fetch/XHR analytics as render-blocking work.
#[derive(Debug, Default)]
pub(crate) struct ModuleLoadActivity {
    pending: std::sync::atomic::AtomicUsize,
    last_activity: std::sync::Mutex<Option<std::time::Instant>>,
}

impl ModuleLoadActivity {
    fn begin(self: &Arc<Self>) -> ModuleLoadGuard {
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::time::Instant::now());
        ModuleLoadGuard(self.clone())
    }

    pub(crate) fn is_pending_or_recent(&self, grace: std::time::Duration) -> bool {
        if self.pending.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            return true;
        }
        self.last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|last| last.elapsed() <= grace)
    }
}

struct ModuleLoadGuard(Arc<ModuleLoadActivity>);

impl Drop for ModuleLoadGuard {
    fn drop(&mut self) {
        let previous = self
            .0
            .pending
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        debug_assert!(previous > 0, "module load activity counter underflow");
        *self
            .0
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::time::Instant::now());
    }
}

pub struct ObscuraModuleLoader {
    pub base_url: String,
    /// Proxy URL threaded through to every dynamic ES-module fetch (#139).
    /// `None` keeps the pre-#139 direct-connection behaviour for callers
    /// that haven't been updated.
    pub proxy_url: Option<String>,
    /// The owning page's network context. Production runtimes always install
    /// this so every module in a graph uses the same cookie jar, configured
    /// identity, redirect/security policy, interception, and callbacks as the
    /// entry module. Directly-constructed standalone loaders remain supported.
    page_state: Option<Weak<RefCell<ObscuraState>>>,
    /// Directly-constructed loaders still use Obscura's network policy and
    /// connection pool; they simply have an isolated cookie jar.
    standalone_client: Option<Arc<obscura_net::ObscuraHttpClient>>,
    import_map: Rc<RefCell<ImportMap>>,
    activity: Arc<ModuleLoadActivity>,
    /// Canonical and requested specifiers fetched into deno_core's module map.
    /// The runtime uses a cursor into this append-only list to associate a
    /// prepared root with the dependencies that its successful evaluation also
    /// evaluates.
    loaded_specifiers: Rc<RefCell<Vec<String>>>,
    /// Module sources fetched ahead of evaluation, keyed by resolved URL.
    /// deno_core loads a graph only when its root is evaluated, and roots are
    /// evaluated one at a time, so every module in every graph used to cost a
    /// separate round trip in series. The page fills this in parallel before
    /// the serial phase begins; a hit here skips the network entirely.
    prefetched: Rc<RefCell<std::collections::HashMap<String, (String, String)>>>,
}

/// The page's side of the module preload cache.
///
/// Holds the two things a preload pass needs -- specifier resolution that
/// matches the loader (import map included) and a way to deposit fetched
/// sources -- without borrowing the runtime. That lets the page fetch module
/// graphs concurrently with everything else it fetches, instead of waiting
/// its turn.
#[derive(Clone)]
pub struct ModulePrefetch {
    resolver: Rc<ObscuraModuleLoader>,
    cache: Rc<RefCell<std::collections::HashMap<String, (String, String)>>>,
}

impl ModulePrefetch {
    /// Resolve an import specifier against a referrer exactly as the loader
    /// would. `None` where the loader could not resolve it either, such as a
    /// bare name with no import-map entry.
    pub fn resolve(&self, specifier: &str, referrer: &str) -> Option<String> {
        use deno_core::ModuleLoader as _;
        self.resolver
            .resolve(specifier, referrer, deno_core::ResolutionKind::Import)
            .ok()
            .map(|url| url.to_string())
    }

    /// Record a fetched module. `found` is the URL the bytes came from, which
    /// becomes the base for the module's own relative imports.
    pub fn insert(&self, requested: String, found: String, code: String) {
        if let Ok(mut cache) = self.cache.try_borrow_mut() {
            cache.entry(requested).or_insert((found, code));
        }
    }

    /// Static import specifiers in a module's source, for the preload scan.
    /// Approximate by design, like a browser's preload scanner: a specifier it
    /// gets wrong only costs an unused fetch, and one it misses simply loads
    /// later over the network. Catches `from "x"`, bare `import "x"` and
    /// dynamic `import("x")`.
    pub fn scan_imports(source: &str) -> Vec<String> {
        /// Reads a quoted specifier starting at `at`, skipping whitespace and
        /// an optional `(`. Returns the specifier and where it ended.
        fn quoted_at(source: &str, at: usize) -> Option<(String, usize)> {
            let rest = source.get(at..)?;
            let mut offset = 0usize;
            let mut seen_paren = false;
            for ch in rest.chars() {
                match ch {
                    ' ' | '\t' | '\r' | '\n' => offset += ch.len_utf8(),
                    '(' if !seen_paren => {
                        seen_paren = true;
                        offset += 1;
                    }
                    '\'' | '"' => {
                        let quote = ch;
                        let body = &rest[offset + 1..];
                        let end = body.find(quote)?;
                        let specifier = &body[..end];
                        if specifier.is_empty() || specifier.len() > 512 {
                            return None;
                        }
                        return Some((specifier.to_string(), at + offset + 1 + end + 1));
                    }
                    _ => return None,
                }
            }
            None
        }

        let mut found = Vec::new();
        for keyword in ["from", "import"] {
            let mut index = 0usize;
            while let Some(offset) = source[index..].find(keyword) {
                let after = index + offset + keyword.len();
                index = after;
                // Only a whole word: `transform "x"` is not an import.
                let before_is_word = source[..index - keyword.len()]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '.');
                if before_is_word {
                    continue;
                }
                if let Some((specifier, end)) = quoted_at(source, after) {
                    found.push(specifier);
                    index = end;
                }
            }
        }
        found.sort();
        found.dedup();
        found
    }

    pub(crate) fn new(
        resolver: Rc<ObscuraModuleLoader>,
        cache: Rc<RefCell<std::collections::HashMap<String, (String, String)>>>,
    ) -> Self {
        Self { resolver, cache }
    }
}

impl ObscuraModuleLoader {
    pub fn new(base_url: &str) -> Self {
        Self::with_proxy(base_url, None)
    }

    pub fn with_proxy(base_url: &str, proxy_url: Option<String>) -> Self {
        let import_map = Rc::new(RefCell::new(ImportMap::default()));
        Self::with_proxy_and_import_map(base_url, proxy_url, import_map)
    }

    fn with_proxy_and_import_map(
        base_url: &str,
        proxy_url: Option<String>,
        import_map: Rc<RefCell<ImportMap>>,
    ) -> Self {
        let standalone_client = Arc::new(obscura_net::ObscuraHttpClient::with_options(
            Arc::new(obscura_net::CookieJar::new()),
            proxy_url.as_deref(),
        ));
        ObscuraModuleLoader {
            base_url: base_url.to_string(),
            proxy_url,
            page_state: None,
            standalone_client: Some(standalone_client),
            import_map,
            activity: Arc::new(ModuleLoadActivity::default()),
            loaded_specifiers: Rc::new(RefCell::new(Vec::new())),
            prefetched: Rc::new(RefCell::new(std::collections::HashMap::new())),
        }
    }

    pub(crate) fn with_page_state(
        base_url: &str,
        proxy_url: Option<String>,
        page_state: &Rc<RefCell<ObscuraState>>,
        import_map: Rc<RefCell<ImportMap>>,
    ) -> Self {
        ObscuraModuleLoader {
            base_url: base_url.to_string(),
            proxy_url,
            page_state: Some(Rc::downgrade(page_state)),
            standalone_client: None,
            import_map,
            activity: Arc::new(ModuleLoadActivity::default()),
            loaded_specifiers: Rc::new(RefCell::new(Vec::new())),
            prefetched: Rc::new(RefCell::new(std::collections::HashMap::new())),
        }
    }

    pub(crate) fn activity(&self) -> Arc<ModuleLoadActivity> {
        self.activity.clone()
    }

    /// Shared handle to the preload cache, so the page can fill it.
    pub(crate) fn prefetched(
        &self,
    ) -> Rc<RefCell<std::collections::HashMap<String, (String, String)>>> {
        self.prefetched.clone()
    }

    pub(crate) fn loaded_specifiers(&self) -> Rc<RefCell<Vec<String>>> {
        self.loaded_specifiers.clone()
    }
}

fn io_err(msg: String) -> ModuleLoaderError {
    std::io::Error::new(std::io::ErrorKind::Other, msg).into()
}

impl ModuleLoader for ObscuraModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: deno_core::ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        // deno_core represents the root passed to load_side_es_module with a
        // synthetic "." referrer. A browser resolves <script type=module src>
        // as a resource URL before it starts a graph; the document import map
        // must not remap that root URL.
        if referrer == "." {
            return deno_core::resolve_import(specifier, &self.base_url)
                .map_err(|error| error.into());
        }

        let base = if referrer.is_empty()
            || referrer.starts_with('<')
            || referrer == "about:blank"
        {
            &self.base_url
        } else {
            referrer
        };

        let base = ModuleSpecifier::parse(base)
            .map_err(|e| io_err(format!("Invalid module referrer {}: {}", base, e)))?;
        self.import_map
            .try_borrow_mut()
            .map_err(|_| io_err("Import map is already borrowed".to_string()))?
            .resolve(specifier, &base)
            .map_err(io_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&ModuleSpecifier>,
        is_dyn_import: bool,
        _requested_module_type: RequestedModuleType,
    ) -> ModuleLoadResponse {
        let url = module_specifier.to_string();
        // Already fetched by the page's preload pass: answer synchronously so
        // the graph builds at memory speed instead of a round trip per module.
        if let Some((found, code)) = self
            .prefetched
            .try_borrow()
            .ok()
            .and_then(|cache| cache.get(&url).cloned())
        {
            let mut specifiers = self.loaded_specifiers.borrow_mut();
            specifiers.push(url.clone());
            if found != url {
                specifiers.push(found.clone());
            }
            drop(specifiers);
            let found = ModuleSpecifier::parse(&found)
                .unwrap_or_else(|_| module_specifier.clone());
            return ModuleLoadResponse::Sync(Ok(ModuleSource::new_with_redirect(
                deno_core::ModuleType::JavaScript,
                ModuleSourceCode::String(code.into()),
                module_specifier,
                &found,
                None,
            )));
        }
        // Module-graph CORS and same-origin credentials are relative to the
        // owning document, not to the importing module. The importer remains
        // the HTTP referrer for a dependency; keeping these URLs distinct
        // prevents a cross-origin parent module from gaining CDN cookies when
        // it imports a sibling on that CDN.
        let document_url = ModuleSpecifier::parse(&self.base_url)
            .unwrap_or_else(|_| module_specifier.clone());
        let referrer = maybe_referrer
            .cloned()
            .unwrap_or_else(|| document_url.clone());
        // Capture the loader's proxy here so the async closure below owns a
        // plain Option<String> rather than borrowing &self across an `await`.
        let proxy_url = self.proxy_url.clone();
        let activity = self.activity.clone();
        let loaded_specifiers = self.loaded_specifiers.clone();
        loaded_specifiers.borrow_mut().push(url.clone());
        // Register before returning the future. The lifecycle can inspect the
        // runtime between deno_core accepting the load and first polling it.
        // Keeping the guard inside the future makes cancellation/navigation
        // decrement the count through Drop as well as success and failure.
        let activity_guard = is_dyn_import.then(|| activity.begin());
        let page_network = match self.page_state.as_ref() {
            Some(weak) => (|| {
                let state = weak
                    .upgrade()
                    .ok_or_else(|| "Module loader page state was dropped".to_string())?;
                let state = state
                    .try_borrow()
                    .map_err(|_| "Module loader page state is already borrowed".to_string())?;
                let client = state
                    .http_client
                    .clone()
                    .ok_or_else(|| "No http_client wired to module loader".to_string())?;
                // Fork: in stealth mode an ES module must be fetched over the
                // same transport as the document. Upstream sends it through the
                // plain reqwest client, so a `type="module"` script arrives with
                // a different TLS fingerprint and none of the browser identity
                // headers, while the HTML that referenced it came over wreq.
                // That cross-transport mismatch is trivially detectable.
                #[cfg(feature = "stealth")]
                let stealth = state.stealth_client.clone();
                #[cfg(not(feature = "stealth"))]
                let stealth: Option<std::sync::Arc<()>> = None;
                Ok((client, stealth, state.callbacks.clone()))
            })(),
            None => self
                .standalone_client
                .clone()
                .map(|client| {
                    #[cfg(feature = "stealth")]
                    let stealth = None;
                    #[cfg(not(feature = "stealth"))]
                    let stealth: Option<std::sync::Arc<()>> = None;
                    (client, stealth, None)
                })
                .ok_or_else(|| "No network context wired to module loader".to_string()),
        };

        ModuleLoadResponse::Async(Pin::from(Box::new(async move {
            // Counted with the other bridge traffic: module graph loading is the
            // phase that dominates a slow page, so its share needs a number.
            let _module_timer = crate::ops::dom_profile::Timer::new("net:module");
            // deno_core propagates `is_dyn_import` to every dependency edge in
            // the recursive graph, so this excludes parser-discovered/static
            // graphs without losing descendant fetches of a lazy graph.
            let _activity_guard = activity_guard;
            tracing::debug!(
                "Loading ES module: {} (proxy: {})",
                url,
                proxy_url.as_deref().unwrap_or("direct")
            );

            match page_network {
                Ok((client, stealth, callbacks)) => {
                    let requested = ModuleSpecifier::parse(&url)
                        .map_err(|e| io_err(format!("Invalid module URL {}: {}", url, e)))?;
                    let request =
                        obscura_net::ResourceRequest::module_script(&document_url, &referrer);
                    #[cfg(feature = "stealth")]
                    let resp = match stealth {
                        Some(stealth) => stealth
                            .fetch_resource_with_callbacks(
                                &requested,
                                request,
                                callbacks.as_deref(),
                            )
                            .await,
                        None => {
                            client
                                .fetch_resource_with_callbacks(
                                    &requested,
                                    request,
                                    callbacks.as_deref(),
                                )
                                .await
                        }
                    }
                    .map_err(|e| io_err(format!("Failed to fetch module {}: {}", url, e)))?;
                    #[cfg(not(feature = "stealth"))]
                    let resp = {
                        let _ = &stealth;
                        client
                            .fetch_resource_with_callbacks(
                                &requested,
                                request,
                                callbacks.as_deref(),
                            )
                            .await
                            .map_err(|e| io_err(format!("Failed to fetch module {}: {}", url, e)))?
                    };
                    if !(200..=299).contains(&resp.status) {
                        return Err(io_err(format!(
                            "Module {} returned HTTP {}",
                            url, resp.status
                        )));
                    }
                    let found = ModuleSpecifier::parse(resp.url.as_str()).map_err(|e| {
                        io_err(format!("Invalid final module URL {}: {}", resp.url, e))
                    })?;
                    if found.as_str() != requested.as_str() {
                        loaded_specifiers
                            .borrow_mut()
                            .push(found.to_string());
                    }
                    let code = obscura_net::decode_non_html(&resp.body, resp.content_type());
                    Ok(ModuleSource::new_with_redirect(
                        deno_core::ModuleType::JavaScript,
                        ModuleSourceCode::String(code.into()),
                        &requested,
                        &found,
                        None,
                    ))
                }
                Err(error) => Err(io_err(error)),
            }
        })))
    }
}
