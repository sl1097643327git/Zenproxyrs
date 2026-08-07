//! Clash/mihomo external-controller driven node provider.
//!
//! In `NODE_PROVIDER_MODE=clash`, each configured Clash instance is treated as
//! a single zen-proxy node whose URL is the local mixed port
//! (`socks5://127.0.0.1:7890`). The real exit IP is chosen *inside* Clash by
//! its Selector group. When a node is rate-limited or errors, the coordinator
//! asks the owning Clash to switch to a different internal node (new exit IP)
//! before the node is re-probed.
//!
//! Two invariants are maintained across all instances:
//!
//! - `in_use`: the internal node currently selected by each instance. No two
//!   instances may select the same internal node (same exit IP), so a switch
//!   candidate is always filtered against every other instance's selection.
//!   Candidate selection and the `in_use` reservation are performed in one
//!   std Mutex critical section (no `await` inside), so concurrent switches
//!   cannot assign the same node to two instances.
//! - `invalid`: internal nodes that failed probing / were rate-limited, kept
//!   per instance with a timestamp. Entries older than `invalid_ttl` are
//!   dropped so a quota that resets (e.g. daily) can be retried later.
//!
//! The in-memory reservation alone only protects the node being switched
//! *to*; the node an instance currently sits on is only protected while its
//! reservation is active. If a switch fails and the rollback runs after
//! another instance already selected, both physical Clash selectors could end
//! up on the same node. A `tokio::sync::Mutex` transaction lock therefore
//! serializes the whole switch — reservation, HTTP switch call, and rollback
//! — across all instances, so another instance's selection always observes
//! the confirmed or rolled-back state, never an in-flight one.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

/// A single Clash/mihomo instance: its external-controller endpoint plus the
/// local mixed port that zen-proxy treats as a node.
#[derive(Debug, Clone)]
pub struct ClashInstance {
    /// External-controller base URL, e.g. `http://127.0.0.1:9090`.
    pub api_url: String,
    /// Optional `secret` configured in Clash; sent as `Authorization: Bearer`.
    pub api_secret: Option<String>,
    /// Selector group name to drive, e.g. `Proxy` or `GLOBAL`.
    pub group_name: String,
    /// Local node URL, e.g. `socks5://127.0.0.1:7890`.
    pub proxy_url: String,
}

#[derive(Debug)]
pub enum ClashError {
    /// Clash API HTTP request failed (transport-level).
    Http(reqwest::Error),
    /// Clash API returned a non-success status or malformed payload.
    Api(String),
    /// The configured group is not a Selector (or does not exist).
    NotSelector(String),
    /// No candidate left after filtering in-use and invalid nodes.
    NoCandidate,
}

impl std::fmt::Display for ClashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(err) => write!(f, "clash api request failed: {err}"),
            Self::Api(msg) => write!(f, "clash api error: {msg}"),
            Self::NotSelector(group) => write!(f, "group {group} is not a Selector"),
            Self::NoCandidate => write!(
                f,
                "no switch candidate after excluding in-use/invalid nodes"
            ),
        }
    }
}

impl std::error::Error for ClashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for ClashError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err)
    }
}

/// Outcome of a successful internal-node switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchResult {
    /// Internal node selected *before* the switch.
    pub from: String,
    /// Internal node selected *after* the switch.
    pub to: String,
}

struct ClashState {
    /// Per-instance currently selected internal node name.
    in_use: Vec<Option<String>>,
    /// Per-instance invalid internal node names with the moment they were
    /// marked and a machine-readable failure reason (see
    /// [`crate::pool::probe_period::ProbeFailReason::as_str`]).
    invalid: Vec<Vec<(String, Instant, String)>>,
    invalid_ttl: Duration,
    max_attempts: usize,
}

/// Coordinator over all configured Clash instances.
pub struct ClashCoordinator {
    instances: Vec<ClashInstance>,
    state: Mutex<ClashState>,
    /// Serializes select/reserve + HTTP switch + rollback across all
    /// instances (see module docs). An *async* mutex so the lock may be held
    /// across the awaiting switch call without blocking the runtime and
    /// without holding a std Mutex across an `await`.
    switch_lock: tokio::sync::Mutex<()>,
    http: reqwest::Client,
}

impl ClashCoordinator {
    pub fn new(instances: Vec<ClashInstance>, max_attempts: usize, invalid_ttl: Duration) -> Arc<Self> {
        let count = instances.len();
        Arc::new(Self {
            instances,
            state: Mutex::new(ClashState {
                in_use: vec![None; count],
                invalid: vec![Vec::new(); count],
                invalid_ttl,
                max_attempts: max_attempts.max(1),
            }),
            switch_lock: tokio::sync::Mutex::new(()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build clash api client"),
        })
    }

    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    /// Build a coordinator from zen-proxy config lists. `api_urls`,
    /// `secrets`, `proxy_urls` and `group_names` are aligned by index; a
    /// missing secret is treated as "no secret", and a missing group name
    /// falls back to the first entry (then `Proxy`).
    ///
    /// The number of instances equals `proxy_urls.len()`: one Clash listener
    /// (local port) is one zen-proxy node. When `api_urls` is shorter than
    /// `proxy_urls` (multiple listeners on a single Clash core), the trailing
    /// instances reuse the last API entry — the common
    /// "one GUI.for.Clash core with several listener ports" layout.
    pub fn from_config(
        api_urls: &[String],
        api_secrets: &[String],
        proxy_urls: &[String],
        group_names: &[String],
        max_attempts: u32,
        invalid_ttl_secs: u64,
    ) -> Option<Arc<Self>> {
        if proxy_urls.is_empty() {
            return None;
        }
        let fallback_group = group_names
            .first()
            .cloned()
            .unwrap_or_else(|| "Proxy".to_string());
        let last_api = api_urls.len().saturating_sub(1);
        let last_secret = api_secrets.len().saturating_sub(1);
        let instances = proxy_urls
            .iter()
            .enumerate()
            .map(|(idx, proxy)| ClashInstance {
                api_url: api_urls
                    .get(idx)
                    .or_else(|| api_urls.get(last_api))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
                api_secret: api_secrets
                    .get(idx)
                    .or_else(|| api_secrets.get(last_secret))
                    .map(|s| s.trim().to_string()),
                group_name: group_names
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| fallback_group.clone()),
                proxy_url: proxy.trim().to_string(),
            })
            .collect::<Vec<_>>();
        Some(Self::new(
            instances,
            max_attempts as usize,
            Duration::from_secs(invalid_ttl_secs),
        ))
    }

    /// Discover Selector group names exposed by a Clash API, in sorted order
    /// so the mapping is deterministic. Groups that cannot actually route
    /// traffic through provider nodes are skipped: `GLOBAL` (mihomo's reserved
    /// top-level group) and groups whose `all` members are all built-ins
    /// (`DIRECT`, `REJECT`, ...). Callers align instances by index; explicit
    /// `CLASH_GROUP_NAMES` still wins over this.
    pub async fn discover_selector_groups(
        api_url: &str,
        api_secret: Option<&str>,
    ) -> Result<Vec<String>, ClashError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(ClashError::Http)?;
        let url = format!("{}/proxies", api_url.trim_end_matches('/'));
        let mut req = http.get(&url);
        if let Some(secret) = api_secret {
            if !secret.is_empty() {
                req = req.header(AUTHORIZATION, format!("Bearer {secret}"));
            }
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(ClashError::Api(format!(
                "GET /proxies returned {status}: {body}"
            )));
        }

        let proxies = body.get("proxies").and_then(Value::as_object);
        Ok(proxies
            .map(usable_selector_groups)
            .unwrap_or_default())
    }

    /// Resolve the group name for each `proxy_url` from a mihomo `config.yaml`
    /// `listeners` section, matched by listening port.
    ///
    /// This is the only way to recover the exact "listener port -> Selector
    /// group" binding (GUI tools such as GUI.for.Clash emit `listeners:` with a
    /// `proxy:` per port; the external-controller API does not expose it). The
    /// returned list is aligned with `proxy_urls` so it can be passed straight
    /// to `from_config`. Explicit `CLASH_GROUP_NAMES` still wins over this.
    pub fn discover_groups_from_config_file(
        config_file: &str,
        proxy_urls: &[String],
    ) -> Result<Vec<String>, ClashError> {
        let content = std::fs::read_to_string(config_file).map_err(|e| {
            ClashError::Api(format!("cannot read clash config file {config_file}: {e}"))
        })?;
        Self::groups_from_config_yaml(&content, proxy_urls)
    }

    /// Pure form of [`discover_groups_from_config_file`] for testability:
    /// parse the `listeners` section of a mihomo YAML string and return the
    /// `proxy` (group) name for each `proxy_url` port, in order.
    pub fn groups_from_config_yaml(
        yaml: &str,
        proxy_urls: &[String],
    ) -> Result<Vec<String>, ClashError> {
        let doc: serde_yaml::Value = serde_yaml::from_str(yaml)
            .map_err(|e| ClashError::Api(format!("cannot parse clash config yaml: {e}")))?;
        let listeners = doc
            .get("listeners")
            .and_then(serde_yaml::Value::as_sequence)
            .ok_or_else(|| ClashError::Api("config yaml has no 'listeners' section".into()))?;

        let mut by_port: std::collections::HashMap<u16, String> = std::collections::HashMap::new();
        for listener in listeners {
            let port = listener.get("port").and_then(serde_yaml::Value::as_u64);
            let proxy = listener.get("proxy").and_then(serde_yaml::Value::as_str);
            if let (Some(port), Some(proxy)) = (port, proxy) {
                by_port.insert(port as u16, proxy.to_string());
            }
        }
        if by_port.is_empty() {
            return Err(ClashError::Api(
                "config yaml 'listeners' has no port/proxy entries".into(),
            ));
        }

        let mut groups = Vec::with_capacity(proxy_urls.len());
        for url in proxy_urls {
            let port = reqwest::Url::parse(url)
                .ok()
                .and_then(|u| u.port())
                .ok_or_else(|| {
                    ClashError::Api(format!("cannot extract port from proxy url {url}"))
                })?;
            match by_port.get(&port) {
                Some(group) => groups.push(group.clone()),
                None => {
                    return Err(ClashError::Api(format!(
                        "no listener on port {port} in config yaml (for proxy {url})"
                    )));
                }
            }
        }
        Ok(groups)
    }

    pub fn max_attempts(&self) -> usize {
        self.state.lock().unwrap().max_attempts
    }

    pub fn proxy_urls(&self) -> Vec<String> {
        self.instances.iter().map(|i| i.proxy_url.clone()).collect()
    }

    /// Last-known selected internal node for an instance, from the cached
    /// `in_use` state. Synchronous — safe to call from admin handlers that
    /// must not block an async runtime.
    pub fn current_node_cached(&self, idx: usize) -> Option<String> {
        self.state.lock().unwrap().in_use.get(idx).cloned().flatten()
    }

    /// Best-effort live refresh of the cached `in_use` selections for all
    /// instances by querying each Clash API. Failures leave the previous
    /// cache intact. Call this from an async context (e.g. an admin handler)
    /// before reading [`Self::current_node_cached`].
    pub async fn refresh_current_nodes(&self) {
        for idx in 0..self.instances.len() {
            if let Ok(Some(node)) = self.current_internal_node(idx).await {
                let mut state = self.state.lock().unwrap();
                if let Some(slot) = state.in_use.get_mut(idx) {
                    *slot = Some(node);
                }
            }
        }
    }

    /// Startup reconciliation: guarantee no two Clash instances serve traffic
    /// through the same internal node.
    ///
    /// Queries each instance's live current node (fresh [`Self::refresh_current_nodes`]),
    /// keeps the first instance's selection, and drives every later instance
    /// that reports a duplicate through [`Self::switch_internal_node`], so the
    /// normal select/reserve/switch/rollback invariants apply: a duplicate
    /// instance is never switched onto a node another instance is already
    /// using, and a failed switch rolls its reservation back. Best-effort:
    /// instances whose Clash API does not answer, or for which no distinct
    /// candidate exists, are left untouched and logged.
    pub async fn ensure_distinct_nodes(&self) {
        let count = self.instances.len();
        if count < 2 {
            return;
        }
        self.refresh_current_nodes().await;
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for idx in 0..count {
            let Some(node) = self.current_node_cached(idx) else {
                continue;
            };
            match seen.entry(node.clone()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(idx);
                }
                std::collections::hash_map::Entry::Occupied(e) => {
                    let first = *e.get();
                    tracing::warn!(
                        instance = idx,
                        first_instance = first,
                        node = %node,
                        "clash startup: duplicate current node across instances; switching later instance"
                    );
                    match self.switch_internal_node(idx).await {
                        Ok(Some(result)) => {
                            tracing::info!(
                                instance = idx,
                                from = %result.from,
                                to = %result.to,
                                "clash startup: switched instance to a distinct internal node"
                            );
                        }
                        Ok(None) => {
                            tracing::warn!(
                                instance = idx,
                                node = %node,
                                "clash startup: no distinct candidate available; instance stays on shared node"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                instance = idx,
                                error = %err,
                                "clash startup: failed to deduplicate instance"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Index of the instance whose local proxy URL equals `url`, if any.
    pub fn instance_for_proxy_url(&self, url: &str) -> Option<usize> {
        self.instances
            .iter()
            .position(|inst| inst.proxy_url == url)
    }

    /// Internal node currently selected for instance `idx` (cached knowledge;
    /// refreshed by `switch_internal_node`).
    pub fn current_in_use(&self, idx: usize) -> Option<String> {
        self.state.lock().unwrap().in_use.get(idx).cloned().flatten()
    }

    /// Mark `node` as invalid for instance `idx` (e.g. it was rate-limited or a
    /// probe failed through it). `reason` is a machine-readable label
    /// (typically [`crate::pool::probe_period::ProbeFailReason::as_str`]) that
    /// the admin failed-nodes list surfaces. The entry expires after
    /// `invalid_ttl`.
    pub fn mark_invalid(&self, idx: usize, node: &str, reason: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(list) = state.invalid.get_mut(idx) {
            list.retain(|(name, _, _)| name != node);
            list.push((node.to_string(), Instant::now(), reason.to_string()));
        }
    }

    /// Clear invalid entries for instance `idx` (called when a probe succeeds,
    /// so the freshly working node is not skipped on the next switch).
    pub fn clear_invalid(&self, idx: usize) {
        if let Some(list) = self.state.lock().unwrap().invalid.get_mut(idx) {
            list.clear();
        }
    }

    /// Clear the invalid blacklist for **all** instances. Used by the admin
    /// "clear stale cache" action: stale invalid entries can pin every instance
    /// onto a single candidate, so flushing them lets the next switch retry the
    /// full internal node set. Returns the number of invalid entries dropped.
    pub fn clear_all_invalid(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        let mut cleared = 0usize;
        for list in state.invalid.iter_mut() {
            cleared += list.len();
            list.clear();
        }
        cleared
    }

    /// Switch instance `idx` to a different internal node.
    ///
    /// Returns `Ok(Some(SwitchResult))` when a switch happened,
    /// `Ok(None)` when the instance is already on the only usable node and
    /// nothing needed to change, and `Err` on API failure / no candidate.
    ///
    /// The live group state is fetched first (network I/O, outside the state
    /// lock); then refresh + candidate filtering + target reservation, the
    /// HTTP switch call and the failure rollback all run inside one
    /// `tokio::sync::Mutex` transaction, serialized against every other
    /// instance's switch. This closes the failure interleaving that the
    /// in-memory reservation alone cannot: without it, instance A could
    /// reserve a target, fail its PUT, and while A's slot still holds the
    /// stale reservation instance B could select A's *old* (physically still
    /// selected) node before A's rollback runs — leaving both physical Clash
    /// selectors on the same node. Because the rollback happens before the
    /// lock is released, B's selection always sees A's true physical node.
    pub async fn switch_internal_node(&self, idx: usize) -> Result<Option<SwitchResult>, ClashError> {
        let instance = self.instances.get(idx).ok_or_else(|| {
            ClashError::Api(format!("clash instance index {idx} out of range"))
        })?;
        let (all, now) = self.fetch_group(&instance).await?;

        // Serialize select/reserve -> HTTP switch -> rollback across all
        // instances. Async mutex: held across the awaiting switch call, never
        // a std::sync::Mutex across an await point.
        let _guard = self.switch_lock.lock().await;

        let target = {
            let mut state = self.state.lock().unwrap();
            Self::select_and_reserve(&mut state, idx, &all, &now)
        };
        let Some(target) = target else {
            return Ok(None);
        };

        if let Err(e) = self.switch_to(&instance, &target).await {
            // Roll back the reservation so the cached selection stays in sync
            // with reality and the failed target is not falsely reported as
            // in-use (which would double-book a node for other instances).
            // Still inside the transaction lock: no other instance can select
            // between the failed PUT and this rollback.
            let mut state = self.state.lock().unwrap();
            if let Some(slot) = state.in_use.get_mut(idx) {
                *slot = Some(now.clone());
            }
            return Err(e);
        }

        Ok(Some(SwitchResult {
            from: now,
            to: target,
        }))
    }

    /// Pick and reserve a switch target for instance `idx` atomically.
    ///
    /// Must be called while holding the `ClashCoordinator::state` mutex and
    /// never across an `await`. Performs, in one critical section:
    ///
    /// 1. refresh `in_use[idx]` to the node the Clash API reported (`now`),
    /// 2. filter `all` against the current node, every *other* instance's
    ///    reservation and the invalid list (see [`pick_candidate`]),
    /// 3. reserve the chosen target in `in_use[idx]`.
    ///
    /// Because selection and reservation share a single lock, any concurrent
    /// caller either observes this reservation or serializes behind it — no
    /// two instances can ever be assigned the same internal node. Returns the
    /// reserved target, or `None` when every other node is in use elsewhere /
    /// invalid (leaving `in_use[idx]` refreshed to `now`).
    fn select_and_reserve(
        state: &mut ClashState,
        idx: usize,
        all: &[String],
        now: &str,
    ) -> Option<String> {
        if let Some(slot) = state.in_use.get_mut(idx) {
            *slot = Some(now.to_string());
        }

        let ttl = state.invalid_ttl;
        let invalid: std::collections::HashSet<&str> = state
            .invalid
            .get(idx)
            .map(|list| {
                list.iter()
                    .filter(|(_, at, _)| at.elapsed() < ttl)
                    .map(|(name, _, _)| name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        let mut others_in_use: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (i, slot) in state.in_use.iter().enumerate() {
            if i != idx {
                if let Some(name) = slot {
                    others_in_use.insert(name.as_str());
                }
            }
        }

        let target = pick_candidate(
            all,
            now,
            &others_in_use.into_iter().collect::<Vec<_>>(),
            &invalid.into_iter().collect::<Vec<_>>(),
        )?;

        if let Some(slot) = state.in_use.get_mut(idx) {
            *slot = Some(target.clone());
        }
        Some(target)
    }

    async fn fetch_group(&self, instance: &ClashInstance) -> Result<(Vec<String>, String), ClashError> {
        let url = format!("{}/proxies", instance.api_url.trim_end_matches('/'));
        let mut req = self.http.get(&url);
        if let Some(secret) = &instance.api_secret {
            if !secret.is_empty() {
                req = req.header(AUTHORIZATION, format!("Bearer {secret}"));
            }
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            return Err(ClashError::Api(format!(
                "GET /proxies returned {status}: {body}"
            )));
        }

        let proxies = body.get("proxies").and_then(Value::as_object);
        let group = proxies
            .and_then(|p| p.get(&instance.group_name))
            .ok_or_else(|| {
                ClashError::Api(format!("group '{}' not found in /proxies", instance.group_name))
            })?;

        if group.get("type").and_then(Value::as_str) != Some("Selector") {
            return Err(ClashError::NotSelector(instance.group_name.clone()));
        }

        let all = group
            .get("all")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let now = group
            .get("now")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if all.is_empty() || now.is_empty() {
            return Err(ClashError::Api(format!(
                "group '{}' has no usable node list (all={all:?}, now={now:?})",
                instance.group_name
            )));
        }

        Ok((all, now))
    }

    /// Query the currently selected internal node of a Clash instance's
    /// Selector group (read-only, never switches). Returns `Ok(None)` when the
    /// instance index is out of range.
    pub async fn current_internal_node(
        &self,
        idx: usize,
    ) -> Result<Option<String>, ClashError> {
        let Some(instance) = self.instances.get(idx) else {
            return Ok(None);
        };
        let (_all, now) = self.fetch_group(instance).await?;
        if now.is_empty() {
            Ok(None)
        } else {
            Ok(Some(now))
        }
    }

    async fn switch_to(&self, instance: &ClashInstance, target: &str) -> Result<(), ClashError> {
        // See `switch_url` for why the base has no trailing slash.
        let url = switch_url(&instance.api_url, &instance.group_name)?;
        // Snapshot the exact URL that will be sent (percent-encoding applied by
        // `path_segments_mut`) before the `Url` is moved into the request.
        let request_url = url.as_str().to_string();
        let body = serde_json::json!({ "name": target }).to_string();
        let mut req = self
            .http
            .put(url)
            .header(CONTENT_TYPE, "application/json")
            .body(body.clone());
        if let Some(secret) = &instance.api_secret {
            if !secret.is_empty() {
                req = req.header(AUTHORIZATION, format!("Bearer {secret}"));
            }
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            // Consume the response body exactly once (never read it again) so
            // the failed PUT can be diagnosed and the connection is released.
            let response_body = resp
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));
            // Debug formatting (`?`) escapes non-ASCII and control characters
            // (`\u{..}`, `\n`, ...) so invisible characters in the node name
            // and the exact percent-encoding of the group path are visible.
            tracing::warn!(
                url = ?request_url,
                body = ?body,
                status = %status,
                response_body = ?response_body,
                "clash switch PUT not successful"
            );
            return Err(ClashError::Api(format!(
                "PUT /proxies/{} -> {} returned {}: {}",
                instance.group_name, target, status, response_body
            )));
        }
        Ok(())
    }

    /// Per-instance invalid internal nodes snapshot for the admin failed-nodes
    /// list. Returns one entry per invalid node with the owning instance's
    /// proxy URL, the node name, the machine-readable failure reason and the
    /// seconds remaining in the blacklist TTL (0 when already expired / about
    /// to expire).
    pub fn invalid_snapshot(&self) -> Vec<serde_json::Value> {
        let state = self.state.lock().unwrap();
        let ttl = state.invalid_ttl;
        let mut out = Vec::new();
        for (idx, inst) in self.instances.iter().enumerate() {
            if let Some(list) = state.invalid.get(idx) {
                for (name, at, reason) in list {
                    let remaining = ttl.saturating_sub(at.elapsed());
                    if remaining.is_zero() {
                        continue; // expired entries are filtered by select_and_reserve anyway
                    }
                    out.push(serde_json::json!({
                        "node_id": format!("clash:{}", inst.proxy_url),
                        "url": crate::ledger::LedgerEvent::redact_node_url(&inst.proxy_url),
                        "instance": inst.proxy_url,
                        "group": inst.group_name,
                        "state": "clash_invalid",
                        "reason": reason,
                        "node": name,
                        "ttl_secs": remaining.as_secs(),
                    }));
                }
            }
        }
        out
    }

    /// Snapshot for admin visibility.
    pub fn snapshot(&self) -> Value {
        let state = self.state.lock().unwrap();
        let instances = self
            .instances
            .iter()
            .enumerate()
            .map(|(idx, inst)| {
                let now: Option<String> = state.in_use.get(idx).cloned().flatten();
                let invalid: Vec<String> = state
                    .invalid
                    .get(idx)
                    .map(|list| list.iter().map(|(n, _, _)| n.clone()).collect())
                    .unwrap_or_default();
                serde_json::json!({
                    "api_url": inst.api_url,
                    "proxy_url": inst.proxy_url,
                    "group": inst.group_name,
                    "in_use": now,
                    "invalid": invalid,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "mode": "clash",
            "instances": instances,
        })
    }
}

/// Built-in mihomo proxy names that never route through a real exit node.
fn is_builtin_proxy(name: &str) -> bool {
    matches!(
        name,
        "DIRECT" | "REJECT" | "REJECT-DROP" | "PASS" | "PASS-RULE" | "COMPATIBLE"
    )
}

/// Select the Selector groups from a `/proxies` payload that can actually
/// route traffic through provider nodes, sorted by name.
///
/// Excludes `GLOBAL` (mihomo's reserved top-level group, always present and
/// not bound to a listener) and groups whose `all` members are only built-ins
/// (e.g. a "direct" or "block" selector), which are useless for exit switching.
pub fn usable_selector_groups(proxies: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut groups = Vec::new();
    for (name, info) in proxies {
        if info.get("type").and_then(Value::as_str) != Some("Selector") {
            continue;
        }
        if name == "GLOBAL" {
            continue;
        }
        let all = info
            .get("all")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // Keep the group only if at least one member is a real routable
        // node/group (not purely built-ins like DIRECT/REJECT).
        if all.iter().any(|member| !is_builtin_proxy(member)) {
            groups.push(name.clone());
        }
    }
    groups.sort();
    groups
}

/// Build the PUT URL for switching a Clash group:
/// `{api_url}/proxies/{group}`.
///
/// The group name is appended through [`Url::path_segments_mut`], so it is
/// percent-encoded per RFC 3986 (e.g. a space becomes `%20`). The base is
/// deliberately built **without** a trailing slash: `Url` reads a trailing `/`
/// as an empty final path segment, so `path_segments_mut().push(group)` would
/// append the group onto that empty segment and yield `/proxies//Group-A`,
/// which mihomo rejects with `404 Resource not found`.
pub fn switch_url(api_url: &str, group: &str) -> Result<reqwest::Url, ClashError> {
    let mut url = reqwest::Url::parse(&format!("{}/proxies", api_url.trim_end_matches('/')))
        .map_err(|e| ClashError::Api(format!("invalid clash api url: {e}")))?;
    url.path_segments_mut()
        .map_err(|_| ClashError::Api("clash api url cannot be a base".into()))?
        .push(group);
    Ok(url)
}

/// Choose the next internal node for a switch.
///
/// Filters `all` (group node list) down to candidates that are:
/// - not the currently selected node (`now`),
/// - not selected by any *other* instance (`others_in_use`),
/// - not in the invalid blacklist (`invalid`).
///
/// Prefers nodes that are neither in use nor invalid; falls back to any node
/// that is merely not the current selection when the pool is small.
pub fn pick_candidate(
    all: &[String],
    now: &str,
    others_in_use: &[&str],
    invalid: &[&str],
) -> Option<String> {
    if all.is_empty() {
        return None;
    }

    let in_use_set: std::collections::HashSet<&str> = others_in_use.iter().copied().collect();
    let invalid_set: std::collections::HashSet<&str> = invalid.iter().copied().collect();

    // First pass: prefer nodes that are clean (not in use elsewhere, not invalid).
    for node in all {
        if node == now {
            continue;
        }
        if in_use_set.contains(node.as_str()) {
            continue;
        }
        if invalid_set.contains(node.as_str()) {
            continue;
        }
        return Some(node.clone());
    }

    // Second pass: allow invalid-but-not-in-use nodes so a cluster with a tiny
    // group still rotates instead of stalling (the probe will re-validate).
    for node in all {
        if node == now {
            continue;
        }
        if in_use_set.contains(node.as_str()) {
            continue;
        }
        return Some(node.clone());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pick_candidate_prefers_clean() {
        let all = vec!["a".into(), "b".into(), "c".into()];
        let got = pick_candidate(&all, "a", &["b"], &[]).unwrap();
        assert_eq!(got, "c");
    }

    #[test]
    fn test_pick_candidate_skips_current() {
        let all = vec!["a".into(), "b".into()];
        let got = pick_candidate(&all, "a", &[], &[]).unwrap();
        assert_eq!(got, "b");
    }

    #[test]
    fn test_pick_candidate_skips_invalid_first_pass() {
        let all = vec!["a".into(), "b".into(), "c".into()];
        // b is invalid, c is in use elsewhere -> second pass returns b (rotates).
        let got = pick_candidate(&all, "a", &["c"], &["b"]).unwrap();
        assert_eq!(got, "b");
    }

    #[test]
    fn test_pick_candidate_none_when_single_node() {
        let all = vec!["a".into()];
        assert!(pick_candidate(&all, "a", &[], &[]).is_none());
    }

    #[test]
    fn test_pick_candidate_empty() {
        assert!(pick_candidate(&[], "a", &[], &[]).is_none());
    }

    fn proxies_map(pairs: &[(String, Value)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn selector(name: &str, all: &[&str]) -> (String, Value) {
        (name.to_string(), serde_json::json!({ "type": "Selector", "all": all }))
    }

    #[test]
    fn test_usable_selector_groups_keeps_routable() {
        let map = proxies_map(&[
            selector("🚀 节点选择", &["node-a", "node-b", "🎈 自动选择"]),
            selector("🎯 全球直连", &["DIRECT", "REJECT"]),
            selector("🛑 全球拦截", &["REJECT", "DIRECT"]),
            selector("GLOBAL", &["🚀 节点选择"]),
            selector("🐟 漏网之鱼", &["🚀 节点选择", "🎯 全球直连"]),
        ]);
        let got = usable_selector_groups(&map);
        assert_eq!(got, vec!["🐟 漏网之鱼", "🚀 节点选择"]);
    }

    #[test]
    fn test_usable_selector_groups_skips_non_selector() {
        let map = proxies_map(&[
            selector("select-a", &["n1"]),
            ("url-test".to_string(), serde_json::json!({ "type": "URLTest", "all": ["n1", "n2"] })),
            ("fallback".to_string(), serde_json::json!({ "type": "Fallback", "all": ["n1"] })),
        ]);
        let got = usable_selector_groups(&map);
        assert_eq!(got, vec!["select-a"]);
    }

    #[test]
    fn test_usable_selector_groups_empty() {
        assert!(usable_selector_groups(&serde_json::Map::new()).is_empty());
    }

    const CONFIG_YAML: &str = r#"
mixed-port: 20112
external-controller: 127.0.0.1:20113
secret: abc
listeners:
  - name: strategy-group-listener-Mixed-7898
    type: mixed
    port: 7898
    listen: 127.0.0.1
    proxy: "🚀 节点选择"
  - name: strategy-group-listener-Mixed-7899
    type: mixed
    port: 7899
    listen: 127.0.0.1
    proxy: "🐟 漏网之鱼"
"#;

    #[test]
    fn test_groups_from_config_yaml_matches_port_to_group() {
        let urls = vec![
            "socks5://127.0.0.1:7898".to_string(),
            "socks5://127.0.0.1:7899".to_string(),
        ];
        let got = ClashCoordinator::groups_from_config_yaml(CONFIG_YAML, &urls).unwrap();
        assert_eq!(got, vec!["🚀 节点选择", "🐟 漏网之鱼"]);
    }

    #[test]
    fn test_groups_from_config_yaml_no_listeners() {
        let urls = vec!["socks5://127.0.0.1:7898".to_string()];
        assert!(ClashCoordinator::groups_from_config_yaml("mixed-port: 1", &urls).is_err());
    }

    #[test]
    fn test_groups_from_config_yaml_port_missing() {
        let urls = vec!["socks5://127.0.0.1:9999".to_string()];
        assert!(ClashCoordinator::groups_from_config_yaml(CONFIG_YAML, &urls).is_err());
    }

    #[test]
    fn test_groups_from_config_yaml_extra_listeners_ignored() {
        let urls = vec!["socks5://127.0.0.1:7898".to_string()];
        let got = ClashCoordinator::groups_from_config_yaml(CONFIG_YAML, &urls).unwrap();
        assert_eq!(got, vec!["🚀 节点选择"]);
    }

    #[test]
    fn test_switch_url_single_slash() {
        // Regression: the switch URL must contain exactly one slash before the
        // group. `http://127.0.0.1:33000/proxies/` + path_segments_mut().push()
        // previously produced `/proxies//Group-A`, which mihomo rejects.
        let url = switch_url("http://127.0.0.1:33000", "Group-A").unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:33000/proxies/Group-A");
    }

    #[test]
    fn test_switch_url_strips_base_trailing_slash() {
        let url = switch_url("http://127.0.0.1:33000/", "Group-A").unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:33000/proxies/Group-A");
    }

    #[test]
    fn test_switch_url_percent_encodes_group() {
        let url = switch_url("http://127.0.0.1:33000", "Group A+B").unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:33000/proxies/Group%20A+B"
        );
    }

    #[test]
    fn test_switch_url_keeps_base_path_prefix() {
        let url = switch_url("http://127.0.0.1:33000/clash", "Group-A").unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:33000/clash/proxies/Group-A"
        );
    }

    #[test]
    fn test_switch_url_rejects_invalid_base() {
        assert!(switch_url("not a url", "Group-A").is_err());
    }

    // --- Atomic selection + reservation across instances ---

    #[test]
    fn test_select_and_reserve_two_instances_get_distinct_nodes() {
        let mut state = ClashState {
            in_use: vec![None; 2],
            invalid: vec![Vec::new(); 2],
            invalid_ttl: Duration::from_secs(60),
            max_attempts: 1,
        };
        let all = vec!["node-a".into(), "node-b".into(), "node-c".into()];

        // Instance 0 reserves first: must not stay on the current node.
        let t0 = ClashCoordinator::select_and_reserve(&mut state, 0, &all, "node-a").unwrap();
        assert_ne!(t0, "node-a");

        // Instance 1 reserves second: it runs the same critical-section logic
        // as instance 0, so it must observe 0's reservation and pick a
        // distinct node.
        let t1 = ClashCoordinator::select_and_reserve(&mut state, 1, &all, "node-a").unwrap();
        assert_ne!(t0, t1);
        assert_ne!(t1, "node-a");

        // Both reservations are recorded and disjoint.
        assert_eq!(state.in_use[0].as_deref(), Some(t0.as_str()));
        assert_eq!(state.in_use[1].as_deref(), Some(t1.as_str()));
    }

    #[test]
    fn test_select_and_reserve_no_candidate_when_only_one_node() {
        let mut state = ClashState {
            in_use: vec![None; 1],
            invalid: vec![Vec::new(); 1],
            invalid_ttl: Duration::from_secs(60),
            max_attempts: 1,
        };
        let all = vec!["node-a".into()];
        // Single node == current node -> nothing to switch to; the cached
        // selection is still refreshed to reality.
        assert!(ClashCoordinator::select_and_reserve(&mut state, 0, &all, "node-a").is_none());
        assert_eq!(state.in_use[0].as_deref(), Some("node-a"));
    }

    // --- Full switch path against a mock Clash API ---

    /// Minimal in-test HTTP server speaking just enough of the Clash
    /// external-controller API for `switch_internal_node`: `GET /proxies`
    /// returns one Selector group; `PUT /proxies/{group}` answers with
    /// `put_status`. When `fetch_barrier` is set, every `GET /proxies`
    /// response is held until that many requests have arrived, deterministically
    /// forcing the "both instances fetched, then both raced to reserve"
    /// interleaving the TOCTOU fix protects against. `get_delay` delays the
    /// `GET` response and `put_delay` the `PUT` response, letting a test pin
    /// down the order in which instances reach their reservation while a
    /// failing switch stays in flight.
    async fn spawn_mock_clash_core(
        now: &'static str,
        all: &'static [&'static str],
        put_status: u16,
        fetch_barrier: Option<Arc<tokio::sync::Barrier>>,
        get_delay: Option<Duration>,
        put_delay: Option<Duration>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock clash server");
        let addr = listener.local_addr().unwrap();
        let server_barrier = fetch_barrier.clone();

        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let barrier = server_barrier.clone();
                let (now, all, put_status, get_delay, put_delay) =
                    (now, all, put_status, get_delay, put_delay);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut len = 0usize;
                    loop {
                        let n = socket.read(&mut buf[len..]).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        len += n;
                        if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                        if len >= buf.len() {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..len]);
                    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
                    let method = parts.next().unwrap_or_default();
                    let path = parts.next().unwrap_or_default();

                    let body = if method == "GET" && path == "/proxies" {
                        if let Some(delay) = get_delay {
                            tokio::time::sleep(delay).await;
                        }
                        if let Some(b) = &barrier {
                            b.wait().await;
                        }
                        let all_json = all
                            .iter()
                            .map(|n| format!("\"{n}\""))
                            .collect::<Vec<_>>()
                            .join(",");
                        format!(
                            "{{\"proxies\":{{\"Test\":{{\"type\":\"Selector\",\"all\":[{all_json}],\"now\":\"{now}\"}}}}}}"
                        )
                    } else {
                        String::new()
                    };

                    // Hold the (failing) switch open so the other instance can
                    // race it while the reservation is still active.
                    if method == "PUT" {
                        if let Some(delay) = put_delay {
                            tokio::time::sleep(delay).await;
                        }
                    }

                    let (status_line, content_type) = if method == "GET" {
                        ("200 OK", "Content-Type: application/json\r\n")
                    } else if put_status == 204 {
                        ("204 No Content", "")
                    } else {
                        ("500 Internal Server Error", "")
                    };
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });

        format!("http://{addr}")
    }

    /// Convenience wrapper: a mock server with its own two-party fetch
    /// rendezvous, returning the server URL and the barrier it waits on.
    async fn spawn_mock_clash(
        now: &'static str,
        all: &'static [&'static str],
        put_status: u16,
        rendezvous_fetches: bool,
    ) -> (String, Arc<tokio::sync::Barrier>) {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let url = spawn_mock_clash_core(
            now,
            all,
            put_status,
            rendezvous_fetches.then(|| barrier.clone()),
            None,
            None,
        )
        .await;
        (url, barrier)
    }

    #[tokio::test]
    async fn test_concurrent_switches_never_assign_same_node() {
        let (api_url, _barrier) =
            spawn_mock_clash("node-a", &["node-a", "node-b", "node-c"], 204, true).await;
        let coordinator = ClashCoordinator::new(
            vec![
                ClashInstance {
                    api_url: api_url.clone(),
                    api_secret: None,
                    group_name: "Test".into(),
                    proxy_url: "socks5://127.0.0.1:32001".into(),
                },
                ClashInstance {
                    api_url,
                    api_secret: None,
                    group_name: "Test".into(),
                    proxy_url: "socks5://127.0.0.1:32002".into(),
                },
            ],
            1,
            Duration::from_secs(60),
        );

        let (r0, r1) = tokio::join!(
            coordinator.switch_internal_node(0),
            coordinator.switch_internal_node(1),
        );

        let t0 = r0
            .expect("instance 0 switch succeeded")
            .expect("instance 0 found a candidate");
        let t1 = r1
            .expect("instance 1 switch succeeded")
            .expect("instance 1 found a candidate");
        // Two instances must never be assigned the same internal node.
        assert_ne!(t0.to, t1.to);
        assert_ne!(t0.from, t0.to);
        assert_ne!(t1.from, t1.to);
        assert_ne!(
            coordinator.current_in_use(0).unwrap(),
            coordinator.current_in_use(1).unwrap()
        );
    }

    #[tokio::test]
    async fn test_failed_switch_rolls_back_reservation() {
        let (api_url, _barrier) = spawn_mock_clash("node-a", &["node-a", "node-b"], 500, false).await;
        let coordinator = ClashCoordinator::new(
            vec![ClashInstance {
                api_url,
                api_secret: None,
                group_name: "Test".into(),
                proxy_url: "socks5://127.0.0.1:32001".into(),
            }],
            1,
            Duration::from_secs(60),
        );

        let err = coordinator.switch_internal_node(0).await.unwrap_err();
        assert!(matches!(err, ClashError::Api(_)));
        // The failed target (node-b) must not linger in the in_use state: the
        // reservation rolled back to the node the Clash API actually reports.
        assert_eq!(coordinator.current_in_use(0).as_deref(), Some("node-a"));
    }

    #[tokio::test]
    async fn test_failed_switch_plus_concurrent_switch_never_duplicates_physical() {
        // Regression for the failure interleaving the in-memory reservation
        // alone cannot prevent. Precondition: both instances already have a
        // confirmed physical node in the cache — instance 0 sits on node-a,
        // instance 1 on node-b (seeded below to match each mock's `now`).
        //
        // Instance 0 then reserves a target (node-c), and its PUT fails. While
        // 0's slot still holds the stale reservation, a concurrent instance 1
        // could select 0's *old* node-a (0's physical node, no longer in the
        // cache) before the rollback runs — leaving both physical Clash
        // selectors on node-a. The async transaction lock must serialize 1's
        // selection until after 0's rollback, so 1 observes node-a and picks
        // node-c instead.
        //
        // Instance 0's PUT is held open (200ms) so the failing switch stays in
        // flight while instance 1 switches concurrently; instance 1's GET is
        // delayed (50ms) so instance 0 deterministically reserves before
        // instance 1 can select — the window in which a duplicate used to
        // occur.
        let url0 = spawn_mock_clash_core(
            "node-a",
            &["node-a", "node-b", "node-c"],
            500,
            None,
            None,
            Some(Duration::from_millis(200)),
        )
        .await;
        let url1 = spawn_mock_clash_core(
            "node-b",
            &["node-a", "node-b", "node-c"],
            204,
            None,
            Some(Duration::from_millis(50)),
            None,
        )
        .await;

        let coordinator = ClashCoordinator::new(
            vec![
                ClashInstance {
                    api_url: url0,
                    api_secret: None,
                    group_name: "Test".into(),
                    proxy_url: "socks5://127.0.0.1:32001".into(),
                },
                ClashInstance {
                    api_url: url1,
                    api_secret: None,
                    group_name: "Test".into(),
                    proxy_url: "socks5://127.0.0.1:32002".into(),
                },
            ],
            1,
            Duration::from_secs(60),
        );
        // Seed the cache with each instance's confirmed physical node (the
        // state a coordinator has after a previous successful switch/refresh).
        {
            let mut state = coordinator.state.lock().unwrap();
            state.in_use[0] = Some("node-a".into());
            state.in_use[1] = Some("node-b".into());
        }

        let (r0, r1) = tokio::join!(
            coordinator.switch_internal_node(0),
            coordinator.switch_internal_node(1),
        );

        // Instance 0: the PUT failed, so its physical selector never left
        // node-a and the reservation rolled back to it.
        let err = r0.expect_err("instance 0's switch must fail (mock PUT 500)");
        assert!(matches!(err, ClashError::Api(_)));
        assert_eq!(coordinator.current_in_use(0).as_deref(), Some("node-a"));

        // Instance 1: switched concurrently and succeeded; it must not have
        // picked instance 0's physical node (node-a), which is exactly what a
        // duplicate would look like.
        let r1 = r1
            .expect("instance 1's switch succeeded")
            .expect("instance 1 found a candidate");
        assert_ne!(
            r1.to, "node-a",
            "instance 1 must not select instance 0's physical node"
        );
        assert_eq!(coordinator.current_in_use(1).as_deref(), Some(r1.to.as_str()));

        // Physical selections (cached in_use after confirm/rollback) are disjoint.
        assert_ne!(
            coordinator.current_in_use(0),
            coordinator.current_in_use(1),
            "two instances must never end up on the same internal node"
        );
    }

    #[tokio::test]
    async fn test_ensure_distinct_nodes_startup_dedups_duplicates() {
        // Regression: a freshly started core can leave every group on the same
        // default selection; startup reconciliation must keep the first
        // instance's node and switch later duplicates to distinct candidates.
        let (api_url, _barrier) =
            spawn_mock_clash("node-a", &["node-a", "node-b", "node-c"], 204, false).await;
        let coordinator = ClashCoordinator::new(
            vec![
                ClashInstance {
                    api_url: api_url.clone(),
                    api_secret: None,
                    group_name: "Test".into(),
                    proxy_url: "socks5://127.0.0.1:32001".into(),
                },
                ClashInstance {
                    api_url,
                    api_secret: None,
                    group_name: "Test".into(),
                    proxy_url: "socks5://127.0.0.1:32002".into(),
                },
            ],
            1,
            Duration::from_secs(60),
        );

        coordinator.ensure_distinct_nodes().await;

        // The first instance keeps its selection; the later duplicate moved
        // onto a distinct candidate (mock PUT 204 => switch confirmed).
        assert_eq!(coordinator.current_in_use(0).as_deref(), Some("node-a"));
        assert_ne!(
            coordinator.current_in_use(0),
            coordinator.current_in_use(1),
            "startup reconciliation must leave both instances on distinct nodes"
        );
        assert_ne!(coordinator.current_in_use(1).as_deref(), Some("node-a"));
    }

    #[tokio::test]
    async fn test_ensure_distinct_nodes_preserves_distinct_selections() {
        // Instances already on distinct nodes must be left untouched.
        let url0 = spawn_mock_clash_core(
            "node-a",
            &["node-a", "node-b", "node-c"],
            204,
            None,
            None,
            None,
        )
        .await;
        let url1 = spawn_mock_clash_core(
            "node-b",
            &["node-a", "node-b", "node-c"],
            204,
            None,
            None,
            None,
        )
        .await;
        let coordinator = ClashCoordinator::new(
            vec![
                ClashInstance {
                    api_url: url0,
                    api_secret: None,
                    group_name: "Test".into(),
                    proxy_url: "socks5://127.0.0.1:32001".into(),
                },
                ClashInstance {
                    api_url: url1,
                    api_secret: None,
                    group_name: "Test".into(),
                    proxy_url: "socks5://127.0.0.1:32002".into(),
                },
            ],
            1,
            Duration::from_secs(60),
        );

        coordinator.ensure_distinct_nodes().await;

        assert_eq!(coordinator.current_in_use(0).as_deref(), Some("node-a"));
        assert_eq!(coordinator.current_in_use(1).as_deref(), Some("node-b"));
    }
}
