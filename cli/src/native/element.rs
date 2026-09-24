use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cdp::client::CdpClient;
use super::cdp::types::*;

#[derive(Debug, Clone)]
pub struct RefEntry {
    pub backend_node_id: Option<i64>,
    pub role: String,
    pub name: String,
    pub nth: Option<usize>,
    pub selector: Option<String>,
    pub frame_id: Option<String>,
}

/// Exact browser and DOM identity used by goal's read-only readiness probe and
/// checked again immediately before a guarded click. It is never resolved by
/// role, name, or a replacement accessibility ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalTargetIdentity {
    pub daemon: String,
    pub browser: String,
    pub page_session: String,
    pub frame_id: Option<String>,
    pub document_session: String,
    pub loader: String,
    pub backend_node_id: i64,
    /// The native hit for a verified component visual, when it differs from
    /// the control. A newly covering sibling cannot inherit this allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_visual_hit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalProbeResult {
    pub status: String,
    pub identity: Option<GoalTargetIdentity>,
    pub detail: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub session: Option<String>,
    /// True only when the actual hit element owns a visible dialog interface.
    pub covering_interface: bool,
}

impl GoalProbeResult {
    fn new(status: &str, identity: Option<GoalTargetIdentity>, detail: Option<String>) -> Self {
        Self {
            status: status.into(),
            identity,
            detail,
            x: None,
            y: None,
            session: None,
            covering_interface: false,
        }
    }
}

#[derive(Clone)]
struct DocumentRefs {
    session: String,
    loader: String,
    refs: HashMap<i64, String>,
}

#[derive(Clone)]
pub struct RefMap {
    map: HashMap<String, RefEntry>,
    documents: HashMap<(String, Option<String>), DocumentRefs>,
    next_ref: usize,
}

impl RefMap {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            documents: HashMap::new(),
            next_ref: 1,
        }
    }

    pub fn add(
        &mut self,
        ref_id: String,
        backend_node_id: Option<i64>,
        role: &str,
        name: &str,
        nth: Option<usize>,
    ) {
        self.add_with_frame(ref_id, backend_node_id, role, name, nth, None);
    }

    pub fn add_with_frame(
        &mut self,
        ref_id: String,
        backend_node_id: Option<i64>,
        role: &str,
        name: &str,
        nth: Option<usize>,
        frame_id: Option<&str>,
    ) {
        self.map.insert(
            ref_id,
            RefEntry {
                backend_node_id,
                role: role.to_string(),
                name: name.to_string(),
                nth,
                selector: None,
                frame_id: frame_id.map(|s| s.to_string()),
            },
        );
    }

    pub fn add_selector(
        &mut self,
        ref_id: String,
        selector: String,
        role: &str,
        name: &str,
        nth: Option<usize>,
    ) {
        self.map.insert(
            ref_id,
            RefEntry {
                backend_node_id: None,
                role: role.to_string(),
                name: name.to_string(),
                nth,
                selector: Some(selector),
                frame_id: None,
            },
        );
    }

    pub fn get(&self, ref_id: &str) -> Option<&RefEntry> {
        self.map.get(ref_id)
    }

    pub fn document_identity(
        &self,
        page_session: &str,
        frame: Option<&str>,
    ) -> Option<(&str, &str)> {
        let key = (page_session.to_string(), frame.map(str::to_string));
        self.documents
            .get(&key)
            .map(|document| (document.session.as_str(), document.loader.as_str()))
    }

    pub fn entries_sorted(&self) -> Vec<(String, RefEntry)> {
        let mut entries = self
            .map
            .iter()
            .map(|(ref_id, entry)| (ref_id.clone(), entry.clone()))
            .collect::<Vec<_>>();

        entries.sort_by_key(|(ref_id, _)| {
            ref_id
                .strip_prefix('e')
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(usize::MAX)
        });

        entries
    }

    pub fn ref_ids(&self) -> std::collections::HashSet<String> {
        self.map.keys().cloned().collect()
    }

    pub fn remove(&mut self, ref_id: &str) {
        self.map.remove(ref_id);
    }

    pub fn begin_snapshot(&mut self) {
        self.map.clear();
    }

    /// Observe the document behind a page/frame pair. Unknown documents never
    /// retain refs, while a changed session or loader replaces the prior bucket.
    pub fn observe_document(
        &mut self,
        page_session: &str,
        frame: Option<&str>,
        session: &str,
        loader: Option<&str>,
    ) -> bool {
        let key = (page_session.to_string(), frame.map(str::to_string));
        let Some(loader) = loader.filter(|id| !id.is_empty()) else {
            self.invalidate_frame(page_session, frame);
            return false;
        };
        let changed = self
            .documents
            .get(&key)
            .is_none_or(|document| document.session != session || document.loader != loader);
        if changed {
            if frame.is_none() {
                self.documents
                    .retain(|(entry_page, _), _| entry_page != page_session);
            }
            self.documents.insert(
                key,
                DocumentRefs {
                    session: session.to_string(),
                    loader: loader.to_string(),
                    refs: HashMap::new(),
                },
            );
        }
        true
    }

    fn invalidate_frame(&mut self, page_session: &str, frame: Option<&str>) {
        if frame.is_none() {
            self.documents
                .retain(|(entry_page, _), _| entry_page != page_session);
        } else {
            self.documents
                .remove(&(page_session.to_string(), frame.map(str::to_string)));
        }
    }

    pub fn durable_ref(
        &self,
        page_session: &str,
        frame: Option<&str>,
        backend_node_id: i64,
    ) -> Option<&str> {
        self.documents
            .get(&(page_session.to_string(), frame.map(str::to_string)))?
            .refs
            .get(&backend_node_id)
            .map(String::as_str)
    }

    pub fn remember_durable_ref(
        &mut self,
        page_session: &str,
        frame: Option<&str>,
        backend_node_id: i64,
        ref_id: &str,
    ) {
        if let Some(document) = self
            .documents
            .get_mut(&(page_session.to_string(), frame.map(str::to_string)))
        {
            document.refs.insert(backend_node_id, ref_id.to_string());
        }
    }

    pub fn invalidate_page(&mut self, page_session: &str) {
        self.documents
            .retain(|(entry_page, _), _| entry_page != page_session);
        self.map.clear();
    }

    /// Drop all document identities while preserving the monotonic ref counter.
    pub fn invalidate_all_documents(&mut self) {
        self.documents.clear();
        self.map.clear();
    }

    pub fn next_ref_num(&self) -> usize {
        self.next_ref
    }

    pub fn set_next_ref_num(&mut self, n: usize) {
        self.next_ref = n;
    }
}

pub fn parse_ref(input: &str) -> Option<String> {
    let trimmed = input.trim();

    if let Some(stripped) = trimmed.strip_prefix('@') {
        if stripped.starts_with('e') && stripped[1..].chars().all(|c| c.is_ascii_digit()) {
            return Some(stripped.to_string());
        }
    }

    if let Some(stripped) = trimmed.strip_prefix("ref=") {
        if stripped.starts_with('e') && stripped[1..].chars().all(|c| c.is_ascii_digit()) {
            return Some(stripped.to_string());
        }
    }

    if trimmed.starts_with('e')
        && trimmed.len() > 1
        && trimmed[1..].chars().all(|c| c.is_ascii_digit())
    {
        return Some(trimmed.to_string());
    }

    None
}

/// Mirror of DaemonState.active_frame_id, refreshed before every command
/// (commands are serialized by the daemon's state lock, so this cannot
/// race). It lets CSS-selector resolution honor `frame <sel>` without
/// threading a parameter through every interaction signature; snapshot refs
/// already carry their frame through the ref map.
static ACTIVE_FRAME: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

pub fn set_active_frame(frame_id: Option<&str>) {
    *ACTIVE_FRAME
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap() = frame_id.map(String::from);
}

fn active_frame() -> Option<String> {
    ACTIVE_FRAME.get().and_then(|m| m.lock().unwrap().clone())
}

/// Object handle for the <iframe> element that owns a frame, resolved on the
/// parent session. Works for same-process frames where no dedicated CDP
/// session exists.
pub(super) async fn frame_owner_object_id(
    client: &CdpClient,
    session_id: &str,
    frame_id: &str,
) -> Result<String, String> {
    let owner = client
        .send_command(
            "DOM.getFrameOwner",
            Some(serde_json::json!({ "frameId": frame_id })),
            Some(session_id),
        )
        .await?;
    let backend_node_id = owner
        .get("backendNodeId")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("Could not resolve the owner element of frame {}", frame_id))?;
    let result: DomResolveNodeResult = client
        .send_command_typed(
            "DOM.resolveNode",
            &DomResolveNodeParams {
                backend_node_id: Some(backend_node_id),
                node_id: None,
                object_group: Some("agent-browser".to_string()),
            },
            Some(session_id),
        )
        .await?;
    result
        .object
        .object_id
        .ok_or_else(|| format!("No objectId for the owner element of frame {}", frame_id))
}

/// Find a selector inside a same-process iframe and return its center in
/// top-level viewport coordinates (input events dispatch in that space).
/// Same-origin access to contentDocument is what makes this possible; a
/// same-session cross-origin frame cannot be resolved through contentDocument.
async fn resolve_center_in_same_process_frame(
    client: &CdpClient,
    session_id: &str,
    frame_id: &str,
    selector: &str,
) -> Result<(f64, f64), String> {
    let owner_object_id = frame_owner_object_id(client, session_id, frame_id).await?;
    let find_expr = build_find_element_js_in("doc", selector);
    let blocker_at = blocker_at_js();
    let function = format!(
        r#"function() {{
            const doc = this.contentDocument;
            if (!doc) return null;
            const el = {find_expr};
            if (!el) return null;
            if (el.scrollIntoViewIfNeeded) el.scrollIntoViewIfNeeded(true);
            else el.scrollIntoView({{ block: 'center', inline: 'center' }});
            const rect = el.getBoundingClientRect();
            let x = rect.x + rect.width / 2;
            let y = rect.y + rect.height / 2;
            let win = doc.defaultView;
            while (win && win.frameElement) {{
                const frameRect = win.frameElement.getBoundingClientRect();
                x += frameRect.x + win.frameElement.clientLeft;
                y += frameRect.y + win.frameElement.clientTop;
                win = win.parent;
            }}
            const blockerAt = {blocker_at};
            const topDoc = win ? win.document : doc;
            return {{ x: x, y: y, blocker: blockerAt(topDoc, el, x, y) }};
        }}"#,
    );
    let result = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(serde_json::json!({
                "objectId": owner_object_id,
                "functionDeclaration": function,
                "returnByValue": true,
            })),
            Some(session_id),
        )
        .await?;
    let value = result.get("result").and_then(|r| r.get("value"));
    if let Some(blocker) = value
        .and_then(|v| v.get("blocker"))
        .and_then(|v| v.as_str())
    {
        return Err(intercepted_error(selector, blocker));
    }
    let x = value.and_then(|v| v.get("x")).and_then(|v| v.as_f64());
    let y = value.and_then(|v| v.get("y")).and_then(|v| v.as_f64());
    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(format!(
            "Element not found in the selected frame: {}",
            selector
        )),
    }
}

/// Find a selector inside a same-process iframe and return its object handle.
async fn resolve_object_in_same_process_frame(
    client: &CdpClient,
    session_id: &str,
    frame_id: &str,
    selector: &str,
) -> Result<String, String> {
    let owner_object_id = frame_owner_object_id(client, session_id, frame_id).await?;
    let find_expr = build_find_element_js_in("doc", selector);
    let function = format!(
        "function() {{ const doc = this.contentDocument; if (!doc) return null; return {find_expr}; }}",
    );
    let result = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(serde_json::json!({
                "objectId": owner_object_id,
                "functionDeclaration": function,
                "returnByValue": false,
            })),
            Some(session_id),
        )
        .await?;
    result
        .get("result")
        .and_then(|r| r.get("objectId"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!("Element not found in the selected frame: {}", selector))
}

pub async fn resolve_element_center(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(f64, f64, String), String> {
    if let Some(ref_id) = parse_ref(selector_or_ref) {
        let entry = ref_map
            .get(&ref_id)
            .ok_or_else(|| format!("Unknown ref: {}", ref_id))?;

        let effective_session_id =
            resolve_frame_session(entry.frame_id.as_deref(), session_id, iframe_sessions);

        // Try cached backend_node_id first (fast path)
        if let Some(backend_node_id) = entry.backend_node_id {
            scroll_node_into_view(client, effective_session_id, backend_node_id).await;
            let result: Result<DomGetBoxModelResult, String> = client
                .send_command_typed(
                    "DOM.getBoxModel",
                    &DomGetBoxModelParams {
                        backend_node_id: Some(backend_node_id),
                        node_id: None,
                        object_id: None,
                    },
                    Some(effective_session_id),
                )
                .await;

            if let Ok(r) = result {
                let (x, y) = box_model_center(&r.model);
                let (x, y) = check_node_interception(
                    client,
                    effective_session_id,
                    backend_node_id,
                    entry.frame_id.as_deref(),
                    selector_or_ref,
                    x,
                    y,
                )
                .await?;
                return Ok((x, y, effective_session_id.to_string()));
            }
            // backend_node_id is stale; re-query the accessibility tree below
        }

        // Fallback: re-query the accessibility tree to find a fresh node by role/name
        let fresh_id = find_node_id_by_role_name(
            client,
            session_id,
            &entry.role,
            &entry.name,
            entry.nth,
            entry.frame_id.as_deref(),
            iframe_sessions,
        )
        .await?;
        scroll_node_into_view(client, effective_session_id, fresh_id).await;
        let result: DomGetBoxModelResult = client
            .send_command_typed(
                "DOM.getBoxModel",
                &DomGetBoxModelParams {
                    backend_node_id: Some(fresh_id),
                    node_id: None,
                    object_id: None,
                },
                Some(effective_session_id),
            )
            .await?;
        let (x, y) = box_model_center(&result.model);
        let (x, y) = check_node_interception(
            client,
            effective_session_id,
            fresh_id,
            entry.frame_id.as_deref(),
            selector_or_ref,
            x,
            y,
        )
        .await?;
        return Ok((x, y, effective_session_id.to_string()));
    }

    // CSS selector: honor an active `frame <sel>` selection.
    if let Some(frame_id) = active_frame() {
        // Cross-process iframe: its dedicated session's main frame IS the
        // iframe, so plain document-rooted resolution works there.
        if let Some(frame_session) = iframe_sessions.get(&frame_id) {
            let (x, y) = resolve_by_selector(client, frame_session, selector_or_ref).await?;
            return Ok((x, y, frame_session.clone()));
        }
        let (x, y) =
            resolve_center_in_same_process_frame(client, session_id, &frame_id, selector_or_ref)
                .await?;
        return Ok((x, y, session_id.to_string()));
    }
    let (x, y) = resolve_by_selector(client, session_id, selector_or_ref).await?;
    Ok((x, y, session_id.to_string()))
}

/// Inspect one snapshot node without changing the page. A failed CDP lookup is
/// Unknown, never evidence that a click would be safe. Cross-process frame
/// ancestors are not yet hit-tested by this path, so those targets stay Unknown.
pub async fn probe_goal_target(
    client: &CdpClient,
    page_session: &str,
    ref_map: &RefMap,
    selector: &str,
    iframe_sessions: &HashMap<String, String>,
    incarnations: (&str, &str),
    expected: Option<&GoalTargetIdentity>,
) -> GoalProbeResult {
    let Some(ref_id) = parse_ref(selector) else {
        return GoalProbeResult::new(
            "unknown",
            None,
            Some("Goal target is not a snapshot ref".into()),
        );
    };
    let Some(entry) = ref_map.get(&ref_id) else {
        return GoalProbeResult::new(
            "unavailable",
            None,
            Some("Snapshot target is unavailable".into()),
        );
    };
    let Some(node) = entry.backend_node_id else {
        return GoalProbeResult::new(
            "unknown",
            None,
            Some("Target has no native node identity".into()),
        );
    };
    let frame = entry.frame_id.as_deref();
    let Some((session, loader)) = ref_map.document_identity(page_session, frame) else {
        return GoalProbeResult::new(
            "unknown",
            None,
            Some("Document identity is unavailable".into()),
        );
    };
    let mut identity = GoalTargetIdentity {
        daemon: incarnations.0.to_string(),
        browser: incarnations.1.to_string(),
        page_session: page_session.to_string(),
        frame_id: entry.frame_id.clone(),
        document_session: session.to_string(),
        loader: loader.to_string(),
        backend_node_id: node,
        component_visual_hit: expected.and_then(|prior| prior.component_visual_hit),
    };
    if expected.is_some_and(|prior| *prior != identity) {
        return GoalProbeResult::new(
            "unavailable",
            Some(identity),
            Some("Target context changed".into()),
        );
    }
    if iframe_sessions.values().any(|value| value == session) {
        return GoalProbeResult::new(
            "unknown",
            Some(identity),
            Some("Cross-process frame ancestor coverage is unsupported".into()),
        );
    }
    if session != page_session {
        return GoalProbeResult::new(
            "unknown",
            Some(identity),
            Some("Target session is unsupported".into()),
        );
    }
    let tree = match client
        .send_command_no_params("Page.getFrameTree", Some(session))
        .await
    {
        Ok(tree) => tree,
        Err(_) => {
            return GoalProbeResult::new(
                "unknown",
                Some(identity),
                Some("Cannot inspect document identity".into()),
            )
        }
    };
    if super::snapshot::frame_loader(&tree["frameTree"], frame) != Some(loader) {
        return GoalProbeResult::new(
            "unavailable",
            Some(identity),
            Some("Target document changed".into()),
        );
    }
    match goal_owner_matches(client, session, node, frame).await {
        Ok(true) => {}
        Ok(false) => {
            return GoalProbeResult::new(
                "unavailable",
                Some(identity),
                Some("Target moved to another document".into()),
            )
        }
        Err(_) => {
            return GoalProbeResult::new(
                "unknown",
                Some(identity),
                Some("Cannot verify target owner document".into()),
            )
        }
    }
    let model: DomGetBoxModelResult = match client
        .send_command_typed(
            "DOM.getBoxModel",
            &DomGetBoxModelParams {
                backend_node_id: Some(node),
                node_id: None,
                object_id: None,
            },
            Some(session),
        )
        .await
    {
        Ok(model) => model,
        Err(_) => {
            return GoalProbeResult::new(
                "unavailable",
                Some(identity),
                Some("Target node is unavailable".into()),
            )
        }
    };
    let (x, y) = box_model_center(&model.model);
    // Start with exact ancestry. A component's visual sibling is accepted
    // only after the native component relation and local surface are checked.
    // Once recorded, only that same hit may receive later guarded input.
    let strict = node_interception(client, session, node, frame, x, y, false).await;
    let interception = match strict {
        Ok((_, _, Some(_), Some(hit_id)))
            if expected
                .map(|prior| prior.component_visual_hit == Some(hit_id))
                .unwrap_or(true)
                && component_visual_is_local(client, session, node, hit_id).await =>
        {
            let component = node_interception(client, session, node, frame, x, y, true).await;
            match component {
                Ok((x, y, None, Some(verified_hit))) if verified_hit == hit_id => {
                    identity.component_visual_hit = Some(hit_id);
                    Ok((x, y, None, Some(hit_id)))
                }
                // The visual may disappear and expose the actual control.
                Ok((x, y, None, None)) => Ok((x, y, None, None)),
                Ok((_, _, None, Some(_))) => Ok((
                    x,
                    y,
                    Some("Component visual changed during hit testing".into()),
                    Some(hit_id),
                )),
                other => other,
            }
        }
        other => other,
    };
    match interception {
        Ok((x, y, None, _)) => {
            let mut result = GoalProbeResult::new("ready", Some(identity), None);
            result.x = Some(x);
            result.y = Some(y);
            result.session = Some(session.to_string());
            result
        }
        Ok((_, _, Some(blocker), hit_id)) => {
            let mut result = GoalProbeResult::new(
                "covered",
                Some(identity),
                Some(blocker.chars().take(160).collect()),
            );
            if let Some(hit_id) = hit_id {
                result.covering_interface = hit_has_visible_dialog(client, session, hit_id, node)
                    .await
                    .unwrap_or(false);
            }
            result
        }
        Err(_) => GoalProbeResult::new(
            "unknown",
            Some(identity),
            Some("Cannot verify target coverage".into()),
        ),
    }
}

/// A component visual must occupy the control's local click surface. This
/// excludes page-sized sibling covers and busy/progress interfaces before the
/// existing native component relationship can grant its visual exemption.
/// A dialog enclosing both the target and visual is their existing owner;
/// a separate dialog over the visual remains coverage.
async fn component_visual_is_local(
    client: &CdpClient,
    session: &str,
    target_id: i64,
    hit_id: i64,
) -> bool {
    let resolve = |backend_node_id| DomResolveNodeParams {
        backend_node_id: Some(backend_node_id),
        node_id: None,
        object_group: Some("agent-browser-goal-visual".into()),
    };
    let target: Result<DomResolveNodeResult, _> = client
        .send_command_typed("DOM.resolveNode", &resolve(target_id), Some(session))
        .await;
    let Ok(target) = target else {
        return false;
    };
    let hit: Result<DomResolveNodeResult, _> = client
        .send_command_typed("DOM.resolveNode", &resolve(hit_id), Some(session))
        .await;
    let Ok(hit) = hit else {
        return false;
    };
    let (Some(target), Some(hit)) = (target.object.object_id, hit.object.object_id) else {
        return false;
    };
    let result = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(serde_json::json!({
                "objectId": target,
                "functionDeclaration": r#"function(hit) {
                    const control = this.getBoundingClientRect();
                    const visual = hit.getBoundingClientRect();
                    if (!control.width || !control.height || !visual.width || !visual.height) return false;
                    if (visual.width > control.width * 8 || visual.height > control.height * 8 ||
                        visual.width * visual.height > control.width * control.height * 16) return false;
                    if (control.left + control.width / 2 < visual.left ||
                        control.left + control.width / 2 > visual.right ||
                        control.top + control.height / 2 < visual.top ||
                        control.top + control.height / 2 > visual.bottom) return false;
                    const busy = '[aria-busy="true"], [role="progressbar"], progress, [role="status"]';
                    if (hit.matches(busy) || hit.closest(busy) || hit.querySelector(busy)) return false;
                    // A sibling control or a cover containing one is not a
                    // decorative surface of the original target, even when
                    // both sit inside the target's owning dialog.
                    const controls = 'button, a[href], input, select, textarea, [role="button"], [role="link"]';
                    if (hit.closest(controls) || hit.querySelector(controls)) return false;
                    const dialogs = 'dialog[open], [role="dialog"], [role="alertdialog"], [aria-modal="true"]';
                    if (hit.querySelector(dialogs)) return false;
                    const slots = new Map();
                    for (let current = hit; current; ) {
                        const root = current.getRootNode();
                        if (!root.host) break;
                        for (const slot of root.querySelectorAll('slot')) {
                            for (const assigned of slot.assignedNodes()) slots.set(assigned, slot);
                        }
                        current = root.host;
                    }
                    const ownsTarget = dialog => {
                        for (let current = this; current; ) {
                            if (current === dialog) return true;
                            const root = current.getRootNode && current.getRootNode();
                            current = current.assignedSlot || slots.get(current) || current.parentNode ||
                                (root && root.host) || null;
                        }
                        return false;
                    };
                    for (let current = hit; current; current = current.parentElement) {
                        if (current.matches(dialogs) && !ownsTarget(current)) return false;
                    }
                    return true;
                }"#,
                "arguments": [{"objectId": hit}],
                "returnByValue": true,
            })),
            Some(session),
        )
        .await;
    result
        .ok()
        .and_then(|value| value.pointer("/result/value").and_then(Value::as_bool))
        == Some(true)
}

/// Compare the node's live ownerDocument with the original frame's document.
/// This catches DOM adoption, which preserves backendNodeId and the cached
/// RefMap token even though the node now belongs to another document.
async fn goal_owner_matches(
    client: &CdpClient,
    session: &str,
    node: i64,
    frame: Option<&str>,
) -> Result<bool, String> {
    let group = "agent-browser-goal-owner";
    let target: DomResolveNodeResult = client
        .send_command_typed(
            "DOM.resolveNode",
            &DomResolveNodeParams {
                backend_node_id: Some(node),
                node_id: None,
                object_group: Some(group.into()),
            },
            Some(session),
        )
        .await?;
    let object_id = target
        .object
        .object_id
        .ok_or("Missing goal target object")?;
    let document = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(serde_json::json!({
                "objectId": object_id,
                "functionDeclaration": "function() { return this.ownerDocument; }",
                "returnByValue": false
            })),
            Some(session),
        )
        .await?;
    let document_object = document["result"]["objectId"]
        .as_str()
        .ok_or("Missing target owner document")?;
    let owner = client
        .send_command(
            "DOM.describeNode",
            Some(serde_json::json!({"objectId": document_object, "depth": 0})),
            Some(session),
        )
        .await?;
    let owner_id = owner["node"]["backendNodeId"]
        .as_i64()
        .ok_or("Missing target owner document id")?;
    let expected_id = if let Some(frame) = frame {
        let frame_owner = client
            .send_command(
                "DOM.getFrameOwner",
                Some(serde_json::json!({"frameId": frame})),
                Some(session),
            )
            .await?;
        let frame_owner_id = frame_owner["backendNodeId"]
            .as_i64()
            .ok_or("Missing frame owner")?;
        let described = client
            .send_command(
                "DOM.describeNode",
                Some(serde_json::json!({"backendNodeId": frame_owner_id, "depth": 1})),
                Some(session),
            )
            .await?;
        described["node"]["contentDocument"]["backendNodeId"]
            .as_i64()
            .ok_or("Missing frame document id")?
    } else {
        let root = client
            .send_command(
                "DOM.getDocument",
                Some(serde_json::json!({"depth": 0})),
                Some(session),
            )
            .await?;
        root["root"]["backendNodeId"]
            .as_i64()
            .ok_or("Missing page document id")?
    };
    let _ = client
        .send_command(
            "Runtime.releaseObjectGroup",
            Some(serde_json::json!({"objectGroup": group})),
            Some(session),
        )
        .await;
    Ok(owner_id == expected_id)
}

/// Dispatch-only scroll of the original backend node. Readiness probes never
/// call this; the guarded click re-probes geometry and cover after scrolling.
pub async fn scroll_goal_node_into_view(
    client: &CdpClient,
    session: &str,
    node: i64,
    deadline: std::time::Instant,
) -> Result<(), String> {
    if std::time::Instant::now() >= deadline {
        return Err("Goal deadline expired before target scroll".into());
    }
    client
        .send_command_typed_before::<_, Value>(
            "DOM.scrollIntoViewIfNeeded",
            &serde_json::json!({"backendNodeId": node}),
            Some(session),
            deadline,
        )
        .await?;
    Ok(())
}

/// Origin of a CDP session's viewport in the recorded page's CSS coordinates.
/// Input sent to an OOPIF is local to that session; cursor history is page-local.
/// Walk owner sessions for nested OOPIFs. Content quads include iframe borders
/// and scrolling in the owning viewport.
pub async fn session_viewport_offset(
    client: &CdpClient,
    page_session: &str,
    target_session: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(f64, f64), String> {
    let mut current = target_session.to_string();
    let mut offset = (0.0, 0.0);
    for _ in 0..=iframe_sessions.len() {
        if current == page_session {
            return Ok(offset);
        }
        let frame = iframe_sessions
            .iter()
            .find(|(_, session)| **session == current)
            .map(|(frame, _)| frame)
            .ok_or("Cannot locate recording cursor frame")?;
        let mut owner = None;
        for candidate in
            std::iter::once(page_session).chain(iframe_sessions.values().map(String::as_str))
        {
            if candidate == current {
                continue;
            }
            let Ok(node) = client
                .send_command(
                    "DOM.getFrameOwner",
                    Some(serde_json::json!({"frameId": frame})),
                    Some(candidate),
                )
                .await
            else {
                continue;
            };
            let Some(backend_id) = node["backendNodeId"].as_i64() else {
                continue;
            };
            let model = client
                .send_command(
                    "DOM.getBoxModel",
                    Some(serde_json::json!({"backendNodeId": backend_id})),
                    Some(candidate),
                )
                .await?;
            let quad = model
                .pointer("/model/content")
                .and_then(Value::as_array)
                .ok_or("Cannot locate recording cursor frame bounds")?;
            let x = quad
                .first()
                .and_then(Value::as_f64)
                .ok_or("Missing frame content x")?;
            let y = quad
                .get(1)
                .and_then(Value::as_f64)
                .ok_or("Missing frame content y")?;
            offset.0 += x;
            offset.1 += y;
            owner = Some(candidate.to_string());
            break;
        }
        current = owner.ok_or("Cannot locate recording cursor frame owner")?;
    }
    Err("Cyclic recording cursor frame ownership".to_string())
}

/// Hit-test in the same renderer/session as the box model and input dispatch.
/// CDP handles cross-origin local frames and inverse embedding transforms; a
/// JavaScript frameElement walk follows origin boundaries, not renderer roots.
/// Return the sampled viewport point for dispatch. Resolution failures retain
/// the original point and the existing best-effort interception behavior.
async fn check_node_interception(
    client: &CdpClient,
    session_id: &str,
    backend_node_id: i64,
    frame_id: Option<&str>,
    target: &str,
    x: f64,
    y: f64,
) -> Result<(f64, f64), String> {
    match node_interception(client, session_id, backend_node_id, frame_id, x, y, true).await {
        Ok((_, _, Some(blocker), _)) => Err(intercepted_error(target, &blocker)),
        Ok((x, y, None, _)) => Ok((x, y)),
        Err(_) => Ok((x, y)),
    }
}

async fn node_interception(
    client: &CdpClient,
    session_id: &str,
    backend_node_id: i64,
    frame_id: Option<&str>,
    x: f64,
    y: f64,
    allow_component_visual: bool,
) -> Result<(f64, f64, Option<String>, Option<i64>), String> {
    // getBoxModel/Input use the session's viewport, while getNodeForLocation
    // uses its root document's CSS pixels. Add only that viewport's scroll
    // offset, including for OOPIF sessions. The hit-test API takes integers:
    // return its sampled point in viewport coordinates so input dispatch never
    // lands on a different side of a fractional overlay/target boundary.
    let metrics = client
        .send_command_no_params("Page.getLayoutMetrics", Some(session_id))
        .await?;
    let viewport = &metrics["cssVisualViewport"];
    let page_x = viewport["pageX"].as_f64().ok_or("Missing viewport x")?;
    let page_y = viewport["pageY"].as_f64().ok_or("Missing viewport y")?;
    let document_x = (x + page_x).round() as i64;
    let document_y = (y + page_y).round() as i64;
    let x = document_x as f64 - page_x;
    let y = document_y as f64 - page_y;
    let hit = client
        .send_command(
            "DOM.getNodeForLocation",
            Some(serde_json::json!({
                "x": document_x,
                "y": document_y,
                "includeUserAgentShadowDOM": false,
                "ignorePointerEventsNone": false,
            })),
            Some(session_id),
        )
        .await?;
    let mut hit_id = hit["backendNodeId"].as_i64().ok_or("Missing hit node")?;
    if hit_id == backend_node_id {
        return Ok((x, y, None, None));
    }

    // Map a native frame hit to the hit-side owner in the lowest common
    // document. Descendant owners can be related to the target; sibling/cousin
    // owners are only used to describe the blocker and must remain rejected.
    // Frame identity also covers containers and customized frame owners.
    let mut compare_relationship = true;
    let mut description_scope_frame = None;
    let hit_frame = hit["frameId"].as_str().ok_or("Missing hit frame")?;
    if frame_id != Some(hit_frame) {
        let tree = client
            .send_command_no_params("Page.getFrameTree", Some(session_id))
            .await?;
        let tree = &tree["frameTree"];
        let target_frame = frame_id
            .or_else(|| tree["frame"]["id"].as_str())
            .ok_or("Missing target frame")?;
        compare_relationship = target_frame == hit_frame;
        if let Some((owner_frame, common_frame)) = hit_frame_owner(tree, target_frame, hit_frame) {
            compare_relationship = common_frame == target_frame;
            let owner = client
                .send_command(
                    "DOM.getFrameOwner",
                    Some(serde_json::json!({"frameId": owner_frame})),
                    Some(session_id),
                )
                .await?;
            hit_id = owner["backendNodeId"]
                .as_i64()
                .ok_or("Missing frame owner")?;
            if compare_relationship && hit_id == backend_node_id {
                return Ok((x, y, None, None));
            }
        }
        if !compare_relationship {
            // Reverse the paths to find the target-side owner in the document
            // where the blocker is described, including ancestor-frame hits.
            description_scope_frame =
                hit_frame_owner(tree, hit_frame, target_frame).map(|(owner, _)| owner.to_string());
        }
    }

    let hit: DomResolveNodeResult = client
        .send_command_typed(
            "DOM.resolveNode",
            &DomResolveNodeParams {
                backend_node_id: Some(hit_id),
                node_id: None,
                object_group: Some("agent-browser".to_string()),
            },
            Some(session_id),
        )
        .await?;
    let hit_object = hit.object.object_id.ok_or("Missing hit object")?;
    let target_object = if compare_relationship {
        let target: DomResolveNodeResult = client
            .send_command_typed(
                "DOM.resolveNode",
                &DomResolveNodeParams {
                    backend_node_id: Some(backend_node_id),
                    node_id: None,
                    object_group: Some("agent-browser".to_string()),
                },
                Some(session_id),
            )
            .await?;
        Some(target.object.object_id.ok_or("Missing target object")?)
    } else {
        None
    };

    if let Some(target_object) = target_object.as_deref() {
        let blocker_for_hit = blocker_for_hit_js();
        let function = format!(
            "function(hit, component) {{ return ({blocker_for_hit})(this, hit, component); }}"
        );
        let mut result = client
            .send_command(
                "Runtime.callFunctionOn",
                Some(serde_json::json!({
                    "objectId": target_object,
                    "functionDeclaration": function,
                    "arguments": [{"objectId": hit_object}, {"value": allow_component_visual}],
                    "returnByValue": true,
                })),
                Some(session_id),
            )
            .await;
        if allow_component_visual
            && result
                .as_ref()
                .ok()
                .and_then(|v| v.pointer("/result/value"))
                .and_then(Value::as_bool)
                == Some(true)
        {
            // Only a component-visual exemption needs this extra native walk.
            // Neither JS node may expose the target's closed slot assignment;
            // trusting its apparent outer root could admit an unrelated dialog.
            let group = format!("agent-browser-hit-{}", uuid::Uuid::new_v4());
            let component = native_shadow_root(client, session_id, target_object, &group)
                .await
                .ok()
                .flatten();
            let component_arg = match component {
                Some(object_id) => serde_json::json!({"objectId": object_id}),
                None => serde_json::json!({"value": null}),
            };
            // If native ancestry cannot be resolved, disable only this optional
            // exemption; a known hit must not become allowed because it failed.
            result = client
                .send_command(
                    "Runtime.callFunctionOn",
                    Some(serde_json::json!({
                        "objectId": target_object,
                        "functionDeclaration": function,
                        "arguments": [{"objectId": hit_object}, component_arg],
                        "returnByValue": true,
                    })),
                    Some(session_id),
                )
                .await;
            let _ = client
                .send_command(
                    "Runtime.releaseObjectGroup",
                    Some(serde_json::json!({"objectGroup": group})),
                    Some(session_id),
                )
                .await;
        }
        if let Ok(value) = result {
            if let Some(blocker) = value.pointer("/result/value") {
                if blocker.is_null() || blocker.is_string() {
                    return Ok((x, y, blocker.as_str().map(String::from), Some(hit_id)));
                }
            }
        }
    }

    // The mapped target owner supplies tree scopes only: a known cross-document
    // blocker must never be granted a relationship/component exemption. Scope
    // lookup is optional; failure falls back to a document-level description.
    let description_scope = if let Some(target) = target_object {
        Some(target)
    } else if let Some(frame) = description_scope_frame {
        frame_owner_object_id(client, session_id, &frame).await.ok()
    } else {
        None
    };
    let mut description = describe_blocking_hit(
        client,
        session_id,
        &hit_object,
        description_scope.as_deref(),
    )
    .await;
    if description.is_err() && description_scope.is_some() {
        description = describe_blocking_hit(client, session_id, &hit_object, None).await;
    }
    let blocker = match description {
        Ok(blocker) => blocker,
        // Even a failed diagnostic must not turn an established cross-document
        // blocker into allowed input. Relationship-resolution errors elsewhere
        // retain the existing best-effort behavior.
        Err(_) if !compare_relationship => "another element".to_string(),
        Err(error) => return Err(error),
    };
    Ok((x, y, Some(blocker), Some(hit_id)))
}

/// A covering interface must be attached to the hit at the target's click
/// point and separate from the target's own enclosing dialog. Only a visible,
/// enabled control in a non-busy interface qualifies; text alone cannot prove
/// that a newly covering dialog offers a useful choice.
async fn hit_has_visible_dialog(
    client: &CdpClient,
    session: &str,
    hit_id: i64,
    target_id: i64,
) -> Result<bool, String> {
    let hit: DomResolveNodeResult = client
        .send_command_typed(
            "DOM.resolveNode",
            &DomResolveNodeParams {
                backend_node_id: Some(hit_id),
                node_id: None,
                object_group: Some("agent-browser".into()),
            },
            Some(session),
        )
        .await?;
    let object_id = hit.object.object_id.ok_or("Missing covering node object")?;
    let target: DomResolveNodeResult = client
        .send_command_typed(
            "DOM.resolveNode",
            &DomResolveNodeParams {
                backend_node_id: Some(target_id),
                node_id: None,
                object_group: Some("agent-browser".into()),
            },
            Some(session),
        )
        .await?;
    let target_object_id = target
        .object
        .object_id
        .ok_or("Missing target node object")?;
    let result = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(serde_json::json!({
                "objectId": object_id,
                "functionDeclaration": r#"function(target) {
                    if (this === document.body || this === document.documentElement) return false;
                    const selector = 'dialog[open], [role="dialog"], [role="alertdialog"], [aria-modal="true"]';
                    const candidates = [];
                    if (this.closest) candidates.push(this.closest(selector));
                    if (this.querySelectorAll) candidates.push(...this.querySelectorAll(selector));
                    const containsTarget = node => {
                        // A light-DOM target's assignedSlot is null in a closed
                        // root. The hit is inside that root, so its slot map is
                        // available even though the host cannot expose it.
                        const slots = new Map();
                        for (const start of [this, node]) {
                            for (let current = start; current; ) {
                                const root = current.getRootNode();
                                if (!root.host) break;
                                for (const slot of root.querySelectorAll('slot')) {
                                    for (const assigned of slot.assignedNodes()) slots.set(assigned, slot);
                                }
                                current = root.host;
                            }
                        }
                        for (let current = target; current; ) {
                            if (current === node) return true;
                            const root = current.getRootNode && current.getRootNode();
                            current = current.assignedSlot || slots.get(current) || current.parentNode ||
                                (root && root.host) || null;
                        }
                        return false;
                    };
                    const visible = node => node.getClientRects().length > 0 &&
                        getComputedStyle(node).visibility !== 'hidden' &&
                        getComputedStyle(node).display !== 'none';
                    const busy = '[aria-busy="true"], [role="progressbar"], progress, [role="status"]';
                    const useful = node => {
                        if (node.matches(busy) || [...node.querySelectorAll(busy)].some(visible)) return false;
                        const controls = node.querySelectorAll('button, a[href], input, select, textarea, [role="button"], [role="link"]');
                        const enabled = [...controls].filter(control => visible(control) &&
                            !control.disabled && !control.matches(':disabled') &&
                            !control.closest('[aria-disabled="true"], [inert]') &&
                            !control.closest(busy));
                        // One generic Cancel button under a spinner is not
                        // enough evidence of a new usable interface. A named
                        // or headed dialog, a form field, or multiple controls
                        // supplies positive structure without reading words.
                        const label = (node.getAttribute('aria-label') || '').trim();
                        const heading = [...node.querySelectorAll('h1,h2,h3,h4,h5,h6,[role="heading"]')].some(visible);
                        const labelled = (node.getAttribute('aria-labelledby') || '').split(/\s+/).some(id => {
                            const root = node.getRootNode();
                            const labelNode = id && root.getElementById && root.getElementById(id);
                            return labelNode && visible(labelNode) && (labelNode.textContent || '').trim();
                        });
                        const formField = enabled.some(control => control.matches('input, select, textarea'));
                        return enabled.length > 0 && (label || heading || labelled || formField || enabled.length > 1);
                    };
                    return candidates.some(node => node && visible(node) &&
                        !containsTarget(node) && useful(node));
                }"#,
                "arguments": [{"objectId": target_object_id}],
                "returnByValue": true,
            })),
            Some(session),
        )
        .await?;
    Ok(result.pointer("/result/value").and_then(Value::as_bool) == Some(true))
}

/// Describe a known blocker without running relationship or component checks.
async fn describe_blocking_hit(
    client: &CdpClient,
    session_id: &str,
    hit_object: &str,
    scope_object: Option<&str>,
) -> Result<String, String> {
    let scope_arg = match scope_object {
        Some(object_id) => serde_json::json!({"objectId": object_id}),
        None => serde_json::json!({"value": null}),
    };
    let description = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(serde_json::json!({
                "objectId": hit_object,
                "functionDeclaration": format!(
                    "function(scope) {{ return ({DESCRIBE_HIT_JS})(scope, this); }}"
                ),
                "arguments": [scope_arg],
                "returnByValue": true,
            })),
            Some(session_id),
        )
        .await?;
    description
        .pointer("/result/value")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| "Cannot describe blocking hit".to_string())
}

/// Follow native assigned-slot edges to the nearest author shadow root. CDP
/// exposes closed assignments and built-in UA slots (for example in details).
/// UA roots are implementation details: continue through their host so they
/// neither replace the author component nor skip its overlay boundary.
/// The caller releases `object_group` after using the returned root or on error.
async fn native_shadow_root(
    client: &CdpClient,
    session_id: &str,
    target_object: &str,
    object_group: &str,
) -> Result<Option<String>, String> {
    let mut object_id = target_object.to_string();
    let mut visited = std::collections::HashSet::new();
    loop {
        let description = client
            .send_command(
                "DOM.describeNode",
                Some(serde_json::json!({"objectId": object_id, "depth": 0})),
                Some(session_id),
            )
            .await?;
        let node = &description["node"];
        let backend_id = node["backendNodeId"]
            .as_i64()
            .ok_or("Missing composed ancestor node")?;
        if !visited.insert(backend_id) {
            return Err("Cyclic composed ancestry".to_string());
        }
        let shadow_root_type = node["shadowRootType"].as_str();
        if node["nodeType"].as_i64() == Some(11)
            && matches!(shadow_root_type, Some("open" | "closed"))
        {
            return Ok(Some(object_id));
        }
        if let Some(slot_id) = node
            .pointer("/assignedSlot/backendNodeId")
            .and_then(Value::as_i64)
        {
            let slot: DomResolveNodeResult = client
                .send_command_typed(
                    "DOM.resolveNode",
                    &DomResolveNodeParams {
                        backend_node_id: Some(slot_id),
                        node_id: None,
                        object_group: Some(object_group.to_string()),
                    },
                    Some(session_id),
                )
                .await?;
            object_id = slot
                .object
                .object_id
                .ok_or("Missing assigned slot object")?;
        } else {
            let parent_function = if shadow_root_type == Some("user-agent") {
                "function() { return this.host; }"
            } else {
                "function() { return this.parentNode; }"
            };
            let parent = client
                .send_command(
                    "Runtime.callFunctionOn",
                    Some(serde_json::json!({
                        "objectId": object_id,
                        "functionDeclaration": parent_function,
                        "objectGroup": object_group,
                        "returnByValue": false,
                    })),
                    Some(session_id),
                )
                .await?;
            if parent.get("exceptionDetails").is_some() {
                return Err("Cannot resolve composed parent".to_string());
            }
            if parent.pointer("/result/subtype").and_then(Value::as_str) == Some("null") {
                return Ok(None);
            }
            object_id = parent
                .pointer("/result/objectId")
                .and_then(Value::as_str)
                .ok_or("Missing composed parent object")?
                .to_string();
        }
    }
}

/// Return the hit-side child frame and its lowest common ancestor document.
/// Descendant hits map to an owner in the target document; sibling/cousin hits
/// map to an owner in the common document. Same-frame, ancestor, or missing
/// hits have no child owner to map to.
fn hit_frame_owner<'a>(
    tree: &'a Value,
    target_frame: &str,
    hit_frame: &str,
) -> Option<(&'a str, &'a str)> {
    if !frame_contains_target(tree, target_frame) {
        return None;
    }
    let child = tree["childFrames"]
        .as_array()?
        .iter()
        .find(|child| frame_contains_target(child, hit_frame))?;
    if frame_contains_target(child, target_frame) {
        return hit_frame_owner(child, target_frame, hit_frame);
    }
    Some((
        child["frame"]["id"].as_str()?,
        tree["frame"]["id"].as_str()?,
    ))
}

fn frame_contains_target(tree: &Value, target: &str) -> bool {
    tree["frame"]["id"].as_str() == Some(target)
        || tree["childFrames"].as_array().is_some_and(|children| {
            children
                .iter()
                .any(|child| frame_contains_target(child, target))
        })
}

/// Coordinates from DOM.getBoxModel are viewport-relative, and input events
/// only land inside the viewport, so make sure the node is visible first.
/// Best effort: a node that cannot be scrolled (display:none, detached) will
/// fail in DOM.getBoxModel with a clearer error anyway.
async fn scroll_node_into_view(client: &CdpClient, session_id: &str, backend_node_id: i64) {
    let _ = client
        .send_command(
            "DOM.scrollIntoViewIfNeeded",
            Some(serde_json::json!({ "backendNodeId": backend_node_id })),
            Some(session_id),
        )
        .await;
}

pub async fn resolve_element_object_id(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(String, String), String> {
    if let Some(ref_id) = parse_ref(selector_or_ref) {
        let entry = ref_map
            .get(&ref_id)
            .ok_or_else(|| format!("Unknown ref: {}", ref_id))?;

        let effective_session_id =
            resolve_frame_session(entry.frame_id.as_deref(), session_id, iframe_sessions);

        // Try cached backend_node_id first (fast path)
        if let Some(backend_node_id) = entry.backend_node_id {
            let result: Result<DomResolveNodeResult, String> = client
                .send_command_typed(
                    "DOM.resolveNode",
                    &DomResolveNodeParams {
                        backend_node_id: Some(backend_node_id),
                        node_id: None,
                        object_group: Some("agent-browser".to_string()),
                    },
                    Some(effective_session_id),
                )
                .await;

            if let Ok(r) = result {
                if let Some(object_id) = r.object.object_id {
                    return Ok((object_id, effective_session_id.to_string()));
                }
            }
            // backend_node_id is stale; re-query the accessibility tree below
        }

        // Fallback: re-query the accessibility tree to find a fresh node by role/name
        let fresh_id = find_node_id_by_role_name(
            client,
            session_id,
            &entry.role,
            &entry.name,
            entry.nth,
            entry.frame_id.as_deref(),
            iframe_sessions,
        )
        .await?;
        let result: DomResolveNodeResult = client
            .send_command_typed(
                "DOM.resolveNode",
                &DomResolveNodeParams {
                    backend_node_id: Some(fresh_id),
                    node_id: None,
                    object_group: Some("agent-browser".to_string()),
                },
                Some(effective_session_id),
            )
            .await?;
        let object_id = result
            .object
            .object_id
            .ok_or_else(|| format!("No objectId for ref {}", ref_id))?;
        return Ok((object_id, effective_session_id.to_string()));
    }

    // Selector fallback (CSS or XPath): honor an active `frame <sel>` selection.
    if let Some(frame_id) = active_frame() {
        if let Some(frame_session) = iframe_sessions.get(&frame_id) {
            let js = build_find_element_js(selector_or_ref);
            let result: EvaluateResult = client
                .send_command_typed(
                    "Runtime.evaluate",
                    &EvaluateParams {
                        expression: js,
                        return_by_value: Some(false),
                        await_promise: Some(false),
                    },
                    Some(frame_session.as_str()),
                )
                .await?;
            let object_id = result
                .result
                .object_id
                .ok_or_else(|| format!("Element not found: {}", selector_or_ref))?;
            return Ok((object_id, frame_session.clone()));
        }
        let object_id =
            resolve_object_in_same_process_frame(client, session_id, &frame_id, selector_or_ref)
                .await?;
        return Ok((object_id, session_id.to_string()));
    }

    let js = build_find_element_js(selector_or_ref);
    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.evaluate",
            &EvaluateParams {
                expression: js,
                return_by_value: Some(false),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;

    let object_id = result
        .result
        .object_id
        .ok_or_else(|| format!("Element not found: {}", selector_or_ref))?;
    Ok((object_id, session_id.to_string()))
}

/// Determine which CDP session and parameters to use for an AX tree query.
/// Out-of-process iframes have a dedicated session (no frameId needed);
/// in-process iframes, including cross-origin ones, use a frameId parameter.
pub(super) fn resolve_ax_session<'a>(
    frame_id: Option<&str>,
    session_id: &'a str,
    iframe_sessions: &'a HashMap<String, String>,
) -> (serde_json::Value, &'a str) {
    if let Some(frame_id) = frame_id {
        if let Some(iframe_sid) = iframe_sessions.get(frame_id) {
            (serde_json::json!({}), iframe_sid.as_str())
        } else {
            (serde_json::json!({ "frameId": frame_id }), session_id)
        }
    } else {
        (serde_json::json!({}), session_id)
    }
}

/// Resolve the effective CDP session for an element's frame.
/// If the element's frame_id has a dedicated out-of-process session, return it.
/// Otherwise, return the parent session.
fn resolve_frame_session<'a>(
    frame_id: Option<&str>,
    session_id: &'a str,
    iframe_sessions: &'a HashMap<String, String>,
) -> &'a str {
    frame_id
        .and_then(|fid| iframe_sessions.get(fid))
        .map(|s| s.as_str())
        .unwrap_or(session_id)
}

/// Re-query the accessibility tree to find a node matching role+name+nth,
/// returning its fresh backendDOMNodeId. This uses the same data source
/// (Accessibility.getFullAXTree) that built the ref map during snapshot,
/// so role/name matching is guaranteed to be consistent.
async fn find_node_id_by_role_name(
    client: &CdpClient,
    session_id: &str,
    role: &str,
    name: &str,
    nth: Option<usize>,
    frame_id: Option<&str>,
    iframe_sessions: &HashMap<String, String>,
) -> Result<i64, String> {
    let (ax_params, effective_session_id) =
        resolve_ax_session(frame_id, session_id, iframe_sessions);
    let ax_tree: GetFullAXTreeResult = client
        .send_command_typed(
            "Accessibility.getFullAXTree",
            &ax_params,
            Some(effective_session_id),
        )
        .await?;

    let nth_index = nth.unwrap_or(0);
    let mut match_count: usize = 0;

    for node in &ax_tree.nodes {
        if node.ignored.unwrap_or(false) {
            continue;
        }
        let node_role = extract_ax_string(&node.role);
        let node_name = extract_ax_string(&node.name);
        if node_role == role && node_name == name {
            if match_count == nth_index {
                return node.backend_d_o_m_node_id.ok_or_else(|| {
                    format!(
                        "AX node has no backendDOMNodeId for role={} name={}",
                        role, name
                    )
                });
            }
            match_count += 1;
        }
    }

    Err(format!(
        "Could not locate element with role={} name={}",
        role, name
    ))
}

pub(super) fn extract_ax_string(value: &Option<AXValue>) -> String {
    match value {
        Some(v) => match &v.value {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            _ => String::new(),
        },
        None => String::new(),
    }
}

/// Build a JS expression that finds a DOM element by CSS selector or XPath.
fn build_find_element_js(selector: &str) -> String {
    build_find_element_js_in("document", selector)
}

/// Same as build_find_element_js but rooted at an arbitrary Document
/// expression (e.g. an iframe's contentDocument).
fn build_find_element_js_in(root: &str, selector: &str) -> String {
    if let Some(xpath) = selector.strip_prefix("xpath=") {
        format!(
            "{root}.evaluate({xpath}, {root}, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue",
            xpath = serde_json::to_string(xpath).unwrap_or_default(),
        )
    } else {
        format!(
            "{root}.querySelector({selector})",
            selector = serde_json::to_string(selector).unwrap_or_default(),
        )
    }
}

/// Build a JS expression that counts matching DOM elements by CSS selector or XPath.
fn build_count_elements_js(selector: &str) -> String {
    if let Some(xpath) = selector.strip_prefix("xpath=") {
        format!(
            "document.evaluate({}, document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null).snapshotLength",
            serde_json::to_string(xpath).unwrap_or_default()
        )
    } else {
        format!(
            "document.querySelectorAll({}).length",
            serde_json::to_string(selector).unwrap_or_default()
        )
    }
}

/// Pure blocker description, retargeted to the caller's tree scopes. A scope
/// element supplies visibility only and cannot make the hit related or allowed.
const DESCRIBE_HIT_JS: &str = r#"(scope, hit) => {
    while (hit && !hit.tagName && hit.element) hit = hit.element;
    if (!hit) return null;
    const scopes = new Set();
    if (scope) {
        for (let node = scope; node; ) {
            const root = node.getRootNode();
            scopes.add(root);
            if (!root.host) break;
            node = root.host;
        }
    }
    // Name the nearest host visible from the target, not an inaccessible inner
    // node or the shared app shell. Null-scope descriptions reach the document.
    for (let root = hit.getRootNode(); root.host && !scopes.has(root); root = hit.getRootNode()) {
        hit = root.host;
    }
    let desc = hit.tagName.toLowerCase();
    if (hit.id) desc += '#' + hit.id;
    else if (typeof hit.className === 'string' && hit.className.trim())
        desc += '.' + hit.className.trim().split(/\s+/).slice(0, 2).join('.');
    if (!hit.id && hit.closest) {
        const anchored = hit.closest('[id]');
        if (anchored && anchored !== hit)
            desc += ' inside ' + anchored.tagName.toLowerCase() + '#' + anchored.id;
    }
    return desc;
}"#;

/// Shared relationship policy for native refs and document-local selectors.
/// Preserve ordinary/composed ancestry and labels; allow internal visuals only
/// within the nearest author component. A true third argument asks native
/// callers to confirm that root, then pass it or null to disable the exemption.
fn blocker_for_hit_js() -> String {
    format!(
        r#"(el, hit, nativeComponent) => {{
    // Native hits may be pseudo-elements or deep nodes in closed shadow roots.
    while (hit && !hit.tagName && hit.element) hit = hit.element;
    if (!hit || hit === el) return null;
    if (el) {{
        // assignedSlot hides closed-root assignments. Either side may expose
        // the assigning root (a slotted target or a slotted hit), so inspect both.
        const slots = new Map();
        const starts = [el, hit];
        if (nativeComponent && nativeComponent !== true) starts.push(nativeComponent);
        for (const start of starts) {{
            for (let node = start; node; ) {{
                const root = node.getRootNode();
                if (!root.host) break;
                for (const slot of root.querySelectorAll('slot')) {{
                    if (slot.assignedNodes) {{
                        for (const assigned of slot.assignedNodes()) slots.set(assigned, slot);
                    }}
                }}
                node = root.host;
            }}
        }}
        const parent = n => n.parentNode || n.host || (n.getRootNode && n.getRootNode().host) || null;
        const composedParent = n => n.assignedSlot || slots.get(n) || parent(n);
        for (const up of [parent, composedParent]) {{
            for (let n = hit; n; n = up(n)) {{ if (n === el) return null; }}
            for (let n = el; n; n = up(n)) {{ if (n === hit) return null; }}
        }}
        const hitLabel = hit.closest ? hit.closest('label') : null;
        if (hitLabel && (hitLabel.control === el || hitLabel.contains(el))) return null;
        const elLabel = el.closest ? el.closest('label') : null;
        if (elLabel && elLabel.contains(hit)) return null;

        // Sharing the outer app host does not make separate components related.
        // Stop at the target's first composed shadow root, including slot edges.
        let component = nativeComponent;
        if (nativeComponent === undefined || nativeComponent === true) {{
            component = null;
            for (let n = composedParent(el); n; n = composedParent(n)) {{
                if (n.nodeType === 11 && n.host) {{ component = n; break; }}
            }}
        }}
        if (component) {{
            for (let n = hit; n; n = composedParent(n)) {{
                if (n === component) return nativeComponent === true ? true : null;
            }}
        }}
    }}
    return ({DESCRIBE_HIT_JS})(el, hit);
}}"#
    )
}

/// Document-local hit testing for the CSS selector path. Reference clicks use
/// CDP hit testing so renderer coordinates never enter a different DOM viewport.
fn blocker_at_js() -> String {
    let blocker_for_hit = blocker_for_hit_js();
    format!(
        r#"(doc, el, x, y) => {{
            let d = doc, lx = x, ly = y;
            let hit = d.elementFromPoint(lx, ly);
            while (hit && (hit.tagName === 'IFRAME' || hit.tagName === 'FRAME') && hit.contentDocument && hit !== el) {{
                const r = hit.getBoundingClientRect();
                lx -= r.x + hit.clientLeft;
                ly -= r.y + hit.clientTop;
                d = hit.contentDocument;
                hit = d.elementFromPoint(lx, ly);
            }}
            return ({blocker_for_hit})(el, hit);
        }}"#
    )
}

fn build_selector_js(selector: &str) -> String {
    let blocker_at = blocker_at_js();
    let find_expr = build_find_element_js(selector);
    // Input events dispatch at viewport coordinates, so an element outside the
    // viewport must be scrolled into view first or the click lands on nothing.
    // The blocker check reports an overlay covering the click point instead of
    // letting the input land on it and silently doing the wrong thing.
    format!(
        r#"(() => {{
            const el = {find_expr};
            if (!el) return null;
            const inView = (r) => r.width > 0 && r.height > 0 &&
                r.bottom > 0 && r.right > 0 &&
                r.top < (window.innerHeight || document.documentElement.clientHeight) &&
                r.left < (window.innerWidth || document.documentElement.clientWidth);
            let rect = el.getBoundingClientRect();
            if (!inView(rect)) {{
                el.scrollIntoView({{ block: 'center', inline: 'center', behavior: 'instant' }});
                rect = el.getBoundingClientRect();
            }}
            const x = rect.x + rect.width / 2;
            const y = rect.y + rect.height / 2;
            const blockerAt = {blocker_at};
            return {{ x: x, y: y, blocker: blockerAt(document, el, x, y) }};
        }})()"#,
    )
}

async fn resolve_by_selector(
    client: &CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<(f64, f64), String> {
    let js = build_selector_js(selector);

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.evaluate",
            &EvaluateParams {
                expression: js,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;

    let val = result.result.value.unwrap_or(Value::Null);
    if let Some(blocker) = val.get("blocker").and_then(|v| v.as_str()) {
        return Err(intercepted_error(selector, blocker));
    }
    let x = val.get("x").and_then(|v| v.as_f64());
    let y = val.get("y").and_then(|v| v.as_f64());

    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(format!("Element not found: {}", selector)),
    }
}

fn intercepted_error(target: &str, blocker: &str) -> String {
    format!(
        "Element '{}' is covered by <{}> at its click point, so the input would land on that element instead. Dismiss or interact with the covering element first (it is often a dialog, banner, or sticky header).",
        target, blocker
    )
}

fn box_model_center(model: &BoxModel) -> (f64, f64) {
    // content quad: [x1,y1, x2,y2, x3,y3, x4,y4]
    if model.content.len() >= 8 {
        let x = (model.content[0] + model.content[2] + model.content[4] + model.content[6]) / 4.0;
        let y = (model.content[1] + model.content[3] + model.content[5] + model.content[7]) / 4.0;
        (x, y)
    } else {
        (0.0, 0.0)
    }
}

pub async fn get_element_text(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<String, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration:
                    "function() { return this.innerText || this.textContent || ''; }".to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result
        .result
        .value
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default())
}

pub async fn get_element_attribute(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    attribute: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<Value, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: format!(
                    "function() {{ return this.getAttribute({}); }}",
                    serde_json::to_string(attribute).unwrap_or_default()
                ),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result.result.value.unwrap_or(Value::Null))
}

pub async fn is_element_visible(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<bool, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    const rect = this.getBoundingClientRect();
                    const style = window.getComputedStyle(this);
                    return rect.width > 0 && rect.height > 0 &&
                           style.visibility !== 'hidden' &&
                           style.display !== 'none' &&
                           parseFloat(style.opacity) > 0;
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result
        .result
        .value
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

pub async fn is_element_enabled(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<bool, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function() { return !this.disabled; }".to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result
        .result
        .value
        .and_then(|v| v.as_bool())
        .unwrap_or(true))
}

pub async fn is_element_checked(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<bool, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    // Mirrors Playwright's getChecked() with follow-label retargeting:
    // 1. If element is a native checkbox/radio input, return .checked
    // 2. If element has an ARIA checked role, return aria-checked
    // 3. Follow label → input association (label.control)
    // 4. Check for nested checkbox/radio input as last resort
    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    var el = this;
                    // Native checkbox/radio input
                    var tag = el.tagName && el.tagName.toUpperCase();
                    if (tag === 'INPUT' && (el.type === 'checkbox' || el.type === 'radio')) {
                        return el.checked;
                    }
                    // ARIA role-based checked state
                    var role = el.getAttribute && el.getAttribute('role');
                    var ariaCheckedRoles = ['checkbox','radio','switch','menuitemcheckbox','menuitemradio','option','treeitem'];
                    if (role && ariaCheckedRoles.indexOf(role) !== -1) {
                        return el.getAttribute('aria-checked') === 'true';
                    }
                    // Follow label association (Playwright follow-label retarget)
                    var label = el;
                    if (tag !== 'LABEL') {
                        label = el.closest && el.closest('label');
                    }
                    if (label && label.tagName && label.tagName.toUpperCase() === 'LABEL' && label.control) {
                        var ctrl = label.control;
                        if (ctrl.type === 'checkbox' || ctrl.type === 'radio') {
                            return ctrl.checked;
                        }
                    }
                    // Check for nested native input
                    var input = el.querySelector && el.querySelector('input[type="checkbox"], input[type="radio"]');
                    if (input) return input.checked;
                    return false;
                }"#.to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result
        .result
        .value
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

pub async fn get_element_inner_text(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<String, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function() { return this.innerText || ''; }".to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result
        .result
        .value
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default())
}

pub async fn get_element_inner_html(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<String, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function() { return this.innerHTML || ''; }".to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result
        .result
        .value
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default())
}

pub async fn get_element_input_value(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<String, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration:
                    "function() { return typeof this.value === 'string' ? this.value : ''; }"
                        .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result
        .result
        .value
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default())
}

pub async fn set_element_value(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    value: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let js = format!(
        "function() {{ this.value = {}; this.dispatchEvent(new Event('input', {{bubbles: true}})); this.dispatchEvent(new Event('change', {{bubbles: true}})); }}",
        serde_json::to_string(value).unwrap_or_default()
    );

    client
        .send_command_typed::<_, EvaluateResult>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: js,
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn get_element_bounding_box(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<Value, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    const r = this.getBoundingClientRect();
                    return { x: r.x, y: r.y, width: r.width, height: r.height };
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    result
        .result
        .value
        .ok_or_else(|| format!("Could not get bounding box for: {}", selector_or_ref))
}

pub async fn get_element_count(
    client: &CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<i64, String> {
    let js = build_count_elements_js(selector);

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.evaluate",
            &EvaluateParams {
                expression: js,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;

    Ok(result.result.value.and_then(|v| v.as_i64()).unwrap_or(0))
}

pub async fn get_element_styles(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    properties: Option<Vec<String>>,
    iframe_sessions: &HashMap<String, String>,
) -> Result<Value, String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let js = match properties {
        Some(props) => {
            let props_json = serde_json::to_string(&props).unwrap_or("[]".to_string());
            format!(
                r#"function() {{
                    const s = window.getComputedStyle(this);
                    const props = {};
                    const result = {{}};
                    for (const p of props) result[p] = s.getPropertyValue(p);
                    return result;
                }}"#,
                props_json
            )
        }
        None => r#"function() {
                    const s = window.getComputedStyle(this);
                    const result = {};
                    for (let i = 0; i < s.length; i++) {
                        const p = s[i];
                        result[p] = s.getPropertyValue(p);
                    }
                    return result;
                }"#
        .to_string(),
    };

    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: js,
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(result.result.value.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ref_at_prefix() {
        assert_eq!(parse_ref("@e1"), Some("e1".to_string()));
        assert_eq!(parse_ref("@e123"), Some("e123".to_string()));
    }

    #[test]
    fn test_parse_ref_equals_prefix() {
        assert_eq!(parse_ref("ref=e1"), Some("e1".to_string()));
    }

    #[test]
    fn test_parse_ref_bare() {
        assert_eq!(parse_ref("e1"), Some("e1".to_string()));
        assert_eq!(parse_ref("e42"), Some("e42".to_string()));
    }

    #[test]
    fn test_parse_ref_invalid() {
        assert_eq!(parse_ref("button"), None);
        assert_eq!(parse_ref("e"), None);
        assert_eq!(parse_ref("1"), None);
        assert_eq!(parse_ref(""), None);
    }

    #[test]
    fn test_ref_map_basic() {
        let mut map = RefMap::new();
        map.add("e1".to_string(), Some(42), "button", "Submit", None);
        assert!(map.get("e1").is_some());
        assert_eq!(map.get("e1").unwrap().role, "button");
        assert!(map.get("e2").is_none());
    }

    #[test]
    fn test_ref_map_clear_preserves_monotonic_numbering() {
        let mut map = RefMap::new();
        map.add("e1".to_string(), Some(42), "button", "Submit", None);
        map.set_next_ref_num(2);

        map.begin_snapshot();

        assert!(map.get("e1").is_none());
        assert_eq!(map.next_ref_num(), 2);
    }

    #[test]
    fn test_durable_refs_are_scoped_by_document_and_frame() {
        let mut map = RefMap::new();
        assert!(map.observe_document("page-a", None, "session-a", Some("loader-a")));
        assert!(map.observe_document(
            "page-a",
            Some("frame-a"),
            "frame-session",
            Some("frame-loader")
        ));
        map.remember_durable_ref("page-a", None, 42, "e1");
        map.remember_durable_ref("page-a", Some("frame-a"), 42, "e2");
        assert_eq!(map.durable_ref("page-a", None, 42), Some("e1"));
        assert_eq!(map.durable_ref("page-a", Some("frame-a"), 42), Some("e2"));
        assert_eq!(map.durable_ref("page-b", None, 42), None);

        map.invalidate_page("page-a");
        assert_eq!(map.durable_ref("page-a", None, 42), None);
        assert_eq!(map.next_ref_num(), 1);
    }

    #[test]
    fn invalidating_one_page_preserves_other_page_documents() {
        let mut map = RefMap::new();
        assert!(map.observe_document("page-a", None, "session-a", Some("loader-a")));
        map.remember_durable_ref("page-a", None, 42, "e1");
        assert!(map.observe_document("page-b", None, "session-b", Some("loader-b")));
        map.remember_durable_ref("page-b", None, 42, "e2");

        map.invalidate_page("page-b");

        assert_eq!(map.durable_ref("page-a", None, 42), Some("e1"));
        assert_eq!(map.durable_ref("page-b", None, 42), None);
    }

    #[test]
    fn test_build_selector_js_css() {
        let js = build_selector_js("#submit-btn");
        assert!(js.contains("document.querySelector(\"#submit-btn\")"));
        assert!(!js.contains("document.evaluate"));
    }

    #[test]
    fn test_build_selector_js_xpath() {
        let js = build_selector_js("xpath=//button[@id='ok']");
        assert!(js.contains("document.evaluate(\"//button[@id='ok']\", document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null)"));
        assert!(!js.contains("document.querySelector"));
    }

    #[test]
    fn test_build_selector_js_xpath_empty() {
        let js = build_selector_js("xpath=");
        assert!(js.contains("document.evaluate"));
    }

    #[test]
    fn test_build_selector_js_not_xpath_prefix() {
        // "xpath" without "=" should be treated as CSS selector
        let js = build_selector_js("xpath//div");
        assert!(js.contains("document.querySelector"));
    }

    #[test]
    fn test_build_count_elements_js_css() {
        let js = build_count_elements_js(".item");
        assert!(js.contains("document.querySelectorAll(\".item\").length"));
        assert!(!js.contains("document.evaluate"));
    }

    #[test]
    fn test_build_count_elements_js_xpath() {
        let js = build_count_elements_js("xpath=//li");
        assert!(js.contains("document.evaluate(\"//li\", document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null).snapshotLength"));
        assert!(!js.contains("querySelectorAll"));
    }

    #[test]
    fn test_box_model_center() {
        let model = BoxModel {
            content: vec![10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0],
            padding: vec![],
            border: vec![],
            margin: vec![],
            width: 100,
            height: 40,
        };
        let (x, y) = box_model_center(&model);
        assert!((x - 60.0).abs() < 0.01);
        assert!((y - 40.0).abs() < 0.01);
    }

    #[test]
    fn test_hit_maps_to_owner_in_common_document() {
        let tree = serde_json::json!({
            "frame": {"id": "root"},
            "childFrames": [
                {"frame": {"id": "owner"}, "childFrames": [
                    {"frame": {"id": "nested"}, "childFrames": [
                        {"frame": {"id": "deep"}}
                    ]},
                    {"frame": {"id": "other"}, "childFrames": [
                        {"frame": {"id": "cousin"}}
                    ]}
                ]},
                {"frame": {"id": "sibling"}}
            ]
        });
        for (target, hit, expected) in [
            ("root", "owner", Some(("owner", "root"))),
            ("root", "deep", Some(("owner", "root"))),
            ("owner", "deep", Some(("nested", "owner"))),
            ("nested", "other", Some(("other", "owner"))),
            ("deep", "cousin", Some(("other", "owner"))),
            ("cousin", "deep", Some(("nested", "owner"))),
            ("nested", "sibling", Some(("sibling", "root"))),
            // Reversed paths select the description scope on the target side.
            ("sibling", "deep", Some(("owner", "root"))),
            ("other", "deep", Some(("nested", "owner"))),
            ("owner", "owner", None),
            ("owner", "root", None),
            ("deep", "nested", None),
            ("missing", "nested", None),
            ("nested", "missing", None),
        ] {
            assert_eq!(
                hit_frame_owner(&tree, target, hit),
                expected,
                "{target} -> {hit}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // resolve_frame_session tests (Issue #925)
    // Cross-origin iframe elements must resolve to the dedicated session.
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_origin_element_uses_dedicated_session() {
        let mut iframe_sessions = HashMap::new();
        iframe_sessions.insert(
            "cross-origin-frame".to_string(),
            "iframe-session".to_string(),
        );

        let session = resolve_frame_session(
            Some("cross-origin-frame"),
            "parent-session",
            &iframe_sessions,
        );

        assert_eq!(session, "iframe-session");
    }

    #[test]
    fn test_same_origin_element_uses_parent_session() {
        let iframe_sessions = HashMap::new();

        let session = resolve_frame_session(
            Some("same-origin-frame"),
            "parent-session",
            &iframe_sessions,
        );

        assert_eq!(session, "parent-session");
    }

    #[test]
    fn test_main_frame_element_uses_parent_session() {
        let iframe_sessions = HashMap::new();

        let session = resolve_frame_session(None, "parent-session", &iframe_sessions);

        assert_eq!(session, "parent-session");
    }
}
