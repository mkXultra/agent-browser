//! End-to-end tests for the native daemon.
//!
//! These tests launch a real Chrome instance and exercise the full command
//! pipeline. They require Chrome to be installed and are marked `#[ignore]`
//! so they don't run during normal `cargo test`.
//!
//! Run serially to avoid Chrome instance contention:
//!   cargo test e2e -- --ignored --test-threads=1

use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::test_utils::EnvGuard;

use super::actions::{
    close_current_browser, execute_command, maybe_autosave_restore_state, DaemonState,
};

fn assert_success(resp: &Value) {
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "Expected success but got: {}",
        serde_json::to_string_pretty(resp).unwrap_or_default()
    );
}

fn get_data(resp: &Value) -> &Value {
    resp.get("data").expect("Missing 'data' in response")
}

async fn goal_fixture_command(cmd: &Value, state: &mut DaemonState) -> Value {
    Box::pin(execute_command(cmd, state)).await
}

#[tokio::test]
#[ignore]
async fn e2e_goal_guarded_click_waits_for_local_spinner_and_rejects_replaced_node() {
    let mut state = DaemonState::new();
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    let html = r#"<h1 id='month'>June</h1><button id='previous' onclick='window.clicks++; document.getElementById("spinner").style.display="block"; window.transition = new Promise(resolve => window.releaseTransition = () => { document.getElementById("month").textContent="May"; document.getElementById("spinner").style.display="none"; resolve(); });'>Previous</button><div id='spinner' style='display:none;position:fixed;inset:0;z-index:10;background:rgba(0,0,0,.1)'></div><script>window.clicks=0</script>"#;
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    assert_success(
        &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
    );
    let snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    let reference = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Previous")
        .map(|(name, _)| name.clone())
        .unwrap();
    let selector = format!("@{reference}");
    let probe = goal_fixture_command(
        &json!({"action":"__goal_probe","selector":selector,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    assert_success(&probe);
    assert_eq!(probe["data"]["status"], "ready", "{probe}");
    let identity = probe["data"]["identity"].clone();
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 30_000;
    let clicked = goal_fixture_command(&json!({"action":"click","selector":selector,"goalGuard":identity,"goalTimeoutMs":1000,"goalDeadlineUnixMs":expiry}), &mut state).await;
    assert_success(&clicked);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let covered = goal_fixture_command(&json!({"action":"__goal_probe","selector":selector,"expectedIdentity":identity,"goalTimeoutMs":500}), &mut state).await;
    assert_success(&covered);
    assert_eq!(covered["data"]["status"], "covered", "{covered}");
    let refused = goal_fixture_command(&json!({"action":"click","selector":selector,"goalGuard":identity,"goalTimeoutMs":500,"goalDeadlineUnixMs":expiry}), &mut state).await;
    assert_eq!(refused["success"], false, "{refused}");
    assert_success(
        &goal_fixture_command(
            &json!({"action":"evaluate","script":"window.releaseTransition()"}),
            &mut state,
        )
        .await,
    );
    let ready = goal_fixture_command(&json!({"action":"__goal_probe","selector":selector,"expectedIdentity":identity,"goalTimeoutMs":500}), &mut state).await;
    assert_eq!(ready["data"]["status"], "ready", "{ready}");
    assert_success(&goal_fixture_command(&json!({"action":"evaluate","script":"document.getElementById('previous').outerHTML = '<button id=\"previous\" onclick=\"window.clicks++\">Previous</button>'"}), &mut state).await);
    let replaced = goal_fixture_command(&json!({"action":"click","selector":selector,"goalGuard":identity,"goalTimeoutMs":500,"goalDeadlineUnixMs":expiry}), &mut state).await;
    assert_eq!(replaced["success"], false, "{replaced}");
    let fresh_snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
    let fresh_ref = get_data(&fresh_snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Previous")
        .map(|(name, _)| name.clone())
        .unwrap();
    let fresh_selector = format!("@{fresh_ref}");
    let fresh_probe = goal_fixture_command(
        &json!({"action":"__goal_probe","selector":fresh_selector,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    assert_eq!(fresh_probe["data"]["status"], "ready", "{fresh_probe}");
    let policy_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(policy_file.path(), r#"{"confirm":["click"]}"#).unwrap();
    state.policy =
        Some(super::policy::ActionPolicy::load(policy_file.path().to_str().unwrap()).unwrap());
    let pending = goal_fixture_command(&json!({"action":"click","selector":fresh_selector,"goalGuard":fresh_probe["data"]["identity"],"goalTimeoutMs":1000,"goalDeadlineUnixMs":expiry}), &mut state).await;
    assert_eq!(pending["data"]["confirmation_required"], true, "{pending}");
    assert_success(&goal_fixture_command(&json!({"action":"evaluate","script":"document.getElementById('previous').outerHTML = '<button id=\"previous\" onclick=\"window.clicks++\">Previous</button>'"}), &mut state).await);
    let confirmed = goal_fixture_command(&json!({"action":"confirm"}), &mut state).await;
    assert_eq!(confirmed["data"]["result"]["success"], false, "{confirmed}");
    assert!(
        confirmed["data"]["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("Goal target unavailable"),
        "{confirmed}"
    );
    assert_success(&goal_fixture_command(&json!({"action":"evaluate","script":"window.clicks === 1 && document.getElementById('month').textContent === 'May'"}), &mut state).await);
    let count = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.clicks"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&count)["result"], 1);
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_covering_dialog_must_be_new_visible_and_useful() {
    let mut state = DaemonState::new();
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    let cases = [
        (
            "existing target dialog",
            "<section role='dialog' style='position:relative'><button>Previous</button><div id='cover' hidden style='position:absolute;inset:0;z-index:10;background:#eee'>Loading...</div></section>",
            false,
        ),
        (
            "busy-only dialog",
            "<button>Previous</button><div id='cover' hidden role='dialog' style='position:fixed;inset:0;z-index:10;background:#eee'>Loading...</div>",
            false,
        ),
        (
            "busy-only dialog with detail",
            "<button>Previous</button><div id='cover' hidden role='dialog' style='position:fixed;inset:0;z-index:10;background:#eee'>Loading your calendar...</div>",
            false,
        ),
        (
            "localized loading dialog",
            "<button>Previous</button><div id='cover' hidden role='dialog' style='position:fixed;inset:0;z-index:10;background:#eee'>読み込み中...</div>",
            false,
        ),
        (
            "text-only settings dialog",
            "<button>Previous</button><div id='cover' hidden role='dialog' style='position:fixed;inset:0;z-index:10;background:#eee'>Settings open</div>",
            false,
        ),
        (
            "busy dialog with cancel",
            "<button>Previous</button><div id='cover' hidden role='dialog' style='position:fixed;inset:0;z-index:10;background:#eee'><div role='progressbar'></div><button>Cancel</button></div>",
            false,
        ),
        (
            "unstructured spinner with cancel",
            "<button>Previous</button><div id='cover' hidden role='dialog' style='position:fixed;inset:0;z-index:10;background:#eee'>Loading...<button>Cancel</button></div>",
            false,
        ),
        (
            "disabled dialog control",
            "<button>Previous</button><div id='cover' hidden role='dialog' style='position:fixed;inset:0;z-index:10;background:#eee'><button aria-disabled='true'>Cancel</button></div>",
            false,
        ),
        (
            "visible dialog after hidden sibling",
            "<button>Previous</button><div id='cover' hidden style='position:fixed;inset:0;z-index:10;background:#eee'><div role='dialog' hidden><button>Inactive</button></div><div role='dialog' aria-label='Settings'>Settings open <button>Dismiss</button></div></div>",
            true,
        ),
    ];
    for (name, html, expected_interface) in cases {
        let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
        assert_success(
            &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
        );
        let snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
        let selector = get_data(&snapshot)["refs"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, entry)| entry["name"] == "Previous")
            .map(|(reference, _)| format!("@{reference}"))
            .unwrap_or_else(|| panic!("missing Previous ref for {name}: {snapshot}"));
        assert_success(
            &goal_fixture_command(
                &json!({"action":"evaluate","script":"document.getElementById('cover').hidden=false"}),
                &mut state,
            )
            .await,
        );
        let probe = goal_fixture_command(
            &json!({"action":"__goal_probe","selector":selector,"goalTimeoutMs":500}),
            &mut state,
        )
        .await;
        assert_success(&probe);
        assert_eq!(probe["data"]["status"], "covered", "{name}: {probe}");
        assert_eq!(
            probe["data"]["coveringInterface"], expected_interface,
            "{name}: {probe}"
        );
    }
    // The assigning slot is the target's rendered parent even when a closed
    // root hides assignedSlot from the light-DOM button.
    for mode in ["open", "closed"] {
        let slotted = r#"<x-calendar><button slot='target' onclick='window.clicks++'>Previous</button></x-calendar>
<script>customElements.define('x-calendar', class extends HTMLElement {
  connectedCallback() {
    window.fixtureRoot=this.attachShadow({mode:'open'});
    window.fixtureRoot.innerHTML = `<div role='dialog' aria-label='Owner settings' style='position:relative;width:300px;height:120px'><slot name='target'></slot><div id='cover' hidden style='position:fixed;inset:0;z-index:2147483647;background:#eee;pointer-events:auto'><button>Dismiss</button></div></div>`;
  }
});window.clicks=0;</script>"#.replace("mode:'open'", &format!("mode:'{mode}'"));
        let url = format!("data:text/html;base64,{}", STANDARD.encode(slotted));
        assert_success(
            &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
        );
        let snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
        let selector = get_data(&snapshot)["refs"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, entry)| entry["name"] == "Previous")
            .map(|(reference, _)| format!("@{reference}"))
            .unwrap_or_else(|| panic!("missing slotted Previous: {snapshot}"));
        assert_success(&goal_fixture_command(&json!({"action":"evaluate","script":"window.fixtureRoot.querySelector('#cover').hidden=false"}), &mut state).await);
        let geometry = goal_fixture_command(
        &json!({"action":"evaluate","script":"(()=>{const host=document.querySelector('x-calendar');const button=host.querySelector('button');const cover=window.fixtureRoot.querySelector('#cover');const box=button.getBoundingClientRect();const hit=window.fixtureRoot.elementFromPoint(box.x+box.width/2,box.y+box.height/2);return {hitInsideCover:cover.contains(hit),coverVisible:getComputedStyle(cover).display!=='none'}})()"}),
        &mut state,
    ).await;
        assert_success(&geometry);
        assert_eq!(
            get_data(&geometry)["result"]["hitInsideCover"],
            true,
            "{geometry}"
        );
        let probe = goal_fixture_command(
            &json!({"action":"__goal_probe","selector":selector,"goalTimeoutMs":500}),
            &mut state,
        )
        .await;
        assert_success(&probe);
        assert_eq!(probe["data"]["status"], "covered", "{probe}");
        assert_eq!(probe["data"]["coveringInterface"], false, "{probe}");
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 1000;
        let refused = goal_fixture_command(
        &json!({"action":"click","selector":selector,"goalGuard":probe["data"]["identity"],"goalTimeoutMs":500,"goalDeadlineUnixMs":expiry}),
        &mut state,
    ).await;
        assert_eq!(refused["success"], false, "{refused}");
        let count = goal_fixture_command(
            &json!({"action":"evaluate","script":"window.clicks"}),
            &mut state,
        )
        .await;
        assert_success(&count);
        assert_eq!(get_data(&count)["result"], 0, "{count}");
    }
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_component_visuals_keep_exact_hit_through_repeat_and_approval() {
    let html = r#"<!doctype html><body style='margin:0'><script>
window.hits={ripple:0,toggle:0,decor:0,details:0,summary:0};
function host(top,width,height) {
  const h=document.createElement('div');
  h.style=`position:absolute;left:20px;top:${top}px;width:${width}px;height:${height}px`;
  document.body.append(h); return h;
}
const ripple=host(20,160,40), rr=ripple.attachShadow({mode:'open'});
rr.innerHTML='<button style="width:160px;height:40px">Ripple target</button><div id="ripple" style="position:absolute;inset:0"></div>';
rr.getElementById('ripple').attachShadow({mode:'closed'}).innerHTML='<div style="position:absolute;inset:0;background:#ccc"></div>';
ripple.onclick=()=>hits.ripple++;
const toggle=host(80,60,30), tr=toggle.attachShadow({mode:'open'});
tr.innerHTML='<input type="checkbox" aria-label="Toggle target" style="position:absolute;inset:0;margin:0;width:60px;height:30px;opacity:.01"><span class="track" style="position:absolute;inset:0;background:#ccc"></span>';
const ti=tr.querySelector('input');toggle.onclick=e=>{if(e.composedPath()[0]!==ti)ti.checked=!ti.checked;hits.toggle++};
const decor=host(130,200,40), di=document.createElement('input');
di.type='checkbox';di.setAttribute('aria-label','Decor target');di.style='position:absolute;inset:0;margin:0;width:200px;height:40px';decor.append(di);
window.decorRoot=decor.attachShadow({mode:'open'});
decorRoot.innerHTML='<slot></slot><span class="decor" style="position:absolute;inset:0;background:#ccc"></span>';
decor.onclick=e=>{if(e.composedPath()[0]!==di)di.checked=!di.checked;hits.decor++};
for(const [i,kind] of ['details','summary'].entries()) {
  const h=host(200+i*110,240,80), root=h.attachShadow({mode:'open'});
  const input='<input type="checkbox" aria-label="'+kind+' target" style="width:40px;height:40px;margin:0">';
  root.innerHTML=(kind==='details'?'<details open><summary>Question</summary>'+input+'</details>':'<details><summary>'+input+'</summary>Body</details>')+
    '<span class="visual" style="position:absolute;inset:0;background:#ddd"></span>';
  const control=root.querySelector('input');h.onclick=e=>{if(e.composedPath()[0]!==control)control.checked=!control.checked;hits[kind]++};
}
</script></body>"#;
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, html).await;
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 20_000;
    let cases = [
        ("Ripple target", "ripple"),
        ("Toggle target", "toggle"),
        ("Decor target", "decor"),
        ("details target", "details"),
        ("summary target", "summary"),
    ];
    let mut decor_identity = Value::Null;
    let mut decor_selector = String::new();
    for (name, kind) in cases {
        let selector = format!("@{}", reference_named(&snapshot, name));
        let probe = goal_fixture_command(
            &json!({"action":"__goal_probe","selector":selector,"goalTimeoutMs":500}),
            &mut state,
        )
        .await;
        assert_eq!(probe["data"]["status"], "ready", "{name}: {probe}");
        let identity = probe["data"]["identity"].clone();
        assert!(
            identity["componentVisualHit"].is_number(),
            "{name}: {probe}"
        );
        if kind == "decor" {
            decor_identity = identity.clone();
            decor_selector = selector.clone();
        }
        for expected in [1, 2] {
            let ready = goal_fixture_command(&json!({"action":"__goal_probe","selector":selector,"expectedIdentity":identity,"goalTimeoutMs":500}), &mut state).await;
            assert_eq!(ready["data"]["status"], "ready", "{name}: {ready}");
            let clicked = goal_fixture_command(&json!({"action":"click","selector":selector,"goalGuard":identity,"goalTimeoutMs":1000,"goalDeadlineUnixMs":expiry}), &mut state).await;
            assert_success(&clicked);
            let count = goal_fixture_command(
                &json!({"action":"evaluate","script":format!("window.hits.{kind}")}),
                &mut state,
            )
            .await;
            assert_eq!(get_data(&count)["result"], expected, "{name}: {count}");
        }
    }
    let policy_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(policy_file.path(), r#"{"confirm":["click"]}"#).unwrap();
    state.policy =
        Some(super::policy::ActionPolicy::load(policy_file.path().to_str().unwrap()).unwrap());
    let ripple = format!("@{}", reference_named(&snapshot, "Ripple target"));
    let ripple_probe = goal_fixture_command(
        &json!({"action":"__goal_probe","selector":ripple,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    assert_eq!(ripple_probe["data"]["status"], "ready", "{ripple_probe}");
    let pending = goal_fixture_command(&json!({"action":"click","selector":ripple,"goalGuard":ripple_probe["data"]["identity"],"goalTimeoutMs":1000,"goalDeadlineUnixMs":expiry}), &mut state).await;
    assert_eq!(pending["data"]["confirmation_required"], true, "{pending}");
    let confirmed = goal_fixture_command(&json!({"action":"confirm"}), &mut state).await;
    assert_eq!(confirmed["data"]["result"]["success"], true, "{confirmed}");
    let count = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.hits.ripple"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&count)["result"], 3);
    let pending_decor = goal_fixture_command(&json!({"action":"click","selector":decor_selector,"goalGuard":decor_identity,"goalTimeoutMs":1000,"goalDeadlineUnixMs":expiry}), &mut state).await;
    assert_eq!(
        pending_decor["data"]["confirmation_required"], true,
        "{pending_decor}"
    );
    assert_success(&goal_fixture_command(&json!({"action":"evaluate","script":"const cover=document.createElement('div');cover.id='new-cover';cover.style='position:fixed;inset:0;z-index:100;background:rgba(0,0,0,.1)';window.decorRoot.append(cover)"}), &mut state).await);
    let refused_after_approval =
        goal_fixture_command(&json!({"action":"confirm"}), &mut state).await;
    assert_eq!(
        refused_after_approval["data"]["result"]["success"], false,
        "{refused_after_approval}"
    );
    state.policy = None;
    let covered = goal_fixture_command(&json!({"action":"__goal_probe","selector":decor_selector,"expectedIdentity":decor_identity,"goalTimeoutMs":500}), &mut state).await;
    assert_eq!(covered["data"]["status"], "covered", "{covered}");
    let refused = goal_fixture_command(&json!({"action":"click","selector":decor_selector,"goalGuard":decor_identity,"goalTimeoutMs":500,"goalDeadlineUnixMs":expiry}), &mut state).await;
    assert_eq!(refused["success"], false, "{refused}");
    let count = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.hits.decor"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&count)["result"], 2);
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_component_visual_inside_owner_dialog_respects_approval_and_busy_cover() {
    let html = r#"<!doctype html><body><x-panel></x-panel><script>
window.hits=0;
customElements.define('x-panel', class extends HTMLElement { connectedCallback() {
  const root=this.attachShadow({mode:'open'});window.panelRoot=root;
  root.innerHTML='<div role="dialog" aria-modal="true" aria-label="Settings" style="position:absolute;left:20px;top:60px;padding:12px;background:white">'+
    '<h2>Settings</h2><div class="switch" style="position:relative;width:60px;height:30px">'+
    '<input type="checkbox" aria-label="Notifications" style="position:absolute;inset:0;width:60px;height:30px;margin:0;opacity:.01">'+
    '<span class="track" style="position:absolute;inset:0;background:#ccc"></span></div></div>';
  const input=root.querySelector('input');root.querySelector('.switch').onclick=e=>{
    if(e.composedPath()[0]!==input)input.checked=!input.checked;window.hits++;
  };
} });</script></body>"#;
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, html).await;
    let selector = format!("@{}", reference_named(&snapshot, "Notifications"));
    let probe = goal_fixture_command(
        &json!({"action":"__goal_probe","selector":selector,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    assert_eq!(probe["data"]["status"], "ready", "{probe}");
    let identity = probe["data"]["identity"].clone();
    assert!(identity["componentVisualHit"].is_number(), "{probe}");
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 20_000;
    let click = json!({"action":"click","selector":selector,"goalGuard":identity,
        "goalTimeoutMs":1000,"goalDeadlineUnixMs":expiry});
    assert_success(&goal_fixture_command(&click, &mut state).await);
    let count = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&count)["result"], 1, "{count}");
    let policy_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(policy_file.path(), r#"{"confirm":["click"]}"#).unwrap();
    state.policy =
        Some(super::policy::ActionPolicy::load(policy_file.path().to_str().unwrap()).unwrap());
    let pending = goal_fixture_command(&click, &mut state).await;
    assert_eq!(pending["data"]["confirmation_required"], true, "{pending}");
    let approved = goal_fixture_command(&json!({"action":"confirm"}), &mut state).await;
    assert_eq!(approved["data"]["result"]["success"], true, "{approved}");
    let count = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&count)["result"], 2, "{count}");
    let pending = goal_fixture_command(&click, &mut state).await;
    assert_eq!(pending["data"]["confirmation_required"], true, "{pending}");
    assert_success(&goal_fixture_command(&json!({"action":"evaluate","script":
        "const cover=document.createElement('div');cover.setAttribute('role','progressbar');cover.style='position:absolute;inset:0;background:#999';window.panelRoot.querySelector('.switch').append(cover)"}), &mut state).await);
    let refused = goal_fixture_command(&json!({"action":"confirm"}), &mut state).await;
    assert_eq!(refused["data"]["result"]["success"], false, "{refused}");
    let count = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&count)["result"], 2, "{count}");
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_expired_deadline_prevents_click_and_late_approval() {
    let mut state = DaemonState::new();
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    let html =
        "<button onclick='window.clicks++'>Previous</button><script>window.clicks=0</script>";
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    assert_success(
        &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
    );
    let snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
    let selector = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Previous")
        .map(|(name, _)| format!("@{name}"))
        .unwrap();
    let probe = goal_fixture_command(
        &json!({"action":"__goal_probe","selector":selector,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    assert_eq!(probe["data"]["status"], "ready", "{probe}");
    let identity = probe["data"]["identity"].clone();
    let expired = goal_fixture_command(&json!({"action":"click","selector":selector,"goalGuard":identity,"goalTimeoutMs":500,"goalDeadlineUnixMs":0}), &mut state).await;
    assert_eq!(expired["success"], false, "{expired}");
    assert!(expired["error"].as_str().unwrap_or("").contains("deadline"));
    let policy_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(policy_file.path(), r#"{"confirm":["click"]}"#).unwrap();
    state.policy =
        Some(super::policy::ActionPolicy::load(policy_file.path().to_str().unwrap()).unwrap());
    let short_expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 50;
    let pending = goal_fixture_command(&json!({"action":"click","selector":selector,"goalGuard":identity,"goalTimeoutMs":500,"goalDeadlineUnixMs":short_expiry}), &mut state).await;
    assert_eq!(pending["data"]["confirmation_required"], true, "{pending}");
    tokio::time::sleep(std::time::Duration::from_millis(70)).await;
    let confirmed = goal_fixture_command(&json!({"action":"confirm"}), &mut state).await;
    assert_eq!(confirmed["data"]["result"]["success"], false, "{confirmed}");
    assert!(confirmed["data"]["result"]["error"]
        .as_str()
        .unwrap_or("")
        .contains("deadline"));
    let count = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.clicks"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&count)["result"], 0);
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_approval_gets_new_execution_slice_within_original_deadline() {
    let mut state = DaemonState::new();
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    let html =
        "<button onclick='window.clicks++'>Previous</button><script>window.clicks=0</script>";
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    assert_success(
        &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
    );
    let snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
    let selector = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Previous")
        .map(|(name, _)| format!("@{name}"))
        .unwrap();
    let probe = goal_fixture_command(
        &json!({"action":"__goal_probe","selector":selector,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    let identity = probe["data"]["identity"].clone();
    for action in ["snapshot", "click"] {
        let policy_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(policy_file.path(), format!(r#"{{"confirm":["{action}"]}}"#)).unwrap();
        state.policy =
            Some(super::policy::ActionPolicy::load(policy_file.path().to_str().unwrap()).unwrap());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut command = json!({
            "action": action,
            "goalTimeoutMs": 250,
            "goalDeadlineUnixMs": now + 250,
            "goalApprovalDeadlineUnixMs": now + 3000,
        });
        if action == "snapshot" {
            command["goalExistingOnly"] = json!(true);
        } else {
            command["selector"] = json!(selector);
            command["goalGuard"] = identity.clone();
        }
        let pending = goal_fixture_command(&command, &mut state).await;
        assert_eq!(pending["data"]["confirmation_required"], true, "{pending}");
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        let confirmed = goal_fixture_command(&json!({"action":"confirm"}), &mut state).await;
        assert_eq!(confirmed["data"]["result"]["success"], true, "{confirmed}");
        state.policy = None;
    }
    let count = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.clicks"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&count)["result"], 1);
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_probe_rejects_prior_browser_incarnation() {
    let mut state = DaemonState::new();
    let html = "<button>Previous</button>";
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    assert_success(
        &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
    );
    let first = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
    let first_ref = get_data(&first)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Previous")
        .map(|(name, _)| format!("@{name}"))
        .unwrap();
    let first_probe = goal_fixture_command(
        &json!({"action":"__goal_probe","selector":first_ref,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    assert_eq!(first_probe["data"]["status"], "ready", "{first_probe}");
    let identity = first_probe["data"]["identity"].clone();
    let _ = close_current_browser(&mut state).await;
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    assert_success(
        &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
    );
    let second = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
    let second_ref = get_data(&second)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Previous")
        .map(|(name, _)| format!("@{name}"))
        .unwrap();
    let stale = goal_fixture_command(&json!({"action":"__goal_probe","selector":second_ref,"expectedIdentity":identity,"goalTimeoutMs":500}), &mut state).await;
    assert_eq!(stale["data"]["status"], "unavailable", "{stale}");
    assert_eq!(stale["data"]["detail"], "Target context changed");
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_probe_preserves_scroll_and_supports_shadow_and_same_process_frame() {
    let mut state = DaemonState::new();
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    let html = r#"<div id='host'></div><iframe srcdoc='<button onclick="parent.frameClicks++">Frame target</button>'></iframe><div style='height:1800px'></div><button onmouseover='window.farHovers++' onclick='window.farClicks++'>Far target</button><script>window.shadowClicks=0;window.frameClicks=0;window.farClicks=0;window.farHovers=0;document.getElementById('host').attachShadow({mode:'open'}).innerHTML='<button onclick="window.shadowClicks++">Shadow target</button>'</script>"#;
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    assert_success(
        &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
    );
    let snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    let refs = get_data(&snapshot)["refs"].as_object().unwrap();
    let target_ref = |name: &str| {
        refs.iter()
            .find(|(_, entry)| entry["name"] == name)
            .map(|(reference, _)| format!("@{reference}"))
            .unwrap_or_else(|| panic!("missing {name} ref in {snapshot}"))
    };
    let far = target_ref("Far target");
    let before = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.scrollY"}),
        &mut state,
    )
    .await;
    let far_probe = goal_fixture_command(
        &json!({"action":"__goal_probe","selector":far,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    assert_success(&far_probe);
    let after = goal_fixture_command(
        &json!({"action":"evaluate","script":"window.scrollY"}),
        &mut state,
    )
    .await;
    assert_eq!(
        get_data(&before)["result"],
        get_data(&after)["result"],
        "probe must not scroll"
    );
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 10_000;
    let far_click = goal_fixture_command(
        &json!({"action":"click","selector":far,"goalGuard":far_probe["data"]["identity"],"goalTimeoutMs":2000,"goalDeadlineUnixMs":expiry}),
        &mut state,
    )
    .await;
    assert_success(&far_click);
    let far_count = goal_fixture_command(
        &json!({"action":"evaluate","script":"({clicks:window.farClicks,hovers:window.farHovers})"}),
        &mut state,
    )
    .await;
    assert_eq!(get_data(&far_count)["result"]["clicks"], 1);
    assert!(get_data(&far_count)["result"]["hovers"].as_u64().unwrap() >= 1);
    assert_success(
        &goal_fixture_command(
            &json!({"action":"evaluate","script":"window.scrollTo(0,0)"}),
            &mut state,
        )
        .await,
    );
    for name in ["Shadow target", "Frame target"] {
        let selector = target_ref(name);
        let probe = goal_fixture_command(
            &json!({"action":"__goal_probe","selector":selector,"goalTimeoutMs":500}),
            &mut state,
        )
        .await;
        assert_eq!(probe["data"]["status"], "ready", "{name}: {probe}");
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 10_000;
        let clicked = goal_fixture_command(&json!({"action":"click","selector":selector,"goalGuard":probe["data"]["identity"],"goalTimeoutMs":1000,"goalDeadlineUnixMs":expiry}), &mut state).await;
        assert_success(&clicked);
    }
    let counts = goal_fixture_command(&json!({"action":"evaluate","script":"({shadow:window.shadowClicks,frame:window.frameClicks})"}), &mut state).await;
    assert_eq!(get_data(&counts)["result"], json!({"shadow":1,"frame":1}));
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_guard_rejects_node_adopted_into_another_document() {
    let mut state = DaemonState::new();
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    let html = r#"<button id='move'>Move target</button><iframe id='frame'></iframe><script>window.clicks=0;document.getElementById('move').addEventListener('click',()=>window.clicks++)</script>"#;
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    for confirm in [false, true] {
        assert_success(
            &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
        );
        let snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
        let reference = get_data(&snapshot)["refs"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, entry)| entry["name"] == "Move target")
            .map(|(name, _)| format!("@{name}"))
            .unwrap();
        let probe = goal_fixture_command(
            &json!({"action":"__goal_probe","selector":reference,"goalTimeoutMs":500}),
            &mut state,
        )
        .await;
        assert_eq!(probe["data"]["status"], "ready", "{probe}");
        let identity = probe["data"]["identity"].clone();
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 10_000;
        let click = json!({"action":"click","selector":reference,"goalGuard":identity,"goalTimeoutMs":1000,"goalDeadlineUnixMs":expiry});
        if confirm {
            let policy_file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(policy_file.path(), r#"{"confirm":["click"]}"#).unwrap();
            state.policy = Some(
                super::policy::ActionPolicy::load(policy_file.path().to_str().unwrap()).unwrap(),
            );
            let pending = goal_fixture_command(&click, &mut state).await;
            assert_eq!(pending["data"]["confirmation_required"], true, "{pending}");
        }
        assert_success(&goal_fixture_command(&json!({"action":"evaluate","script":"document.getElementById('frame').contentDocument.body.appendChild(document.getElementById('move')); true"}), &mut state).await);
        let moved = goal_fixture_command(&json!({"action":"__goal_probe","selector":reference,"expectedIdentity":identity,"goalTimeoutMs":500}), &mut state).await;
        assert_eq!(moved["data"]["status"], "unavailable", "{moved}");
        let refused = if confirm {
            let confirmation = goal_fixture_command(&json!({"action":"confirm"}), &mut state).await;
            confirmation["data"]["result"].clone()
        } else {
            goal_fixture_command(&click, &mut state).await
        };
        assert_eq!(refused["success"], false, "{refused}");
        let count = goal_fixture_command(
            &json!({"action":"evaluate","script":"window.clicks"}),
            &mut state,
        )
        .await;
        assert_eq!(get_data(&count)["result"], 0);
        state.policy = None;
    }
    let _ = close_current_browser(&mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_goal_slow_mouse_press_reports_uncertainty_before_late_release() {
    let mut state = DaemonState::new();
    assert_success(
        &goal_fixture_command(&json!({"action":"launch","headless":true}), &mut state).await,
    );
    let html = r#"<button id='slow'>Slow target</button><script>window.clicks=0;const b=document.getElementById('slow');b.addEventListener('mousedown',()=>{const end=performance.now()+700;while(performance.now()<end){};});b.addEventListener('click',()=>window.clicks++);</script>"#;
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    for confirm in [false, true] {
        assert_success(
            &goal_fixture_command(&json!({"action":"navigate","url":url}), &mut state).await,
        );
        let snapshot = goal_fixture_command(&json!({"action":"snapshot"}), &mut state).await;
        let reference = get_data(&snapshot)["refs"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, entry)| entry["name"] == "Slow target")
            .map(|(name, _)| format!("@{name}"))
            .unwrap();
        let probe = goal_fixture_command(
            &json!({"action":"__goal_probe","selector":reference,"goalTimeoutMs":500}),
            &mut state,
        )
        .await;
        assert_eq!(probe["data"]["status"], "ready", "{probe}");
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 350;
        let click = json!({"action":"click","selector":reference,"goalGuard":probe["data"]["identity"],"goalTimeoutMs":350,"goalDeadlineUnixMs":expiry});
        if confirm {
            let policy_file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(policy_file.path(), r#"{"confirm":["click"]}"#).unwrap();
            state.policy = Some(
                super::policy::ActionPolicy::load(policy_file.path().to_str().unwrap()).unwrap(),
            );
            let pending = goal_fixture_command(&click, &mut state).await;
            assert_eq!(pending["data"]["confirmation_required"], true, "{pending}");
        }
        let started = std::time::Instant::now();
        let result = if confirm {
            goal_fixture_command(&json!({"action":"confirm"}), &mut state).await["data"]["result"]
                .clone()
        } else {
            goal_fixture_command(&click, &mut state).await
        };
        assert_eq!(result["code"], "goal_input_uncertain", "{result}");
        assert!(started.elapsed() < std::time::Duration::from_millis(600));
        state.policy = None;
    }
    let _ = close_current_browser(&mut state).await;
}

async fn select_values(
    state: &mut DaemonState,
    id: &str,
    selector: &str,
    values: &[&str],
) -> Value {
    execute_command(
        &json!({ "id": id, "action": "select", "selector": selector, "values": values }),
        state,
    )
    .await
}

async fn assert_evaluate(state: &mut DaemonState, id: &str, script: &str, expected: Value) {
    let resp = execute_command(
        &json!({ "id": id, "action": "evaluate", "script": script }),
        state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], expected);
}

fn assert_error_code(resp: &Value, code: &str) {
    assert_eq!(
        resp.get("success").and_then(Value::as_bool),
        Some(false),
        "Expected failure but got: {}",
        serde_json::to_string_pretty(resp).unwrap_or_default()
    );
    assert_eq!(resp.get("code").and_then(Value::as_str), Some(code));
}

fn native_test_fixture_html(name: &str) -> &'static str {
    match name {
        "drag_probe" => include_str!("test_fixtures/drag_probe.html"),
        "html5_drag_probe" => include_str!("test_fixtures/html5_drag_probe.html"),
        "pointer_capture_probe" => include_str!("test_fixtures/pointer_capture_probe.html"),
        "snapshot_diff_probe" => include_str!("test_fixtures/snapshot_diff_probe.html"),
        "upload_probe" => include_str!("test_fixtures/upload_probe.html"),
        "webmcp_delayed_probe" => include_str!("test_fixtures/webmcp_delayed_probe.html"),
        "webmcp_frame_probe" => include_str!("test_fixtures/webmcp_frame_probe.html"),
        "webmcp_probe" => include_str!("test_fixtures/webmcp_probe.html"),
        "webmcp_context_probe" => include_str!("test_fixtures/webmcp_context_probe.html"),
        _ => panic!("Unknown native test fixture: {}", name),
    }
}

#[tokio::test]
#[ignore]
async fn e2e_webmcp_discovery_invocation_and_cancellation() {
    let (fixture_url, fixture_server) = start_webmcp_server().await;
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": fixture_url
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["webmcp"]["experimental"], true);
    assert_eq!(get_data(&resp)["webmcp"]["available"], true);
    assert!(
        get_data(&resp)["webmcp"]["toolCount"]
            .as_u64()
            .is_some_and(|count| count >= 4),
        "navigation did not advertise the fixture's WebMCP tools: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );
    let child_ready = tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
        loop {
            let resp = execute_command(
                &json!({
                    "id": "2b",
                    "action": "evaluate",
                    "script": "document.getElementById('tool-frame')?.contentDocument?.body?.dataset?.webmcpReady === 'true'"
                }),
                &mut state,
            )
            .await;
            if get_data(&resp)["result"] == true {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        child_ready.is_ok(),
        "child WebMCP fixture did not become ready"
    );
    state.drain_cdp_events_background().await.unwrap();

    let resp = execute_command(&json!({ "id": "3", "action": "webmcp_list" }), &mut state).await;
    assert_success(&resp);
    let tools = get_data(&resp)["tools"].as_array().unwrap();
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "set_message")
        .expect("set_message should be discovered");
    assert!(tool["frameId"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(tool["origin"]
        .as_str()
        .is_some_and(|origin| origin.starts_with("http://127.0.0.1:")));
    assert_eq!(tool["inputSchema"]["type"], "object");
    let duplicate_tools = tools
        .iter()
        .filter(|tool| tool["name"] == "duplicate_tool")
        .collect::<Vec<_>>();
    assert_eq!(
        duplicate_tools.len(),
        2,
        "unexpected tools: {}",
        serde_json::to_string_pretty(tools).unwrap_or_default()
    );
    let main_frame_id = tool["frameId"].as_str().unwrap();
    let child_frame_id = duplicate_tools
        .iter()
        .find_map(|tool| {
            let frame_id = tool["frameId"].as_str()?;
            (frame_id != main_frame_id).then_some(frame_id)
        })
        .unwrap()
        .to_string();

    let resp = execute_command(
        &json!({
            "id": "3b",
            "action": "webmcp_invoke",
            "tool": "duplicate_tool",
            "params": {}
        }),
        &mut state,
    )
    .await;
    assert_error_code(&resp, "webmcp_ambiguous_tool");

    let resp = execute_command(
        &json!({
            "id": "3c",
            "action": "webmcp_invoke",
            "tool": "duplicate_tool",
            "frameId": child_frame_id,
            "params": {},
            "timeout": 5000
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["output"]["scope"], "frame");

    let resp = execute_command(
        &json!({
            "id": "4",
            "action": "webmcp_invoke",
            "tool": "set_message",
            "params": { "message": "WebMCP works" },
            "timeout": 5000
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["status"], "completed");
    assert_eq!(get_data(&resp)["output"]["message"], "WebMCP works");

    let resp = execute_command(
        &json!({
            "id": "4b",
            "action": "webmcp_invoke",
            "tool": "fail_tool",
            "params": {},
            "timeout": 5000
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["status"], "failed");
    assert!(get_data(&resp)["error"].is_string());

    let resp = execute_command(
        &json!({
            "id": "5",
            "action": "evaluate",
            "script": "document.getElementById('result').textContent"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "WebMCP works");

    let resp = execute_command(
        &json!({
            "id": "6",
            "action": "webmcp_invoke",
            "tool": "wait_for_cancel",
            "params": {},
            "detach": true
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let invocation_id = get_data(&resp)["invocationId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(get_data(&resp)["status"], "pending");
    let pending = execute_command(
        &json!({
            "id": "6b",
            "action": "webmcp_result",
            "invocationId": invocation_id,
            "timeout": 10
        }),
        &mut state,
    )
    .await;
    assert_success(&pending);
    assert_eq!(get_data(&pending)["status"], "timed_out");

    let resp = execute_command(
        &json!({
            "id": "6c",
            "action": "webmcp_invoke",
            "tool": "wait_for_cancel",
            "params": {},
            "detach": true
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let invocation_id = get_data(&resp)["invocationId"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    state.drain_cdp_events_background().await.unwrap();
    assert_eq!(
        state.webmcp.invocations[&invocation_id].status,
        "pending",
        "long-running fixture terminated before cancellation: {}",
        state.webmcp.invocations[&invocation_id].to_json()
    );

    let resp = execute_command(
        &json!({
            "id": "7",
            "action": "webmcp_cancel",
            "invocationId": invocation_id,
            "timeout": 5000
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["status"], "canceled");

    let resp = execute_command(
        &json!({
            "id": "8",
            "action": "webmcp_invoke",
            "tool": "wait_for_cancel",
            "params": {},
            "timeout": 25
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["status"], "timed_out");

    let resp = execute_command(
        &json!({
            "id": "9",
            "action": "webmcp_invoke",
            "tool": "missing_tool",
            "params": {}
        }),
        &mut state,
    )
    .await;
    assert_error_code(&resp, "webmcp_tool_not_found");

    let resp = execute_command(
        &json!({
            "id": "10",
            "action": "webmcp_invoke",
            "tool": "set_message",
            "params": ["not", "an", "object"]
        }),
        &mut state,
    )
    .await;
    assert_error_code(&resp, "webmcp_invalid_input");

    let resp = execute_command(
        &json!({
            "id": "11",
            "action": "webmcp_invoke",
            "tool": "wait_for_cancel",
            "params": {},
            "detach": true
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let stale_id = get_data(&resp)["invocationId"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = execute_command(
        &json!({
            "id": "12",
            "action": "navigate",
            "url": "about:blank"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({
            "id": "13",
            "action": "webmcp_result",
            "invocationId": stale_id,
            "timeout": 100
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["status"], "failed");
    assert!(get_data(&resp)["error"]
        .as_str()
        .is_some_and(|error| error.starts_with("webmcp_context_changed:")));

    let resp = execute_command(
        &json!({
            "id": "14",
            "action": "navigate",
            "url": fixture_url
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(&json!({ "id": "15", "action": "webmcp_list" }), &mut state).await;
    assert_success(&resp);
    let frame_id = get_data(&resp)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "frame_wait")
        .and_then(|tool| tool["frameId"].as_str())
        .unwrap()
        .to_string();
    let resp = execute_command(
        &json!({
            "id": "16",
            "action": "webmcp_invoke",
            "tool": "frame_wait",
            "frameId": frame_id,
            "params": {},
            "detach": true
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let frame_invocation_id = get_data(&resp)["invocationId"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = execute_command(
        &json!({
            "id": "17",
            "action": "evaluate",
            "script": "document.getElementById('tool-frame').remove()"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let resp = execute_command(
        &json!({
            "id": "18",
            "action": "webmcp_result",
            "invocationId": frame_invocation_id,
            "timeout": 100
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["status"], "failed");
    assert!(get_data(&resp)["error"]
        .as_str()
        .is_some_and(|error| error.starts_with("webmcp_context_changed:")));

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    assert!(state.webmcp.invocations.is_empty());
    fixture_server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_webmcp_delayed_registration_appears_on_next_action() {
    let (fixture_url, fixture_server) = start_webmcp_server().await;
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": format!("{fixture_url}/delayed.html")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let already_observed = get_data(&resp).get("webmcp").is_some();
    let resp = execute_command(
        &json!({"id": "wait", "action": "wait", "timeout": 150}),
        &mut state,
    )
    .await;
    assert_success(&resp);
    if !already_observed {
        assert_eq!(get_data(&resp)["webmcp"]["experimental"], true);
        assert_eq!(get_data(&resp)["webmcp"]["available"], true);
        assert_eq!(get_data(&resp)["webmcp"]["toolCount"], 1);
    }

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    fixture_server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_webmcp_context_follows_actions_without_explicit_discovery() {
    let (url, server) = start_webmcp_server().await;
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({"id": "launch", "action": "launch", "headless": true}),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let commands = [
        (
            json!({"action": "navigate", "url": format!("{url}/context.html")}),
            -1,
        ),
        (json!({"action": "click", "selector": "#register"}), 1),
        (json!({"action": "snapshot"}), -1),
        (json!({"action": "webmcp_list", "tool": "set_message"}), -1),
        (
            json!({"action": "fill", "selector": "#description", "value": "Updated description"}),
            1,
        ),
        (
            json!({"action": "webmcp_invoke", "tool": "set_message", "params": {"message": "Hello from discovered tool"}}),
            -1,
        ),
        (json!({"action": "gettext", "selector": "#result"}), -1),
        (
            json!({"action": "evaluate", "script": "history.pushState({}, '', '#route')"}),
            -1,
        ),
        (
            json!({"action": "tab_new", "url": format!("{url}/empty.html")}),
            0,
        ),
        (json!({"action": "tab_switch", "tabId": "t1"}), 1),
        (json!({"action": "tab_close", "tabId": "t2"}), -1),
        (json!({"action": "click", "selector": "#remove"}), 0),
        (json!({"action": "wait", "timeout": 50}), -1),
        (json!({"action": "click", "selector": "#register"}), 1),
        (
            json!({"action": "navigate", "url": format!("{url}/empty.html")}),
            0,
        ),
    ];
    for (mut cmd, count) in commands {
        cmd["id"] = json!("context");
        let resp = execute_command(&cmd, &mut state).await;
        assert_success(&resp);
        let mut context = get_data(&resp)["webmcp"].clone();
        // Chrome may deliver tool events after the action's reply. Passive
        // observation reports them on a later ordinary action, without listing
        // tools or retrying the mutation. Re-registration can also briefly
        // report removal before the replacement tool is observed.
        if count >= 0 && context["toolCount"] != count {
            context = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    let update = execute_command(
                        &json!({"id": "context-update", "action": "wait", "timeout": 50}),
                        &mut state,
                    )
                    .await;
                    assert_success(&update);
                    let context = &get_data(&update)["webmcp"];
                    if context["toolCount"] == count {
                        break context.clone();
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("Missing WebMCP update after {cmd}: {resp}"));
        }
        if count == -1 {
            assert!(
                context.is_null(),
                "Unchanged or empty page must stay quiet: {cmd}: {resp}"
            );
        } else {
            assert_eq!(context["status"], "ready", "{cmd}: {resp}");
            assert_eq!(context["toolCount"], count, "{cmd}: {resp}");
            assert_eq!(context["available"], count > 0);
        }
        if count > 0 {
            let tool = &context["tools"][0];
            assert_eq!(tool["name"], "set_message");
            assert!(tool.get("inputSchema").is_none());
            assert!(tool["frameId"].as_str().is_some_and(|id| !id.is_empty()));
            assert_eq!(tool["origin"], url);
            if cmd["action"] == "fill" {
                assert_eq!(tool["description"], "Updated description");
            }
        }
        if cmd["action"] == "webmcp_list" {
            assert_eq!(get_data(&resp)["tools"].as_array().unwrap().len(), 1);
            assert_eq!(
                get_data(&resp)["tools"][0]["inputSchema"]["required"],
                json!(["message"])
            );
        }
        if cmd["action"] == "gettext" {
            assert_eq!(get_data(&resp)["text"], "Hello from discovered tool");
        }
    }
    let resp = execute_command(&json!({"id": "close", "action": "close"}), &mut state).await;
    assert_success(&resp);
    assert!(get_data(&resp).get("webmcp").is_none());
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_webmcp_same_document_navigation_preserves_catalog_and_invocations() {
    let (url, server) = start_webmcp_server().await;
    let mut state = DaemonState::new();
    for mut cmd in [
        json!({"action": "launch", "headless": true}),
        json!({"action": "navigate", "url": url}),
    ] {
        cmd["id"] = json!("setup");
        let resp = execute_command(&cmd, &mut state).await;
        assert_success(&resp);
    }
    let resp = execute_command(
        &json!({"id": "invoke", "action": "webmcp_invoke", "tool": "wait_for_cancel", "params": {}, "detach": true}),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let invocation_id = get_data(&resp)["invocationId"]
        .as_str()
        .unwrap()
        .to_string();
    let browser = state.browser.as_ref().unwrap();
    let session = browser.active_session_id().unwrap().to_string();
    // Stop tool events so this test cannot pass by silently rediscovering the
    // catalog. Page navigation events remain enabled for document invalidation.
    browser
        .client
        .send_command_no_params("WebMCP.disable", Some(&session))
        .await
        .unwrap();
    let resp = execute_command(&json!({"id": "settle", "action": "title"}), &mut state).await;
    assert_success(&resp);
    let tools = state.webmcp.tools(&session).unwrap();
    assert!(tools.iter().any(|tool| tool.name == "wait_for_cancel"));
    assert_eq!(state.webmcp.invocations[&invocation_id].status, "pending");

    for mut cmd in [
        json!({"action": "navigate", "url": format!("{url}/#route")}),
        json!({"action": "snapshot"}),
        json!({"action": "navigate", "url": format!("{url}/#next")}),
        json!({"action": "evaluate", "script": "history.pushState({}, '', '#history')"}),
        json!({"action": "title"}),
    ] {
        cmd["id"] = json!("same-document");
        let resp = execute_command(&cmd, &mut state).await;
        assert_success(&resp);
        assert!(get_data(&resp).get("webmcp").is_none(), "{cmd}: {resp}");
        assert_eq!(state.webmcp.tools(&session).unwrap(), tools, "{cmd}");
        assert_eq!(state.webmcp.invocations[&invocation_id].status, "pending");
    }

    let resp = execute_command(
        &json!({"id": "new-document", "action": "navigate", "url": format!("{url}/empty.html")}),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["webmcp"]["toolCount"], 0);
    assert!(state.webmcp.tools(&session).unwrap().is_empty());
    assert!(state.webmcp.invocations[&invocation_id].to_json()["error"]
        .as_str()
        .is_some_and(|error| error.starts_with("webmcp_context_changed:")));

    // New documents still populate the catalog through the existing
    // subscription and reannounce tools even when the records are identical.
    state
        .browser
        .as_ref()
        .unwrap()
        .client
        .send_command_no_params("WebMCP.enable", Some(&session))
        .await
        .unwrap();
    for _ in 0..2 {
        let resp = execute_command(
            &json!({"id": "reload", "action": "navigate", "url": url}),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert!(get_data(&resp)["webmcp"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "wait_for_cancel"));
    }
    let resp = execute_command(&json!({"id": "close", "action": "close"}), &mut state).await;
    assert_success(&resp);
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_webmcp_ordinary_page_stays_quiet_without_reprobing() {
    let (url, server) = start_webmcp_server().await;
    let mut state = DaemonState::new();
    for (index, mut cmd) in [
        json!({"action": "launch", "headless": true}),
        json!({"action": "navigate", "url": format!("{url}/empty.html")}),
        json!({"action": "snapshot"}),
        json!({"action": "title"}),
        json!({"action": "evaluate", "script": "document.title"}),
        json!({"action": "wait", "timeout": 1}),
    ]
    .into_iter()
    .enumerate()
    {
        cmd["id"] = json!(index.to_string());
        let resp = execute_command(&cmd, &mut state).await;
        assert_success(&resp);
        assert!(get_data(&resp).get("webmcp").is_none(), "{cmd}: {resp}");
    }
    // Disable browser-side events behind the daemon. If ordinary commands
    // re-enable discovery, the registration below would incorrectly appear.
    let browser = state.browser.as_ref().unwrap();
    let session = browser.active_session_id().unwrap().to_string();
    browser
        .client
        .send_command_no_params("WebMCP.disable", Some(&session))
        .await
        .unwrap();
    let resp = execute_command(&json!({"id": "register", "action": "evaluate", "script": "document.modelContext.registerTool({name: 'probe', description: 'probe', inputSchema: {type: 'object'}, execute: async () => ({ok: true})}); true"}), &mut state).await;
    assert_success(&resp);
    assert!(get_data(&resp).get("webmcp").is_none());
    assert_eq!(state.webmcp.observations.len(), 1);
    let resp = execute_command(&json!({"id": "snapshot", "action": "snapshot"}), &mut state).await;
    assert_success(&resp);
    assert!(get_data(&resp).get("webmcp").is_none());
    execute_command(&json!({"id": "close", "action": "close"}), &mut state).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_webmcp_opt_out_returns_no_tools() {
    let (fixture_url, fixture_server) = start_webmcp_server().await;
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({
            "id": "1",
            "action": "launch",
            "headless": true,
            "webmcp": false
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "2", "action": "webmcp_list" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["tools"], json!([]));

    let resp = execute_command(
        &json!({ "id": "3", "action": "navigate", "url": fixture_url }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(get_data(&resp).get("webmcp").is_none());

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    fixture_server.abort();
}

fn native_test_fixture_url(name: &str) -> String {
    format!(
        "data:text/html;base64,{}",
        STANDARD.encode(native_test_fixture_html(name))
    )
}

async fn create_storage_state_with_cookie(path: &str, cookie_name: &str, cookie_value: &str) {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({
            "id": "1",
            "action": "launch",
            "headless": true,
            "args": ["--no-sandbox", "--disable-dev-shm-usage"]
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "cookies_set",
            "name": cookie_name,
            "value": cookie_value,
            "domain": ".example.com",
            "path": "/",
            "expires": 2000000000
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "4", "action": "state_save", "path": path }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "5", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

async fn create_restore_state_with_cookie(
    restore_key: &str,
    cookie_name: &str,
    cookie_value: &str,
) {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({
            "id": "1",
            "action": "navigate",
            "url": "https://example.com",
            "restoreKey": restore_key
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "cookies_set",
            "name": cookie_name,
            "value": cookie_value,
            "domain": ".example.com",
            "path": "/",
            "expires": 2000000000
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "3", "action": "close" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["saveStatus"], "saved");
}

fn cleanup_restore_state_files(restore_key: &str) {
    let Some(sessions_dir) = dirs::home_dir().map(|home| home.join(".agent-browser/sessions"))
    else {
        return;
    };

    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with(&format!("{}-", restore_key)) {
                let path = entry.path();
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(format!("{}.previous", path.to_string_lossy()));
            }
        }
    }
}

async fn send_raw_http_request(port: u64, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("HTTP client should connect to stream server");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("HTTP request should be written");
    stream
        .shutdown()
        .await
        .expect("HTTP client write side should shut down");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("HTTP response should be read");
    String::from_utf8(response).expect("HTTP response should be utf-8")
}

#[cfg(unix)]
async fn spawn_fake_daemon_socket(
    socket_dir: &std::path::Path,
    session_name: &str,
) -> tokio::sync::oneshot::Receiver<String> {
    use tokio::io::AsyncBufReadExt;

    let socket_path = socket_dir.join(format!("{session_name}.sock"));
    let _ = std::fs::remove_file(&socket_path);
    let listener =
        tokio::net::UnixListener::bind(&socket_path).expect("fake daemon socket should bind");
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let mut reader = tokio::io::BufReader::new(stream);
        let mut command = String::new();
        if reader.read_line(&mut command).await.is_err() {
            return;
        }

        let mut stream = reader.into_inner();
        let _ = stream
            .write_all(br#"{"success":true,"data":{"ok":true}}"#)
            .await;
        let _ = stream.write_all(b"\n").await;
        let _ = tx.send(command);
    });

    rx
}

// ---------------------------------------------------------------------------
// Core: launch, navigate, evaluate, url, title, close
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_launch_navigate_evaluate_close() {
    let mut state = DaemonState::new();

    // Launch headless Chrome
    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["launched"], true);

    // Navigate to example.com
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["url"], "https://example.com/");
    assert_eq!(get_data(&resp)["title"], "Example Domain");

    // Get URL
    let resp = execute_command(&json!({ "id": "3", "action": "url" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["url"], "https://example.com/");

    // Get title
    let resp = execute_command(&json!({ "id": "4", "action": "title" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["title"], "Example Domain");

    // Evaluate JS
    let resp = execute_command(
        &json!({ "id": "5", "action": "evaluate", "script": "1 + 2" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], 3);

    // Evaluate document.title
    let resp = execute_command(
        &json!({ "id": "6", "action": "evaluate", "script": "document.title" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "Example Domain");

    // Close
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["closed"], true);
}

#[tokio::test]
#[ignore]
async fn e2e_lightpanda_launch_can_open_page() {
    let lightpanda_bin = match std::env::var("LIGHTPANDA_BIN") {
        Ok(path) if !path.is_empty() => path,
        _ => return,
    };

    let mut state = DaemonState::new();

    let resp = tokio::time::timeout(
        tokio::time::Duration::from_secs(20),
        execute_command(
            &json!({
                "id": "1",
                "action": "launch",
                "headless": true,
                "engine": "lightpanda",
                "executablePath": lightpanda_bin,
            }),
            &mut state,
        ),
    )
    .await
    .expect("Lightpanda launch should not hang");

    assert_success(&resp);
    assert_eq!(get_data(&resp)["launched"], true);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["url"], "https://example.com/");
    assert_eq!(get_data(&resp)["title"], "Example Domain");

    let resp = execute_command(&json!({ "id": "3", "action": "close" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["closed"], true);
}

#[tokio::test]
#[ignore]
async fn e2e_lightpanda_auto_launch_can_open_page() {
    let lightpanda_bin = match std::env::var("LIGHTPANDA_BIN") {
        Ok(path) if !path.is_empty() => path,
        _ => return,
    };

    let prev_engine = std::env::var("AGENT_BROWSER_ENGINE").ok();
    let prev_path = std::env::var("AGENT_BROWSER_EXECUTABLE_PATH").ok();
    std::env::set_var("AGENT_BROWSER_ENGINE", "lightpanda");
    std::env::set_var("AGENT_BROWSER_EXECUTABLE_PATH", &lightpanda_bin);

    let mut state = DaemonState::new();

    let resp = tokio::time::timeout(
        tokio::time::Duration::from_secs(20),
        execute_command(
            &json!({ "id": "1", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        ),
    )
    .await
    .expect("Lightpanda auto-launch should not hang");

    match prev_engine {
        Some(value) => std::env::set_var("AGENT_BROWSER_ENGINE", value),
        None => std::env::remove_var("AGENT_BROWSER_ENGINE"),
    }
    match prev_path {
        Some(value) => std::env::set_var("AGENT_BROWSER_EXECUTABLE_PATH", value),
        None => std::env::remove_var("AGENT_BROWSER_EXECUTABLE_PATH"),
    }

    assert_success(&resp);
    assert_eq!(get_data(&resp)["url"], "https://example.com/");
    assert_eq!(get_data(&resp)["title"], "Example Domain");

    let resp = execute_command(&json!({ "id": "2", "action": "close" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["closed"], true);
}

// ---------------------------------------------------------------------------
// Runtime stream lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_runtime_stream_enable_before_launch_attaches_and_disables() {
    let guard = EnvGuard::new(&["AGENT_BROWSER_SOCKET_DIR", "AGENT_BROWSER_SESSION"]);
    let socket_dir = std::env::temp_dir().join(format!(
        "agent-browser-e2e-stream-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&socket_dir).expect("socket dir should be created");
    guard.set(
        "AGENT_BROWSER_SOCKET_DIR",
        socket_dir.to_str().expect("socket dir should be utf-8"),
    );
    guard.set("AGENT_BROWSER_SESSION", "e2e-runtime-stream");

    let mut state = DaemonState::new();

    let resp = execute_command(&json!({ "id": "1", "action": "stream_status" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["enabled"], false);

    let resp = execute_command(
        &json!({ "id": "2", "action": "stream_enable", "port": 0 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let port = get_data(&resp)["port"]
        .as_u64()
        .expect("stream enable should report the bound port");
    assert_eq!(get_data(&resp)["connected"], false);

    let stream_path = socket_dir.join("e2e-runtime-stream.stream");
    assert!(
        stream_path.exists(),
        "runtime enable should create .stream metadata"
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("websocket client should connect to runtime stream");

    let initial = tokio::time::timeout(tokio::time::Duration::from_secs(5), ws.next())
        .await
        .expect("websocket should emit initial status")
        .expect("websocket should stay open")
        .expect("websocket message should be valid");
    let initial_text = initial.into_text().expect("initial message should be text");
    let initial_status: Value =
        serde_json::from_str(&initial_text).expect("status JSON should parse");
    assert_eq!(initial_status["type"], "status");
    assert_eq!(initial_status["connected"], false);

    let resp = execute_command(
        &json!({ "id": "3", "action": "navigate", "url": "data:text/html,<h1>Runtime Stream</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let mut observed_connected = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let Some(message) = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws.next())
            .await
            .expect("websocket should emit status after browser launch")
        else {
            continue;
        };
        let message = message.expect("websocket message should be valid");
        if !message.is_text() {
            continue;
        }
        let parsed: Value =
            serde_json::from_str(message.to_text().expect("text message should be readable"))
                .expect("runtime stream payload should be valid JSON");
        if parsed.get("type") == Some(&json!("status"))
            && parsed.get("connected") == Some(&json!(true))
        {
            observed_connected = true;
            break;
        }
    }
    assert!(
        observed_connected,
        "runtime stream should report connected=true after browser launch"
    );

    let resp = execute_command(
        &json!({ "id": "4", "action": "stream_disable" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["disabled"], true);
    assert!(
        !stream_path.exists(),
        "stream disable should remove .stream metadata"
    );

    let close_message = tokio::time::timeout(tokio::time::Duration::from_secs(5), ws.next())
        .await
        .expect("websocket should close after disable");
    assert!(
        close_message.is_none() || close_message.expect("ws result should exist").is_ok(),
        "websocket should shut down cleanly when the runtime stream is disabled"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    let _ = std::fs::remove_dir_all(&socket_dir);
}

#[cfg(unix)]
#[tokio::test]
#[ignore]
async fn e2e_stream_command_requires_same_origin_before_daemon_relay() {
    let guard = EnvGuard::new(&["AGENT_BROWSER_SOCKET_DIR", "AGENT_BROWSER_SESSION"]);
    let temp_parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("t");
    std::fs::create_dir_all(&temp_parent).expect("socket temp parent should be created");
    let socket_dir = tempfile::Builder::new()
        .prefix("ab-e2e-")
        .tempdir_in(temp_parent)
        .expect("socket dir should be created");
    guard.set(
        "AGENT_BROWSER_SOCKET_DIR",
        socket_dir
            .path()
            .to_str()
            .expect("socket dir should be utf-8"),
    );
    guard.set("AGENT_BROWSER_SESSION", "x");

    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({ "id": "1", "action": "stream_enable", "port": 0 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let port = get_data(&resp)["port"]
        .as_u64()
        .expect("stream enable should report the bound port");

    let mut daemon_command = spawn_fake_daemon_socket(socket_dir.path(), "x").await;
    let body = r#"{"action":"tabs"}"#;
    let cross_origin_request = format!(
        "POST /api/command HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );

    let response = send_raw_http_request(port, &cross_origin_request).await;
    assert!(
        response.starts_with("HTTP/1.1 403 Forbidden"),
        "unexpected cross-origin response: {response}"
    );
    assert!(
        !response.contains("Access-Control-Allow-Origin: *"),
        "forbidden command response exposed wildcard CORS: {response}"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut daemon_command)
            .await
            .is_err(),
        "cross-origin command request reached daemon relay"
    );

    let same_origin_request = format!(
        "POST /api/command HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: http://localhost:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let response = send_raw_http_request(port, &same_origin_request).await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected same-origin response: {response}"
    );
    assert!(
        response.contains(&format!(
            "Access-Control-Allow-Origin: http://localhost:{port}"
        )),
        "same-origin command response did not reflect origin: {response}"
    );
    assert!(
        !response.contains("Access-Control-Allow-Origin: *"),
        "same-origin command response exposed wildcard CORS: {response}"
    );

    let relayed = tokio::time::timeout(std::time::Duration::from_secs(1), daemon_command)
        .await
        .expect("same-origin request should reach fake daemon")
        .expect("fake daemon should return relayed command");
    assert!(relayed.contains(r#""action":"tabs""#), "{relayed}");

    let resp = execute_command(
        &json!({ "id": "2", "action": "stream_disable" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Snapshot with refs and ref-based click
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_snapshot_and_click_ref() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Take snapshot
    let resp = execute_command(&json!({ "id": "3", "action": "snapshot" }), &mut state).await;
    assert_success(&resp);
    let snapshot = get_data(&resp)["snapshot"].as_str().unwrap();
    assert!(
        snapshot.contains("Example Domain"),
        "Snapshot should contain heading"
    );
    assert!(snapshot.contains("ref=e1"), "Snapshot should have ref e1");
    assert!(snapshot.contains("ref=e2"), "Snapshot should have ref e2");
    assert!(
        snapshot.contains("link"),
        "Snapshot should have a link element"
    );

    // Click the link by ref (e2 is the "More information..." link)
    let resp = execute_command(
        &json!({ "id": "4", "action": "click", "selector": "e2" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Wait for navigation
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Verify URL changed
    let resp = execute_command(&json!({ "id": "5", "action": "url" }), &mut state).await;
    assert_success(&resp);
    let url = get_data(&resp)["url"].as_str().unwrap();
    assert!(
        url.contains("iana.org"),
        "Should have navigated to iana.org, got: {}",
        url
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_snapshot_refs_survive_dom_updates_and_never_recycle() {
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "2", "action": "navigate", "url": "about:blank" }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "3", "action": "setcontent", "html": "<button id='a'>Alpha</button><button id='b'>Beta</button>" }),
            &mut state,
        )
        .await,
    );

    let first = execute_command(&json!({ "id": "4", "action": "snapshot" }), &mut state).await;
    assert_success(&first);
    let alpha_ref = get_data(&first)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, node)| node["name"] == "Alpha")
        .map(|(ref_id, _)| ref_id.clone())
        .unwrap();

    assert_success(
        &execute_command(
            &json!({ "id": "5", "action": "evaluate", "script": "document.body.prepend(document.getElementById('a')); document.getElementById('b').remove()" }),
            &mut state,
        )
        .await,
    );
    let second = execute_command(&json!({ "id": "6", "action": "snapshot" }), &mut state).await;
    assert_success(&second);
    assert_eq!(get_data(&second)["refs"][&alpha_ref]["name"], "Alpha");
    assert_eq!(
        get_data(&second)["removedRefs"].as_array().unwrap().len(),
        1
    );

    assert_success(
        &execute_command(
            &json!({ "id": "7", "action": "navigate", "url": "about:blank?new-document" }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "8", "action": "setcontent", "html": "<button>Alpha</button>" }),
            &mut state,
        )
        .await,
    );
    let third = execute_command(&json!({ "id": "9", "action": "snapshot" }), &mut state).await;
    assert_success(&third);
    assert!(get_data(&third)["refs"].get(&alpha_ref).is_none());
    assert_success(&execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await);
}

#[tokio::test]
#[ignore]
async fn e2e_snapshot_refs_invalidate_iframe_navigation() {
    let mut state = DaemonState::new();
    for command in [
        json!({"action": "launch", "headless": true}),
        json!({"action": "navigate", "url": "about:blank"}),
        json!({"action": "setcontent", "html": "<button>Parent</button><iframe id='child'></iframe>"}),
    ] {
        assert_success(&execute_command(&command, &mut state).await);
    }
    let replace = json!({"action": "evaluate", "script": "new Promise(resolve => { const f = document.getElementById('child'); f.onload = () => resolve(true); f.srcdoc = '<button>Child</button>'; })"});
    assert_success(&execute_command(&replace, &mut state).await);
    let first = execute_command(&json!({"action": "snapshot"}), &mut state).await;
    assert_success(&first);
    let find_ref = |snapshot: &Value, name: &str| {
        get_data(snapshot)["refs"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, node)| node["name"] == name)
            .unwrap()
            .0
            .clone()
    };
    let parent = find_ref(&first, "Parent");
    let child = find_ref(&first, "Child");
    let unchanged = execute_command(&json!({"action": "snapshot"}), &mut state).await;
    assert_eq!(find_ref(&unchanged, "Child"), child);
    assert_success(&execute_command(&replace, &mut state).await);
    let replaced = execute_command(&json!({"action": "snapshot"}), &mut state).await;
    assert_success(&replaced);
    assert_eq!(find_ref(&replaced, "Parent"), parent);
    assert_ne!(find_ref(&replaced, "Child"), child);
    assert!(get_data(&replaced)["removedRefs"]
        .as_array()
        .unwrap()
        .contains(&json!(format!("@{child}"))));
    assert_success(&execute_command(&json!({"action": "close"}), &mut state).await);
}

// ---------------------------------------------------------------------------
// Screenshot
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_screenshot() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Default screenshot
    let resp = execute_command(&json!({ "id": "3", "action": "screenshot" }), &mut state).await;
    assert_success(&resp);
    let path = get_data(&resp)["path"].as_str().unwrap();
    assert!(path.ends_with(".png"), "Screenshot path should be .png");
    let metadata = std::fs::metadata(path).expect("Screenshot file should exist");
    assert!(
        metadata.len() > 1000,
        "Screenshot should be non-trivial size"
    );

    // Named screenshot
    let tmp_path = std::env::temp_dir()
        .join("agent-browser-e2e-test-screenshot.png")
        .to_string_lossy()
        .to_string();
    let resp = execute_command(
        &json!({ "id": "4", "action": "screenshot", "path": tmp_path }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(std::path::Path::new(&tmp_path).exists());
    let _ = std::fs::remove_file(&tmp_path);

    let resp = execute_command(
        &json!({ "id": "4a", "action": "screenshot", "ifChanged": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["changed"], true);
    assert_eq!(get_data(&resp)["revision"], 1);
    let conditional_path = get_data(&resp)["path"].as_str().unwrap().to_string();

    let resp = execute_command(
        &json!({ "id": "4b", "action": "screenshot", "ifChanged": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["changed"], false);
    assert_eq!(get_data(&resp)["revision"], 2);
    assert_eq!(get_data(&resp)["pixelChangeRatio"], 0.0);
    assert!(get_data(&resp).get("path").is_none());
    let _ = std::fs::remove_file(conditional_path);

    let resp = execute_command(
        &json!({
            "id": "5",
            "action": "setcontent",
            "html": r##"
                <html><body>
                  <button onclick="document.getElementById('result').textContent = 'clicked'">Submit</button>
                  <a href="#">Home</a>
                  <div id="result"></div>
                </body></html>
            "##,
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "5a", "action": "screenshot", "ifChanged": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["changed"], true);
    assert_eq!(get_data(&resp)["revision"], 3);
    assert!(get_data(&resp)["pixelChangeRatio"].as_f64().unwrap() > 0.0);
    let _ = std::fs::remove_file(get_data(&resp)["path"].as_str().unwrap());

    let resp = execute_command(
        &json!({ "id": "6", "action": "screenshot", "annotate": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let annotations = get_data(&resp)["annotations"]
        .as_array()
        .expect("Annotated screenshot should return annotations");
    assert!(
        !annotations.is_empty(),
        "Annotated screenshot should have at least one annotation"
    );

    let submit_ref = annotations
        .iter()
        .find(|ann| ann.get("name").and_then(|v| v.as_str()) == Some("Submit"))
        .and_then(|ann| ann.get("ref").and_then(|v| v.as_str()))
        .expect("Expected a Submit annotation");

    let resp = execute_command(
        &json!({
            "id": "7",
            "action": "evaluate",
            "script": "document.getElementById('__agent_browser_annotations__') === null"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], true);

    let resp = execute_command(
        &json!({ "id": "8", "action": "click", "selector": format!("@{}", submit_ref) }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "9",
            "action": "evaluate",
            "script": "document.getElementById('result').textContent"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "clicked");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Form interaction: fill, type, select, check
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_form_interaction() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = concat!(
        "data:text/html,<html><body>",
        "<input id='name' type='text' placeholder='Name'>",
        "<input id='email' type='email'>",
        "<select id='color'><option value='red'>Red</option><option value='blue'>Blue</option></select>",
        "<input id='agree' type='checkbox'>",
        "<textarea id='bio'></textarea>",
        "<button id='submit'>Submit</button>",
        "</body></html>"
    );

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Fill name
    let resp = execute_command(
        &json!({ "id": "10", "action": "fill", "selector": "#name", "value": "John Doe" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Verify fill
    let resp = execute_command(
        &json!({ "id": "11", "action": "evaluate", "script": "document.getElementById('name').value" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "John Doe");

    // Type email – the type action now correctly handles punctuation like '.'
    let resp = execute_command(
        &json!({ "id": "12", "action": "type", "selector": "#email", "text": "john@example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "13", "action": "evaluate", "script": "document.getElementById('email').value" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "john@example.com");

    // Select option
    let resp = execute_command(
        &json!({ "id": "14", "action": "select", "selector": "#color", "values": ["blue"] }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "15", "action": "evaluate", "script": "document.getElementById('color').value" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "blue");

    // Check checkbox
    let resp = execute_command(
        &json!({ "id": "16", "action": "check", "selector": "#agree" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "17", "action": "ischecked", "selector": "#agree" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["checked"], true);

    // Uncheck
    let resp = execute_command(
        &json!({ "id": "18", "action": "uncheck", "selector": "#agree" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "19", "action": "ischecked", "selector": "#agree" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["checked"], false);

    // Snapshot should show form state
    let resp = execute_command(&json!({ "id": "20", "action": "snapshot" }), &mut state).await;
    assert_success(&resp);
    let snap = get_data(&resp)["snapshot"].as_str().unwrap();
    assert!(
        snap.contains("John Doe"),
        "Snapshot should show filled value"
    );
    assert!(snap.contains("textbox"), "Snapshot should show textbox");
    assert!(snap.contains("button"), "Snapshot should show button");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_select_option_label_override_names() {
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<html><body></body></html>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let script = r#"(() => {
        const select = document.createElement('select');
        select.id = 'label-overrides';
        for (const [value, label, text] of [
            ['initial', 'Initial', 'Initial'],
            ['target', 'Capital\u00A0Federal', 'Target source'],
            ['decoy', 'Other Province', 'Capital\u00A0\u00A0Federal'],
            ['hidden', 'Visible Province', 'Hidden\u00A0Only'],
            ['plain', null, 'Plain\u00A0Text']
        ]) {
            const option = document.createElement('option');
            option.value = value;
            if (label !== null) option.label = label;
            option.textContent = text;
            select.appendChild(option);
        }
        select.value = 'initial';
        select.dataset.changes = '0';
        select.addEventListener('change', () => select.dataset.changes++);
        document.body.appendChild(select);
        const multi = select.cloneNode(true);
        multi.id = 'label-overrides-multi';
        multi.multiple = true;
        multi.value = 'initial';
        multi.addEventListener('change', () => multi.dataset.changes++);
        document.body.appendChild(multi);
    })()"#;
    let resp = execute_command(
        &json!({ "id": "3", "action": "evaluate", "script": script }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let snapshot = execute_command(&json!({ "id": "4", "action": "snapshot" }), &mut state).await;
    let mut results = Vec::new();
    for (selector, values, expected_values, changes, success) in [
        (
            "#label-overrides",
            vec!["Capital Federal"],
            vec!["target"],
            "1",
            true,
        ),
        (
            "#label-overrides",
            vec!["Hidden Only"],
            vec!["target"],
            "1",
            false,
        ),
        ("#label-overrides", vec!["decoy"], vec!["decoy"], "2", true),
        (
            "#label-overrides",
            vec!["Capital\u{00A0}Federal"],
            vec!["target"],
            "3",
            true,
        ),
        (
            "#label-overrides",
            vec!["Hidden\u{00A0}Only"],
            vec!["hidden"],
            "4",
            true,
        ),
        (
            "#label-overrides",
            vec!["Plain Text"],
            vec!["plain"],
            "5",
            true,
        ),
        (
            "#label-overrides-multi",
            vec!["target", "Hidden Only"],
            vec!["initial"],
            "0",
            false,
        ),
    ] {
        let selection = select_values(&mut state, "select", selector, &values).await;
        let script = format!(
            "(() => {{ const select = document.querySelector('{}'); return {{ values: [...select.selectedOptions].map(option => option.value), changes: select.dataset.changes }}; }})()",
            selector
        );
        let actual = execute_command(
            &json!({ "id": "state", "action": "evaluate", "script": script }),
            &mut state,
        )
        .await;
        results.push((selection, actual, expected_values, changes, success));
    }
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    assert_success(&snapshot);
    let snapshot = get_data(&snapshot)["snapshot"].as_str().unwrap();
    assert!(snapshot.contains("option \"Capital Federal\""));
    assert!(snapshot.contains("option \"Other Province\""));
    assert!(!snapshot.contains("option \"Hidden Only\""));
    for (selection, actual, expected_values, changes, success) in results {
        assert_eq!(selection["success"], success, "{selection}");
        if !success {
            assert!(selection["error"]
                .as_str()
                .unwrap()
                .contains("No option matched"));
        }
        assert_success(&actual);
        assert_eq!(
            get_data(&actual)["result"],
            json!({ "values": expected_values, "changes": changes })
        );
    }
}

#[tokio::test]
#[ignore]
async fn e2e_select_option_normalized_names() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<html><body></body></html>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let script = r#"(() => {
        const add = (select, value, label, selected = false) => {
            const option = document.createElement('option');
            option.value = value;
            option.textContent = label;
            option.selected = selected;
            select.appendChild(option);
        };
        const province = document.createElement('select');
        province.id = 'province';
        add(province, '', 'Provincia');
        add(province, 'B', 'Buenos\u00A0Aires', true);
        add(province, 'C', 'Capital\u00A0Federal');
        add(province, 'Z', 'Zero\u200BWidth');
        province.dataset.changes = '0';
        province.addEventListener('change', () => province.dataset.changes++);
        document.body.appendChild(province);

        const collision = document.createElement('select');
        collision.id = 'collision';
        add(collision, 'ascii', 'Alpha Beta', true);
        add(collision, 'nbsp', 'Alpha\u00A0Beta');
        document.body.appendChild(collision);

        const cases = document.createElement('select');
        cases.id = 'cases';
        add(cases, 'US', 'Upper');
        add(cases, 'us', 'Lower');
        document.body.appendChild(cases);

        const multi = document.createElement('select');
        multi.id = 'multi';
        multi.multiple = true;
        add(multi, 'a', 'One\u00A0A');
        add(multi, 'b', 'Two B');
        document.body.appendChild(multi);
    })()"#;
    let resp = execute_command(
        &json!({ "id": "3", "action": "evaluate", "script": script }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "4", "action": "snapshot" }), &mut state).await;
    assert_success(&resp);
    let snapshot = get_data(&resp)["snapshot"].as_str().unwrap();
    assert!(snapshot.contains("option \"Capital Federal\""));
    assert!(!snapshot.contains("CapitalFederal"));
    assert!(snapshot.contains("option \"ZeroWidth\""));

    let resp = select_values(&mut state, "5", "#province", &["Capital Federal"]).await;
    assert_success(&resp);
    assert_evaluate(
        &mut state,
        "6",
        "({ value: province.value, changes: province.dataset.changes })",
        json!({ "value": "C", "changes": "1" }),
    )
    .await;

    let resp = select_values(&mut state, "7", "#province", &["missing"]).await;
    assert_eq!(resp["success"], false);
    assert_evaluate(
        &mut state,
        "8",
        "({ value: province.value, changes: province.dataset.changes })",
        json!({ "value": "C", "changes": "1" }),
    )
    .await;

    let resp = select_values(&mut state, "9", "#collision", &["Alpha Beta"]).await;
    assert_success(&resp);
    assert_evaluate(&mut state, "10", "collision.value", json!("ascii")).await;

    let resp = select_values(&mut state, "11", "#collision", &["Alpha  Beta"]).await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .contains("Multiple options matched"));
    assert_evaluate(&mut state, "12", "collision.value", json!("ascii")).await;

    let resp = select_values(&mut state, "13", "#cases", &["us"]).await;
    assert_success(&resp);
    assert_evaluate(&mut state, "14", "cases.value", json!("us")).await;

    let resp = select_values(&mut state, "15", "#multi", &["One A", "b"]).await;
    assert_success(&resp);
    assert_evaluate(
        &mut state,
        "16",
        "[...multi.selectedOptions].map(option => option.value)",
        json!(["a", "b"]),
    )
    .await;

    let resp = select_values(&mut state, "17", "#multi", &["a", "missing"]).await;
    assert_eq!(resp["success"], false);
    assert_evaluate(
        &mut state,
        "18",
        "[...multi.selectedOptions].map(option => option.value)",
        json!(["a", "b"]),
    )
    .await;

    let resp = select_values(&mut state, "19", "#province", &["ZeroWidth"]).await;
    assert_success(&resp);
    assert_evaluate(&mut state, "20", "province.value", json!("Z")).await;

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Navigation: back, forward, reload
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_navigation_history() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to page 1
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<h1>Page 1</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to page 2
    let resp = execute_command(
        &json!({ "id": "3", "action": "navigate", "url": "data:text/html,<h1>Page 2</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Back
    let resp = execute_command(&json!({ "id": "4", "action": "back" }), &mut state).await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "5", "action": "evaluate", "script": "document.querySelector('h1').textContent" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "Page 1");

    // Forward
    let resp = execute_command(&json!({ "id": "6", "action": "forward" }), &mut state).await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "7", "action": "evaluate", "script": "document.querySelector('h1').textContent" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "Page 2");

    // Reload
    let resp = execute_command(&json!({ "id": "8", "action": "reload" }), &mut state).await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "9", "action": "evaluate", "script": "document.querySelector('h1').textContent" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "Page 2");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_cookies() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set cookie
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "cookies_set",
            "name": "test_cookie",
            "value": "hello123"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Get cookies
    let resp = execute_command(&json!({ "id": "4", "action": "cookies_get" }), &mut state).await;
    assert_success(&resp);
    let cookies = get_data(&resp)["cookies"].as_array().unwrap();
    let found = cookies
        .iter()
        .any(|c| c["name"] == "test_cookie" && c["value"] == "hello123");
    assert!(found, "Should find the set cookie");

    // Clear cookies
    let resp = execute_command(&json!({ "id": "5", "action": "cookies_clear" }), &mut state).await;
    assert_success(&resp);

    // Verify cleared
    let resp = execute_command(&json!({ "id": "6", "action": "cookies_get" }), &mut state).await;
    assert_success(&resp);
    let cookies = get_data(&resp)["cookies"].as_array().unwrap();
    let found = cookies.iter().any(|c| c["name"] == "test_cookie");
    assert!(!found, "Cookie should be cleared");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// localStorage / sessionStorage
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_storage() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set local storage
    let resp = execute_command(
        &json!({ "id": "3", "action": "storage_set", "type": "local", "key": "mykey", "value": "myvalue" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Get local storage key
    let resp = execute_command(
        &json!({ "id": "4", "action": "storage_get", "type": "local", "key": "mykey" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["value"], "myvalue");

    // Get all local storage
    let resp = execute_command(
        &json!({ "id": "5", "action": "storage_get", "type": "local" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["data"]["mykey"], "myvalue");

    // Clear
    let resp = execute_command(
        &json!({ "id": "6", "action": "storage_clear", "type": "local" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Verify cleared
    let resp = execute_command(
        &json!({ "id": "7", "action": "storage_get", "type": "local" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let data = &get_data(&resp)["data"];
    assert!(
        data.as_object().map(|m| m.is_empty()).unwrap_or(true),
        "Storage should be empty after clear"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Tab management
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_tabs() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<h1>Tab 1</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Tab list should show 1 tab with tabId 1
    let resp = execute_command(&json!({ "id": "3", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0]["active"], true);
    assert_eq!(tabs[0]["tabId"], "t1", "First tab should have tabId t1");

    // Open new tab
    let resp = execute_command(
        &json!({ "id": "4", "action": "tab_new", "url": "data:text/html,<h1>Tab 2</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["tabId"],
        "t2",
        "New tab should have tabId t2"
    );
    assert_eq!(get_data(&resp)["total"], 2);

    // Tab list should show 2 tabs with distinct, incrementing tabIds
    let resp = execute_command(&json!({ "id": "5", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[1]["active"], true);
    assert_eq!(tabs[0]["tabId"], "t1", "First tab should keep tabId t1");
    assert_eq!(tabs[1]["tabId"], "t2", "Second tab should have tabId t2");

    // Switch to first tab
    let resp = execute_command(
        &json!({ "id": "6", "action": "tab_switch", "tabId": "t1" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "7", "action": "evaluate", "script": "document.querySelector('h1').textContent" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "Tab 1");

    // Close second tab
    let resp = execute_command(
        &json!({ "id": "8", "action": "tab_close", "tabId": "t2" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Should have 1 tab left
    let resp = execute_command(&json!({ "id": "9", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 1);

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_tab_ids_not_reused() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // First tab gets tabId 1
    let resp = execute_command(&json!({ "id": "2", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();
    assert_eq!(tabs[0]["tabId"], "t1");

    // Open tab 2 and tab 3
    let resp = execute_command(
        &json!({ "id": "3", "action": "tab_new", "url": "data:text/html,<h1>Tab 2</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["tabId"], "t2");

    let resp = execute_command(
        &json!({ "id": "4", "action": "tab_new", "url": "data:text/html,<h1>Tab 3</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["tabId"], "t3");

    // Close tab 2
    let resp = execute_command(
        &json!({ "id": "5", "action": "tab_close", "tabId": "t2" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Open a new tab — should get tabId 4, NOT 2
    let resp = execute_command(
        &json!({ "id": "6", "action": "tab_new", "url": "data:text/html,<h1>Tab 4</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["tabId"],
        "t4",
        "Tab IDs must not be reused after closing"
    );

    // Verify final state: tabs t1, t3, t4
    let resp = execute_command(&json!({ "id": "7", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 3);
    let ids: Vec<String> = tabs
        .iter()
        .map(|t| t["tabId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["t1", "t3", "t4"]);

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// `tab_close` with an explicit `tabId` must close that tab regardless of
/// whether it's active, and leave the remaining tab active without leaking
/// per-tab state (refs, iframe sessions, frame id) from the closed tab.
#[tokio::test]
#[ignore]
async fn e2e_tab_close_with_tab_id_closes_active_tab() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<title>A</title>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "tab_new", "url": "data:text/html,<title>B</title>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "4", "action": "tab_close", "tabId": "t2" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "5", "action": "title" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["title"], "A");
    assert!(state.ref_map.get("e1").is_none());
    assert!(state.iframe_sessions.is_empty());
    assert!(state.active_frame_id.is_none());

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Tabs can be opened with a user-assigned label and then addressed by that
/// label anywhere a `t<N>` id is accepted (switch, close, and JSON `tabId`
/// on `tab_switch` / `tab_close`). Labels are the agent-friendly way to
/// write multi-tab workflows without memorizing ids.
#[tokio::test]
#[ignore]
async fn e2e_tab_new_with_label_can_be_switched_and_closed() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<title>Home</title>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Open a labeled tab and verify the response echoes the label and a
    // `t<N>` style tabId.
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "tab_new",
            "url": "data:text/html,<title>Docs</title>",
            "label": "docs",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["tabId"], "t2");
    assert_eq!(get_data(&resp)["label"], "docs");

    // tab_list exposes the label alongside the id.
    let resp = execute_command(&json!({ "id": "4", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();
    let docs = tabs
        .iter()
        .find(|t| t["tabId"] == "t2")
        .expect("docs tab should be present");
    assert_eq!(docs["label"], "docs");

    // tab_switch accepts the label.
    let resp = execute_command(
        &json!({ "id": "5", "action": "tab_switch", "tabId": "t1" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(state.browser.as_ref().unwrap().active_tab_id(), Some(1));

    let resp = execute_command(
        &json!({ "id": "6", "action": "tab_switch", "tabId": "docs" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(state.browser.as_ref().unwrap().active_tab_id(), Some(2));

    // Once switched, the active tab is the labeled one and normal commands
    // work against it.
    let resp = execute_command(&json!({ "id": "7", "action": "title" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["title"], "Docs");

    // tab_close accepts the label.
    let resp = execute_command(
        &json!({ "id": "8", "action": "tab_close", "tabId": "docs" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["label"], "docs");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Duplicate labels must be rejected so agents can treat a label as a unique
/// handle. The first tab keeps the label; the second tab's creation errors.
#[tokio::test]
#[ignore]
async fn e2e_tab_new_with_duplicate_label_errors() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "tab_new", "url": "about:blank", "label": "docs" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "tab_new", "url": "about:blank", "label": "docs" }),
        &mut state,
    )
    .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "duplicate label should error: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );
    let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        err.contains("already used"),
        "error should explain the collision: {}",
        err
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Positional integers passed as `tabId` on tab-switch / tab-close must be
/// rejected by the daemon-layer parser, not silently coerced. The error
/// should teach the user the correct form (`t<N>`).
#[tokio::test]
#[ignore]
async fn e2e_tab_switch_rejects_bare_integer() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "tab_switch", "tabId": "2" }),
        &mut state,
    )
    .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "bare integer tabId on tab_switch should error: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );
    let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        err.contains("t2") && err.contains("positional integers"),
        "error should teach `t<N>` convention: {}",
        err
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Element queries: isvisible, isenabled, gettext, getattribute
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_element_queries() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = concat!(
        "data:text/html,<html><body>",
        "<p id='visible'>Hello World</p>",
        "<p id='hidden' style='display:none'>Hidden</p>",
        "<input id='enabled' value='test'>",
        "<input id='disabled' disabled value='nope'>",
        "<a id='link' href='https://example.com' data-testid='my-link'>Click me</a>",
        "</body></html>"
    );

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // isvisible
    let resp = execute_command(
        &json!({ "id": "3", "action": "isvisible", "selector": "#visible" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["visible"], true);

    let resp = execute_command(
        &json!({ "id": "4", "action": "isvisible", "selector": "#hidden" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["visible"], false);

    // isenabled
    let resp = execute_command(
        &json!({ "id": "5", "action": "isenabled", "selector": "#enabled" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["enabled"], true);

    let resp = execute_command(
        &json!({ "id": "6", "action": "isenabled", "selector": "#disabled" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["enabled"], false);

    // gettext
    let resp = execute_command(
        &json!({ "id": "7", "action": "gettext", "selector": "#visible" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["text"], "Hello World");

    // getattribute
    let resp = execute_command(
        &json!({ "id": "8", "action": "getattribute", "selector": "#link", "attribute": "href" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["value"], "https://example.com");

    let resp = execute_command(
        &json!({ "id": "9", "action": "getattribute", "selector": "#link", "attribute": "data-testid" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["value"], "my-link");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_getbyrole_uses_accessibility_tree_for_implicit_roles() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = concat!(
        "<html><head>",
        "<link rel='stylesheet' href='data:text/css,body{}'>",
        "</head><body>",
        "<h1 style='text-transform:uppercase'>Welcome</h1>",
        "<a id='services' href='#services' onclick='window.__clicked = \"services\"'>Services</a>",
        "<button id='submit'>Submit</button>",
        "</body></html>"
    );
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Implicit role from a plain HTML tag (<h1> -> heading), matched
    // case-insensitively against the accessible name despite the CSS
    // text-transform rendering it as "WELCOME".
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "getbyrole",
            "role": "heading",
            "name": "welcome",
            "subaction": "text"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["text"], "WELCOME");

    // A real <a href> must win over the unrelated <link rel="stylesheet">
    // element, which is not exposed as an AX "link" node at all.
    let resp = execute_command(
        &json!({
            "id": "4",
            "action": "getbyrole",
            "role": "link",
            "name": "Services",
            "exact": true,
            "subaction": "click"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "5", "action": "evaluate", "script": "window.__clicked" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "services");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Wait command
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_wait() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = concat!(
        "data:text/html,<html><body>",
        "<div id='target' style='display:none'>Appeared!</div>",
        "<script>setTimeout(() => document.getElementById('target').style.display='block', 500)</script>",
        "</body></html>"
    );

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Wait for selector to become visible
    let resp = execute_command(
        &json!({ "id": "3", "action": "wait", "selector": "#target", "state": "visible", "timeout": 5000 }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Wait for text
    let resp = execute_command(
        &json!({ "id": "4", "action": "wait", "text": "Appeared!", "timeout": 5000 }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Timeout wait
    let start = std::time::Instant::now();
    let resp = execute_command(
        &json!({ "id": "5", "action": "wait", "timeout": 200 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(
        start.elapsed().as_millis() >= 150,
        "Timeout wait should sleep at least 150ms"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// wait --load on a page that already finished loading must resolve
// immediately instead of waiting for a Page.loadEventFired that will never
// come (the common case after a click that triggers an SPA navigation).
#[tokio::test]
#[ignore]
async fn e2e_wait_load_state_resolves_immediately_when_already_loaded() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<h1>Loaded</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    for (id, load_state) in [("3", "load"), ("4", "domcontentloaded")] {
        let start = std::time::Instant::now();
        let resp = execute_command(
            &json!({ "id": id, "action": "waitforloadstate", "state": load_state, "timeout": 10000 }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert!(
            start.elapsed().as_millis() < 3000,
            "wait --load {} on an already-loaded page should resolve immediately, took {}ms",
            load_state,
            start.elapsed().as_millis()
        );
    }

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Same-document navigation regression test
// ---------------------------------------------------------------------------
//
// Chrome may perform a same-document navigation when it determines the target
// URL is the same document as the current page (ignoring fragment). This
// causes Page.loadEventFired to not fire, making wait_for_lifecycle
// hang forever waiting for an event that never comes.
//
// The fix checks loader_id in the Page.navigate response - if None,
// it's a same-document navigation and we skip waiting for lifecycle events.

#[tokio::test]
#[ignore]
async fn e2e_navigate_same_url_twice_should_not_hang() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to about:blank first to start from a known state
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "about:blank" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Create a simple HTML page that changes its own URL via history.pushState
    // This simulates SPA routing behavior which triggers same-document navigation
    let base_page = "data:text/html,<html><body><script>
        // On first load, change URL via pushState without navigation
        history.pushState({}, '', '/#/home');
    </script><h1>Test</h1></body></html>";

    // Navigate to the page (first time)
    let resp = execute_command(
        &json!({ "id": "3", "action": "navigate", "url": base_page }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Verify URL changed due to pushState
    let resp = execute_command(&json!({ "id": "4", "action": "url" }), &mut state).await;
    assert_success(&resp);
    let url_after_push = get_data(&resp)["url"].as_str().unwrap();
    // URL should have changed to include /#/home due to pushState
    assert!(
        url_after_push.contains("/%23/home") || url_after_push.contains("/#/home"),
        "URL should have changed via pushState, got: {}",
        url_after_push
    );

    // Navigate to the SAME base URL again
    // Without fix: Chrome may do same-document nav, wait_for_lifecycle hangs
    // With fix: We detect loader_id is None and skip waiting
    let start = std::time::Instant::now();
    let resp = execute_command(
        &json!({ "id": "5", "action": "navigate", "url": base_page }),
        &mut state,
    )
    .await;
    let elapsed = start.elapsed().as_secs();

    // Should complete quickly (< 5 seconds) without hanging
    // Without fix, this times out after 25 seconds (default_timeout_ms)
    assert!(
        elapsed < 5,
        "Second navigation should not hang, but took {}s",
        elapsed
    );
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Viewport with deviceScaleFactor (retina)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_viewport_scale_factor() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "about:blank" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Default devicePixelRatio should be 1
    let resp = execute_command(
        &json!({ "id": "3", "action": "evaluate", "script": "window.devicePixelRatio" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let default_dpr = get_data(&resp)["result"].as_f64().unwrap();
    assert_eq!(default_dpr, 1.0, "Default devicePixelRatio should be 1");

    // Set viewport with 2x scale factor
    let resp = execute_command(
        &json!({ "id": "4", "action": "viewport", "width": 1920, "height": 1080, "deviceScaleFactor": 2.0 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["width"], 1920);
    assert_eq!(get_data(&resp)["height"], 1080);
    assert_eq!(get_data(&resp)["deviceScaleFactor"], 2.0);

    // devicePixelRatio should now be 2
    let resp = execute_command(
        &json!({ "id": "5", "action": "evaluate", "script": "window.devicePixelRatio" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let new_dpr = get_data(&resp)["result"].as_f64().unwrap();
    assert_eq!(
        new_dpr, 2.0,
        "devicePixelRatio should be 2 after setting scale factor"
    );

    // CSS viewport width should still be 1920 (not 3840)
    let resp = execute_command(
        &json!({ "id": "6", "action": "evaluate", "script": "window.innerWidth" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let css_width = get_data(&resp)["result"].as_i64().unwrap();
    assert_eq!(css_width, 1920, "CSS width should remain 1920 at 2x scale");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Viewport and emulation
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_viewport_emulation() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<h1>Viewport</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Get initial width
    let resp = execute_command(
        &json!({ "id": "3", "action": "evaluate", "script": "window.innerWidth" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let initial_width = get_data(&resp)["result"].as_i64().unwrap();

    // Set viewport to a different size
    let resp = execute_command(
        &json!({ "id": "4", "action": "viewport", "width": 375, "height": 812, "deviceScaleFactor": 3.0, "mobile": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["width"], 375);
    assert_eq!(get_data(&resp)["height"], 812);
    assert_eq!(get_data(&resp)["mobile"], true);

    // Reload to apply viewport change
    let resp = execute_command(&json!({ "id": "5", "action": "reload" }), &mut state).await;
    assert_success(&resp);

    // Width should differ from default (setDeviceMetricsOverride applied)
    let resp = execute_command(
        &json!({ "id": "6", "action": "evaluate", "script": "window.innerWidth" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let new_width = get_data(&resp)["result"].as_i64().unwrap();
    assert!(
        new_width != initial_width || new_width == 375,
        "Viewport should change from {} after setDeviceMetricsOverride (got {})",
        initial_width,
        new_width
    );

    // Set user agent
    let resp = execute_command(
        &json!({ "id": "5", "action": "user_agent", "userAgent": "TestBot/1.0" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "6", "action": "evaluate", "script": "navigator.userAgent" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "TestBot/1.0");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Hover, scroll, press
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_hover_scroll_press() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = concat!(
        "data:text/html,<html style='scroll-behavior:smooth'><body style='height:3000px'>",
        "<button id='btn' onmouseover=\"this.textContent='hovered'\">Hover me</button>",
        "<input id='input' onkeydown=\"this.dataset.key=event.key\">",
        "</body></html>"
    );

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Hover
    let resp = execute_command(
        &json!({ "id": "3", "action": "hover", "selector": "#btn" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Scroll
    let scroll_started = std::time::Instant::now();
    let resp = execute_command(
        &json!({ "id": "4", "action": "scroll", "y": 500 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(
        scroll_started.elapsed() < std::time::Duration::from_secs(2),
        "scroll position measurement should settle promptly"
    );
    assert_eq!(get_data(&resp)["moved"], true);
    assert!(get_data(&resp)["after"]["y"].as_f64().unwrap() > 0.0);

    let resp = execute_command(
        &json!({ "id": "5", "action": "evaluate", "script": "window.scrollY" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let scroll_y = get_data(&resp)["result"].as_f64().unwrap();
    assert!(scroll_y > 0.0, "Should have scrolled down");

    let resp = execute_command(
        &json!({
            "id": "7",
            "action": "evaluate",
            "script": "document.documentElement.style.scrollBehavior = 'auto'; true"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "8", "action": "scroll", "y": 10000 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let bottom = get_data(&resp)["after"]["y"].as_f64().unwrap();

    let resp = execute_command(
        &json!({ "id": "9", "action": "scroll", "y": 500 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["moved"], false);
    assert_eq!(get_data(&resp)["before"]["y"].as_f64().unwrap(), bottom);
    assert_eq!(get_data(&resp)["after"]["y"].as_f64().unwrap(), bottom);

    let resp = execute_command(
        &json!({
            "id": "10",
            "action": "evaluate",
            "script": "Object.defineProperty(window, 'requestAnimationFrame', { value: undefined, configurable: true }); true"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let fallback_started = std::time::Instant::now();
    let resp = execute_command(
        &json!({ "id": "11", "action": "scroll", "y": -100 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["moved"], true);
    assert!(
        fallback_started.elapsed() < std::time::Duration::from_secs(2),
        "scroll should use its bounded timeout when requestAnimationFrame is absent"
    );

    // Press key
    let resp = execute_command(
        &json!({ "id": "6", "action": "press", "key": "Enter" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["pressed"], "Enter");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Raw mouse regressions
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_mouse_down_move_up_preserves_drag_state() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": native_test_fixture_url("drag_probe")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "evaluate",
            "script": r#"(() => {
                const rect = document.getElementById('target').getBoundingClientRect();
                return {
                    left: Math.round(rect.left),
                    top: Math.round(rect.top),
                    x: Math.round(rect.left + rect.width / 2),
                    y: Math.round(rect.top + rect.height / 2)
                };
            })()"#
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let start = &get_data(&resp)["result"];
    let initial_left = start["left"]
        .as_i64()
        .expect("target left should be numeric");
    let initial_top = start["top"].as_i64().expect("target top should be numeric");
    let start_x = start["x"].as_i64().expect("target x should be numeric");
    let start_y = start["y"].as_i64().expect("target y should be numeric");
    let end_x = start_x + 80;
    let end_y = start_y + 60;

    let resp = execute_command(
        &json!({ "id": "4", "action": "mousemove", "x": start_x, "y": start_y }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "5", "action": "mousedown", "button": "left" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "6", "action": "mousemove", "x": end_x, "y": end_y }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "7", "action": "mouseup", "button": "left" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "8", "action": "evaluate", "script": "window.__dragProbe" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let probe = &get_data(&resp)["result"];
    assert_eq!(probe["finalLeft"].as_i64(), Some(initial_left + 80));
    assert_eq!(probe["finalTop"].as_i64(), Some(initial_top + 60));

    let events = probe["events"]
        .as_array()
        .expect("drag probe should expose events");
    assert!(
        events.iter().any(|event| {
            event["type"] == "mousedown"
                && event["x"].as_f64() == Some(start_x as f64)
                && event["y"].as_f64() == Some(start_y as f64)
                && event["buttons"].as_i64() == Some(1)
        }),
        "Expected a non-zero mousedown event in drag probe"
    );
    assert!(
        events.iter().any(|event| {
            event["type"] == "mousemove"
                && event["x"].as_f64() == Some(end_x as f64)
                && event["y"].as_f64() == Some(end_y as f64)
                && event["buttons"].as_i64() == Some(1)
        }),
        "Expected a drag mousemove with the button still pressed"
    );
    assert!(
        events.iter().any(|event| {
            event["type"] == "mouseup"
                && event["x"].as_f64() == Some(end_x as f64)
                && event["y"].as_f64() == Some(end_y as f64)
                && event["buttons"].as_i64() == Some(0)
        }),
        "Expected mouseup at the last drag position"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_mouse_drag_reaches_pointer_capture_target() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": native_test_fixture_url("pointer_capture_probe")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "evaluate",
            "script": r#"(() => {
                const rect = document.getElementById('handle').getBoundingClientRect();
                return {
                    x: Math.round(rect.left + rect.width / 2),
                    y: Math.round(rect.top + rect.height / 2)
                };
            })()"#
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let start = &get_data(&resp)["result"];
    let start_x = start["x"].as_i64().expect("handle x should be numeric");
    let start_y = start["y"].as_i64().expect("handle y should be numeric");
    let end_x = start_x + 80;
    let end_y = start_y + 60;

    let resp = execute_command(
        &json!({ "id": "4", "action": "mousemove", "x": start_x, "y": start_y }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "5", "action": "mousedown", "button": "left" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "6", "action": "mousemove", "x": end_x, "y": end_y }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "7", "action": "mouseup", "button": "left" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "8", "action": "evaluate", "script": "window.__pointerCaptureProbe" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let probe = &get_data(&resp)["result"];
    assert_eq!(probe["moved"].as_bool(), Some(true));

    let events = probe["events"]
        .as_array()
        .expect("pointer capture probe should expose events");
    assert!(
        events.iter().any(|event| {
            event["type"] == "pointermove"
                && event["phase"] == "drag"
                && event["hasCapture"].as_bool() == Some(true)
                && event["x"].as_f64() == Some(end_x as f64)
                && event["y"].as_f64() == Some(end_y as f64)
        }),
        "Expected pointermove with capture during the drag"
    );
    assert!(
        events.iter().any(|event| {
            event["type"] == "pointerup"
                && event["phase"] == "up"
                && event["hadCapture"].as_bool() == Some(true)
        }),
        "Expected pointerup to observe an active pointer capture"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_drag_action_sends_buttons_during_move() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": native_test_fixture_url("html5_drag_probe")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "drag",
            "source": "#source",
            "target": "#dest"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["dragged"].as_bool(), Some(true));

    let resp = execute_command(
        &json!({ "id": "4", "action": "evaluate", "script": "window.__html5DragProbe" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let probe = &get_data(&resp)["result"];
    let events = probe["events"]
        .as_array()
        .expect("html5 drag probe should expose events");

    // The mousemove events emitted while the button is held should carry
    // buttons == 1 so the browser recognises the gesture as a drag.
    assert!(
        events
            .iter()
            .any(|event| { event["type"] == "mousemove" && event["buttons"].as_i64() == Some(1) }),
        "Expected at least one mousemove with buttons == 1 during drag"
    );

    // dragstart must fire on the source element.
    assert!(
        events.iter().any(|event| event["type"] == "dragstart"),
        "Expected dragstart to fire on the source element"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// State save/load, state management
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_state_management() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set some storage
    let resp = execute_command(
        &json!({ "id": "3", "action": "storage_set", "type": "local", "key": "persist_key", "value": "persist_val" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Save state
    let tmp_state = std::env::temp_dir()
        .join("agent-browser-e2e-state.json")
        .to_string_lossy()
        .to_string();
    let resp = execute_command(
        &json!({ "id": "4", "action": "state_save", "path": &tmp_state }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(std::path::Path::new(&tmp_state).exists());

    // State show
    let resp = execute_command(
        &json!({ "id": "5", "action": "state_show", "path": &tmp_state }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let state_data = get_data(&resp);
    assert!(state_data.get("state").is_some());

    // State list
    let resp = execute_command(&json!({ "id": "6", "action": "state_list" }), &mut state).await;
    assert_success(&resp);
    assert!(get_data(&resp)["files"].is_array());

    // Clean up
    let _ = std::fs::remove_file(&tmp_state);

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Cross-domain state save (issue #1060)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_save_state_cross_domain() {
    let mut state = DaemonState::new();

    // Launch
    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to domain A and set cookie + localStorage
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://httpbin.org/html" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3", "action": "cookies_set",
            "name": "domainA_cookie", "value": "from_httpbin"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "4", "action": "storage_set",
            "type": "local", "key": "domainA_key", "value": "domainA_val"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to domain B and set cookie + localStorage
    let resp = execute_command(
        &json!({ "id": "5", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "6", "action": "cookies_set",
            "name": "domainB_cookie", "value": "from_example"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "7", "action": "storage_set",
            "type": "local", "key": "domainB_key", "value": "domainB_val"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Save state (currently on example.com)
    let tmp_state = std::env::temp_dir()
        .join("agent-browser-e2e-cross-domain-state.json")
        .to_string_lossy()
        .to_string();
    let resp = execute_command(
        &json!({ "id": "8", "action": "state_save", "path": &tmp_state }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Read and verify saved state
    let saved = std::fs::read_to_string(&tmp_state).expect("State file should exist");
    let state_data: serde_json::Value = serde_json::from_str(&saved).unwrap();

    // Verify BOTH domain cookies are present
    let cookies = state_data["cookies"].as_array().unwrap();
    let has_domain_a = cookies.iter().any(|c| c["name"] == "domainA_cookie");
    let has_domain_b = cookies.iter().any(|c| c["name"] == "domainB_cookie");
    assert!(
        has_domain_a,
        "Should include cross-domain cookie from httpbin.org: {:?}",
        cookies
    );
    assert!(
        has_domain_b,
        "Should include cookie from example.com: {:?}",
        cookies
    );

    // Verify BOTH origins' localStorage are present
    let origins = state_data["origins"].as_array().unwrap();
    let has_origin_a = origins.iter().any(|o| {
        o["origin"].as_str().is_some_and(|s| s.contains("httpbin"))
            && o["localStorage"]
                .as_array()
                .is_some_and(|ls| ls.iter().any(|e| e["name"] == "domainA_key"))
    });
    let has_origin_b = origins.iter().any(|o| {
        o["origin"].as_str().is_some_and(|s| s.contains("example"))
            && o["localStorage"]
                .as_array()
                .is_some_and(|ls| ls.iter().any(|e| e["name"] == "domainB_key"))
    });
    assert!(
        has_origin_a,
        "Should include localStorage from httpbin.org origin: {:?}",
        origins
    );
    assert!(
        has_origin_b,
        "Should include localStorage from example.com origin: {:?}",
        origins
    );

    // Clean up
    let _ = std::fs::remove_file(&tmp_state);

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Domain filter
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_domain_filter() {
    let mut state = DaemonState::new();

    // Set domain filter BEFORE launch so Fetch.enable is called during
    // launch and the background fetch handler intercepts from the start.
    {
        let mut df = state.domain_filter.write().await;
        *df = Some(super::network::DomainFilter::new("example.com"));
    }

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // The active about:blank document must be patched immediately, not only
    // after a later navigation.
    let resp = execute_command(
        &json!({
            "id": "1-rtc", "action": "evaluate",
            "script": "(() => { try { new RTCPeerConnection({iceServers:[{urls:'stun:secret.blocked.com:3478'}]}); return 'NOT_BLOCKED'; } catch (error) { return error.name; } })()",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "SecurityError");

    // New tabs created after launch must receive the same controls before a
    // requested URL starts loading.
    let resp = execute_command(
        &json!({ "id": "1-tab", "action": "tab_new", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({
            "id": "1-tab-rtc", "action": "evaluate",
            "script": "(() => { try { new RTCPeerConnection({iceServers:[{urls:'stun:secret.blocked.com:3478'}]}); return 'NOT_BLOCKED'; } catch (error) { return error.name; } })()",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "SecurityError");

    let resp = execute_command(
        &json!({ "id": "1-tab-blocked", "action": "tab_new", "url": "https://blocked.com" }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);
    let error = resp["error"].as_str().unwrap_or("");
    assert!(
        error.contains("blocked.com") || error.contains("not allowed"),
        "Blocked tab URL should fail before loading, got: {}",
        error
    );

    let resp = execute_command(
        &json!({ "id": "1-window", "action": "window_new" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({
            "id": "1-window-rtc", "action": "evaluate",
            "script": "(() => { try { new RTCPeerConnection({iceServers:[{urls:'stun:secret.blocked.com:3478'}]}); return 'NOT_BLOCKED'; } catch (error) { return error.name; } })()",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "SecurityError");

    // Allowed domain
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Blocked domain
    let resp = execute_command(
        &json!({ "id": "3", "action": "navigate", "url": "https://blocked.com" }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);
    let error = resp["error"].as_str().unwrap();
    assert!(
        error.contains("blocked") || error.contains("not allowed"),
        "Should reject blocked domain, got: {}",
        error
    );

    // Verify that in-page fetch to a blocked domain is also blocked by
    // the Fetch interception layer (not just the navigate-level check).
    // First navigate to the allowed domain.
    let resp = execute_command(
        &json!({ "id": "4", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Attempt a cross-origin fetch to a blocked domain from the page.
    let resp = execute_command(
        &json!({
            "id": "5", "action": "evaluate",
            "script": "fetch('https://blocked.com/data').then(() => 'ok').catch(e => 'blocked:' + e.message)",
            "await": true,
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let result = get_data(&resp)["result"].as_str().unwrap_or("");
    assert!(
        result.starts_with("blocked:"),
        "Fetch to blocked domain should fail, got: {}",
        result,
    );

    // WebRTC uses DNS and UDP outside CDP Fetch interception, so the domain
    // filter must disable both Chromium constructor names before page scripts
    // can create a peer connection.
    let resp = execute_command(
        &json!({
            "id": "6", "action": "evaluate",
            "script": "['RTCPeerConnection','webkitRTCPeerConnection'].filter(name => typeof window[name] === 'function').map(name => { try { new window[name]({iceServers:[{urls:'stun:secret.blocked.com:3478'}]}); return name + ':NOT_BLOCKED'; } catch (error) { return name + ':' + error.name; } })",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let results = get_data(&resp)["result"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "RTCPeerConnection should be available in Chrome"
    );
    assert!(
        results.iter().all(|result| result
            .as_str()
            .is_some_and(|value| value.ends_with(":SecurityError"))),
        "Every peer connection constructor should be blocked, got: {:?}",
        results,
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_domain_filter_blocks_page_created_popup_before_first_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests_for_server = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..20 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let requests = requests_for_server.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("").to_string();
                if let Ok(mut logged) = requests.lock() {
                    logged.push(request_line);
                }

                let body = "<!doctype html><title>allowed</title><button id=\"go\">go</button>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    let mut state = DaemonState::new();
    {
        let mut df = state.domain_filter.write().await;
        *df = Some(super::network::DomainFilter::new("localhost"));
    }

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": format!("http://localhost:{}/", port),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "evaluate",
            "script": format!("window.open('http://127.0.0.1:{}/leak'); 'done'", port),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let logged = requests.lock().unwrap().clone();
    assert!(
        !logged.iter().any(|line| line.contains(" /leak ")),
        "Blocked popup made a server request: {:?}",
        logged
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_domain_filter_blocks_service_worker_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests_for_server = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..20 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let requests = requests_for_server.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("").to_string();
                if let Ok(mut logged) = requests.lock() {
                    logged.push(request_line.clone());
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");

                let (content_type, body) = if path == "/sw.js" {
                    (
                        "application/javascript",
                        format!(
                            r#"self.addEventListener('message', event => {{
    event.source && event.source.postMessage('started');
    const controller = new AbortController();
    setTimeout(() => controller.abort(), 500);
    fetch('http://127.0.0.1:{}/leak', {{ mode: 'no-cors', signal: controller.signal }})
        .then(() => event.source && event.source.postMessage('leaked'))
        .catch(error => event.source && event.source.postMessage('blocked:' + error.name));
}});"#,
                            port
                        ),
                    )
                } else {
                    (
                        "text/html",
                        r#"<!doctype html><title>allowed</title><script>
window.swResult = 'pending';
navigator.serviceWorker.register('/sw.js').then(async reg => {
    await navigator.serviceWorker.ready;
    const sw = reg.active || reg.waiting || reg.installing;
    navigator.serviceWorker.addEventListener('message', event => {
        window.swResult = event.data;
    });
    sw.postMessage('go');
}).catch(error => {
    window.swResult = 'register:' + error.name;
});
</script>"#
                            .to_string(),
                    )
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    content_type,
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    let mut state = DaemonState::new();
    {
        let mut df = state.domain_filter.write().await;
        *df = Some(super::network::DomainFilter::new("localhost"));
    }

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": format!("http://localhost:{}/", port),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let mut sw_result = "pending".to_string();
    for _ in 0..50 {
        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "evaluate",
                "script": "window.swResult || 'pending'",
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        sw_result = get_data(&resp)["result"]
            .as_str()
            .unwrap_or("pending")
            .to_string();
        if !matches!(sw_result.as_str(), "pending" | "started") {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let logged = requests.lock().unwrap().clone();
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    server.abort();

    assert!(
        sw_result.starts_with("blocked:"),
        "Service worker request should be blocked, got: {}",
        sw_result
    );
    assert!(
        !logged.iter().any(|line| line.contains(" /leak ")),
        "Blocked service worker request reached the server: {:?}",
        logged
    );
}

#[tokio::test]
#[ignore]
async fn e2e_domain_filter_blocks_worker_websocket_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests_for_server = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..20 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let requests = requests_for_server.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("").to_string();
                if let Ok(mut logged) = requests.lock() {
                    logged.push(request_line.clone());
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");

                let (content_type, body) = if path == "/worker.js" {
                    (
                        "application/javascript",
                        format!(
                            r#"self.addEventListener('message', async () => {{
    try {{
        const response = await fetch('/worker-ping');
        const text = await response.text();
        if (text !== 'pong') {{
            self.postMessage('fetch:' + text);
            return;
        }}
    }} catch (error) {{
        self.postMessage('fetch-error:' + error.name);
        return;
    }}

    try {{
        const ws = new WebSocket('ws://127.0.0.1:{}/leak');
        ws.onopen = () => self.postMessage('leaked');
        ws.onerror = () => self.postMessage('blocked:error');
    }} catch (error) {{
        self.postMessage('blocked:' + error.name);
    }}
}});"#,
                            port
                        ),
                    )
                } else if path == "/worker-ping" {
                    ("text/plain", "pong".to_string())
                } else {
                    (
                        "text/html",
                        r#"<!doctype html><title>allowed</title><script>
window.workerWsResult = 'pending';
const worker = new Worker('/worker.js');
worker.onmessage = event => {
    window.workerWsResult = event.data;
};
worker.onerror = () => {
    window.workerWsResult = 'worker:error';
};
worker.postMessage('go');
</script>"#
                            .to_string(),
                    )
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    content_type,
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    let mut state = DaemonState::new();
    {
        let mut df = state.domain_filter.write().await;
        *df = Some(super::network::DomainFilter::new("localhost"));
    }

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": format!("http://localhost:{}/", port),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let mut worker_result = "pending".to_string();
    for _ in 0..50 {
        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "evaluate",
                "script": "window.workerWsResult || 'pending'",
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        worker_result = get_data(&resp)["result"]
            .as_str()
            .unwrap_or("pending")
            .to_string();
        if worker_result != "pending" {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let logged = requests.lock().unwrap().clone();
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    server.abort();

    assert!(
        worker_result.starts_with("blocked:"),
        "Worker WebSocket should be blocked, got: {}",
        worker_result
    );
    assert!(
        !logged.iter().any(|line| line.contains(" /leak ")),
        "Blocked worker WebSocket reached the server: {:?}",
        logged
    );
}

#[tokio::test]
#[ignore]
async fn e2e_domain_filter_blocks_csp_self_worker_fallback() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests_for_server = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..20 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let requests = requests_for_server.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("").to_string();
                if let Ok(mut logged) = requests.lock() {
                    logged.push(request_line.clone());
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");

                let (content_type, body, csp) = if path == "/worker.js" {
                    (
                        "application/javascript",
                        format!(
                            r#"self.addEventListener('message', async () => {{
    try {{
        const response = await fetch('/worker-ping');
        const text = await response.text();
        try {{
            await fetch('http://127.0.0.1:{}/leak', {{ mode: 'no-cors' }});
            self.postMessage('leaked');
        }} catch (error) {{
            self.postMessage('started:' + text + ';blocked:' + error.name);
        }}
    }} catch (error) {{
        self.postMessage('blocked:' + error.name);
    }}
}});"#,
                            port
                        ),
                        None,
                    )
                } else if path == "/worker-ping" {
                    ("text/plain", "pong".to_string(), None)
                } else {
                    (
                        "text/html",
                        r#"<!doctype html><title>allowed</title><script>
window.workerCspResult = 'pending';
const worker = new Worker('/worker.js');
worker.onmessage = event => {
    window.workerCspResult = event.data;
};
worker.onerror = () => {
    window.workerCspResult = 'worker:error';
};
worker.postMessage('go');
</script>"#
                            .to_string(),
                        Some("Content-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; worker-src 'self'\r\n"),
                    )
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    content_type,
                    csp.unwrap_or(""),
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    let mut state = DaemonState::new();
    {
        let mut df = state.domain_filter.write().await;
        *df = Some(super::network::DomainFilter::new("localhost"));
    }

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": format!("http://localhost:{}/", port),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let mut worker_result = "pending".to_string();
    for _ in 0..50 {
        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "evaluate",
                "script": "window.workerCspResult || 'pending'",
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        worker_result = get_data(&resp)["result"]
            .as_str()
            .unwrap_or("pending")
            .to_string();
        if worker_result != "pending" {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let logged = requests.lock().unwrap().clone();
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    server.abort();

    assert_eq!(
        worker_result, "worker:error",
        "CSP-blocked worker bootstrap must fail closed instead of running an unguarded worker"
    );
    assert!(
        !logged.iter().any(|line| line.contains(" /leak ")),
        "Blocked CSP fallback worker request reached the server: {:?}",
        logged
    );
}

#[tokio::test]
#[ignore]
async fn e2e_domain_filter_blocks_module_worker_top_level_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests_for_server = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..20 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let requests = requests_for_server.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("").to_string();
                if let Ok(mut logged) = requests.lock() {
                    logged.push(request_line.clone());
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");

                let (content_type, body) = if path == "/module-worker.js" {
                    (
                        "application/javascript",
                        format!(
                            r#"try {{
    await fetch('http://127.0.0.1:{}/leak', {{ mode: 'no-cors' }});
    self.postMessage('leaked');
}} catch (error) {{
    self.postMessage('blocked:' + error.name);
}}"#,
                            port
                        ),
                    )
                } else {
                    (
                        "text/html",
                        r#"<!doctype html><title>allowed</title><script>
window.moduleWorkerResult = 'pending';
const worker = new Worker('/module-worker.js', { type: 'module' });
worker.onmessage = event => {
    window.moduleWorkerResult = event.data;
};
worker.onerror = () => {
    window.moduleWorkerResult = 'worker:error';
};
</script>"#
                            .to_string(),
                    )
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    content_type,
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    let mut state = DaemonState::new();
    {
        let mut df = state.domain_filter.write().await;
        *df = Some(super::network::DomainFilter::new("localhost"));
    }

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": format!("http://localhost:{}/", port),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let mut worker_result = "pending".to_string();
    for _ in 0..50 {
        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "evaluate",
                "script": "window.moduleWorkerResult || 'pending'",
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        worker_result = get_data(&resp)["result"]
            .as_str()
            .unwrap_or("pending")
            .to_string();
        if worker_result != "pending" {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let logged = requests.lock().unwrap().clone();
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    server.abort();

    assert!(
        worker_result.starts_with("blocked:"),
        "Module worker top-level request should be blocked, got: {}",
        worker_result
    );
    assert!(
        !logged.iter().any(|line| line.contains(" /leak ")),
        "Blocked module worker request reached the server: {:?}",
        logged
    );
}

// ---------------------------------------------------------------------------
// Diff engine
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_diff_snapshot() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": native_test_fixture_url("snapshot_diff_probe")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Take a snapshot and use it as baseline for diff
    let resp = execute_command(&json!({ "id": "3", "action": "snapshot" }), &mut state).await;
    assert_success(&resp);
    let baseline = get_data(&resp)["snapshot"].as_str().unwrap().to_string();
    assert!(baseline.starts_with("- button \"Primary action\" [ref=e1]"));
    let baseline_dir = tempfile::tempdir().unwrap();
    let baseline_path = baseline_dir.path().join("baseline.txt");
    std::fs::write(&baseline_path, format!("{}\n", baseline)).unwrap();

    // A failed diff must preserve the refs from the last successful snapshot.
    let resp = execute_command(
        &json!({
            "id": "4",
            "action": "diff_snapshot",
            "baseline": baseline_path,
            "selector": "#missing"
        }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .contains("did not match any element"));

    let resp = execute_command(
        &json!({ "id": "5", "action": "click", "selector": "e1" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Repeated diffs preserve document refs and report no content changes.
    for id in ["6", "7"] {
        let resp = execute_command(
            &json!({ "id": id, "action": "diff_snapshot", "baseline": baseline_path }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        let data = get_data(&resp);
        assert_eq!(data["changed"], false);
        assert_eq!(data["additions"], 0);
        assert_eq!(data["removals"], 0);
        assert_eq!(data["diff"], "");
    }

    // Modify the page
    let resp = execute_command(
        &json!({
            "id": "8",
            "action": "evaluate",
            "script": "document.querySelector('#primary-action').textContent = 'Updated action'"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Diff against baseline
    let resp = execute_command(
        &json!({ "id": "9", "action": "diff_snapshot", "baseline": baseline_path }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let data = get_data(&resp);
    assert_eq!(
        data["changed"], true,
        "Diff should detect the button change"
    );
    assert_eq!(data["additions"], 1);
    assert_eq!(data["removals"], 1);
    assert!(data["diff"].as_str().unwrap().contains("Updated action"));

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_diff_url_aligns_refs_after_snapshot() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let stale_url = "data:text/html,<button id='stale-action'>Stale action</button>";
    let url = native_test_fixture_url("snapshot_diff_probe");
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": stale_url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Populate refs that must be invalidated once the URL diff starts navigating.
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "snapshot",
            "selector": "#stale-action"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(state.ref_map.get("e1").is_some());

    let resp = execute_command(
        &json!({
            "id": "4",
            "action": "diff_url",
            "url1": url,
            "url2": "http://[invalid"
        }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(state.ref_map.entries_sorted().is_empty());

    let resp = execute_command(
        &json!({
            "id": "5",
            "action": "evaluate",
            "script": "document.querySelector('#primary-action')?.textContent"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "Primary action");

    let resp = execute_command(
        &json!({ "id": "6", "action": "click", "selector": "e1" }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"].as_str().unwrap().contains("Unknown ref: e1"));

    // Populate the session ref map before comparing the same URL to itself.
    let resp = execute_command(&json!({ "id": "7", "action": "snapshot" }), &mut state).await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "8",
            "action": "diff_url",
            "url1": url,
            "url2": url
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let data = get_data(&resp);
    assert_eq!(data["diff"]["identical"], true);
    assert_eq!(data["diff"]["changed"], false);
    assert_eq!(data["diff"]["additions"], 0);
    assert_eq!(data["diff"]["removals"], 0);
    assert_ne!(
        data["snapshot1"], data["snapshot2"],
        "Replaced documents must not recycle actionable IDs"
    );
    assert_eq!(
        super::diff::snapshot_comparison_text(data["snapshot1"].as_str().unwrap()),
        super::diff::snapshot_comparison_text(data["snapshot2"].as_str().unwrap())
    );
    let primary_ref = state
        .ref_map
        .entries_sorted()
        .into_iter()
        .find(|(_, entry)| entry.name == "Primary action")
        .unwrap()
        .0;
    let next = execute_command(&json!({"id": "9", "action": "snapshot"}), &mut state).await;
    assert_success(&next);
    assert_eq!(
        get_data(&next)["refs"][&primary_ref]["name"],
        "Primary action"
    );
    assert_success(
        &execute_command(
            &json!({"id": "10", "action": "click", "selector": primary_ref}),
            &mut state,
        )
        .await,
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Phase 8 commands: focus, clear, count, boundingbox, innertext, setvalue
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_phase8_commands() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = concat!(
        "data:text/html,<html><body>",
        "<input id='a' value='original'>",
        "<input id='b' value='other'>",
        "<p class='item'>One</p>",
        "<p class='item'>Two</p>",
        "<p class='item'>Three</p>",
        "<div id='box' style='width:200px;height:100px;background:red'>Box</div>",
        "</body></html>"
    );

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Focus
    let resp = execute_command(
        &json!({ "id": "10", "action": "focus", "selector": "#a" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Clear
    let resp = execute_command(
        &json!({ "id": "11", "action": "clear", "selector": "#a" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "12", "action": "evaluate", "script": "document.getElementById('a').value" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "");

    // Set value
    let resp = execute_command(
        &json!({ "id": "13", "action": "setvalue", "selector": "#b", "value": "new-value" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "14", "action": "inputvalue", "selector": "#b" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["value"], "new-value");

    // Count
    let resp = execute_command(
        &json!({ "id": "15", "action": "count", "selector": ".item" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["count"], 3);

    // Bounding box
    let resp = execute_command(
        &json!({ "id": "16", "action": "boundingbox", "selector": "#box" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let bbox = get_data(&resp);
    assert_eq!(bbox["width"], 200.0);
    assert_eq!(bbox["height"], 100.0);
    assert!(bbox["x"].as_f64().is_some());
    assert!(bbox["y"].as_f64().is_some());

    // Inner text
    let resp = execute_command(
        &json!({ "id": "17", "action": "innertext", "selector": "#box" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["text"], "Box");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Auto-launch (tests that commands auto-launch when no browser exists)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_auto_launch() {
    let mut state = DaemonState::new();

    // Navigate without explicit launch -- should auto-launch
    let resp = execute_command(
        &json!({ "id": "1", "action": "navigate", "url": "data:text/html,<h1>Auto</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(state.browser.is_some(), "Browser should be auto-launched");

    let resp = execute_command(
        &json!({ "id": "2", "action": "evaluate", "script": "document.querySelector('h1').textContent" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "Auto");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_error_handling() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<h1>Errors</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Unknown action
    let resp = execute_command(
        &json!({ "id": "10", "action": "nonexistent_action" }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .contains("Not yet implemented"));

    // Missing required parameter
    let resp = execute_command(
        &json!({ "id": "11", "action": "fill", "selector": "#x" }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"].as_str().unwrap().contains("value"));

    // Click on non-existent element
    let resp = execute_command(
        &json!({ "id": "12", "action": "click", "selector": "#does-not-exist" }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);

    // Evaluate syntax error
    let resp = execute_command(
        &json!({ "id": "13", "action": "evaluate", "script": "}{invalid" }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"].as_str().unwrap().contains("error"));

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_click_reports_covering_overlay() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = r#"
        <html>
        <body>
            <button id="target" onclick="document.getElementById('result').textContent = 'clicked'">
                Target
            </button>
            <div id="consent-banner" style="position:fixed;inset:0;z-index:10;background:rgba(0,0,0,0.1)">
                <button id="dismiss" style="position:absolute;right:20px;bottom:20px"
                    onclick="document.getElementById('consent-banner').remove()">
                    Dismiss
                </button>
            </div>
            <div id="result">idle</div>
        </body>
        </html>
    "#;
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "click", "selector": "#target" }),
        &mut state,
    )
    .await;
    assert_eq!(resp["success"], false, "covered target should fail: {resp}");
    let error = resp["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("covered by <div#consent-banner>"),
        "unexpected covered-click error: {}",
        error
    );

    let resp = execute_command(
        &json!({ "id": "4", "action": "gettext", "selector": "#result" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["text"], "idle");

    let resp = execute_command(
        &json!({ "id": "5", "action": "click", "selector": "#dismiss" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "6", "action": "click", "selector": "#target" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "7", "action": "gettext", "selector": "#result" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["text"], "clicked");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Serve both documents over HTTP so different localhost ports produce a
/// cross-origin iframe that still shares the page's renderer/CDP session.
async fn start_iframe_click_server(
    child_url: Option<String>,
    transform: &str,
    overlay: &str,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let child_url = child_url.unwrap_or_else(|| format!("http://localhost:{port}/child"));
    let child_overlay = if overlay == "child" {
        "<div id='child-overlay' style='position:absolute;left:20px;top:20px;width:100px;height:30px;z-index:10'></div>"
    } else {
        ""
    };
    let child = format!(
        r#"<!doctype html><html><body style="margin:0">
<button id="target" style="position:absolute;left:20px;top:20px;width:100px;height:30px"
    onclick="parent.postMessage('target', '*')">Frame target</button>
<button id="decoy" style="position:absolute;left:0;top:120px;width:390px;height:60px"
    onclick="parent.postMessage('decoy', '*')">Lower decoy</button>
{child_overlay}<script>parent.postMessage('ready', '*')</script></body></html>"#
    );
    let parent_overlay = if overlay == "parent" {
        "<div id='parent-overlay' style='position:fixed;inset:0;z-index:10'></div>"
    } else {
        ""
    };
    let parent = format!(
        r#"<!doctype html><html><body style="margin:0">
<script>
window.clicks = {{target:0, decoy:0, top:0}};
window.childReady = false;
addEventListener('message', event => {{
    if (event.source !== document.getElementById('click-frame').contentWindow) return;
    if (event.data === 'ready') window.childReady = true;
    else if (event.data === 'target' || event.data === 'decoy') window.clicks[event.data]++;
}});
</script>
<button style="position:absolute;left:10px;top:10px" onclick="window.clicks.top++">Top target</button>
<iframe id="click-frame" name="click-frame" title="Click frame" src="{child_url}"
    style="position:absolute;left:100px;top:120px;width:400px;height:240px;border:0;transform-origin:0 0;transform:{transform}"></iframe>
{parent_overlay}</body></html>"#
    );
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let parent = parent.clone();
            let child = child.clone();
            tokio::spawn(async move {
                let mut buf = [0; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let body = if request.starts_with("GET /child ") {
                    child
                } else {
                    parent
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nOrigin-Agent-Cluster: ?0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (port, server)
}

async fn assert_iframe_reference_click(
    topology: &str,
    transform: &str,
    select_frame: bool,
    overlay: &str,
    scroll_and_scale: bool,
) {
    let (child_port, child_server) = start_iframe_click_server(None, "none", overlay).await;
    let child_url = match topology {
        "same-origin" => None,
        "same-session" => Some(format!("http://localhost:{child_port}/child")),
        "oopif" => Some(format!("http://127.0.0.1:{child_port}/child")),
        _ => panic!("Unknown frame topology: {topology}"),
    };
    let (port, parent_server) = start_iframe_click_server(child_url, transform, overlay).await;
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({"action":"launch", "headless":true}), &mut state).await,
    );
    if scroll_and_scale {
        assert_success(
            &execute_command(
                &json!({"action":"viewport", "width":1280, "height":720, "deviceScaleFactor":2}),
                &mut state,
            )
            .await,
        );
    }
    assert_success(
        &execute_command(
            &json!({"action":"navigate", "url":format!("http://localhost:{port}/")}),
            &mut state,
        )
        .await,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let ready = execute_command(
                &json!({"action":"evaluate", "script":"window.childReady"}),
                &mut state,
            )
            .await;
            assert_success(&ready);
            if get_data(&ready)["result"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("iframe fixture did not load");
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    let top_reference = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Top target")
        .unwrap()
        .0
        .clone();
    let top_click = execute_command(
        &json!({"action":"click", "selector":format!("@{top_reference}")}),
        &mut state,
    )
    .await;
    if overlay == "parent" {
        assert_eq!(top_click["success"], false);
        assert!(top_click["error"]
            .as_str()
            .unwrap_or_default()
            .contains("covered by <div#parent-overlay>"));
    } else {
        assert_success(&top_click);
    }
    if scroll_and_scale {
        assert_evaluate(&mut state, "scroll", "document.body.style.height = '2000px'; window.scrollTo(0, 60); [window.scrollY, window.devicePixelRatio]", json!([60, 2])).await;
    }

    if select_frame {
        let frame_reference = get_data(&snapshot)["refs"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, entry)| entry["name"] == "Click frame")
            .unwrap()
            .0;
        assert_success(
            &execute_command(
                &json!({"action":"frame", "selector":format!("@{frame_reference}")}),
                &mut state,
            )
            .await,
        );
    }
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    let reference = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Frame target")
        .expect("frame target must be exposed by the Rust snapshot")
        .0
        .clone();
    let entry = state.ref_map.get(&reference).unwrap();
    let frame_id = entry
        .frame_id
        .as_deref()
        .expect("target ref must belong to the iframe");
    assert_eq!(
        state.iframe_sessions.contains_key(frame_id),
        topology == "oopif",
        "fixture must exercise {topology}, sessions: {:?}",
        state.iframe_sessions
    );
    if select_frame {
        assert_eq!(state.active_frame_id.as_deref(), Some(frame_id));
    }
    // Assert the DOM geometry is disjoint, independently of the click result.
    let browser = state.browser.as_ref().unwrap();
    let session = state
        .iframe_sessions
        .get(frame_id)
        .map(String::as_str)
        .unwrap_or(browser.active_session_id().unwrap());
    let object = browser
        .client
        .send_command(
            "DOM.resolveNode",
            Some(json!({"backendNodeId":entry.backend_node_id.unwrap()})),
            Some(session),
        )
        .await
        .unwrap();
    if topology == "oopif" && scroll_and_scale {
        // Keep both nodes visible while giving the OOPIF its own scroll offset.
        // The decoy occupies the wrong document point used when that session's
        // viewport offset is omitted, so a best-effort miss cannot hide the bug.
        let scrolled = browser.client.send_command("Runtime.callFunctionOn", Some(json!({
            "objectId":object["object"]["objectId"],
            "functionDeclaration":"function() { const doc = this.ownerDocument; doc.body.style.height = '2000px'; this.style.top = '400px'; doc.getElementById('decoy').style.top = '200px'; doc.defaultView.scrollTo(0, 200); const rect = this.getBoundingClientRect(); return [doc.defaultView.scrollY, rect.top >= 0 && rect.bottom <= doc.defaultView.innerHeight]; }",
            "returnByValue":true
        })), Some(session)).await.unwrap();
        assert_eq!(scrolled["result"]["value"], json!([200, true]));
        let wrong_hit = browser
            .client
            .send_command(
                "DOM.getNodeForLocation",
                Some(json!({"x":70,"y":215})),
                Some(session),
            )
            .await
            .unwrap();
        let wrong_node = browser
            .client
            .send_command(
                "DOM.describeNode",
                Some(json!({"backendNodeId":wrong_hit["backendNodeId"]})),
                Some(session),
            )
            .await
            .unwrap();
        assert!(
            wrong_node["node"]["attributes"]
                .as_array()
                .unwrap()
                .as_chunks::<2>()
                .0
                .iter()
                .any(|attribute| attribute[0] == "id" && attribute[1] == "decoy"),
            "omitting the OOPIF's viewport offset must sample its unrelated decoy: {wrong_node}"
        );
    }
    let geometry = browser.client.send_command("Runtime.callFunctionOn", Some(json!({
        "objectId":object["object"]["objectId"],
        "functionDeclaration":"function() { const target = this.getBoundingClientRect(); const decoy = this.ownerDocument.getElementById('decoy').getBoundingClientRect(); return {disjoint:target.bottom < decoy.top || decoy.bottom < target.top, parentAccessible:!!this.ownerDocument.defaultView.frameElement}; }",
        "returnByValue":true
    })), Some(session)).await.unwrap();
    assert_eq!(geometry["result"]["value"]["disjoint"], true);
    assert_eq!(
        geometry["result"]["value"]["parentAccessible"],
        topology == "same-origin"
    );

    let clicked = execute_command(
        &json!({"action":"click", "selector":format!("@{reference}")}),
        &mut state,
    )
    .await;
    // Collect counters and close before asserting the expected baseline failure.
    let counters = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve(window.clicks), 50))"}), &mut state).await;
    assert_success(&counters);
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    child_server.abort();
    parent_server.abort();
    if overlay.is_empty() {
        assert_success(&clicked);
        assert_eq!(
            get_data(&counters)["result"],
            json!({"target":1,"decoy":0,"top":1}),
            "{topology}, transform={transform}, selected={select_frame}"
        );
    } else {
        assert_eq!(
            clicked["success"], false,
            "a genuine {overlay} overlay must reject the click: {clicked}"
        );
        assert!(
            clicked["error"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!("covered by <div#{overlay}-overlay>")),
            "unexpected interception error: {clicked}"
        );
        assert_eq!(
            get_data(&counters)["result"],
            json!({"target":0,"decoy":0,"top":if overlay == "parent" {0} else {1}})
        );
    }
}

#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_same_session_cross_origin() {
    assert_iframe_reference_click("same-session", "none", false, "", false).await;
}

#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_selected_same_session_cross_origin() {
    assert_iframe_reference_click("same-session", "none", true, "", false).await;
}

#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_transformed_same_origin() {
    for transform in ["none", "scale(4)"] {
        assert_iframe_reference_click("same-origin", transform, false, "", false).await;
    }
}

#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_rotated_same_origin() {
    assert_iframe_reference_click("same-origin", "scale(3) rotate(10deg)", false, "", false).await;
}

#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_oopif() {
    for select_frame in [false, true] {
        assert_iframe_reference_click("oopif", "none", select_frame, "", false).await;
    }
}

#[tokio::test]
#[ignore]
async fn e2e_goal_probe_reports_oopif_ancestor_coverage_unknown() {
    let (child_port, child_server) = start_iframe_click_server(None, "none", "").await;
    let child_url = format!("http://127.0.0.1:{child_port}/child");
    let (parent_port, parent_server) =
        start_iframe_click_server(Some(child_url), "none", "parent").await;
    let mut state = DaemonState::new();
    assert_success(&execute_command(&json!({"action":"launch","headless":true}), &mut state).await);
    assert_success(
        &execute_command(
            &json!({"action":"navigate","url":format!("http://localhost:{parent_port}/")}),
            &mut state,
        )
        .await,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let ready = execute_command(
                &json!({"action":"evaluate","script":"window.childReady"}),
                &mut state,
            )
            .await;
            if get_data(&ready)["result"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    let target = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Frame target")
        .map(|(name, _)| format!("@{name}"))
        .unwrap();
    let probe = execute_command(
        &json!({"action":"__goal_probe","selector":target,"goalTimeoutMs":500}),
        &mut state,
    )
    .await;
    assert_eq!(probe["data"]["status"], "unknown", "{probe}");
    assert!(probe["data"]["detail"]
        .as_str()
        .unwrap_or("")
        .contains("ancestor coverage"));
    let _ = close_current_browser(&mut state).await;
    child_server.abort();
    parent_server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_rejects_real_overlays() {
    for topology in ["same-origin", "same-session", "oopif"] {
        for overlay in ["child", "parent"] {
            // Parent overlays above an OOPIF require a separate cross-session
            // check; this test preserves interception within the input session.
            if topology == "oopif" && overlay == "parent" {
                continue;
            }
            assert_iframe_reference_click(topology, "none", false, overlay, false).await;
        }
    }
    assert_iframe_reference_click("same-origin", "scale(4)", false, "child", false).await;
}

#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_scrolled_device_scale() {
    assert_iframe_reference_click("same-session", "none", false, "", true).await;
}

#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_scrolled_oopif() {
    assert_iframe_reference_click("oopif", "none", false, "", true).await;
}

/// Replacing the snapshotted node must retain interception and checked-point
/// dispatch when role/name lookup resolves its same-session cross-origin peer.
#[tokio::test]
#[ignore]
async fn e2e_iframe_reference_click_stale_node_interception() {
    let (child_port, child_server) = start_iframe_click_server(None, "none", "").await;
    let (port, parent_server) = start_iframe_click_server(
        Some(format!("http://localhost:{child_port}/child")),
        "none",
        "",
    )
    .await;
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({"action":"launch", "headless":true}), &mut state).await,
    );
    assert_success(
        &execute_command(
            &json!({"action":"navigate", "url":format!("http://localhost:{port}/")}),
            &mut state,
        )
        .await,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let ready = execute_command(
                &json!({"action":"evaluate", "script":"window.childReady"}),
                &mut state,
            )
            .await;
            assert_success(&ready);
            if get_data(&ready)["result"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("cross-origin stale-node fixture did not load");
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    let reference = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Frame target")
        .expect("stale target must originate in the Rust snapshot")
        .0
        .clone();
    let entry = state.ref_map.get(&reference).unwrap();
    let cached_id = entry.backend_node_id.unwrap();
    let frame_id = entry.frame_id.as_ref().unwrap();
    assert!(
        !state.iframe_sessions.contains_key(frame_id),
        "fixture must use a same-session cross-origin iframe"
    );
    let browser = state.browser.as_ref().unwrap();
    let session = browser.active_session_id().unwrap().to_string();
    let old_object = browser
        .client
        .send_command(
            "DOM.resolveNode",
            Some(json!({"backendNodeId":cached_id})),
            Some(&session),
        )
        .await
        .unwrap();
    let replacement = browser.client.send_command("Runtime.callFunctionOn", Some(json!({
        "objectId":old_object["object"]["objectId"],
        "functionDeclaration":"function() { const fresh = this.cloneNode(true); this.replaceWith(fresh); Object.assign(fresh.style, {left:'19.4px', padding:'0', border:'0'}); Object.assign(fresh.ownerDocument.getElementById('decoy').style, {left:'70.3px', top:'20px', width:'30px', height:'30px', padding:'0', border:'0'}); return fresh; }"
    })), Some(&session)).await.unwrap();
    let fresh_object = replacement["result"]["objectId"].as_str().unwrap();
    let fresh_node = browser
        .client
        .send_command(
            "DOM.describeNode",
            Some(json!({"objectId":fresh_object})),
            Some(&session),
        )
        .await
        .unwrap();
    assert_ne!(fresh_node["node"]["backendNodeId"], cached_id);
    assert!(
        browser
            .client
            .send_command(
                "DOM.getBoxModel",
                Some(json!({"backendNodeId":cached_id})),
                Some(&session),
            )
            .await
            .is_err(),
        "detached snapshot backend must fail geometry and require the fresh-node fallback"
    );
    // Verify that the fractional center and integer sample lie on opposite
    // sides of the decoy boundary, and that the frame is really cross-origin.
    let geometry = browser.client.send_command("Runtime.callFunctionOn", Some(json!({
        "objectId":fresh_object,
        "functionDeclaration":"function() { const doc = this.ownerDocument; const rect = this.getBoundingClientRect(); const x = rect.x + rect.width / 2; const y = rect.y + rect.height / 2; return [doc.elementFromPoint(x, y).id, doc.elementFromPoint(Math.round(x), Math.round(y)).id, doc.defaultView.frameElement === null]; }",
        "returnByValue":true
    })), Some(&session)).await.unwrap();
    assert_eq!(
        geometry["result"]["value"],
        json!(["decoy", "target", true])
    );
    let clicked = execute_command(
        &json!({"action":"click", "selector":format!("@{reference}")}),
        &mut state,
    )
    .await;
    let first_counts = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve(window.clicks), 50))"}), &mut state).await;

    assert_eq!(
        state.ref_map.get(&reference).unwrap().backend_node_id,
        Some(cached_id),
        "repeat the stale reference to exercise fallback with a genuine overlay too"
    );
    let browser = state.browser.as_ref().unwrap();
    let covered_geometry = browser.client.send_command("Runtime.callFunctionOn", Some(json!({
        "objectId":fresh_object,
        "functionDeclaration":"function() { const doc = this.ownerDocument; Object.assign(doc.getElementById('decoy').style, {left:'19px', width:'101px'}); const rect = this.getBoundingClientRect(); return doc.elementFromPoint(Math.round(rect.x + rect.width / 2), Math.round(rect.y + rect.height / 2)).id; }",
        "returnByValue":true
    })), Some(&session)).await.unwrap();
    assert_eq!(covered_geometry["result"]["value"], "decoy");
    let covered = execute_command(
        &json!({"action":"click", "selector":format!("@{reference}")}),
        &mut state,
    )
    .await;
    let final_counts = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve(window.clicks), 50))"}), &mut state).await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    child_server.abort();
    parent_server.abort();

    assert_success(&clicked);
    assert_success(&first_counts);
    assert_eq!(
        get_data(&first_counts)["result"],
        json!({"target":1,"decoy":0,"top":0}),
        "fresh-node dispatch must use the point that passed interception"
    );
    assert_eq!(
        covered["success"], false,
        "genuine overlay must block: {covered}"
    );
    assert!(covered["error"]
        .as_str()
        .unwrap_or_default()
        .contains("covered by <button#decoy>"));
    assert_success(&final_counts);
    assert_eq!(
        get_data(&final_counts)["result"],
        json!({"target":1,"decoy":0,"top":0}),
        "rejected fallback must not dispatch to target or overlay"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_preserves_related_hit_targets() {
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({"action":"launch", "headless":true}), &mut state).await,
    );
    let html = r#"<!doctype html><html><body>
<script>window.counts = {descendant:0, checkbox:0, shadow:0, frame:0, ownPseudo:0, covered:0}</script>
<style>
#own-pseudo::before {content:'';position:absolute;inset:0;background:rgba(0,0,0,0.1)}
#pseudo-overlay::before {content:'';position:fixed;left:240px;top:80px;width:180px;height:40px;z-index:10;background:rgba(0,0,0,0.1)}
</style>
<button id="own-pseudo" style="position:absolute;left:240px;top:20px;width:180px;height:40px" onclick="counts.ownPseudo++">Own pseudo target</button>
<button style="position:absolute;left:240px;top:80px;width:180px;height:40px" onclick="counts.covered++">Pseudo covered target</button>
<div id="pseudo-overlay"></div>
<button style="position:absolute;left:20px;top:20px;width:180px;height:40px" onclick="counts.descendant++"><span>Descendant target</span></button>
<input id="checkbox" aria-label="Associated checkbox" type="checkbox" style="position:absolute;left:20px;top:80px;width:30px;height:30px;margin:0" onchange="counts.checkbox++">
<label for="checkbox" style="position:absolute;left:20px;top:80px;width:30px;height:30px;background:white"><span>Label</span></label>
<div id="shadow-host" style="position:absolute;left:20px;top:140px"></div>
<script>document.getElementById('shadow-host').attachShadow({mode:'open'}).innerHTML = '<button style="width:180px;height:40px" onclick="window.counts.shadow++"><span>Shadow target</span></button>'</script>
<iframe title="Owner frame" style="position:absolute;left:20px;top:220px;width:200px;height:100px;border:0"
    srcdoc="<body style='margin:0'><button style='width:200px;height:100px' onclick='parent.counts.frame++'>Embedded child</button></body>"></iframe>
</body></html>"#;
    assert_success(&execute_command(&json!({"action":"navigate", "url":format!("data:text/html;base64,{}", STANDARD.encode(html))}), &mut state).await);
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    for name in [
        "Descendant target",
        "Associated checkbox",
        "Shadow target",
        "Owner frame",
        "Own pseudo target",
    ] {
        let reference = get_data(&snapshot)["refs"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, entry)| entry["name"] == name)
            .unwrap_or_else(|| panic!("missing reference for {name}: {snapshot}"))
            .0;
        assert_success(
            &execute_command(
                &json!({"action":"click", "selector":format!("@{reference}")}),
                &mut state,
            )
            .await,
        );
    }
    let covered_reference = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Pseudo covered target")
        .unwrap()
        .0;
    let covered = execute_command(
        &json!({"action":"click", "selector":format!("@{covered_reference}")}),
        &mut state,
    )
    .await;
    assert_eq!(
        covered["success"], false,
        "pseudo-element overlay must block: {covered}"
    );
    assert!(
        covered["error"]
            .as_str()
            .unwrap_or_default()
            .contains("covered by <div#pseudo-overlay>"),
        "unexpected pseudo-element blocker: {covered}"
    );
    assert_evaluate(
        &mut state,
        "counts",
        "window.counts",
        json!({"descendant":1,"checkbox":1,"shadow":1,"frame":1,"ownPseudo":1,"covered":0}),
    )
    .await;
    assert_evaluate(
        &mut state,
        "checked",
        "document.getElementById('checkbox').checked",
        json!(true),
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
}

// ---------------------------------------------------------------------------
// Profile cookie persistence across restarts
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_profile_cookie_persistence() {
    let profile_dir = std::env::temp_dir().join(format!(
        "agent-browser-e2e-profile-{}",
        uuid::Uuid::new_v4()
    ));

    // Session 1: launch with profile, set a cookie, close
    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({
                "id": "1",
                "action": "launch",
                "headless": true,
                "profile": profile_dir.to_str().unwrap()
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "cookies_set",
                "name": "persist_test",
                "value": "should_survive_restart",
                "domain": ".example.com",
                "path": "/",
                "expires": 2000000000
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        // Verify cookie is set
        let resp =
            execute_command(&json!({ "id": "4", "action": "cookies_get" }), &mut state).await;
        assert_success(&resp);
        let cookies = get_data(&resp)["cookies"].as_array().unwrap();
        let found = cookies
            .iter()
            .any(|c| c["name"] == "persist_test" && c["value"] == "should_survive_restart");
        assert!(found, "Cookie should exist before close");

        let resp = execute_command(&json!({ "id": "5", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Session 2: reopen with the same profile, verify cookie persisted
    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({
                "id": "10",
                "action": "launch",
                "headless": true,
                "profile": profile_dir.to_str().unwrap()
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "11", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp =
            execute_command(&json!({ "id": "12", "action": "cookies_get" }), &mut state).await;
        assert_success(&resp);
        let cookies = get_data(&resp)["cookies"].as_array().unwrap();
        let found = cookies
            .iter()
            .any(|c| c["name"] == "persist_test" && c["value"] == "should_survive_restart");
        assert!(
            found,
            "Cookie should persist across restart with --profile. Cookies found: {:?}",
            cookies
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );

        let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    let _ = std::fs::remove_dir_all(&profile_dir);
}

// ---------------------------------------------------------------------------
// Inspect / CDP URL
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_get_cdp_url() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "2", "action": "cdp_url" }), &mut state).await;
    assert_success(&resp);
    let cdp_url = get_data(&resp)["cdpUrl"]
        .as_str()
        .expect("cdpUrl should be a string");
    assert!(
        cdp_url.starts_with("ws://"),
        "CDP URL should start with ws://, got: {}",
        cdp_url
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_inspect() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "3", "action": "inspect" }), &mut state).await;
    assert_success(&resp);
    let data = get_data(&resp);
    assert_eq!(data["opened"], true);
    let url = data["url"]
        .as_str()
        .expect("inspect url should be a string");
    assert!(
        url.starts_with("http://127.0.0.1:"),
        "Inspect URL should be http://127.0.0.1:<port>, got: {}",
        url
    );

    // Verify the HTTP redirect serves a 302 to the DevTools frontend
    let http_resp = reqwest::get(url).await;
    match http_resp {
        Ok(r) => {
            let final_url = r.url().to_string();
            assert!(
                final_url.contains("devtools/devtools_app.html"),
                "Redirect should point to DevTools frontend, got: {}",
                final_url
            );
        }
        Err(e) => {
            panic!("HTTP GET to inspect URL failed: {}", e);
        }
    }

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Stale ref fallback (#805): clicking a ref after the DOM has been replaced
// should fall back to role/name lookup instead of failing.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_click_stale_ref_falls_back_to_role_name() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to a page with a button that replaces the DOM when clicked.
    let html = r#"data:text/html,<body>
        <div id="c">
            <button onclick="
                var c = document.getElementById('c');
                c.innerHTML = '';
                var b = document.createElement('button');
                b.textContent = 'Target';
                b.onclick = function() { document.title = 'clicked'; };
                c.appendChild(b);
                document.title = 'replaced';
            ">Replace</button>
            <button>Target</button>
        </div>
    </body>"#;

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Snapshot to populate the ref_map with backend_node_ids.
    let resp = execute_command(&json!({ "id": "3", "action": "snapshot" }), &mut state).await;
    assert_success(&resp);
    let snapshot = get_data(&resp)["snapshot"].as_str().unwrap();
    assert!(
        snapshot.contains("Replace"),
        "Snapshot should contain Replace button"
    );
    assert!(
        snapshot.contains("Target"),
        "Snapshot should contain Target button"
    );

    // Click "Replace" — this removes all DOM nodes and recreates them,
    // making the backend_node_id for "Target" stale.
    let resp = execute_command(
        &json!({ "id": "4", "action": "click", "selector": "e1" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Verify the DOM was actually replaced.
    let resp = execute_command(&json!({ "id": "5", "action": "title" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["title"], "replaced");

    // Now click the stale "Target" ref. Before the fix this returned:
    //   "CDP error (DOM.getBoxModel): Could not compute box model."
    // After the fix it falls back to role/name lookup and succeeds.
    let resp = execute_command(
        &json!({ "id": "6", "action": "click", "selector": "e2" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Verify the fallback click hit the right (recreated) button.
    let resp = execute_command(&json!({ "id": "7", "action": "title" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["title"],
        "clicked",
        "Stale ref should have been resolved via role/name fallback"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Regression: Material Design checkbox/radio (#832)
//
// Material Design controls hide the native <input> off-screen and place
// overlay elements (ripple, touch-target) on top.  Coordinate-based CDP
// clicks may therefore miss the actual input.  The check/uncheck actions
// must detect this and fall back to a JS .click() — matching the behaviour
// that Playwright provided in v0.19.0.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_material_checkbox_check_uncheck() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Inline HTML that reproduces the Material Design DOM pattern:
    // - Native <input> is visually hidden (position:absolute, opacity:0, off-screen)
    // - A ripple overlay sits on top with pointer-events:all, intercepting coordinate clicks
    // - An ARIA-only checkbox uses role="checkbox" + aria-checked (no native input)
    let html = concat!(
        "data:text/html,<html><body>",
        // -- Native baseline --
        "<input id='native' type='checkbox'>",
        // -- Material-style hidden-input checkbox --
        "<div id='mat' style='position:relative;padding:12px'>",
          "<input id='mat-input' type='checkbox' style='position:absolute;opacity:0;width:1px;height:1px;top:-9999px;left:-9999px;pointer-events:none'>",
          "<div style='position:absolute;top:0;left:0;width:48px;height:48px;pointer-events:all;z-index:10'></div>",
          "<span>Material CB</span>",
        "</div>",
        // -- ARIA-only checkbox (no native input) --
        "<div id='aria' role='checkbox' aria-checked='false' tabindex='0'>ARIA CB</div>",
        "<script>",
          "document.getElementById('aria').addEventListener('click',function(){",
            "var c=this.getAttribute('aria-checked')==='true';",
            "this.setAttribute('aria-checked',String(!c));",
          "});",
        "</script>",
        "</body></html>"
    );

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // ---- Native checkbox (sanity baseline) ----
    let resp = execute_command(
        &json!({ "id": "10", "action": "ischecked", "selector": "#native" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["checked"], false);

    let resp = execute_command(
        &json!({ "id": "11", "action": "check", "selector": "#native" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "12", "action": "ischecked", "selector": "#native" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["checked"], true, "native check failed");

    // ---- Material checkbox (hidden input + overlay) ----
    // ischecked on the wrapper should detect the nested hidden input's state
    let resp = execute_command(
        &json!({ "id": "20", "action": "ischecked", "selector": "#mat" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["checked"], false);

    let resp = execute_command(
        &json!({ "id": "21", "action": "check", "selector": "#mat" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "22", "action": "ischecked", "selector": "#mat" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["checked"],
        true,
        "Material checkbox should be checked after check action (#832)"
    );

    // Idempotency: check again should be a no-op
    let resp = execute_command(
        &json!({ "id": "23", "action": "check", "selector": "#mat" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "24", "action": "ischecked", "selector": "#mat" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["checked"],
        true,
        "Material checkbox should stay checked on redundant check"
    );

    // Uncheck
    let resp = execute_command(
        &json!({ "id": "25", "action": "uncheck", "selector": "#mat" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "26", "action": "ischecked", "selector": "#mat" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["checked"],
        false,
        "Material checkbox should be unchecked after uncheck action"
    );

    // ---- ARIA-only checkbox ----
    let resp = execute_command(
        &json!({ "id": "30", "action": "ischecked", "selector": "#aria" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["checked"], false);

    let resp = execute_command(
        &json!({ "id": "31", "action": "check", "selector": "#aria" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "32", "action": "ischecked", "selector": "#aria" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["checked"],
        true,
        "ARIA checkbox should be checked after check action"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Issue #841 – snapshot -C and screenshot --annotate must not hang over WSS
// (PS: -C is deprecated, cursor-interactive elements are referred by default now)
// ---------------------------------------------------------------------------

/// Verifies that `snapshot` detects elements with cursor:pointer / onclick / tabindex,
/// produces the correct v0.19.0-compatible output format, deduplicates against the ARIA
/// tree, and completes in bounded time (no sequential CDP round-trip explosion).
#[tokio::test]
#[ignore]
async fn e2e_snapshot_cursor_interactive() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Page with:
    //  - <button> and <a> (standard interactive – ARIA tree)
    //  - <div cursor:pointer onclick> (clickable – cursor section)
    //  - <div tabindex=0> (focusable – cursor section)
    //  - <span cursor:pointer> (clickable – cursor section)
    //  - <span cursor:pointer> child of <div cursor:pointer> (inherited – skip)
    let html = concat!(
        "<html><body>",
        "<a href='#'>Link</a>",
        "<button>Btn</button>",
        "<div style='cursor:pointer' onclick='x()'>ClickDiv</div>",
        "<div tabindex='0'>FocusDiv</div>",
        "<span style='cursor:pointer'>PointerSpan</span>",
        "<div style='cursor:pointer'><span>InheritChild</span></div>",
        "</body></html>",
    );

    let resp = execute_command(
        &json!({ "id": "2", "action": "setcontent", "html": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // snapshot -i: interactive tree
    let start = std::time::Instant::now();
    let resp = execute_command(
        &json!({ "id": "3", "action": "snapshot", "interactive": true }),
        &mut state,
    )
    .await;
    let elapsed = start.elapsed();
    assert_success(&resp);

    let snapshot = get_data(&resp)["snapshot"].as_str().unwrap();

    // v0.19.0 output format: role + hints
    assert!(
        snapshot.contains("clickable") && snapshot.contains("[cursor:pointer"),
        "Expected v0.19.0-format cursor output with hints:\n{}",
        snapshot,
    );

    // Role differentiation: tabindex-only → focusable
    assert!(
        snapshot.contains("focusable") && snapshot.contains("[tabindex]"),
        "Expected focusable role for tabindex-only element:\n{}",
        snapshot,
    );

    // Text dedup: "Link" and "Btn" are in the ARIA tree, so must NOT suffix
    // with cursor-interactive info. Verify line by line.
    for line in snapshot.lines() {
        assert!(
            !(line.contains("\"Link\"")
                && (line.contains("clickable")
                    || line.contains("focusable")
                    || line.contains("editable"))),
            "Standard <a> element should not have cursor-interactive info:\n{}",
            line
        );
        assert!(
            !(line.contains("\"Btn\"")
                && (line.contains("clickable")
                    || line.contains("focusable")
                    || line.contains("editable"))),
            "Standard <button> element should not have cursor-interactive info:\n{}",
            line
        );
    }

    // Must complete quickly (< 5s), not hit the 30s CDP timeout
    assert!(
        elapsed.as_secs() < 5,
        "snapshot took {:?}, expected < 5s (Issue #841 regression)",
        elapsed,
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Verifies that `screenshot --annotate` completes in bounded time even with
/// many interactive elements. Guards against the sequential CDP round-trip
/// regression that caused hangs over high-latency WSS (Issue #841).
#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotate_many_elements() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // 50 buttons: old sequential code would do 50×2×200ms ≈ 20s over WSS.
    let mut html = String::from("<html><body>");
    for i in 1..=50 {
        html.push_str(&format!("<button>Button {}</button>", i));
    }
    html.push_str("</body></html>");

    let resp = execute_command(
        &json!({ "id": "2", "action": "setcontent", "html": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let start = std::time::Instant::now();
    let resp = execute_command(
        &json!({ "id": "3", "action": "screenshot", "annotate": true }),
        &mut state,
    )
    .await;
    let elapsed = start.elapsed();
    assert_success(&resp);

    let annotations = get_data(&resp)["annotations"]
        .as_array()
        .expect("Annotated screenshot should return annotations");

    assert!(
        annotations.len() >= 50,
        "Expected at least 50 annotations, got {}",
        annotations.len(),
    );

    // Must complete quickly (< 10s), not hit the 30s CDP timeout
    assert!(
        elapsed.as_secs() < 10,
        "screenshot --annotate with 50 elements took {:?}, expected < 10s (Issue #841)",
        elapsed,
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Verifies `snapshot` with many cursor-interactive elements completes in
/// bounded time. Direct regression test for Issue #841's root cause: N×2
/// sequential CDP round-trips per cursor-interactive element.
#[tokio::test]
#[ignore]
async fn e2e_snapshot_cursor_many_elements() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // 100 cursor-interactive divs: old code = 200 sequential CDP calls,
    // at 200ms WSS latency = 40s timeout. New code must finish in seconds.
    let mut html = String::from("<html><body>");
    for i in 1..=100 {
        html.push_str(&format!(
            "<div style='cursor:pointer' onclick='x()'>Item {}</div>",
            i,
        ));
    }
    html.push_str("</body></html>");

    let resp = execute_command(
        &json!({ "id": "2", "action": "setcontent", "html": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let start = std::time::Instant::now();
    let resp = execute_command(
        &json!({ "id": "3", "action": "snapshot", "interactive": true }),
        &mut state,
    )
    .await;
    let elapsed = start.elapsed();
    assert_success(&resp);

    let snapshot = get_data(&resp)["snapshot"].as_str().unwrap();

    // All 100 items should appear
    assert!(
        snapshot.contains("Item 1") && snapshot.contains("Item 100"),
        "Expected all 100 cursor-interactive items in output",
    );

    // All should have v0.19.0-format hints
    assert!(
        snapshot.contains("[cursor:pointer, onclick]"),
        "Expected v0.19.0-format hints",
    );

    // Must complete quickly
    assert!(
        elapsed.as_secs() < 10,
        "snapshot with 100 cursor elements took {:?}, expected < 10s (Issue #841)",
        elapsed,
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Test that InlineTextBox nodes are filtered from snapshot output while preserving
/// the actual text content from parent elements.
#[tokio::test]
#[ignore]
async fn e2e_snapshot_continuous_static_text() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Simple HTML with text content that would generate InlineTextBox nodes and sperate to multiple StaticText nodes
    let html =
        "data:text/html,<html><body><div><span>Hello</span> <span>World</span></div></body></html>";

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Take snapshot to capture full output and verify InlineTextBox filtering and StaticText aggregation
    let start = std::time::Instant::now();
    let resp = execute_command(&json!({ "id": "3", "action": "snapshot" }), &mut state).await;
    assert_success(&resp);
    let elapsed = start.elapsed();

    let snapshot_output = get_data(&resp)["snapshot"].as_str().unwrap();

    // Verify that InlineTextBox does not appear in the output
    assert!(
        !snapshot_output.contains("InlineTextBox"),
        "Snapshot output should not contain InlineTextBox: {}",
        snapshot_output
    );

    // Verify that the actual text content is preserved
    assert!(
        snapshot_output.contains("Hello World"),
        "Snapshot should contain 'Hello World': {}",
        snapshot_output
    );

    // Must complete quickly
    assert!(
        elapsed.as_secs() < 5,
        "snapshot with InlineTextBox filtering took {:?}, expected < 5s",
        elapsed,
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Helper: tiny HTTP server that echoes request headers as JSON
// ---------------------------------------------------------------------------

/// Starts a TCP listener on localhost:0 and spawns a task that accepts
/// connections, reads the HTTP request, and responds with a JSON body
/// containing all received request headers. Returns the server's base URL.
async fn start_echo_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{}", port);

    let handle = tokio::spawn(async move {
        // Serve up to 20 requests then exit (enough for all tests).
        for _ in 0..20 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);

                // Parse headers from the HTTP request.
                let mut headers = serde_json::Map::new();
                for line in request.lines().skip(1) {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(": ") {
                        headers.insert(key.to_string(), Value::String(value.to_string()));
                    }
                }

                let body = serde_json::to_string(&json!({ "headers": headers })).unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    (base_url, handle)
}

async fn start_webmcp_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let request = String::from_utf8_lossy(&buffer);
                let body = if request.starts_with("GET /frame.html ") {
                    native_test_fixture_html("webmcp_frame_probe")
                        .replace("__PORT__", &port.to_string())
                } else if request.starts_with("GET /context.html ") {
                    native_test_fixture_html("webmcp_context_probe").to_string()
                } else if request.starts_with("GET /empty.html ") {
                    "<!doctype html><title>No page tools</title>".to_string()
                } else if request.starts_with("GET /delayed.html ") {
                    native_test_fixture_html("webmcp_delayed_probe").to_string()
                } else {
                    native_test_fixture_html("webmcp_probe").replace("__PORT__", &port.to_string())
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

/// Starts a tiny cookie-gated app that behaves like a Next dev target with
/// cookie-backed login state.
async fn start_cookie_login_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{}", port);

    let handle = tokio::spawn(async move {
        for _ in 0..100 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };

            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or_default();
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let has_auth_cookie = request.lines().any(|line| {
                    line.to_ascii_lowercase().starts_with("cookie:")
                        && line.contains("next_dev_loop_auth=1")
                });

                let mut headers = vec!["Content-Type: text/html".to_string()];
                let body = if path.starts_with("/login") {
                    headers.push(
                        "Set-Cookie: next_dev_loop_auth=1; Path=/; Max-Age=3600; SameSite=Lax"
                            .to_string(),
                    );
                    "<!doctype html><title>Logged in</title><main>Login complete</main>".to_string()
                } else if has_auth_cookie {
                    "<!doctype html><title>Home</title><main>Welcome back</main>".to_string()
                } else {
                    "<!doctype html><title>Home</title><main>Please sign in</main>".to_string()
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\n{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    headers.join("\r\n"),
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    (base_url, handle)
}

/// Starts a tiny HTTP server that serves a delayed-render login form.
///
/// The page continuously fetches `/ping` so `networkidle` is hard to reach,
/// while the login form itself appears after `render_delay_ms`.
async fn start_delayed_login_server(
    render_delay_ms: u64,
    ping_interval_ms: u64,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{}", port);

    let handle = tokio::spawn(async move {
        // Serve enough requests for navigation + many background /ping calls.
        for _ in 0..1000 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };

            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or_default();
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");

                let (status, content_type, body) = if path.starts_with("/ping") {
                    ("204 No Content", "text/plain", String::new())
                } else {
                    let html = format!(
                        r#"<!doctype html>
<html>
  <head><meta charset="utf-8"><title>Delayed Login</title></head>
  <body>
    <input id="search" type="text" name="search" />
    <div id="root">loading...</div>
    <script>
      setInterval(() => {{
        fetch('/ping?ts=' + Date.now()).catch(() => {{}});
      }}, {ping_interval_ms});

      setTimeout(() => {{
        const root = document.getElementById('root');
        root.innerHTML = `
          <form id="login-form">
            <input type="email" name="email" />
            <input type="password" name="password" />
            <button type="submit">Sign in</button>
          </form>
        `;
        document.getElementById('login-form').addEventListener('submit', function(e) {{
          e.preventDefault();
          e.stopPropagation();
          window.__submitted = true;
        }});
      }}, {render_delay_ms});
    </script>
  </body>
</html>"#,
                    );
                    ("200 OK", "text/html", html)
                };

                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    content_type,
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    (base_url, handle)
}

/// Starts a stateful login page whose form appears only after an in-page
/// action. The counter covers top-level documents so tests can prove that
/// no-navigation login did not reload or replace the page.
async fn start_stateful_auth_login_server(
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{}", port);
    let document_requests = Arc::new(AtomicUsize::new(0));
    let counter = document_requests.clone();

    let handle = tokio::spawn(async move {
        for _ in 0..100 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                if !path.starts_with("/favicon") {
                    counter.fetch_add(1, Ordering::SeqCst);
                }

                let body = r#"<!doctype html>
<html>
  <head><meta charset="utf-8"><title>Stateful Login</title><link rel="icon" href="data:," /></head>
  <body>
    <button id="reveal" type="button">Continue to login</button>
    <form id="login-form" style="display:none">
      <input type="email" name="email" />
      <input type="password" name="password" />
      <button type="submit">Sign in</button>
    </form>
    <script>
      document.getElementById('reveal').addEventListener('click', () => {
        document.getElementById('login-form').style.display = 'block';
        window.__revealed = true;
      });
      document.getElementById('login-form').addEventListener('submit', (event) => {
        event.preventDefault();
        window.__submitted = true;
      });
    </script>
  </body>
</html>"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    (base_url, document_requests, handle)
}

fn unique_auth_profile_name(suffix: &str) -> String {
    format!(
        "e2e-auth-login-{}-{}",
        suffix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_else(|_| std::time::Duration::from_secs(0))
            .as_nanos()
    )
}

#[tokio::test]
#[ignore]
async fn e2e_auth_login_no_navigate_preserves_active_page_state() {
    let (base_url, document_requests, _server) = start_stateful_auth_login_server().await;
    let mut state = DaemonState::new();
    let profile_name = unique_auth_profile_name("no-navigate");

    assert_success(
        &execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "2", "action": "navigate", "url": format!("{}/flow/start?state=ready#login", base_url) }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "3", "action": "click", "selector": "#reveal" }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "4", "action": "evaluate", "script": "window.__marker = 'preserved'" }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({
                "id": "5",
                "action": "auth_save",
                "name": profile_name.clone(),
                "url": format!("{}/credentials/login?credential-query-sentinel#credential-fragment-sentinel", base_url),
                "username": "stateful-user@example.com",
                "password": "stateful-password-secret",
            }),
            &mut state,
        )
        .await,
    );
    let request_count_before = document_requests.load(Ordering::SeqCst);

    let login = execute_command(
        &json!({ "id": "6", "action": "auth_login", "name": profile_name.clone(), "noNavigate": true }),
        &mut state,
    )
    .await;
    assert_success(&login);
    assert_eq!(get_data(&login)["loggedIn"], true);
    assert_eq!(
        document_requests.load(Ordering::SeqCst),
        request_count_before
    );

    let verify = execute_command(
        &json!({
            "id": "7",
            "action": "evaluate",
            "script": "({ marker: window.__marker, revealed: !!window.__revealed, submitted: !!window.__submitted, user: document.querySelector('input[type=email]').value, pass: document.querySelector('input[type=password]').value })",
        }),
        &mut state,
    )
    .await;
    assert_success(&verify);
    let result = &get_data(&verify)["result"];
    assert_eq!(result["marker"], "preserved");
    assert_eq!(result["revealed"], true);
    assert_eq!(result["submitted"], true);
    assert_eq!(result["user"], "stateful-user@example.com");
    assert_eq!(result["pass"], "stateful-password-secret");

    let _ = execute_command(
        &json!({ "id": "8", "action": "auth_delete", "name": profile_name }),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await);
}

#[cfg(unix)]
#[tokio::test]
#[ignore]
async fn e2e_auth_login_no_navigate_preserves_state_with_credential_provider() {
    use std::os::unix::fs::PermissionsExt;

    let (base_url, document_requests, _server) = start_stateful_auth_login_server().await;
    let plugin_dir = tempfile::tempdir().unwrap();
    let plugin_path = plugin_dir.path().join("stateful-credential-provider");
    std::fs::write(
        &plugin_path,
        format!(
            r#"#!/bin/sh
cat >/dev/null
printf '%s' '{{"protocol":"agent-browser.plugin.v1","success":true,"credential":{{"username":"provider-user@example.com","password":"provider-password-secret","url":"{}/provider/login"}}}}'
"#,
            base_url
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&plugin_path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&plugin_path, permissions).unwrap();

    let mut state = DaemonState::new();
    assert_success(
        &execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "2", "action": "navigate", "url": format!("{}/flow/provider", base_url) }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "3", "action": "click", "selector": "#reveal" }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "4", "action": "evaluate", "script": "window.__marker = 'provider-preserved'" }),
            &mut state,
        )
        .await,
    );
    let request_count_before = document_requests.load(Ordering::SeqCst);

    let login = execute_command(
        &json!({
            "id": "5",
            "action": "auth_login",
            "name": "provider-stateful-login",
            "noNavigate": true,
            "credentialProvider": "mock",
            "plugins": [{
                "name": "mock",
                "command": plugin_path.to_string_lossy(),
                "capabilities": ["credential.read"]
            }]
        }),
        &mut state,
    )
    .await;
    assert_success(&login);
    let serialized_login = serde_json::to_string(&login).unwrap();
    assert!(!serialized_login.contains("provider-user@example.com"));
    assert!(!serialized_login.contains("provider-password-secret"));
    assert_eq!(
        document_requests.load(Ordering::SeqCst),
        request_count_before
    );

    let verify = execute_command(
        &json!({
            "id": "6",
            "action": "evaluate",
            "script": "({ marker: window.__marker, submitted: !!window.__submitted, user: document.querySelector('input[type=email]').value, pass: document.querySelector('input[type=password]').value })",
        }),
        &mut state,
    )
    .await;
    assert_success(&verify);
    let result = &get_data(&verify)["result"];
    assert_eq!(result["marker"], "provider-preserved");
    assert_eq!(result["submitted"], true);
    assert_eq!(result["user"], "provider-user@example.com");
    assert_eq!(result["pass"], "provider-password-secret");

    assert_success(&execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await);
}

#[tokio::test]
#[ignore]
async fn e2e_auth_login_rejects_cross_origin_no_navigate() {
    let (active_origin, _, _active_server) = start_stateful_auth_login_server().await;
    let (credential_origin, credential_requests, _credential_server) =
        start_stateful_auth_login_server().await;
    let mut state = DaemonState::new();
    let profile_name = unique_auth_profile_name("origin-mismatch");

    assert_success(
        &execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "2", "action": "navigate", "url": format!("{}/flow/current-path-sentinel", active_origin) }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({ "id": "3", "action": "click", "selector": "#reveal" }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({
                "id": "4",
                "action": "auth_save",
                "name": profile_name.clone(),
                "url": format!("{}/credential-path-sentinel?credential-query-sentinel#credential-fragment-sentinel", credential_origin),
                "username": "username-secret-sentinel",
                "password": "password-secret-sentinel",
            }),
            &mut state,
        )
        .await,
    );

    let login = execute_command(
        &json!({ "id": "5", "action": "auth_login", "name": profile_name.clone(), "noNavigate": true }),
        &mut state,
    )
    .await;
    assert_eq!(login["success"], false);
    let error = login["error"].as_str().unwrap_or_default();
    assert!(error.contains("does not match credential origin"));
    for sentinel in [
        "current-path-sentinel",
        "credential-path-sentinel",
        "credential-query-sentinel",
        "credential-fragment-sentinel",
        "username-secret-sentinel",
        "password-secret-sentinel",
    ] {
        assert!(!error.contains(sentinel));
    }
    assert_eq!(credential_requests.load(Ordering::SeqCst), 0);

    let verify = execute_command(
        &json!({
            "id": "6",
            "action": "evaluate",
            "script": "({ user: document.querySelector('input[type=email]').value, pass: document.querySelector('input[type=password]').value, submitted: !!window.__submitted })",
        }),
        &mut state,
    )
    .await;
    assert_success(&verify);
    let result = &get_data(&verify)["result"];
    assert_eq!(result["user"], "");
    assert_eq!(result["pass"], "");
    assert_eq!(result["submitted"], false);

    let _ = execute_command(
        &json!({ "id": "7", "action": "auth_delete", "name": profile_name }),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await);
}

#[tokio::test]
#[ignore]
async fn e2e_auth_login_no_navigate_requires_usable_active_page() {
    let mut state = DaemonState::new();
    let profile_name = unique_auth_profile_name("missing-page");

    assert_success(
        &execute_command(
            &json!({
                "id": "1",
                "action": "auth_save",
                "name": profile_name.clone(),
                "url": "https://example.com/login",
                "username": "unused-user",
                "password": "unused-password",
            }),
            &mut state,
        )
        .await,
    );
    let login = execute_command(
        &json!({ "id": "2", "action": "auth_login", "name": profile_name.clone(), "noNavigate": true }),
        &mut state,
    )
    .await;
    assert_eq!(login["success"], false);
    assert!(login["error"]
        .as_str()
        .unwrap_or_default()
        .contains("requires an existing active HTTP(S) browser page"));
    assert!(
        state.browser.is_none(),
        "no-navigation login must not launch a browser"
    );

    let _ = execute_command(
        &json!({ "id": "3", "action": "auth_delete", "name": profile_name }),
        &mut state,
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn e2e_auth_login_waits_for_delayed_spa_form_render() {
    let (base_url, _server) = start_delayed_login_server(800, 100).await;
    let mut state = DaemonState::new();

    let profile_name = format!(
        "e2e-auth-login-spa-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_else(|_| std::time::Duration::from_secs(0))
            .as_millis()
    );

    let launch = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&launch);

    let save = execute_command(
        &json!({
            "id": "2",
            "action": "auth_save",
            "name": profile_name.clone(),
            "url": format!("{}/login", base_url),
            "username": "user@example.com",
            "password": "super-secret",
        }),
        &mut state,
    )
    .await;
    assert_success(&save);

    let login = execute_command(
        &json!({ "id": "3", "action": "auth_login", "name": profile_name.clone() }),
        &mut state,
    )
    .await;
    assert_success(&login);
    assert_eq!(get_data(&login)["loggedIn"], true);

    let current_url = execute_command(&json!({ "id": "3b", "action": "url" }), &mut state).await;
    assert_success(&current_url);
    assert!(get_data(&current_url)["url"]
        .as_str()
        .unwrap_or_default()
        .starts_with(&format!("{}/login", base_url)));

    let verify = execute_command(
        &json!({
            "id": "4",
            "action": "evaluate",
            "script": "({ user: document.querySelector('input[type=email]')?.value ?? '', pass: document.querySelector('input[type=password]')?.value ?? '', search: document.querySelector('#search')?.value ?? '', submitted: !!window.__submitted })",
        }),
        &mut state,
    )
    .await;
    assert_success(&verify);
    let result = &get_data(&verify)["result"];
    assert_eq!(result["user"], "user@example.com");
    assert_eq!(result["pass"], "super-secret");
    assert_eq!(result["search"], "");
    assert_eq!(result["submitted"], true);

    let _ = execute_command(
        &json!({ "id": "5", "action": "auth_delete", "name": profile_name }),
        &mut state,
    )
    .await;

    let close = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&close);
}

// ---------------------------------------------------------------------------
// Origin-scoped --headers tests
// ---------------------------------------------------------------------------

/// Headers passed via --headers on open persist for subsequent same-origin
/// navigations (the core regression from the Rust rewrite).
#[tokio::test]
#[ignore]
async fn e2e_headers_persist_same_origin_navigation() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate with --headers.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "navigate",
            "url": format!("{}/first", base_url),
            "headers": { "X-Test": "scoped" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to the same origin WITHOUT --headers.
    let resp = execute_command(
        &json!({
            "id": "3", "action": "navigate",
            "url": format!("{}/second", base_url),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // The page body is the echo JSON. Read it via evaluate.
    let resp = execute_command(
        &json!({
            "id": "4", "action": "evaluate",
            "script": "JSON.parse(document.body.innerText)",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let result = &get_data(&resp)["result"];
    assert_eq!(
        result["headers"]["X-Test"], "scoped",
        "X-Test header should persist on same-origin navigation without --headers"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Headers passed via --headers on open persist for in-page fetch/XHR to
/// the same origin.
#[tokio::test]
#[ignore]
async fn e2e_headers_persist_same_origin_fetch() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate with --headers.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "navigate",
            "url": format!("{}/page", base_url),
            "headers": { "X-Test": "fetched" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // In-page fetch to the same origin (relative URL).
    let resp = execute_command(
        &json!({
            "id": "3", "action": "evaluate",
            "script": "fetch('/echo').then(r => r.json())",
            "await": true,
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let result = &get_data(&resp)["result"];
    assert_eq!(
        result["headers"]["X-Test"], "fetched",
        "X-Test header should be present on in-page fetch to same origin"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Headers set via --headers do NOT leak to a different origin.
#[tokio::test]
#[ignore]
async fn e2e_headers_do_not_leak_cross_origin() {
    let (server_a, _ha) = start_echo_server().await;
    let (server_b, _hb) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to server A with --headers.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "navigate",
            "url": format!("{}/page", server_a),
            "headers": { "X-Secret": "a-only" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to server B (different origin) without --headers.
    let resp = execute_command(
        &json!({
            "id": "3", "action": "navigate",
            "url": format!("{}/page", server_b),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "4", "action": "evaluate",
            "script": "JSON.parse(document.body.innerText)",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let result = &get_data(&resp)["result"];
    assert!(
        result["headers"].get("X-Secret").is_none(),
        "X-Secret header must NOT leak to a different origin, got: {}",
        result["headers"],
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// In-page fetch to a cross-origin URL must NOT include the origin-scoped
/// headers (sub-resource isolation).
#[tokio::test]
#[ignore]
async fn e2e_headers_do_not_leak_cross_origin_fetch() {
    let (server_a, _ha) = start_echo_server().await;
    let (server_b, _hb) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate to server A with --headers.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "navigate",
            "url": format!("{}/page", server_a),
            "headers": { "X-Secret": "a-only" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Fetch from the page to server B (cross-origin sub-resource).
    let resp = execute_command(
        &json!({
            "id": "3", "action": "evaluate",
            "script": format!("fetch('{}/echo').then(r => r.json())", server_b),
            "await": true,
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let result = &get_data(&resp)["result"];
    assert!(
        result["headers"].get("X-Secret").is_none(),
        "X-Secret header must NOT leak to cross-origin fetch, got: {}",
        result["headers"],
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// `set headers` (global headers via the headers action) must not be
/// regressed — they should persist across navigations without being
/// cleared by the origin-scoped header logic.
#[tokio::test]
#[ignore]
async fn e2e_set_headers_not_regressed() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set global headers via the `headers` action (not --headers on navigate).
    let resp = execute_command(
        &json!({
            "id": "2", "action": "headers",
            "headers": { "X-Global": "everywhere" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate — global headers should be present.
    let resp = execute_command(
        &json!({
            "id": "3", "action": "navigate",
            "url": format!("{}/page", base_url),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "4", "action": "evaluate",
            "script": "JSON.parse(document.body.innerText)",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let result = &get_data(&resp)["result"];
    assert_eq!(
        result["headers"]["X-Global"], "everywhere",
        "Global headers set via `set headers` must persist across navigations"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Multiple origins each get their own independent headers.
#[tokio::test]
#[ignore]
async fn e2e_headers_multiple_origins_independent() {
    let (server_a, _ha) = start_echo_server().await;
    let (server_b, _hb) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set headers for origin A.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "navigate",
            "url": format!("{}/page", server_a),
            "headers": { "X-From": "alpha" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set different headers for origin B.
    let resp = execute_command(
        &json!({
            "id": "3", "action": "navigate",
            "url": format!("{}/page", server_b),
            "headers": { "X-From": "beta" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Verify B got its own header.
    let resp = execute_command(
        &json!({ "id": "4", "action": "evaluate", "script": "JSON.parse(document.body.innerText)" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"]["headers"]["X-From"], "beta");

    // Navigate back to A — should get A's header, not B's.
    let resp = execute_command(
        &json!({ "id": "5", "action": "navigate", "url": format!("{}/check", server_a) }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "6", "action": "evaluate", "script": "JSON.parse(document.body.innerText)" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"]["headers"]["X-From"], "alpha");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Headers persist when navigating away to a different origin and back.
#[tokio::test]
#[ignore]
async fn e2e_headers_persist_after_roundtrip() {
    let (server_a, _ha) = start_echo_server().await;
    let (server_b, _hb) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set headers for origin A.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "navigate",
            "url": format!("{}/page", server_a),
            "headers": { "X-Persist": "roundtrip" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate away to B (no headers).
    let resp = execute_command(
        &json!({ "id": "3", "action": "navigate", "url": format!("{}/page", server_b) }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Navigate back to A without --headers.
    let resp = execute_command(
        &json!({ "id": "4", "action": "navigate", "url": format!("{}/back", server_a) }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "5", "action": "evaluate", "script": "JSON.parse(document.body.innerText)" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["result"]["headers"]["X-Persist"],
        "roundtrip",
        "Headers should persist after navigating away and back to the same origin"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Passing --headers a second time to the same origin replaces the previous headers.
#[tokio::test]
#[ignore]
async fn e2e_headers_override_same_origin() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set initial headers.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "navigate",
            "url": format!("{}/first", base_url),
            "headers": { "X-Version": "v1" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Override with new headers.
    let resp = execute_command(
        &json!({
            "id": "3", "action": "navigate",
            "url": format!("{}/second", base_url),
            "headers": { "X-Version": "v2" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "4", "action": "evaluate", "script": "JSON.parse(document.body.innerText)" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["result"]["headers"]["X-Version"],
        "v2",
        "Second --headers should replace the first for the same origin"
    );

    // Subsequent navigation without --headers should use v2.
    let resp = execute_command(
        &json!({ "id": "5", "action": "navigate", "url": format!("{}/third", base_url) }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "6", "action": "evaluate", "script": "JSON.parse(document.body.innerText)" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"]["headers"]["X-Version"], "v2");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// `set headers` (global) and `--headers` (origin-scoped) stack together.
#[tokio::test]
#[ignore]
async fn e2e_global_and_scoped_headers_stack() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set global headers via `set headers`.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "headers",
            "headers": { "X-Global": "everywhere" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Set origin-scoped headers via --headers.
    let resp = execute_command(
        &json!({
            "id": "3", "action": "navigate",
            "url": format!("{}/page", base_url),
            "headers": { "X-Scoped": "this-origin" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "4", "action": "evaluate", "script": "JSON.parse(document.body.innerText)" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let headers = &get_data(&resp)["result"]["headers"];
    assert_eq!(
        headers["X-Global"], "everywhere",
        "Global header should be present alongside scoped header"
    );
    assert_eq!(
        headers["X-Scoped"], "this-origin",
        "Scoped header should be present alongside global header"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Origin-scoped headers with different casing than the browser's original
/// request headers must not produce duplicates (HTTP headers are
/// case-insensitive per RFC 7230).
#[tokio::test]
#[ignore]
async fn e2e_headers_case_insensitive_no_duplicates() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Chrome sends "Accept: ..." by default on navigations. Pass "accept"
    // (lowercase) via --headers to verify the merge is case-insensitive
    // and doesn't produce a duplicate Accept header.
    let resp = execute_command(
        &json!({
            "id": "2", "action": "navigate",
            "url": format!("{}/page", base_url),
            "headers": { "accept": "application/test" },
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3", "action": "evaluate",
            "script": "JSON.parse(document.body.innerText)",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let result = &get_data(&resp)["result"]["headers"];

    // The echo server stores headers keyed by name as received on the wire.
    // If deduplication works, only our custom "accept" value should appear
    // (Chrome's original "Accept: text/html,..." should be suppressed).
    let accept_val = result
        .get("accept")
        .or_else(|| result.get("Accept"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        accept_val, "application/test",
        "Case-insensitive merge should replace Chrome's Accept header, got headers: {}",
        result,
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Regression: externally opened tabs must appear in tab_list (#1037)
//
// When connected to Chrome (launched or via --cdp), a tab opened outside of
// agent-browser (e.g. by the user or another CDP client) should be detected
// and listed. Previously, chrome://newtab/ was filtered by
// is_internal_chrome_target, and Target.targetInfoChanged for untracked
// targets was silently ignored.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_externally_opened_tab_detected() {
    let mut state = DaemonState::new();

    // Launch headless Chrome
    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Verify initial tab count
    let resp = execute_command(&json!({ "id": "2", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let initial_count = get_data(&resp)["tabs"].as_array().unwrap().len();

    // Simulate an external client opening a new tab via the browser-level CDP
    // session (no sessionId). This mirrors what happens when a user manually
    // opens a tab while agent-browser is connected via --cdp.
    let browser = state.browser.as_ref().expect("browser should be launched");
    let _: Value = browser
        .client
        .send_command(
            "Target.createTarget",
            Some(json!({ "url": "data:text/html,<h1>External Tab</h1>" })),
            None, // browser-level session
        )
        .await
        .expect("Target.createTarget should succeed");

    // Give Chrome a moment to fire targetCreated / targetInfoChanged events
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Drain events by issuing tab_list — this triggers execute_command's
    // drain_cdp_events path which processes new and changed targets.
    let resp = execute_command(&json!({ "id": "3", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();

    assert_eq!(
        tabs.len(),
        initial_count + 1,
        "Externally opened tab should appear in tab_list, got: {:?}",
        tabs,
    );

    // Verify the new tab's URL is the data URL we navigated to
    let new_tab = tabs.iter().find(|t| {
        t["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("data:text/html"))
    });
    assert!(
        new_tab.is_some(),
        "Should find the externally opened tab by URL, tabs: {:?}",
        tabs,
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Regression: issue #993 — launch options change must trigger relaunch
// ---------------------------------------------------------------------------

/// When the browser is already running and a second launch command arrives with
/// different options (e.g., extensions added), the daemon must relaunch the
/// browser instead of silently reusing the old one.
///
/// Before the fix, `handle_launch` only checked connection type and liveness,
/// so changed options like extensions were ignored and the old browser was reused.
#[tokio::test]
#[ignore]
async fn e2e_relaunch_on_options_change() {
    let mut state = DaemonState::new();

    // First launch — headless, no extensions.
    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["launched"], true);
    assert!(
        get_data(&resp).get("reused").is_none(),
        "first launch must not be a reuse"
    );

    // Second launch — same options → should reuse.
    let resp = execute_command(
        &json!({ "id": "2", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["reused"],
        true,
        "identical options must reuse the browser"
    );

    // Third launch — different options (userAgent changed) → must relaunch, not reuse.
    // We use userAgent instead of extensions because extensions force headed mode,
    // which requires a display server and fails in headless CI environments.
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "launch",
            "headless": true,
            "userAgent": "agent-browser-test/1.0"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(
        get_data(&resp).get("reused").is_none(),
        "changed options must trigger a relaunch, not reuse (issue #993)"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Stream: URL events follow active main-frame navigation
// ---------------------------------------------------------------------------

async fn start_stream_navigation_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stream navigation server should bind");
    let port = listener
        .local_addr()
        .expect("stream navigation server should have an address")
        .port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let Ok(size) = stream.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..size]);
                let request_target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let path = request_target.split('?').next().unwrap_or("/");
                if let Some(destination) = path.strip_prefix("/redirect/") {
                    let location = format!("/landed/{destination}");
                    let response = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    return;
                }
                let body = match path {
                    "/child" => {
                        "<!doctype html><title>child</title><p id=\"child\">child</p>"
                    }
                    _ => {
                        "<!doctype html><title>main</title><a id=\"anchor\" href=\"#section\">anchor</a><div id=\"section\">section</div><iframe id=\"child\" src=\"/child\"></iframe>"
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

async fn next_stream_url(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> String {
    loop {
        let message = tokio::time::timeout(tokio::time::Duration::from_secs(5), ws.next())
            .await
            .expect("stream should emit a URL message")
            .expect("stream should stay open")
            .expect("stream message should be valid");
        if !message.is_text() {
            continue;
        }
        let payload: Value =
            serde_json::from_str(message.to_text().expect("message should be text"))
                .expect("stream payload should be JSON");
        if payload["type"] == "url" {
            return payload["url"]
                .as_str()
                .expect("URL message should carry a URL")
                .to_string();
        }
    }
}

async fn wait_for_stream_navigation_ready(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    loop {
        let message = tokio::time::timeout(tokio::time::Duration::from_secs(10), ws.next())
            .await
            .expect("stream navigation observer should become ready")
            .expect("stream should stay open")
            .expect("stream message should be valid");
        if !message.is_text() {
            continue;
        }
        let payload: Value =
            serde_json::from_str(message.to_text().expect("message should be text"))
                .expect("stream payload should be JSON");
        if payload["type"] == "status"
            && payload["connected"] == true
            && payload["screencasting"] == true
        {
            return;
        }
    }
}

async fn expect_no_stream_url(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    duration: tokio::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let result = tokio::time::timeout(remaining, ws.next()).await;
        let Ok(Some(Ok(message))) = result else {
            return;
        };
        if !message.is_text() {
            continue;
        }
        let payload: Value =
            serde_json::from_str(message.to_text().expect("message should be text"))
                .expect("stream payload should be JSON");
        assert_ne!(
            payload["type"], "url",
            "unexpected background or child URL: {payload}"
        );
    }
}

async fn next_collected_stream_message(
    messages: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    iteration: usize,
    phase: &str,
) -> Value {
    tokio::time::timeout(tokio::time::Duration::from_secs(5), messages.recv())
        .await
        .unwrap_or_else(|_| panic!("stress iteration {iteration} timed out during {phase}"))
        .unwrap_or_else(|| {
            panic!("stress iteration {iteration} stream collector stopped during {phase}")
        })
}

async fn wait_for_active_stream_tab(
    messages: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    tab_id: &str,
    iteration: usize,
) {
    loop {
        let payload =
            next_collected_stream_message(messages, iteration, "active-tab stream rebind").await;
        if payload["type"] != "tabs" {
            continue;
        }
        let is_active = payload["tabs"].as_array().is_some_and(|tabs| {
            tabs.iter()
                .any(|tab| tab["tabId"] == tab_id && tab["active"].as_bool() == Some(true))
        });
        if is_active {
            return;
        }
    }
}

async fn wait_for_stress_stream_url(
    messages: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    expected_url: &str,
    forbidden_marker: &str,
    iteration: usize,
) {
    loop {
        let payload =
            next_collected_stream_message(messages, iteration, "active URL convergence").await;
        if payload["type"] != "url" {
            continue;
        }
        let url = payload["url"]
            .as_str()
            .expect("stress URL payload should contain a URL");
        assert!(
            !url.contains(forbidden_marker),
            "stress iteration {iteration} attributed the previous tab URL to the new active tab: {payload}"
        );
        if url == expected_url {
            return;
        }
    }
}

async fn expect_no_forbidden_stream_url(
    messages: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    forbidden_marker: &str,
    iteration: usize,
) {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(100);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(payload)) = tokio::time::timeout(remaining, messages.recv()).await else {
            return;
        };
        if payload["type"] != "url" {
            continue;
        }
        let url = payload["url"]
            .as_str()
            .expect("stress URL payload should contain a URL");
        assert!(
            !url.contains(forbidden_marker),
            "stress iteration {iteration} emitted a forbidden URL after convergence: {payload}"
        );
    }
}

async fn seeded_stream_tabs(port: u64, iteration: usize) -> Vec<Value> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("stress probe should connect to runtime stream");
    loop {
        let message = tokio::time::timeout(tokio::time::Duration::from_secs(5), ws.next())
            .await
            .unwrap_or_else(|_| {
                panic!("stress iteration {iteration} probe timed out waiting for tabs")
            })
            .expect("stress probe stream should stay open")
            .expect("stress probe message should be valid");
        if !message.is_text() {
            continue;
        }
        let payload: Value =
            serde_json::from_str(message.to_text().expect("probe message should be text"))
                .expect("probe stream payload should be JSON");
        if payload["type"] == "tabs" {
            return payload["tabs"]
                .as_array()
                .expect("probe tabs payload should be an array")
                .clone();
        }
    }
}

#[tokio::test]
#[ignore]
async fn e2e_stream_url_tracks_active_main_frame_navigation_categories() {
    let guard = EnvGuard::new(&["AGENT_BROWSER_SOCKET_DIR", "AGENT_BROWSER_SESSION"]);
    let socket_dir = std::env::temp_dir().join(format!(
        "agent-browser-e2e-stream-url-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&socket_dir).expect("socket dir should be created");
    guard.set(
        "AGENT_BROWSER_SOCKET_DIR",
        socket_dir.to_str().expect("socket dir should be utf-8"),
    );
    guard.set("AGENT_BROWSER_SESSION", "e2e-stream-url");

    let (base_url, server) = start_stream_navigation_server().await;
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({ "id": "1", "action": "stream_enable", "port": 0 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let port = get_data(&resp)["port"]
        .as_u64()
        .expect("stream enable should report the bound port");

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": format!("{base_url}/") }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("websocket client should connect to runtime stream");
    wait_for_stream_navigation_ready(&mut ws).await;

    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "evaluate",
            "script": "history.pushState({}, '', '/spa'); location.href"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(next_stream_url(&mut ws).await, format!("{base_url}/spa"));

    let resp = execute_command(
        &json!({
            "id": "4",
            "action": "evaluate",
            "script": "location.hash = 'section'; location.href"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        next_stream_url(&mut ws).await,
        format!("{base_url}/spa#section")
    );

    let resp = execute_command(
        &json!({
            "id": "5",
            "action": "evaluate",
            "script": "document.querySelector('#child').contentWindow.history.pushState({}, '', '/child-spa'); 'done'"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    expect_no_stream_url(&mut ws, tokio::time::Duration::from_millis(500)).await;

    let resp = execute_command(
        &json!({ "id": "6", "action": "navigate", "url": format!("{base_url}/full") }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(next_stream_url(&mut ws).await, format!("{base_url}/full"));

    let resp = execute_command(
        &json!({
            "id": "7",
            "action": "tab_new",
            "url": format!("{base_url}/")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let background_session = state
        .browser
        .as_ref()
        .expect("browser should exist")
        .pages_list()
        .into_iter()
        .find(|page| page.tab_id == 2)
        .expect("background tab should exist")
        .session_id;
    let resp = execute_command(
        &json!({ "id": "8", "action": "tab_switch", "tabId": "t1" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    while tokio::time::timeout(tokio::time::Duration::from_millis(100), ws.next())
        .await
        .is_ok()
    {}

    let resp = execute_command(
        &json!({
            "id": "9",
            "action": "evaluate",
            "script": "history.pushState({}, '', '/after-switch'); location.href"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        next_stream_url(&mut ws).await,
        format!("{base_url}/after-switch")
    );

    state
        .browser
        .as_ref()
        .expect("browser should exist")
        .client
        .send_command(
            "Page.navigate",
            Some(json!({ "url": format!("{base_url}/background-full") })),
            Some(&background_session),
        )
        .await
        .expect("background tab should navigate");
    expect_no_stream_url(&mut ws, tokio::time::Duration::from_millis(500)).await;

    state
        .browser
        .as_ref()
        .expect("browser should exist")
        .client
        .send_command(
            "Target.createTarget",
            Some(json!({ "url": format!("{base_url}/external") })),
            None,
        )
        .await
        .expect("external tab should open");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let resp = execute_command(&json!({ "id": "10", "action": "url" }), &mut state).await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["url"], format!("{base_url}/external"));

    let external_session = state
        .browser
        .as_ref()
        .expect("browser should exist")
        .active_session_id()
        .expect("external tab should become active")
        .to_string();
    state
        .browser
        .as_ref()
        .expect("browser should exist")
        .client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": "history.pushState({}, '', '/external-spa'); location.href"
            })),
            Some(&external_session),
        )
        .await
        .expect("external tab should navigate within its document");
    assert_eq!(
        next_stream_url(&mut ws).await,
        format!("{base_url}/external-spa")
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    server.abort();
    let _ = std::fs::remove_dir_all(&socket_dir);
}

#[tokio::test]
#[ignore]
async fn e2e_lightpanda_stream_url_tracks_active_full_navigation() {
    let lightpanda_bin = match std::env::var("LIGHTPANDA_BIN") {
        Ok(path) if !path.is_empty() => path,
        _ => return,
    };
    let guard = EnvGuard::new(&["AGENT_BROWSER_SOCKET_DIR", "AGENT_BROWSER_SESSION"]);
    let socket_dir = std::env::temp_dir().join(format!(
        "agent-browser-e2e-lightpanda-stream-url-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&socket_dir).expect("socket dir should be created");
    guard.set(
        "AGENT_BROWSER_SOCKET_DIR",
        socket_dir.to_str().expect("socket dir should be utf-8"),
    );
    guard.set("AGENT_BROWSER_SESSION", "e2e-lightpanda-stream-url");

    let (base_url, server) = start_stream_navigation_server().await;
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({ "id": "lp-stream", "action": "stream_enable", "port": 0 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let port = get_data(&resp)["port"]
        .as_u64()
        .expect("stream enable should report the bound port");

    let resp = tokio::time::timeout(
        tokio::time::Duration::from_secs(20),
        execute_command(
            &json!({
                "id": "lp-launch",
                "action": "launch",
                "headless": true,
                "engine": "lightpanda",
                "executablePath": lightpanda_bin
            }),
            &mut state,
        ),
    )
    .await
    .expect("Lightpanda stream launch should not hang");
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "lp-initial",
            "action": "navigate",
            "url": format!("{base_url}/lp-initial")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("Lightpanda stream client should connect");
    while tokio::time::timeout(tokio::time::Duration::from_millis(100), ws.next())
        .await
        .is_ok()
    {}

    let active_before_background = format!("{base_url}/lp-active-before-background");
    let resp = execute_command(
        &json!({
            "id": "lp-active-before",
            "action": "navigate",
            "url": active_before_background
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(next_stream_url(&mut ws).await, active_before_background);

    let resp = execute_command(
        &json!({
            "id": "lp-child-navigation",
            "action": "evaluate",
            "script": "document.querySelector('#child').src = '/lp-forbidden-child'; 'done'"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    expect_no_stream_url(&mut ws, tokio::time::Duration::from_millis(500)).await;

    let tabs = seeded_stream_tabs(port, 0).await;
    let active = tabs
        .iter()
        .find(|tab| tab["active"].as_bool() == Some(true))
        .expect("Lightpanda reconnect should seed an active tab");
    assert_eq!(active["tabId"], "t1");
    assert_eq!(active["url"], active_before_background);

    let final_url = format!("{base_url}/lp-active-final");
    let resp = execute_command(
        &json!({
            "id": "lp-active-final",
            "action": "navigate",
            "url": final_url
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(next_stream_url(&mut ws).await, final_url);

    let resp = execute_command(&json!({ "id": "lp-close", "action": "close" }), &mut state).await;
    assert_success(&resp);
    server.abort();
    let _ = std::fs::remove_dir_all(&socket_dir);
}

#[tokio::test]
#[ignore]
async fn e2e_stream_url_survives_real_world_navigation_stress() {
    let guard = EnvGuard::new(&["AGENT_BROWSER_SOCKET_DIR", "AGENT_BROWSER_SESSION"]);
    let socket_dir = std::env::temp_dir().join(format!(
        "agent-browser-e2e-stream-url-stress-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&socket_dir).expect("socket dir should be created");
    guard.set(
        "AGENT_BROWSER_SOCKET_DIR",
        socket_dir.to_str().expect("socket dir should be utf-8"),
    );
    guard.set("AGENT_BROWSER_SESSION", "e2e-stream-url-stress");

    let iterations = std::env::var("AGENT_BROWSER_STRESS_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(40);
    let mut seed = std::env::var("AGENT_BROWSER_STRESS_SEED")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1677);

    let (base_url, server) = start_stream_navigation_server().await;
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({ "id": "stress-stream", "action": "stream_enable", "port": 0 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let port = get_data(&resp)["port"]
        .as_u64()
        .expect("stream enable should report the bound port");

    let resp = execute_command(
        &json!({
            "id": "stress-tab-a",
            "action": "navigate",
            "url": format!("{base_url}/app-a")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({
            "id": "stress-tab-b",
            "action": "tab_new",
            "url": format!("{base_url}/app-b")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let pages = state
        .browser
        .as_ref()
        .expect("stress browser should exist")
        .pages_list();
    let session_a = pages
        .iter()
        .find(|page| page.tab_id == 1)
        .expect("stress tab A should exist")
        .session_id
        .clone();
    let session_b = pages
        .iter()
        .find(|page| page.tab_id == 2)
        .expect("stress tab B should exist")
        .session_id
        .clone();
    let client = Arc::clone(
        &state
            .browser
            .as_ref()
            .expect("stress browser should exist")
            .client,
    );

    let (fast_ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("stress collector should connect to runtime stream");
    let (message_tx, mut messages) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let collector = tokio::spawn(async move {
        let mut ws = fast_ws;
        while let Some(Ok(message)) = ws.next().await {
            if !message.is_text() {
                continue;
            }
            let Ok(payload) = serde_json::from_str::<Value>(
                message.to_text().expect("collector message should be text"),
            ) else {
                continue;
            };
            if message_tx.send(payload).is_err() {
                break;
            }
        }
    });
    wait_for_active_stream_tab(&mut messages, "t2", 0).await;

    let (mut slow_ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("slow stress client should connect to runtime stream");

    let mut active_tab = "t2";
    for iteration in 0..iterations {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let delay_ms = 1 + (seed % 12);
        let target_tab = if active_tab == "t1" { "t2" } else { "t1" };
        let old_session = if active_tab == "t1" {
            session_a.clone()
        } else {
            session_b.clone()
        };
        let forbidden_marker = format!("forbidden-{iteration}");
        let same_document_path = format!("/{forbidden_marker}-old-spa");
        let redirect_url = format!("{base_url}/redirect/{forbidden_marker}-old-full");

        let late_client = Arc::clone(&client);
        let late_session = old_session.clone();
        let late_same_document_path = same_document_path.clone();
        let late_child_marker = forbidden_marker.clone();
        let late_same_document = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            let expression = format!(
                "history.pushState({{}}, '', {}); const child = document.querySelector('#child'); if (child?.contentWindow) child.contentWindow.history.pushState({{}}, '', '/{late_child_marker}-old-child'); location.href",
                serde_json::to_string(&late_same_document_path)
                    .expect("stress path should serialize")
            );
            late_client
                .send_command(
                    "Runtime.evaluate",
                    Some(json!({ "expression": expression })),
                    Some(&late_session),
                )
                .await
        });

        let redirect_client = Arc::clone(&client);
        let redirect_session = old_session.clone();
        let late_redirect = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms + 1)).await;
            redirect_client
                .send_command(
                    "Page.navigate",
                    Some(json!({ "url": redirect_url })),
                    Some(&redirect_session),
                )
                .await
        });

        let resp = execute_command(
            &json!({
                "id": format!("stress-switch-{iteration}"),
                "action": "tab_switch",
                "tabId": target_tab
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        wait_for_active_stream_tab(&mut messages, target_tab, iteration).await;
        tokio::time::timeout(tokio::time::Duration::from_secs(5), late_same_document)
            .await
            .unwrap_or_else(|_| panic!("stress iteration {iteration} late SPA task timed out"))
            .expect("late SPA task should join")
            .expect("late SPA CDP command should succeed");
        tokio::time::timeout(tokio::time::Duration::from_secs(5), late_redirect)
            .await
            .unwrap_or_else(|_| panic!("stress iteration {iteration} late redirect task timed out"))
            .expect("late redirect task should join")
            .expect("late redirect CDP command should succeed");

        let resp = execute_command(
            &json!({
                "id": format!("stress-child-{iteration}"),
                "action": "evaluate",
                "script": format!(
                    "const child = document.querySelector('#child'); if (!child?.contentWindow) throw new Error('missing stress iframe'); child.contentWindow.history.pushState({{}}, '', '/{forbidden_marker}-active-child'); 'done'"
                )
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let expected_url = format!("{base_url}/{target_tab}-active-{iteration}");
        let resp = execute_command(
            &json!({
                "id": format!("stress-active-{iteration}"),
                "action": "evaluate",
                "script": format!(
                    "history.replaceState({{}}, '', {}); location.href",
                    serde_json::to_string(&expected_url).expect("stress URL should serialize")
                )
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        wait_for_stress_stream_url(&mut messages, &expected_url, &forbidden_marker, iteration)
            .await;

        let resp = execute_command(
            &json!({ "id": format!("stress-url-{iteration}"), "action": "url" }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert_eq!(
            get_data(&resp)["url"],
            expected_url,
            "stress iteration {iteration} active browser URL diverged"
        );
        expect_no_forbidden_stream_url(&mut messages, &forbidden_marker, iteration).await;

        if iteration % 5 == 0 {
            let tabs = seeded_stream_tabs(port, iteration).await;
            let active = tabs
                .iter()
                .find(|tab| tab["active"].as_bool() == Some(true))
                .unwrap_or_else(|| {
                    panic!("stress iteration {iteration} reconnect had no active tab")
                });
            assert_eq!(
                active["tabId"], target_tab,
                "stress iteration {iteration} reconnect seeded the wrong active tab"
            );
            assert_eq!(
                active["url"], expected_url,
                "stress iteration {iteration} reconnect seeded a stale URL"
            );
        }

        if iteration % 7 == 6 {
            drop(slow_ws);
            let (replacement, _) =
                tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
                    .await
                    .expect("replacement slow stress client should connect");
            slow_ws = replacement;
        }

        active_tab = target_tab;
    }

    drop(slow_ws);
    collector.abort();
    let resp = execute_command(
        &json!({ "id": "stress-close", "action": "close" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    server.abort();
    let _ = std::fs::remove_dir_all(&socket_dir);
}

// ---------------------------------------------------------------------------
// Stream: custom viewport is reflected in screencast frame metadata
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_stream_frame_metadata_respects_custom_viewport() {
    let guard = EnvGuard::new(&["AGENT_BROWSER_SOCKET_DIR", "AGENT_BROWSER_SESSION"]);
    let socket_dir = std::env::temp_dir().join(format!(
        "agent-browser-e2e-stream-viewport-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&socket_dir).expect("socket dir should be created");
    guard.set(
        "AGENT_BROWSER_SOCKET_DIR",
        socket_dir.to_str().expect("socket dir should be utf-8"),
    );
    guard.set("AGENT_BROWSER_SESSION", "e2e-stream-viewport");

    let mut state = DaemonState::new();

    // Enable stream on an ephemeral port
    let resp = execute_command(
        &json!({ "id": "1", "action": "stream_enable", "port": 0 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let port = get_data(&resp)["port"]
        .as_u64()
        .expect("stream enable should report the bound port");

    // Set a custom viewport before launching the browser
    let resp = execute_command(
        &json!({ "id": "2", "action": "viewport", "width": 800, "height": 600 }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Connect a WebSocket client
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("websocket client should connect to runtime stream");

    // Navigate to trigger browser launch and screencast
    let resp = execute_command(
        &json!({ "id": "3", "action": "navigate", "url": "data:text/html,<h1>Viewport Test</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Wait for a frame whose JPEG dimensions match the custom viewport.
    // Early frames may arrive before Chrome fully applies the viewport resize,
    // so skip frames with stale dimensions rather than failing immediately.
    let mut found_frame = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(3), ws.next()).await;
        let Some(Ok(message)) = msg.ok().flatten() else {
            continue;
        };
        if !message.is_text() {
            continue;
        }
        let parsed: Value =
            serde_json::from_str(message.to_text().expect("text message should be readable"))
                .expect("stream payload should be valid JSON");
        if parsed.get("type") == Some(&json!("frame")) {
            let meta = &parsed["metadata"];
            assert_eq!(
                meta["deviceWidth"], 800,
                "frame metadata deviceWidth should match custom viewport, got: {}",
                meta
            );
            assert_eq!(
                meta["deviceHeight"], 600,
                "frame metadata deviceHeight should match custom viewport, got: {}",
                meta
            );

            let data_str = parsed
                .get("data")
                .and_then(|v| v.as_str())
                .expect("frame message should include base64-encoded 'data' field");
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_str)
                .expect("frame data should be valid base64");
            let (img_w, img_h) =
                jpeg_dimensions(&bytes).expect("frame data should be a valid JPEG with SOF marker");
            if img_w != 800 || img_h != 600 {
                continue;
            }

            found_frame = true;
            break;
        }
    }
    assert!(
        found_frame,
        "should have received a frame with JPEG dimensions 800x600 within the deadline"
    );

    // Cleanup
    let resp = execute_command(
        &json!({ "id": "4", "action": "stream_disable" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    let _ = std::fs::remove_dir_all(&socket_dir);
}

// ---------------------------------------------------------------------------
// Stream: a click is not queued behind a mouse sweep
// ---------------------------------------------------------------------------

/// Guards the no-await dispatch. Awaiting Chrome's reply per event serialized
/// the reader, so a click arrived one CDP round trip behind every mousemove
/// ahead of it: 300 moves delayed it by ~2.5s. Measured at ~6ms with the fix,
/// so the threshold has three orders of magnitude of headroom and fails only on
/// a real regression.
#[tokio::test]
#[ignore]
async fn e2e_stream_click_is_not_queued_behind_a_mouse_sweep() {
    let guard = EnvGuard::new(&["AGENT_BROWSER_SOCKET_DIR", "AGENT_BROWSER_SESSION"]);
    let socket_dir = std::env::temp_dir().join(format!(
        "agent-browser-e2e-stream-latency-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&socket_dir).expect("socket dir should be created");
    guard.set(
        "AGENT_BROWSER_SOCKET_DIR",
        socket_dir.to_str().expect("socket dir should be utf-8"),
    );
    guard.set("AGENT_BROWSER_SESSION", "e2e-stream-latency");

    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "stream_enable", "port": 0 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let port = get_data(&resp)["port"]
        .as_u64()
        .expect("stream enable should report the bound port");

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<h1>latency</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Record the arrival time of the first mousedown inside the page.
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "evaluate",
            "script": "window.__down = null; document.addEventListener('mousedown', () => { if (window.__down === null) window.__down = Date.now(); }); 'armed'"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("websocket client should connect to runtime stream");
    let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), ws.next()).await;

    // A sweep of moves, then the click that must not wait for them.
    for i in 0..300 {
        let mv = json!({
            "type": "input_mouse", "eventType": "mouseMoved",
            "x": 100 + (i % 400), "y": 100 + (i % 300),
            "button": "none", "clickCount": 0
        });
        ws.send(Message::Text(mv.to_string()))
            .await
            .expect("mouse move should be accepted by the stream socket");
    }
    let sent_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_millis() as u64;
    for event in ["mousePressed", "mouseReleased"] {
        let click = json!({
            "type": "input_mouse", "eventType": event,
            "x": 200, "y": 200, "button": "left", "clickCount": 1
        });
        ws.send(Message::Text(click.to_string()))
            .await
            .expect("click should be accepted by the stream socket");
    }

    // Poll the page for the recorded arrival time.
    let mut latency_ms: Option<u64> = None;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && latency_ms.is_none() {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let resp = execute_command(
            &json!({ "id": "4", "action": "evaluate", "script": "window.__down" }),
            &mut state,
        )
        .await;
        if let Some(down) = get_data(&resp)["result"].as_u64() {
            latency_ms = Some(down.saturating_sub(sent_at_ms));
        }
    }

    let latency = latency_ms.expect("the click should reach the page within the deadline");
    assert!(
        latency < 500,
        "click landed {latency}ms after a 300-event mouse sweep; input dispatch is waiting on CDP replies again"
    );

    let resp = execute_command(
        &json!({ "id": "98", "action": "stream_disable" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
    let _ = std::fs::remove_dir_all(&socket_dir);
}

/// Extract width and height from a JPEG's SOF0 (0xFFC0) or SOF2 (0xFFC2) marker.
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    for i in 0..data.len().saturating_sub(8) {
        if data[i] == 0xFF && (data[i + 1] == 0xC0 || data[i + 1] == 0xC2) {
            let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((width, height));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Upload: ref-based selector support (issue #1107)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_upload_with_ref_selector() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": native_test_fixture_url("upload_probe") }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "3", "action": "snapshot" }), &mut state).await;
    assert_success(&resp);
    let snapshot = get_data(&resp)["snapshot"].as_str().unwrap();

    // Match by label text, not by role which may vary across Chrome versions
    let file_input_ref = snapshot
        .lines()
        .filter_map(|line| {
            if line.contains("Choose file") && line.contains("ref=") {
                let start = line.find("ref=")? + 4;
                let end = line[start..].find(']')? + start;
                Some(line[start..end].to_string())
            } else {
                None
            }
        })
        .next()
        .expect("Snapshot should contain the file input with a ref");

    let tmp = std::env::temp_dir().join(format!("ab-upload-ref-{}.txt", std::process::id()));
    std::fs::write(&tmp, "test").unwrap();

    let resp = execute_command(
        &json!({ "id": "4", "action": "upload", "selector": file_input_ref, "files": [tmp.to_string_lossy()] }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["uploaded"], 1);

    let _ = std::fs::remove_file(&tmp);
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_upload_with_css_selector() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": native_test_fixture_url("upload_probe") }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let tmp = std::env::temp_dir().join(format!("ab-upload-css-{}.txt", std::process::id()));
    std::fs::write(&tmp, "test").unwrap();

    let resp = execute_command(
        &json!({ "id": "3", "action": "upload", "selector": "#fileInput", "files": [tmp.to_string_lossy()] }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["uploaded"], 1);

    let _ = std::fs::remove_file(&tmp);
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Recording: default records the current active page
// ---------------------------------------------------------------------------

/// `recording_start` attaches the recorder to the active page
/// as-is: no new browser context, no new tab, no navigation. Page state set
/// before `record start` must survive, and the viewport must be untouched.
#[tokio::test]
#[ignore]
async fn e2e_recording_default_records_active_page() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let page_url = "data:text/html,<h1>Current</h1>";
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": page_url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "viewport", "width": 800, "height": 600 }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // In-memory page state: a cold navigation or a new page would lose this.
    let resp = execute_command(
        &json!({ "id": "4", "action": "evaluate", "script": "window.__abMarker = 42; window.__abMarker" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], 42);

    let tmp_dir = std::env::temp_dir();
    let rec_path = tmp_dir.join(format!("ab-e2e-rec-current-{}.webm", std::process::id()));
    let resp = execute_command(
        &json!({ "id": "5", "action": "recording_start", "path": rec_path.to_string_lossy() }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    tokio::time::sleep(tokio::time::Duration::from_millis(700)).await;

    // Still exactly one tab: no recording tab was added.
    let resp = execute_command(&json!({ "id": "6", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();
    assert_eq!(
        tabs.len(),
        1,
        "default record start must not open a new tab, got {tabs:?}"
    );

    // Same page, same JS heap: the marker survived and the URL is unchanged.
    let resp = execute_command(
        &json!({ "id": "7", "action": "evaluate", "script": "window.__abMarker" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["result"],
        42,
        "default record start must not navigate or replace the page"
    );

    let resp = execute_command(
        &json!({ "id": "8", "action": "evaluate", "script": "location.href" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], page_url);

    let resp = execute_command(
        &json!({ "id": "9", "action": "evaluate", "script": "[window.innerWidth, window.innerHeight]" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], json!([800, 600]));

    let resp = execute_command(
        &json!({ "id": "10", "action": "recording_stop" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(
        get_data(&resp)["frames"].as_u64().unwrap_or(0) > 0,
        "recorder attached to the active page should have captured frames"
    );

    let _ = std::fs::remove_file(&rec_path);
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// `recording_start` with a URL navigates the active
/// tab to that URL before recording. No new tab is created.
#[tokio::test]
#[ignore]
async fn e2e_recording_default_with_url_navigates_active_tab() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": "data:text/html,<button id='before'>Before</button>"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Refs from the page being replaced must not survive the navigation.
    let resp = execute_command(
        &json!({ "id": "2b", "action": "snapshot", "selector": "#before" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(state.ref_map.get("e1").is_some());

    let target_url = "data:text/html,<h1>Recorded</h1>";
    let tmp_dir = std::env::temp_dir();
    let rec_path = tmp_dir.join(format!("ab-e2e-rec-url-{}.webm", std::process::id()));
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "recording_start",
            "path": rec_path.to_string_lossy(),
            "url": target_url
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(
        state.ref_map.entries_sorted().is_empty(),
        "record start <url> must clear refs like navigate does"
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let resp = execute_command(&json!({ "id": "4", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap();
    assert_eq!(
        tabs.len(),
        1,
        "record start <url> must reuse the active tab"
    );

    let resp = execute_command(
        &json!({ "id": "5", "action": "evaluate", "script": "location.href" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], target_url);

    let resp = execute_command(
        &json!({ "id": "6", "action": "recording_stop" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let _ = std::fs::remove_file(&rec_path);
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// Recording: requested frame rate
// ---------------------------------------------------------------------------

/// Verify that a requested frame rate sets the timestamp resolution without
/// filling a static recording with duplicate encoded frames.
#[tokio::test]
#[ignore]
async fn e2e_recording_honors_requested_fps() {
    const FPS: u64 = 60;
    const RECORD_MS: u64 = 1000;

    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "data:text/html,<h1>Frame rate</h1>" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let tmp_dir = std::env::temp_dir();
    let rec_path = tmp_dir.join(format!("ab-e2e-rec-fps-{}.webm", std::process::id()));
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "recording_start",
            "path": rec_path.to_string_lossy(),
            "fps": FPS,
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["fps"].as_u64(), Some(FPS));

    // The recorder screencasts on its own CDP session attached to the
    // active page. That attachment must not surface as a tab.
    let resp = execute_command(&json!({ "id": "3b", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap().len();
    assert_eq!(tabs, 1, "the recorded page only, nothing else");

    tokio::time::sleep(tokio::time::Duration::from_millis(RECORD_MS)).await;

    let resp = execute_command(
        &json!({ "id": "4", "action": "recording_stop" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let data = get_data(&resp);
    assert_eq!(data["fps"].as_u64(), Some(FPS));

    let frames = data["frames"].as_u64().unwrap();
    assert!(
        (FPS * 8 / 10..FPS * 15 / 10).contains(&frames),
        "static page should repeat frames at {FPS} fps, got {frames}"
    );
    let captured = data["capturedFrames"].as_u64().unwrap();
    assert!(captured >= 1, "static page should produce an initial frame");

    let size = std::fs::metadata(&rec_path).map(|m| m.len()).unwrap_or(0);
    assert!(size > 0, "recording file should not be empty");
    let probe = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(&rec_path)
        .output()
        .expect("ffprobe should inspect the recording");
    assert!(probe.status.success());
    let duration: f64 = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse()
        .expect("ffprobe duration should be numeric");
    assert!(
        (0.8..1.5).contains(&duration),
        "recording should retain wall-clock duration, got {duration}"
    );
    assert!(
        (duration - frames as f64 / FPS as f64).abs() < 0.03,
        "{frames} frames at {FPS} fps should match duration {duration}"
    );

    let _ = std::fs::remove_file(&rec_path);
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Verify that an output path without an extension is rejected before the
/// recording context exists: no new tab, no file, nothing to stop.
#[tokio::test]
#[ignore]
async fn e2e_recording_rejects_extensionless_path_before_context() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let rec_path = std::env::temp_dir().join(format!("ab-e2e-rec-noext-{}", std::process::id()));
    let resp = execute_command(
        &json!({ "id": "2", "action": "recording_start", "path": rec_path.to_string_lossy() }),
        &mut state,
    )
    .await;
    assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(false));
    let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        err.contains("no extension"),
        "error should explain the path: {}",
        err
    );
    assert!(!rec_path.exists(), "no file should be created");
    assert!(!state.recording_state.active);

    let resp = execute_command(&json!({ "id": "3", "action": "tab_list" }), &mut state).await;
    assert_success(&resp);
    let tabs = get_data(&resp)["tabs"].as_array().unwrap().len();
    assert_eq!(
        tabs, 1,
        "a rejected path must not leave a recording tab behind"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Verify that a missing ffmpeg fails `recording_start` itself before an
/// optional navigation, and leaves the state ready for the next start.
#[tokio::test]
#[ignore]
async fn e2e_recording_fails_fast_without_ffmpeg() {
    let guard = EnvGuard::new(&["PATH"]);
    let original_path = std::env::var("PATH").unwrap_or_default();
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let before_url = "data:text/html,<h1>Before</h1>";
    let after_url = "data:text/html,<h1>After</h1>";
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": before_url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let tmp_dir = std::env::temp_dir();
    let rec_path = tmp_dir.join(format!("ab-e2e-rec-noffmpeg-{}.webm", std::process::id()));

    // A PATH with no ffmpeg on it. Chrome is already running, so nothing else
    // needs resolving.
    let empty_dir = tmp_dir.join(format!("ab-e2e-empty-path-{}", std::process::id()));
    std::fs::create_dir_all(&empty_dir).unwrap();
    guard.set("PATH", &empty_dir.to_string_lossy());

    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "recording_start",
            "path": rec_path.to_string_lossy(),
            "url": after_url
        }),
        &mut state,
    )
    .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "record start must fail without ffmpeg: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );
    let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(err.contains("ffmpeg"), "error should name ffmpeg: {}", err);
    assert!(
        !state.recording_state.active,
        "failed start must not leave the recording active"
    );
    let resp = execute_command(
        &json!({ "id": "4", "action": "evaluate", "script": "location.href" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["result"],
        before_url,
        "ffmpeg preflight must fail before the requested navigation"
    );

    // With ffmpeg back, the same start succeeds: nothing stale was left behind.
    guard.set("PATH", &original_path);
    let resp = execute_command(
        &json!({
            "id": "5",
            "action": "recording_start",
            "path": rec_path.to_string_lossy(),
            "url": after_url
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let resp = execute_command(
        &json!({ "id": "6", "action": "recording_stop" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let _ = std::fs::remove_file(&rec_path);
    let _ = std::fs::remove_dir(&empty_dir);
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Verify that an out-of-range frame rate is rejected before the recorder
/// builds its context or attaches to the page, leaving no file and no active
/// recording behind.
#[tokio::test]
#[ignore]
async fn e2e_recording_rejects_invalid_fps() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let tmp_dir = std::env::temp_dir();
    let rec_path = tmp_dir.join(format!("ab-e2e-rec-badfps-{}.webm", std::process::id()));
    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "recording_start",
            "path": rec_path.to_string_lossy(),
            "fps": 240,
        }),
        &mut state,
    )
    .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "240 fps should be rejected: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );
    let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(err.contains("fps"), "error should name the field: {}", err);
    assert!(!rec_path.exists(), "no file should be created");

    // Nothing was started, so there is nothing to stop.
    let resp = execute_command(
        &json!({ "id": "3", "action": "recording_stop" }),
        &mut state,
    )
    .await;
    assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(false));

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Verify the composited pointer and changed-frame contact sheet through the
/// full daemon pipeline.
#[tokio::test]
#[ignore]
async fn e2e_recording_cursor_and_contact_sheet() {
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = r#"data:text/html,<style>body{margin:0;background:%23f3f4f6;font-family:sans-serif}.card{margin:80px;padding:48px;background:white;border-radius:24px}button{padding:18px 28px;background:%232563eb;color:white;border:0;border-radius:12px}</style><div class=card><h1>Contact sheet demo</h1><p>Review important visual changes at a glance.</p><button>Continue</button></div>"#;
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": html }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let rec_path = std::env::temp_dir().join(format!(
        "ab-e2e-rec-contact-sheet-{}.webm",
        std::process::id()
    ));
    let sheet_path = rec_path.with_file_name(format!(
        "{}.contact-sheet.png",
        rec_path.file_stem().unwrap().to_string_lossy()
    ));
    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "recording_start",
            "path": rec_path.to_string_lossy(),
            "cursor": true,
            "contactSheet": true,
            "contactSheetThreshold": 0.01
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["cursor"], true);
    assert_eq!(get_data(&resp)["contactSheet"], true);

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    let resp = execute_command(
        &json!({ "id": "4", "action": "evaluate", "script": "Boolean(document.getElementById('__agent_browser_recording_cursor__'))" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], false);

    let resp = execute_command(
        &json!({ "id": "5", "action": "mousemove", "x": 360, "y": 260 }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let cursor = state
        .recording_state
        .shared_cursor
        .lock()
        .unwrap()
        .at(super::recording::cursor_timestamp());
    assert!(cursor.visible);
    assert_eq!((cursor.x, cursor.y), (360.0, 260.0));
    let resp = execute_command(
        &json!({ "id": "6", "action": "evaluate", "script": "document.querySelector('.card').style.background='#dbeafe'; document.querySelector('h1').textContent='Ready to continue'; true" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    tokio::time::sleep(tokio::time::Duration::from_millis(350)).await;
    let resp = execute_command(
        &json!({ "id": "7", "action": "evaluate", "script": "document.querySelector('.card').style.background='#dcfce7'; document.querySelector('p').textContent='The important region is highlighted.'; true" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    tokio::time::sleep(tokio::time::Duration::from_millis(350)).await;

    let resp = execute_command(
        &json!({ "id": "8", "action": "recording_stop" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let data = get_data(&resp);
    assert_eq!(
        data["contactSheetPath"],
        sheet_path.to_string_lossy().as_ref()
    );
    assert!(data["contactSheetFrames"].as_u64().unwrap_or(0) >= 2);
    assert!(std::fs::metadata(&rec_path).unwrap().len() > 0);
    let sheet = image::open(&sheet_path).expect("contact sheet should be a valid PNG");
    assert!(sheet.width() >= 320);
    assert!(sheet.height() >= 150);
    if let Some(example_path) = std::env::var_os("AGENT_BROWSER_CONTACT_SHEET_EXAMPLE_PATH") {
        let example_path = std::path::PathBuf::from(example_path);
        if let Some(parent) = example_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::copy(&sheet_path, example_path).unwrap();
    }

    let resp = execute_command(
        &json!({ "id": "9", "action": "evaluate", "script": "Boolean(document.getElementById('__agent_browser_recording_cursor__'))" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], false);

    let _ = std::fs::remove_file(&rec_path);
    let _ = std::fs::remove_file(&sheet_path);
    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// The cursor must not intercept clicks, leak into snapshots, or survive stop.
#[tokio::test]
#[ignore]
async fn e2e_recording_cursor_overlay_lifecycle() {
    let mut state = DaemonState::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cursor-lifecycle.mp4");
    let url =
        "data:text/html,<button id=keep onclick='window.clicks=(window.clicks||0)+1'>Keep</button>";
    for command in [
        json!({"action":"launch","headless":true}),
        json!({"action":"navigate","url":url}),
        json!({"action":"recording_start","path":path,"cursor":true}),
        json!({"action":"click","selector":"#keep"}),
    ] {
        assert_success(&execute_command(&command, &mut state).await);
    }
    let inspected = execute_command(&json!({"action":"evaluate","script":"(() => { const host = document.querySelector('[data-agent-browser-recording-cursor]'); return !!host && host.inert && host.getAttribute('aria-hidden') === 'true' && host.shadowRoot === null && typeof globalThis.__agentBrowserRecordingCursorCleanup === 'undefined' && window.clicks === 1; })()"}), &mut state).await;
    assert_success(&inspected);
    assert_eq!(get_data(&inspected)["result"], true);
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    assert!(!get_data(&snapshot)
        .to_string()
        .contains("agent-browser-recording-cursor"));
    assert_success(
        &execute_command(
            &json!({"action":"navigate","url":"data:text/html,<button>After navigation</button>"}),
            &mut state,
        )
        .await,
    );
    let after_navigation = execute_command(&json!({"action":"evaluate","script":"document.querySelectorAll('[data-agent-browser-recording-cursor]').length"}), &mut state).await;
    let stopped = execute_command(&json!({"action":"recording_stop"}), &mut state).await;
    let after_stop = execute_command(&json!({"action":"evaluate","script":"document.querySelectorAll('[data-agent-browser-recording-cursor]').length"}), &mut state).await;
    assert_success(&execute_command(&json!({"action":"navigate","url":url}), &mut state).await);
    let after_next_navigation = execute_command(&json!({"action":"evaluate","script":"document.querySelectorAll('[data-agent-browser-recording-cursor]').length"}), &mut state).await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    assert_success(&stopped);
    assert_eq!(get_data(&after_navigation)["result"], 1);
    assert_eq!(get_data(&after_stop)["result"], 0);
    assert_eq!(get_data(&after_next_navigation)["result"], 0);
}

/// Check every encoded frame while a real page control follows drag input.
#[tokio::test]
#[ignore]
async fn e2e_recording_cursor_stays_aligned_during_drag() {
    let mut state = DaemonState::new();
    for command in [
        json!({ "action": "launch", "headless": true }),
        json!({ "action": "viewport", "width": 640, "height": 480 }),
        json!({
            "action": "navigate",
            "url": "data:text/html,<style>body{margin:0;background:%23303030}div{position:fixed;left:80px;top:0;width:2px;height:100vh;background:%2300ff00}</style><div></div><script>document.addEventListener('pointermove',e=>{if(e.buttons)document.querySelector('div').style.left=e.clientX+'px'})</script>"
        }),
        json!({ "action": "mousemove", "x": 80, "y": 240 }),
    ] {
        assert_success(&execute_command(&command, &mut state).await);
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("drag-cursor.mp4");
    assert_success(
        &execute_command(
            &json!({ "action": "recording_start", "path": path, "cursor": true, "fps": 60 }),
            &mut state,
        )
        .await,
    );
    assert_success(&execute_command(&json!({ "action": "mousedown" }), &mut state).await);
    for x in [560, 80, 560] {
        assert_success(
            &execute_command(
                &json!({ "action": "mousemove", "x": x, "y": 240, "duration": 1000, "inputMode": "human", "seed": 42 }),
                &mut state,
            )
            .await,
        );
    }
    assert_success(&execute_command(&json!({ "action": "mouseup" }), &mut state).await);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_success(&execute_command(&json!({ "action": "recording_stop" }), &mut state).await);
    assert_success(&execute_command(&json!({ "action": "close" }), &mut state).await);

    let output = tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args([
            "-vf",
            "crop=640:80:0:200",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let mut measured = 0;
    let mut worst = 0;
    let mut positions = std::collections::HashSet::new();
    for bytes in output.stdout.chunks_exact(640 * 80 * 3) {
        let frame = image::RgbImage::from_raw(640, 80, bytes.to_vec()).unwrap();
        let line = (0..640).find(|&x| {
            let [r, g, b] = frame.get_pixel(x, 0).0;
            g > 150 && g > r.saturating_add(60) && g > b.saturating_add(60)
        });
        let cursor = frame
            .enumerate_pixels()
            .filter_map(|(x, _, pixel)| {
                let [r, g, b] = pixel.0;
                (r > 180 && g > 180 && b > 180).then_some(x)
            })
            .min();
        if let (Some(line), Some(cursor)) = (line, cursor) {
            measured += 1;
            positions.insert(line);
            worst = worst.max(line.abs_diff(cursor));
        }
    }
    assert!(measured >= 100, "only {measured} drag frames measured");
    assert!(
        positions.len() >= 60,
        "drag must exercise moving page frames"
    );
    // The cursor's white fill starts 1-2 pixels inside its black outline.
    assert!(
        worst <= 3,
        "recorded cursor separated from the dragged control by {worst} pixels"
    );
}

/// A completed drag must not pin the recorded cursor to a stale page frame.
#[tokio::test]
#[ignore]
async fn e2e_recording_cursor_moves_after_release_without_repaint() {
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({ "action": "launch", "headless": true }), &mut state).await,
    );
    assert_success(
        &execute_command(
            &json!({ "action": "viewport", "width": 640, "height": 480 }),
            &mut state,
        )
        .await,
    );
    assert_success(
        &execute_command(
            &json!({
                "action": "navigate",
                "url": "data:text/html,<style>body{margin:0;background:%23202020}</style>"
            }),
            &mut state,
        )
        .await,
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("released-cursor.mp4");
    assert_success(
        &execute_command(
            &json!({ "action": "recording_start", "path": path, "cursor": true, "fps": 60 }),
            &mut state,
        )
        .await,
    );
    for command in [
        json!({ "action": "mousemove", "x": 80, "y": 80 }),
        json!({ "action": "mousedown" }),
        json!({ "action": "mousemove", "x": 120, "y": 100, "duration": 150, "inputMode": "human" }),
    ] {
        assert_success(&execute_command(&command, &mut state).await);
    }
    let captured = state
        .recording_state
        .shared_captured_count
        .as_ref()
        .unwrap()
        .clone();
    let before = captured.load(Ordering::Relaxed);
    // Paint while pressed, then leave the page completely static after release.
    assert_success(
        &execute_command(
            &json!({ "action": "evaluate", "script": "document.body.style.background = '#303030'" }),
            &mut state,
        )
        .await,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while captured.load(Ordering::Relaxed) == before {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the pressed page frame should reach the recorder");
    for command in [
        json!({ "action": "mouseup" }),
        json!({ "action": "mousemove", "x": 240, "y": 180, "duration": 250, "inputMode": "human" }),
    ] {
        assert_success(&execute_command(&command, &mut state).await);
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_success(&execute_command(&json!({ "action": "recording_stop" }), &mut state).await);
    assert_success(&execute_command(&json!({ "action": "close" }), &mut state).await);

    let output = tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-sseof", "-0.1", "-i"])
        .arg(&path)
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "pipe:1",
        ])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let frame = image::load_from_memory(&output.stdout).unwrap().to_rgb8();
    assert_eq!(frame.dimensions(), (640, 480));
    assert!(
        frame.get_pixel(244, 184).0.iter().all(|value| *value > 180),
        "the released cursor should reach the new position in the encoded video; got {:?}",
        frame.get_pixel(244, 184).0
    );
    assert!(
        frame.get_pixel(124, 104).0.iter().all(|value| *value < 80),
        "the old drag position should contain only the page background"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_initial_recording_frame_uses_css_viewport_dimensions() {
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({ "action": "launch", "headless": true }), &mut state).await,
    );
    assert_success(
        &execute_command(
            &json!({
                "action": "viewport",
                "width": 400,
                "height": 300,
                "deviceScaleFactor": 2.0
            }),
            &mut state,
        )
        .await,
    );

    let browser = state.browser.as_ref().unwrap();
    let initial = super::recording::capture_initial_image(
        &browser.client,
        browser.active_session_id().unwrap(),
    )
    .await
    .unwrap();
    let (pixel_width, pixel_height) = image::ImageReader::with_format(
        std::io::Cursor::new(&initial.image_data),
        image::ImageFormat::Png,
    )
    .into_dimensions()
    .unwrap();

    assert_eq!((pixel_width, pixel_height), (800, 600));
    assert_eq!(
        (initial.device_width, initial.device_height),
        (400.0, 300.0)
    );

    assert_success(&execute_command(&json!({ "action": "close" }), &mut state).await);
}

// ---------------------------------------------------------------------------
// tab new: session setup inheritance
// ---------------------------------------------------------------------------

/// `tab new <url>` must replay the session's setup onto the new tab before
/// its first document: an init script registered on the primary page has to
/// run on the initial load, not only after a later navigation.
#[tokio::test]
#[ignore]
async fn e2e_tab_new_inherits_init_script_on_first_load() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "addinitscript", "script": "window.__abTab = 'seeded';" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3", "action": "tab_new",
            "url": "data:text/html,<script>document.title = String(window.__abTab)</script>",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "4", "action": "evaluate", "script": "document.title" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["result"],
        "seeded",
        "init script should run on the new tab's first document"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Replayed init scripts receive target-specific CDP identifiers. Removing a
/// script from the new tab must translate the original user-facing identifier
/// for every tab where Chrome registered it.
#[tokio::test]
#[ignore]
async fn e2e_tab_new_removes_replayed_init_script_by_original_identifier() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let first = execute_command(
        &json!({
            "id": "2", "action": "addinitscript",
            "script": "window.__abFirstInit = true;",
        }),
        &mut state,
    )
    .await;
    assert_success(&first);
    let first_id = get_data(&first)["identifier"]
        .as_str()
        .expect("first init script should return an identifier")
        .to_string();

    let second = execute_command(
        &json!({
            "id": "3", "action": "addinitscript",
            "script": "window.__abSecondInit = true;",
        }),
        &mut state,
    )
    .await;
    assert_success(&second);
    let second_id = get_data(&second)["identifier"]
        .as_str()
        .expect("second init script should return an identifier")
        .to_string();

    let resp = execute_command(
        &json!({ "id": "4", "action": "removeinitscript", "identifier": first_id }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "5", "action": "tab_new",
            "url": "data:text/html,<title>new tab</title>",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "6", "action": "removeinitscript", "identifier": second_id }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "7", "action": "navigate",
            "url": "data:text/html,<title>after removal</title>",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "8", "action": "evaluate",
            "script": "window.__abSecondInit === true",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], false);

    let resp = execute_command(
        &json!({ "id": "9", "action": "tab_switch", "tabId": "t1" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "10", "action": "navigate",
            "url": "data:text/html,<title>original after removal</title>",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "11", "action": "evaluate",
            "script": "window.__abSecondInit === true",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["result"],
        false,
        "removing a replayed script should also remove its original registration"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// CDP allocates init-script identifiers independently in each target. Two
/// pre-existing tabs can therefore both return `1` for different scripts, but
/// the daemon must expose distinct handles and remove only the requested one.
#[tokio::test]
#[ignore]
async fn e2e_tab_init_script_handles_are_unique_across_preexisting_tabs() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "2", "action": "tab_new" }), &mut state).await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "tab_switch", "tabId": "t1" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let first = execute_command(
        &json!({
            "id": "4", "action": "addinitscript",
            "script": "window.__abFirstExistingTab = true;",
        }),
        &mut state,
    )
    .await;
    assert_success(&first);
    let first_id = get_data(&first)["identifier"]
        .as_str()
        .expect("first init script should return an identifier")
        .to_string();

    let resp = execute_command(
        &json!({ "id": "5", "action": "tab_switch", "tabId": "t2" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let second = execute_command(
        &json!({
            "id": "6", "action": "addinitscript",
            "script": "window.__abSecondExistingTab = true;",
        }),
        &mut state,
    )
    .await;
    assert_success(&second);
    let second_id = get_data(&second)["identifier"]
        .as_str()
        .expect("second init script should return an identifier")
        .to_string();

    assert_ne!(first_id, second_id, "user-facing handles must be unique");

    let resp = execute_command(
        &json!({ "id": "7", "action": "removeinitscript", "identifier": second_id }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "8", "action": "tab_new",
            "url": "data:text/html,<title>future tab</title>",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "9", "action": "evaluate",
            "script": "[window.__abFirstExistingTab === true, window.__abSecondExistingTab === true]",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], json!([true, false]));

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// `click --new-tab` creates a tab through a separate handler from `tab new`,
/// but it must apply the same session setup before the first request.
#[tokio::test]
#[ignore]
async fn e2e_click_new_tab_inherits_user_agent_and_headers() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({
            "id": "1", "action": "launch", "headless": true,
            "userAgent": "ab-click-new-tab-test/1.0",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "headers", "headers": { "X-Global": "global" } }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3", "action": "navigate",
            "url": format!("data:text/html,<a id='next' href='{}/click'>next</a>", base_url),
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "4", "action": "click", "selector": "#next", "newTab": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "5", "action": "evaluate",
            "script": "JSON.parse(document.body.innerText)",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let headers = &get_data(&resp)["result"]["headers"];
    assert_eq!(headers["X-Global"], "global");
    assert_eq!(headers["User-Agent"], "ab-click-new-tab-test/1.0");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// Clearing headers and offline mode restores the default setup, so future
/// tabs can use the non-blocking target creation path.
#[tokio::test]
#[ignore]
async fn e2e_cleared_session_setup_is_not_pending() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "offline", "offline": false }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "headers", "headers": {} }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    assert!(state.session_setup.offline.is_none());
    assert!(state.session_setup.extra_headers.is_none());

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// The launch `--user-agent` (Emulation.setUserAgentOverride) and global
/// `set headers` (Network.setExtraHTTPHeaders) are per CDP session. A new tab
/// opened with a URL must send both on its very first document request.
#[tokio::test]
#[ignore]
async fn e2e_tab_new_inherits_user_agent_and_headers() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({
            "id": "1", "action": "launch", "headless": true,
            "userAgent": "ab-tab-new-test/1.0",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "headers", "headers": { "X-Global": "global" } }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "tab_new", "url": format!("{}/tab", base_url) }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "4", "action": "evaluate",
            "script": "JSON.parse(document.body.innerText)",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let headers = &get_data(&resp)["result"]["headers"];
    assert_eq!(
        headers["X-Global"], "global",
        "global `headers` should apply to the new tab's first document request, got {headers}"
    );
    assert_eq!(
        headers["User-Agent"], "ab-tab-new-test/1.0",
        "new tab's first document request should carry the launch user agent, got {headers}"
    );

    let resp = execute_command(
        &json!({ "id": "5", "action": "evaluate", "script": "navigator.userAgent" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["result"], "ab-tab-new-test/1.0");

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

/// `set credentials` uses target-scoped extra headers, so a new tab must send
/// the resulting Authorization header on its first document request.
#[tokio::test]
#[ignore]
async fn e2e_tab_new_inherits_http_credentials_on_first_load() {
    let (base_url, _server) = start_echo_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2", "action": "credentials",
            "username": "tab-user", "password": "tab-password",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "tab_new", "url": format!("{}/credentials", base_url) }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "4", "action": "evaluate",
            "script": "JSON.parse(document.body.innerText)",
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let expected = format!("Basic {}", STANDARD.encode("tab-user:tab-password"));
    assert_eq!(
        get_data(&resp)["result"]["headers"]["Authorization"],
        expected,
        "HTTP credentials should apply to the new tab's first document request"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);
}

// ---------------------------------------------------------------------------
// --state / storageState flag: cookies should be loaded at launch time
// ---------------------------------------------------------------------------

/// Verify that launching with `storageState` in the launch command restores
/// cookies that were previously saved with `state_save`.
///
/// This is the e2e equivalent of `agent-browser --state ./auth.json open <url>`.
/// The launch command accepts a `storageState` field that should load the
/// state file (cookies + localStorage) before the first navigation.
#[tokio::test]
#[ignore]
async fn e2e_state_flag_restores_cookies() {
    let state_path = std::env::temp_dir()
        .join(format!(
            "agent-browser-e2e-state-flag-{}.json",
            uuid::Uuid::new_v4()
        ))
        .to_string_lossy()
        .to_string();

    // Session 1: launch, set a cookie, save state, close
    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "cookies_set",
                "name": "state_flag_test",
                "value": "from_state_file",
                "domain": ".example.com",
                "path": "/",
                "expires": 2000000000
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "4", "action": "state_save", "path": &state_path }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(&json!({ "id": "5", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    // Session 2: launch with storageState pointing to saved file, verify
    // cookies are present before any explicit state_load call.
    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({
                "id": "10",
                "action": "launch",
                "headless": true,
                "storageState": &state_path
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "11", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp =
            execute_command(&json!({ "id": "12", "action": "cookies_get" }), &mut state).await;
        assert_success(&resp);
        let cookies = get_data(&resp)["cookies"].as_array().unwrap();
        let found = cookies
            .iter()
            .any(|c| c["name"] == "state_flag_test" && c["value"] == "from_state_file");
        assert!(
            found,
            "Cookie from state file should be present after launch with storageState. \
             Cookies found: {:?}",
            cookies
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );

        let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    let _ = std::fs::remove_file(&state_path);
}

/// Verify that explicit `launch` surfaces storageState load failures instead
/// of reporting success with an empty browser state.
#[tokio::test]
#[ignore]
async fn e2e_state_flag_missing_file_fails_launch() {
    let guard = EnvGuard::new(&["CI"]);
    guard.set("CI", "1");

    let missing_path = std::env::temp_dir()
        .join(format!(
            "agent-browser-e2e-missing-state-{}.json",
            uuid::Uuid::new_v4()
        ))
        .to_string_lossy()
        .to_string();

    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({
            "id": "10",
            "action": "launch",
            "headless": true,
            "args": ["--no-sandbox", "--disable-dev-shm-usage"],
            "storageState": &missing_path
        }),
        &mut state,
    )
    .await;

    assert_eq!(resp["success"], false);
    let error = resp["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("Failed to read state from") || error.contains("storage state"),
        "Unexpected error for missing storageState file: {}",
        error
    );
    assert!(
        state.browser.is_none(),
        "failed storageState launch should roll back the browser"
    );
}

/// Repeated launch calls with `storageState` should relaunch a clean browser so
/// stale cookies do not survive from the previous state file.
#[tokio::test]
#[ignore]
async fn e2e_storage_state_launch_restarts_clean_browser() {
    let state_one = std::env::temp_dir()
        .join(format!(
            "agent-browser-e2e-storage-reuse-1-{}.json",
            uuid::Uuid::new_v4()
        ))
        .to_string_lossy()
        .to_string();
    let state_two = std::env::temp_dir()
        .join(format!(
            "agent-browser-e2e-storage-reuse-2-{}.json",
            uuid::Uuid::new_v4()
        ))
        .to_string_lossy()
        .to_string();

    create_storage_state_with_cookie(&state_one, "storage_reload_first", "first").await;
    create_storage_state_with_cookie(&state_two, "storage_reload_second", "second").await;

    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({
            "id": "10",
            "action": "launch",
            "headless": true,
            "args": ["--no-sandbox", "--disable-dev-shm-usage"],
            "storageState": &state_one
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(
        get_data(&resp).get("reused").is_none(),
        "first launch must create the browser"
    );

    let resp = execute_command(
        &json!({ "id": "11", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "12", "action": "cookies_get" }), &mut state).await;
    assert_success(&resp);
    let cookies = get_data(&resp)["cookies"].as_array().unwrap();
    assert!(
        cookies
            .iter()
            .any(|c| c["name"] == "storage_reload_first" && c["value"] == "first"),
        "first storageState should be applied on the initial launch"
    );

    let resp = execute_command(
        &json!({
            "id": "13",
            "action": "launch",
            "headless": true,
            "args": ["--no-sandbox", "--disable-dev-shm-usage"],
            "storageState": &state_two
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp).get("reused"),
        None,
        "storageState launch should start from a clean browser"
    );

    let resp = execute_command(
        &json!({ "id": "14", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "15", "action": "cookies_get" }), &mut state).await;
    assert_success(&resp);
    let cookies = get_data(&resp)["cookies"].as_array().unwrap();
    assert!(
        cookies
            .iter()
            .any(|c| c["name"] == "storage_reload_second" && c["value"] == "second"),
        "second storageState should be applied after relaunch"
    );
    assert!(
        !cookies
            .iter()
            .any(|c| c["name"] == "storage_reload_first" && c["value"] == "first"),
        "stale cookies from the first storageState should not survive relaunch"
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);

    let _ = std::fs::remove_file(&state_one);
    let _ = std::fs::remove_file(&state_two);
}

/// Verify that AGENT_BROWSER_STATE env var restores cookies at auto-launch
/// time (when the browser is lazily launched by a command like `navigate`
/// rather than an explicit `launch` command).
#[tokio::test]
#[ignore]
async fn e2e_state_env_restores_cookies_on_auto_launch() {
    let state_path = std::env::temp_dir()
        .join(format!(
            "agent-browser-e2e-state-env-{}.json",
            uuid::Uuid::new_v4()
        ))
        .to_string_lossy()
        .to_string();

    // Session 1: launch, set a cookie, save state, close
    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "cookies_set",
                "name": "env_state_test",
                "value": "from_env_state",
                "domain": ".example.com",
                "path": "/",
                "expires": 2000000000
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "4", "action": "state_save", "path": &state_path }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(&json!({ "id": "5", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    // Session 2: set AGENT_BROWSER_STATE env var and let auto_launch pick it
    // up. No explicit `launch` command — just navigate, which triggers
    // auto_launch internally.
    {
        let env = EnvGuard::new(&["AGENT_BROWSER_STATE"]);
        env.set("AGENT_BROWSER_STATE", &state_path);

        let mut state = DaemonState::new();

        // Navigate without explicit launch — triggers auto_launch
        let resp = execute_command(
            &json!({ "id": "10", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp =
            execute_command(&json!({ "id": "11", "action": "cookies_get" }), &mut state).await;
        assert_success(&resp);
        let cookies = get_data(&resp)["cookies"].as_array().unwrap();
        let found = cookies
            .iter()
            .any(|c| c["name"] == "env_state_test" && c["value"] == "from_env_state");
        assert!(
            found,
            "Cookie should be restored via AGENT_BROWSER_STATE env on auto-launch. \
             Cookies found: {:?}",
            cookies
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );

        let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    let _ = std::fs::remove_file(&state_path);
}

/// Verify that --session-name auto-restores cookies saved from a prior
/// session with the same name.
#[tokio::test]
#[ignore]
async fn e2e_session_name_auto_restores_cookies() {
    let session_name = format!(
        "e2e-session-name-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );

    let env = EnvGuard::new(&["AGENT_BROWSER_SESSION_NAME"]);
    env.set("AGENT_BROWSER_SESSION_NAME", &session_name);

    // Session 1: launch, set a cookie, close (which auto-saves state)
    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "cookies_set",
                "name": "session_name_test",
                "value": "auto_restored",
                "domain": ".example.com",
                "path": "/",
                "expires": 2000000000
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        // close triggers auto-save when session_name is set
        let resp = execute_command(&json!({ "id": "5", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    // Session 2: fresh DaemonState with same session_name. Navigate without
    // explicit launch, which triggers auto_launch and restore.
    {
        let mut state = DaemonState::new();

        // Navigate without explicit launch — triggers auto_launch → try_auto_restore_state
        let resp = execute_command(
            &json!({ "id": "10", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp =
            execute_command(&json!({ "id": "12", "action": "cookies_get" }), &mut state).await;
        assert_success(&resp);
        let cookies = get_data(&resp)["cookies"].as_array().unwrap();
        let found = cookies
            .iter()
            .any(|c| c["name"] == "session_name_test" && c["value"] == "auto_restored");
        assert!(
            found,
            "Cookie should be auto-restored via --session-name. Cookies found: {:?}",
            cookies
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );

        let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    // Clean up auto-saved state files
    let sessions_dir = dirs::home_dir()
        .unwrap()
        .join(".agent-browser")
        .join("sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with(&format!("{}-", session_name)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn e2e_restore_loads_during_explicit_launch_before_navigation() {
    let restore_key = format!(
        "e2e-explicit-restore-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let env = EnvGuard::new(&[
        "AGENT_BROWSER_SESSION_NAME",
        "AGENT_BROWSER_RESTORE_SAVE",
        "AGENT_BROWSER_STATE",
        "AGENT_BROWSER_ENCRYPTION_KEY",
    ]);
    env.remove("AGENT_BROWSER_SESSION_NAME");
    env.remove("AGENT_BROWSER_RESTORE_SAVE");
    env.remove("AGENT_BROWSER_STATE");
    env.remove("AGENT_BROWSER_ENCRYPTION_KEY");

    {
        let mut state = DaemonState::new();
        let resp = execute_command(
            &json!({
                "id": "1",
                "action": "launch",
                "headless": true,
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "cookies_set",
                "name": "explicit_restore_test",
                "value": "loaded_before_navigation",
                "domain": ".example.com",
                "path": "/",
                "expires": 2000000000
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(&json!({ "id": "4", "action": "close" }), &mut state).await;
        assert_success(&resp);
        assert_eq!(get_data(&resp)["saveStatus"], "saved");
    }

    let path = super::state::find_auto_state_file(&restore_key)
        .expect("first close should create restore state");

    {
        let mut state = DaemonState::new();
        let resp = execute_command(
            &json!({
                "id": "10",
                "action": "launch",
                "headless": true,
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert_eq!(get_data(&resp)["lifecycle"]["restoreStatus"], "loaded");

        let resp = execute_command(
            &json!({ "id": "11", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp =
            execute_command(&json!({ "id": "12", "action": "cookies_get" }), &mut state).await;
        assert_success(&resp);
        let cookies = get_data(&resp)["cookies"].as_array().unwrap();
        let found = cookies.iter().any(|c| {
            c["name"] == "explicit_restore_test" && c["value"] == "loaded_before_navigation"
        });
        assert!(
            found,
            "Cookie should be restored by explicit launch before first navigation. Cookies found: {:?}",
            cookies
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );

        let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.previous", path));
}

/// Verify that periodic autosave persists session state while the browser is
/// open, so state survives the browser dying WITHOUT the daemon's
/// save-on-close path running (e.g. the user closes the window by hand).
#[tokio::test]
#[ignore]
async fn e2e_periodic_autosave_survives_abrupt_browser_exit() {
    let restore_key = format!(
        "e2e-autosave-abrupt-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let env = EnvGuard::new(&[
        "AGENT_BROWSER_SESSION_NAME",
        "AGENT_BROWSER_RESTORE_SAVE",
        "AGENT_BROWSER_STATE",
        "AGENT_BROWSER_ENCRYPTION_KEY",
    ]);
    env.remove("AGENT_BROWSER_SESSION_NAME");
    env.remove("AGENT_BROWSER_RESTORE_SAVE");
    env.remove("AGENT_BROWSER_STATE");
    env.remove("AGENT_BROWSER_ENCRYPTION_KEY");

    {
        let mut state = DaemonState::new();
        let resp = execute_command(
            &json!({
                "id": "1",
                "action": "launch",
                "headless": true,
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "cookies_set",
                "name": "autosave_test",
                "value": "saved_by_tick",
                "domain": ".example.com",
                "path": "/",
                "expires": 2000000000
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        // The command just finished, so the quiet period must block the tick.
        maybe_autosave_restore_state(&mut state, 30_000).await;
        assert_eq!(state.restore_save_status, "not_attempted");
        assert!(
            super::state::find_auto_state_file(&restore_key).is_none(),
            "autosave must not fire inside the post-command quiet period"
        );

        // Simulate the quiet period having elapsed, then run the tick again.
        state.last_command_finished =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(10));
        maybe_autosave_restore_state(&mut state, 30_000).await;
        assert_eq!(state.restore_save_status, "saved");
        assert!(
            state.last_autosave_attempt.is_some(),
            "successful save should reset the periodic interval"
        );
        assert!(
            super::state::find_auto_state_file(&restore_key).is_some(),
            "periodic autosave should write the session state file"
        );

        // An idle session stays eligible: once the interval elapses again the
        // tick re-saves, capturing page-driven mutations like token refreshes.
        state.last_autosave_attempt =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(31));
        state.restore_save_status = "not_attempted".to_string();
        maybe_autosave_restore_state(&mut state, 30_000).await;
        assert_eq!(
            state.restore_save_status, "saved",
            "idle session should be re-saved on the next interval without new commands"
        );

        // Kill the browser out from under the daemon, the way a user closing
        // the window does: the process exits and CDP dies, so no further save
        // is possible. Then mimic the daemon drain tick, which only closes.
        let mgr = state.browser.as_mut().expect("browser should be running");
        let _ = mgr
            .client
            .send_command_no_params("Browser.close", None)
            .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !mgr.has_process_exited() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            state
                .browser
                .as_mut()
                .expect("browser manager should still be present")
                .has_process_exited(),
            "browser process should have exited after Browser.close"
        );
        let _ = close_current_browser(&mut state).await;
    }

    let path = super::state::find_auto_state_file(&restore_key)
        .expect("autosaved state should survive the abrupt browser exit");

    {
        let mut state = DaemonState::new();
        let resp = execute_command(
            &json!({
                "id": "10",
                "action": "launch",
                "headless": true,
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert_eq!(get_data(&resp)["lifecycle"]["restoreStatus"], "loaded");

        let resp = execute_command(
            &json!({ "id": "11", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp =
            execute_command(&json!({ "id": "12", "action": "cookies_get" }), &mut state).await;
        assert_success(&resp);
        let cookies = get_data(&resp)["cookies"].as_array().unwrap();
        let found = cookies
            .iter()
            .any(|c| c["name"] == "autosave_test" && c["value"] == "saved_by_tick");
        assert!(
            found,
            "Cookie saved only by periodic autosave should be restored after the browser was killed without a graceful close. Cookies found: {:?}",
            cookies
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );

        let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.previous", path));
}

#[tokio::test]
#[ignore]
async fn e2e_restore_preserves_cookie_login_after_close_and_reopen() {
    let restore_key = format!(
        "e2e-next-cookie-login-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let env = EnvGuard::new(&[
        "AGENT_BROWSER_SESSION_NAME",
        "AGENT_BROWSER_RESTORE_SAVE",
        "AGENT_BROWSER_STATE",
        "AGENT_BROWSER_ENCRYPTION_KEY",
    ]);
    env.remove("AGENT_BROWSER_SESSION_NAME");
    env.remove("AGENT_BROWSER_RESTORE_SAVE");
    env.remove("AGENT_BROWSER_STATE");
    env.remove("AGENT_BROWSER_ENCRYPTION_KEY");

    let (base_url, _server) = start_cookie_login_server().await;

    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({
                "id": "1",
                "action": "navigate",
                "url": base_url.clone(),
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert_eq!(get_data(&resp)["lifecycle"]["restoreStatus"], "missing");

        let resp = execute_command(
            &json!({ "id": "2", "action": "evaluate", "script": "document.body.innerText" }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert!(
            get_data(&resp)["result"]
                .as_str()
                .unwrap_or_default()
                .contains("Please sign in"),
            "first homepage load should be logged out: {}",
            resp
        );

        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "navigate",
                "url": format!("{}/login", base_url),
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "4",
                "action": "navigate",
                "url": base_url.clone(),
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "5", "action": "evaluate", "script": "document.body.innerText" }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert!(
            get_data(&resp)["result"]
                .as_str()
                .unwrap_or_default()
                .contains("Welcome back"),
            "login route should set cookie for current browser: {}",
            resp
        );

        let resp = execute_command(&json!({ "id": "6", "action": "close" }), &mut state).await;
        assert_success(&resp);
        assert_eq!(get_data(&resp)["saveStatus"], "saved");
    }

    let path = super::state::find_auto_state_file(&restore_key)
        .expect("close should save cookie-backed restore state");

    {
        let mut state = DaemonState::new();
        let resp = execute_command(
            &json!({
                "id": "10",
                "action": "navigate",
                "url": base_url.clone(),
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert_eq!(get_data(&resp)["lifecycle"]["restoreStatus"], "loaded");

        let resp = execute_command(
            &json!({ "id": "11", "action": "evaluate", "script": "document.body.innerText" }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert!(
            get_data(&resp)["result"]
                .as_str()
                .unwrap_or_default()
                .contains("Welcome back"),
            "reopened session should keep cookie login on homepage: {}",
            resp
        );

        let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.previous", path));
    cleanup_restore_state_files(&restore_key);
}

#[tokio::test]
#[ignore]
async fn e2e_restore_validation_failure_does_not_overwrite_state() {
    let restore_key = format!(
        "e2e-restore-validation-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );

    {
        let mut state = DaemonState::new();
        let resp = execute_command(
            &json!({
                "id": "1",
                "action": "navigate",
                "url": "https://example.com",
                "restoreKey": restore_key
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "2",
                "action": "cookies_set",
                "name": "restore_validation_test",
                "value": "known_good",
                "domain": ".example.com",
                "path": "/",
                "expires": 2000000000
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(&json!({ "id": "3", "action": "close" }), &mut state).await;
        assert_success(&resp);
        assert_eq!(get_data(&resp)["saveStatus"], "saved");
    }

    let path = super::state::find_auto_state_file(&restore_key)
        .expect("first close should create restore state");
    let before = std::fs::read_to_string(&path).expect("state file should be readable");

    {
        let mut state = DaemonState::new();
        let resp = execute_command(
            &json!({
                "id": "10",
                "action": "navigate",
                "url": "https://example.com",
                "restoreKey": restore_key,
                "restoreCheckText": "text-that-does-not-exist-in-the-page"
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);
        assert_eq!(
            get_data(&resp)["lifecycle"]["restoreStatus"],
            "loaded_but_invalid"
        );

        let resp = execute_command(&json!({ "id": "11", "action": "close" }), &mut state).await;
        assert_success(&resp);
        assert_eq!(get_data(&resp)["saveStatus"], "skipped_restore_failed");
    }

    let after = std::fs::read_to_string(&path).expect("state file should still be readable");
    assert_eq!(
        before, after,
        "failed restore validation must not overwrite previous state"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.previous", path));
}

#[tokio::test]
#[ignore]
async fn e2e_restore_key_switch_reloads_instead_of_reusing_live_browser() {
    let restore_key_a = format!(
        "e2e-restore-switch-a-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let restore_key_b = format!(
        "e2e-restore-switch-b-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let cookie_name = "restore_switch_test";
    let env = EnvGuard::new(&[
        "AGENT_BROWSER_SESSION_NAME",
        "AGENT_BROWSER_RESTORE_SAVE",
        "AGENT_BROWSER_STATE",
        "AGENT_BROWSER_ENCRYPTION_KEY",
    ]);
    env.remove("AGENT_BROWSER_SESSION_NAME");
    env.remove("AGENT_BROWSER_RESTORE_SAVE");
    env.remove("AGENT_BROWSER_STATE");
    env.remove("AGENT_BROWSER_ENCRYPTION_KEY");

    create_restore_state_with_cookie(&restore_key_a, cookie_name, "value-a").await;
    create_restore_state_with_cookie(&restore_key_b, cookie_name, "value-b").await;

    let path_a = super::state::find_auto_state_file(&restore_key_a)
        .expect("first restore key should have saved state");
    let path_b = super::state::find_auto_state_file(&restore_key_b)
        .expect("second restore key should have saved state");

    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({
            "id": "10",
            "action": "navigate",
            "url": "https://example.com",
            "restoreKey": restore_key_a
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "11", "action": "cookies_get" }), &mut state).await;
    assert_success(&resp);
    let cookies = get_data(&resp)["cookies"].as_array().unwrap();
    assert!(
        cookies
            .iter()
            .any(|c| c["name"] == cookie_name && c["value"] == "value-a"),
        "first restore key should load value-a: {:?}",
        cookies
    );

    let resp = execute_command(
        &json!({
            "id": "20",
            "action": "navigate",
            "url": "https://example.com",
            "restoreKey": restore_key_b
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["lifecycle"]["relaunchedBrowser"], true);

    let resp = execute_command(&json!({ "id": "21", "action": "cookies_get" }), &mut state).await;
    assert_success(&resp);
    let cookies = get_data(&resp)["cookies"].as_array().unwrap();
    assert!(
        cookies
            .iter()
            .any(|c| c["name"] == cookie_name && c["value"] == "value-b"),
        "switching restore keys should load value-b, not reuse value-a: {:?}",
        cookies
    );

    let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    assert_success(&resp);

    let saved_b = std::fs::read_to_string(&path_b).expect("state file should remain readable");
    assert!(saved_b.contains("value-b"));
    assert!(!saved_b.contains("value-a"));

    let _ = std::fs::remove_file(&path_a);
    let _ = std::fs::remove_file(format!("{}.previous", path_a));
    let _ = std::fs::remove_file(&path_b);
    let _ = std::fs::remove_file(format!("{}.previous", path_b));
}

/// Verify that explicit `state_load` restores cookies into an existing
/// session (baseline sanity check — this path is known to work).
#[tokio::test]
#[ignore]
async fn e2e_explicit_state_load_restores_cookies() {
    let state_path = std::env::temp_dir()
        .join(format!(
            "agent-browser-e2e-explicit-load-{}.json",
            uuid::Uuid::new_v4()
        ))
        .to_string_lossy()
        .to_string();

    // Session 1: set cookie, save state
    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({
                "id": "3",
                "action": "cookies_set",
                "name": "explicit_load_test",
                "value": "manually_loaded",
                "domain": ".example.com",
                "path": "/",
                "expires": 2000000000
            }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "4", "action": "state_save", "path": &state_path }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(&json!({ "id": "5", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    // Session 2: launch clean, then explicitly load state
    {
        let mut state = DaemonState::new();

        let resp = execute_command(
            &json!({ "id": "10", "action": "launch", "headless": true }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "11", "action": "state_load", "path": &state_path }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp = execute_command(
            &json!({ "id": "12", "action": "navigate", "url": "https://example.com" }),
            &mut state,
        )
        .await;
        assert_success(&resp);

        let resp =
            execute_command(&json!({ "id": "13", "action": "cookies_get" }), &mut state).await;
        assert_success(&resp);
        let cookies = get_data(&resp)["cookies"].as_array().unwrap();
        let found = cookies
            .iter()
            .any(|c| c["name"] == "explicit_load_test" && c["value"] == "manually_loaded");
        assert!(
            found,
            "Cookie should be present after explicit state_load. Cookies found: {:?}",
            cookies
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );

        let resp = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
        assert_success(&resp);
    }

    let _ = std::fs::remove_file(&state_path);
}

// === React / Web Vitals primitives ===

const REACT_FIXTURE_HTML: &str = r#"<!doctype html>
<html>
  <head><title>React fixture</title></head>
  <body>
    <div id="root"></div>
    <script crossorigin src="https://unpkg.com/react@18/umd/react.production.min.js"></script>
    <script crossorigin src="https://unpkg.com/react-dom@18/umd/react-dom.production.min.js"></script>
    <script>
      const { useState, createElement: h } = React;
      function Counter({ label }) {
        const [n, setN] = useState(0);
        return h("button", { onClick: () => setN(n + 1) }, label + ": " + n);
      }
      function App() {
        return h("div", {}, [
          h("h1", { key: "t" }, "Hello"),
          h(Counter, { key: "c1", label: "A" }),
          h(Counter, { key: "c2", label: "B" }),
        ]);
      }
      ReactDOM.createRoot(document.getElementById("root")).render(h(App));
    </script>
  </body>
</html>
"#;

fn react_fixture_url() -> String {
    format!(
        "data:text/html;base64,{}",
        STANDARD.encode(REACT_FIXTURE_HTML)
    )
}

#[tokio::test]
#[ignore]
async fn e2e_react_tree_errors_without_hook() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Without --enable react-devtools, the hook isn't installed and the
    // command should error.
    let resp = execute_command(&json!({ "id": "3", "action": "react_tree" }), &mut state).await;
    let err = resp
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        err.contains("React DevTools") || err.contains("renderer"),
        "Expected hook-missing error, got: {:?}",
        resp
    );

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_react_tree_with_enable_hook() {
    let guard = EnvGuard::new(&["AGENT_BROWSER_ENABLE"]);
    guard.set("AGENT_BROWSER_ENABLE", "react-devtools");
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": &react_fixture_url() }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Give React a moment to boot and register with the hook.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let resp = execute_command(&json!({ "id": "3", "action": "react_tree" }), &mut state).await;
    assert_success(&resp);
    let tree = get_data(&resp)
        .get("tree")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        tree.contains("App"),
        "Expected tree to contain 'App': {}",
        tree
    );
    assert!(
        tree.contains("Counter"),
        "Expected tree to contain 'Counter': {}",
        tree
    );

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_relaunch_when_enable_changes_installs_react_hook() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": &react_fixture_url() }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "3", "action": "react_tree" }), &mut state).await;
    assert!(
        resp.get("success").and_then(|v| v.as_bool()) == Some(false),
        "React tree should fail before react-devtools is enabled: {}",
        resp
    );

    let resp = execute_command(
        &json!({
            "id": "4",
            "action": "launch",
            "headless": true,
            "enable": ["react-devtools"]
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(
        get_data(&resp).get("reused").is_none(),
        "changed enable list must relaunch, not reuse: {}",
        resp
    );
    assert_eq!(
        get_data(&resp)["lifecycle"]["relaunchedBrowser"],
        true,
        "lifecycle should report browser relaunch"
    );

    let resp = execute_command(
        &json!({ "id": "5", "action": "navigate", "url": &react_fixture_url() }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let resp = execute_command(&json!({ "id": "6", "action": "react_tree" }), &mut state).await;
    assert_success(&resp);
    let tree = get_data(&resp)
        .get("tree")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        tree.contains("App"),
        "Expected tree to contain App: {}",
        tree
    );

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_vitals_reports_metrics() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": &react_fixture_url() }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "3", "action": "vitals" }), &mut state).await;
    assert_success(&resp);
    let data = get_data(&resp);
    assert!(data.get("url").and_then(|v| v.as_str()).is_some());
    assert!(data.get("ttfb").is_some());
    assert!(data.get("cls").and_then(|v| v.get("score")).is_some());
    assert!(data.get("phases").and_then(|v| v.as_array()).is_some());
    assert!(data
        .get("hydratedComponents")
        .and_then(|v| v.as_array())
        .is_some());

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

async fn start_a11y_frame_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        for _ in 0..100 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                let (status, content_type, body) = match path {
                    "/top" => (
                        "200 OK",
                        "text/html",
                        format!(
                            r#"<!doctype html><html lang="en"><head><title>Top</title></head>
<body><main aria-label="Top"><h1>Top</h1>
<iframe id="outer" title="Outer" src="http://127.0.0.1:{port}/outer"></iframe>
</main></body></html>"#
                        ),
                    ),
                    "/outer" => (
                        "200 OK",
                        "text/html",
                        r#"<!doctype html><html lang="en"><head><title>Outer</title></head>
<body><main aria-label="Outer"><h1>Outer</h1><img id="outer-image" src="/missing-outer.png">
<iframe id="inner" title="Inner" src="/inner"></iframe></main></body></html>"#
                            .to_string(),
                    ),
                    "/inner" => (
                        "200 OK",
                        "text/html",
                        r#"<!doctype html><html lang="en"><head><title>Inner</title></head>
<body><main aria-label="Inner"><h1>Inner</h1><img id="inner-image" src="/missing-inner.png"></main></body></html>"#
                            .to_string(),
                    ),
                    "/siblings" => (
                        "200 OK",
                        "text/html",
                        format!(
                            r#"<!doctype html><html lang="en"><head><title>Siblings</title></head>
<body><main><h1>Siblings</h1>
<iframe id="first-frame" title="First" src="http://127.0.0.1:{port}/first-frame"></iframe>
<iframe id="second-frame" title="Second" src="http://127.0.0.1:{port}/second-frame"></iframe>
</main></body></html>"#
                        ),
                    ),
                    "/first-frame" => (
                        "200 OK",
                        "text/html",
                        r#"<!doctype html><html lang="en"><head><title>First</title></head>
<body><main><h1>First</h1><img id="first-image" src="/missing-first.png"></main></body></html>"#
                            .to_string(),
                    ),
                    "/second-frame" => (
                        "200 OK",
                        "text/html",
                        r#"<!doctype html><html lang="en"><head><title>Second</title></head>
<body><main><h1>Second</h1><img id="second-image" src="/missing-second.png"></main></body></html>"#
                            .to_string(),
                    ),
                    "/background" => (
                        "200 OK",
                        "text/html",
                        format!(
                            r#"<!doctype html><html lang="en"><head><title>Background</title></head>
<body><main><h1>Background</h1>
<iframe id="background-frame" title="Background frame" src="http://127.0.0.1:{port}/background-frame"></iframe>
</main></body></html>"#
                        ),
                    ),
                    "/background-frame" => (
                        "200 OK",
                        "text/html",
                        r#"<!doctype html><html lang="en"><head><title>Background frame</title></head>
<body><main><h1>Background frame</h1><img id="background-image" src="/missing-background.png"></main></body></html>"#
                            .to_string(),
                    ),
                    _ => ("404 Not Found", "text/plain", "not found".to_string()),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    (port, handle)
}

#[tokio::test]
#[ignore]
async fn e2e_recording_cursor_uses_page_coordinates_for_oopif() {
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({"action": "launch", "headless": true}), &mut state).await,
    );
    assert_success(
        &execute_command(
            &json!({"action": "navigate", "url": format!("http://localhost:{port}/top")}),
            &mut state,
        )
        .await,
    );
    assert_success(&execute_command(&json!({"action": "evaluate", "script": "document.getElementById('outer').style.cssText = 'position:absolute;left:240px;top:160px;width:400px;height:300px;border:10px solid black'"}), &mut state).await);
    let child_session = state
        .iframe_sessions
        .values()
        .next()
        .expect("fixture must use an OOPIF")
        .clone();
    state.browser.as_ref().unwrap().client.send_command("Runtime.evaluate", Some(json!({
        "expression": "document.body.innerHTML = '<button style=\"position:absolute;left:20px;top:30px;width:80px;height:40px\">Cursor target</button>'"
    })), Some(&child_session)).await.unwrap();
    let snapshot = execute_command(&json!({"action": "snapshot"}), &mut state).await;
    assert_success(&snapshot);
    let reference = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Cursor target")
        .unwrap()
        .0
        .clone();
    // Verify the input samples retain top-level coordinates for frame actions.
    super::recording::recording_start(
        &mut state.recording_state,
        "unused.webm",
        super::recording::RecordingOptions {
            cursor: true,
            ..Default::default()
        },
    )
    .unwrap();
    for action in ["hover", "click", "dblclick"] {
        assert_success(&execute_command(&json!({"action": action, "selector": format!("@{reference}"), "inputMode": "human"}), &mut state).await);
        let cursor = state
            .recording_state
            .shared_cursor
            .lock()
            .unwrap()
            .at(f64::INFINITY);
        assert!(cursor.visible);
        assert!(
            (cursor.x - 310.0).abs() < 1.0 && (cursor.y - 220.0).abs() < 1.0,
            "{action}: cursor should be at page (310, 220), got {cursor:?}"
        );
        assert!((state.mouse_state.x - cursor.x).abs() < 1.0);
        assert!((state.mouse_state.y - cursor.y).abs() < 1.0);
    }
    state.recording_state.active = false;
    state.browser.as_ref().unwrap().client.send_command("Runtime.evaluate", Some(json!({
        "expression": "document.body.style.background='#303030'; const button = document.querySelector('button'); button.style.background='#303030'; button.style.border='0'; button.textContent=''"
    })), Some(&child_session)).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("iframe-cursor.mp4");
    assert_success(
        &execute_command(
            &json!({"action":"recording_start", "path":path,"cursor":true,"fps":60}),
            &mut state,
        )
        .await,
    );
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_success(
        &execute_command(
            &json!({"action":"hover", "selector":format!("@{reference}"), "inputMode":"human"}),
            &mut state,
        )
        .await,
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let stopped = execute_command(&json!({"action":"recording_stop"}), &mut state).await;
    assert_success(&execute_command(&json!({"action": "close"}), &mut state).await);
    server.abort();
    assert_success(&stopped);
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-sseof", "-0.1", "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "pipe:1",
        ])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let image = image::load_from_memory(&output.stdout).unwrap().to_rgb8();
    assert!(
        image
            .get_pixel(314, 224)
            .0
            .iter()
            .all(|channel| *channel > 180),
        "the cursor should be visible inside the out-of-process iframe"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_a11y_uses_vendored_engine_and_preserves_shadow_targets() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = r#"<!doctype html>
<html lang="en">
<head><title>Accessibility audit fixture</title></head>
<body>
  <main>
    <h1>Accessibility audit fixture</h1>
    <img id="light-image" src="missing.png">
    <div id="shadow-host"></div>
    <iframe id="audit-frame" title="Audit frame" tabindex="-1" srcdoc="
      <!doctype html><html lang='en'><head><title>Frame</title></head>
      <body><main><h1>Frame</h1><a href='https://example.com'>Frame link</a><img id='frame-image' src='missing.png'></main>
      <script>
        window.frameAxe = { version: 'frame-spoofed' };
        window.frameAxeSetterCalls = 0;
        Object.defineProperty(window, 'axe', {
          configurable: false,
          get() { return window.frameAxe; },
          set() {
            window.frameAxeSetterCalls += 1;
            throw new Error('frame axe setter must not run');
          }
        });
      </script></body></html>
    "></iframe>
  </main>
  <script>
    window.pageAxe = {
      version: 'spoofed',
      run: () => Promise.resolve({
        url: 'spoofed',
        testEngine: { version: 'spoofed' },
        violations: [],
        incomplete: [],
        passes: [],
        inapplicable: []
      })
    };
    window.axeSetterCalls = 0;
    Object.defineProperty(window, 'axe', {
      configurable: false,
      get() { return window.pageAxe; },
      set() {
        window.axeSetterCalls += 1;
        throw new Error('page axe setter must not run');
      }
    });
    window.amdCalls = 0;
    window.define = () => { window.amdCalls += 1; };
    window.define.amd = {};
    document.getElementById('shadow-host').attachShadow({ mode: 'open' }).innerHTML =
      '<img id="shadow-image" src="missing.png">';
  </script>
</body>
</html>"#;
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "3", "action": "a11y" }), &mut state).await;
    assert_success(&resp);
    let data = get_data(&resp);
    assert_eq!(data["axeVersion"], "4.12.1");
    let image_alt = data["violations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|violation| violation["id"] == "image-alt")
        .expect("vendored axe should report missing image alternatives");
    assert_eq!(image_alt["nodeCount"], 3);
    let nodes = image_alt["nodes"].as_array().unwrap();
    assert!(nodes
        .iter()
        .any(|node| node["target"] == json!(["#light-image"])));
    assert!(nodes
        .iter()
        .any(|node| node["target"] == json!([["#shadow-host", "#shadow-image"]])));
    assert!(nodes
        .iter()
        .any(|node| node["target"] == json!(["#audit-frame", "#frame-image"])));
    let frame_focusable_content = data["violations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|violation| violation["id"] == "frame-focusable-content")
        .expect("audit should preserve the child context for non-focusable frames");
    assert!(frame_focusable_content["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["target"] == json!(["#audit-frame", "html"])));

    let resp = execute_command(
        &json!({ "id": "3-selector", "action": "a11y", "selector": "#shadow-host" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let scoped_image_alt = get_data(&resp)["violations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|violation| violation["id"] == "image-alt")
        .expect("scoped audit should report the shadow image");
    assert_eq!(scoped_image_alt["nodeCount"], 1);

    let resp = execute_command(
        &json!({
            "id": "4",
            "action": "evaluate",
            "script": "[window.axe.version, document.querySelector('#audit-frame').contentWindow.axe.version, window.axeSetterCalls, document.querySelector('#audit-frame').contentWindow.frameAxeSetterCalls, window.amdCalls]"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["result"],
        json!(["spoofed", "frame-spoofed", 0, 0, 0])
    );

    state
        .ref_map
        .add("e999".to_string(), Some(999), "button", "stale", None);
    state.active_frame_id = Some("stale-frame".to_string());
    state
        .iframe_sessions
        .insert("stale-frame".to_string(), "stale-session".to_string());
    let fresh_url = format!(
        "data:text/html;base64,{}",
        STANDARD.encode(
            "<!doctype html><html lang='en'><head><title>Fresh audit</title></head><body><main><h1>Fresh audit</h1></main></body></html>"
        )
    );
    let resp = execute_command(
        &json!({ "id": "5", "action": "a11y", "url": fresh_url }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(state.ref_map.get("e999").is_none());
    assert!(state.active_frame_id.is_none());
    assert!(!state.iframe_sessions.contains_key("stale-frame"));

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_a11y_preserves_nested_frame_sessions_across_tab_switches() {
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": format!("http://localhost:{port}/top")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert!(
        !state.iframe_sessions.is_empty(),
        "cross-origin frame should have an attached target session"
    );
    let top_iframe_sessions = state.active_iframe_sessions.clone();
    assert!(
        !top_iframe_sessions.is_empty(),
        "active frame sessions should include the top tab's cross-origin frame"
    );

    let assert_frame_violations = |resp: &Value| {
        assert_success(resp);
        let image_alt = get_data(resp)["violations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|violation| violation["id"] == "image-alt")
            .unwrap_or_else(|| {
                panic!(
                    "audit should include images in nested cross-origin frames: {}",
                    serde_json::to_string_pretty(resp).unwrap_or_default()
                )
            });
        assert_eq!(image_alt["nodeCount"], 2);
        let nodes = image_alt["nodes"].as_array().unwrap();
        assert!(nodes
            .iter()
            .any(|node| node["target"] == json!(["#outer", "#outer-image"])));
        assert!(nodes
            .iter()
            .any(|node| node["target"] == json!(["#outer", "#inner", "#inner-image"])));
    };

    let resp = execute_command(&json!({ "id": "3", "action": "a11y" }), &mut state).await;
    assert_frame_violations(&resp);

    let resp = execute_command(
        &json!({ "id": "3-selector", "action": "a11y", "selector": "main" }),
        &mut state,
    )
    .await;
    assert_frame_violations(&resp);

    // Simulate a popup created outside the tab commands. Target lifecycle
    // events must move active iframe scoping to the popup and back when it is
    // externally closed.
    let browser_client = state.browser.as_ref().unwrap().client.clone();
    let created = browser_client
        .send_command(
            "Target.createTarget",
            Some(json!({
                "url": format!("http://localhost:{port}/background")
            })),
            None,
        )
        .await
        .unwrap();
    let external_target_id = created["targetId"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let resp = execute_command(
        &json!({ "id": "external-open", "action": "tab_list" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let resp = execute_command(
        &json!({ "id": "external-loaded", "action": "tab_list" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let external_iframe_sessions = state.active_iframe_sessions.clone();
    assert!(!external_iframe_sessions.is_empty());
    assert!(top_iframe_sessions.is_disjoint(&external_iframe_sessions));

    browser_client
        .send_command(
            "Target.closeTarget",
            Some(json!({ "targetId": external_target_id })),
            None,
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let resp = execute_command(
        &json!({ "id": "external-close", "action": "tab_list" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(state.active_iframe_sessions, top_iframe_sessions);

    let resp = execute_command(
        &json!({
            "id": "4",
            "action": "tab_new",
            "url": format!("http://localhost:{port}/background")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let background_iframe_sessions = state.active_iframe_sessions.clone();
    assert!(
        !background_iframe_sessions.is_empty(),
        "active frame sessions should follow the newly opened tab"
    );
    assert!(top_iframe_sessions.is_disjoint(&background_iframe_sessions));
    let resp = execute_command(
        &json!({ "id": "5", "action": "tab_switch", "tabId": "t1" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(state.active_iframe_sessions, top_iframe_sessions);

    let resp = execute_command(&json!({ "id": "6", "action": "a11y" }), &mut state).await;
    assert_frame_violations(&resp);

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_a11y_preserves_sibling_frame_dom_order() {
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "2",
            "action": "navigate",
            "url": format!("http://localhost:{port}/siblings")
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(&json!({ "id": "3", "action": "a11y" }), &mut state).await;
    assert_success(&resp);
    let image_alt = get_data(&resp)["violations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|violation| violation["id"] == "image-alt")
        .expect("audit should report both sibling frame images");
    assert_eq!(image_alt["nodeCount"], 2);
    let targets: Vec<_> = image_alt["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["target"].clone())
        .collect();
    assert_eq!(
        targets,
        vec![
            json!(["#first-frame", "#first-image"]),
            json!(["#second-frame", "#second-image"]),
        ]
    );

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_pushstate_changes_url() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com/" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "pushstate", "url": "/newpath" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let url = get_data(&resp)
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        url.ends_with("/newpath"),
        "Expected pushstate URL to end with /newpath, got: {}",
        url
    );

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

/// Completion URL reads observe the existing Chrome and never create a target
/// or replace a browser that disappears while the model decides to finish.
#[tokio::test]
#[ignore]
async fn e2e_completion_url_never_launches_or_recovers_browser() {
    let (guard, _dir) = binding_test_env();
    let (mut state, ws_url) = launch_binding_host(&guard).await;
    let url = "data:text/html,completion-url";
    let resp = execute_command(
        &json!({ "id": "completion-navigate", "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let cmd = json!({ "id": "completion-read", "action": "url", "existingBrowserOnly": true });
    let resp = execute_command(&cmd, &mut state).await;
    assert_success(&resp);
    assert_eq!(resp["data"]["url"], url);
    assert_eq!(resp["data"]["lifecycle"]["launched"], false);
    assert_eq!(resp["data"]["lifecycle"]["relaunchedBrowser"], false);

    let page = state.browser.as_ref().unwrap().pages_list()[0].clone();
    let client = state.browser.as_ref().unwrap().client.clone();
    let targets_before = client
        .send_command_no_params("Target.getTargets", None)
        .await
        .unwrap();
    state
        .browser
        .as_mut()
        .unwrap()
        .remove_page_by_target_id(&page.target_id);
    let resp = execute_command(&cmd, &mut state).await;
    assert_eq!(resp["success"], false);
    assert_eq!(state.browser.as_ref().unwrap().page_count(), 0);
    let targets_after = client
        .send_command_no_params("Target.getTargets", None)
        .await
        .unwrap();
    assert_eq!(
        targets_before, targets_after,
        "completion must not create or activate a replacement target"
    );

    state.browser.as_mut().unwrap().add_page(page.clone());
    state.browser.as_mut().unwrap().set_pin_tab(true);
    state.pin_tab = true;
    state
        .browser
        .as_mut()
        .unwrap()
        .remove_page_by_target_id(&page.target_id);
    let resp = execute_command(&cmd, &mut state).await;
    assert_error_code(&resp, "tab_gone");
    assert!(state.browser.as_ref().unwrap().bound_target_is_gone());
    state.browser.as_mut().unwrap().add_page(page);

    let _ = client.send_command_no_params("Browser.close", None).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !state.browser.as_mut().unwrap().has_process_exited() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Chrome must exit before the final read");
    let resp = execute_command(&cmd, &mut state).await;
    assert_eq!(resp["success"], false);
    assert!(state.browser.as_mut().unwrap().has_process_exited());
    assert_eq!(state.browser.as_ref().unwrap().get_cdp_url(), ws_url);
    assert!(Arc::ptr_eq(
        &client,
        &state.browser.as_ref().unwrap().client
    ));

    close_current_browser(&mut state).await.unwrap();
    let resp = execute_command(&cmd, &mut state).await;
    assert_eq!(resp["success"], false);
    assert_eq!(resp["error"], "Browser not launched");
    assert!(state.browser.is_none());

    // Ordinary get url retains its established implicit launch behavior.
    let resp = execute_command(
        &json!({ "id": "ordinary-url", "action": "url" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(resp["data"]["url"], "about:blank");
    assert_eq!(resp["data"]["lifecycle"]["launched"], true);
    close_current_browser(&mut state).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn e2e_removeinitscript_roundtrip() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": "https://example.com" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({
            "id": "3",
            "action": "addinitscript",
            "script": "window.__AB_ROUNDTRIP__ = 1;"
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let identifier = get_data(&resp)["identifier"]
        .as_str()
        .expect("addinitscript should return an identifier")
        .to_string();
    assert!(!identifier.is_empty());

    let resp = execute_command(
        &json!({ "id": "4", "action": "removeinitscript", "identifier": identifier }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["removed"], true);

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

// ---------------------------------------------------------------------------
// Session-to-tab binding (--pin-tab): two sessions sharing one Chrome over
// CDP must not hijack each other's tabs. A restarted daemon re-binds to its
// persisted target instead of adopting the most recently active tab, and a
// pinned session whose tab is closed gets a tab_gone error instead of
// silently acting on another session's tab.
// ---------------------------------------------------------------------------

/// Environment guard for binding tests: isolated socket dir (so binding
/// files cannot leak between test runs) plus per-session env vars.
fn binding_test_env() -> (EnvGuard<'static>, tempfile::TempDir) {
    let guard = EnvGuard::new(&[
        "AGENT_BROWSER_SOCKET_DIR",
        "XDG_RUNTIME_DIR",
        "AGENT_BROWSER_NAMESPACE",
        "AGENT_BROWSER_SESSION",
        "AGENT_BROWSER_PIN_TAB",
    ]);
    let dir = tempfile::tempdir().unwrap();
    guard.set("AGENT_BROWSER_SOCKET_DIR", dir.path().to_str().unwrap());
    guard.remove("XDG_RUNTIME_DIR");
    guard.remove("AGENT_BROWSER_NAMESPACE");
    guard.remove("AGENT_BROWSER_PIN_TAB");
    (guard, dir)
}

/// Launch a host Chrome and return (host_state, ws_url) for other sessions
/// to attach to over CDP.
async fn launch_binding_host(guard: &EnvGuard<'static>) -> (DaemonState, String) {
    guard.set("AGENT_BROWSER_SESSION", "e2e-bind-host");
    let mut host = DaemonState::new();
    let resp = execute_command(
        &json!({
            "id": "host-1",
            "action": "launch",
            "headless": true,
            "args": ["--no-sandbox", "--disable-dev-shm-usage"]
        }),
        &mut host,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(&json!({ "id": "host-2", "action": "cdp_url" }), &mut host).await;
    assert_success(&resp);
    let ws_url = get_data(&resp)["cdpUrl"]
        .as_str()
        .expect("cdpUrl should be a string")
        .to_string();
    (host, ws_url)
}

/// Create a pinned session attached to the shared Chrome and navigate its
/// bound tab to `url`. Returns the session's state.
async fn attach_pinned_session(
    guard: &EnvGuard<'static>,
    session: &str,
    ws_url: &str,
    url: &str,
) -> DaemonState {
    guard.set("AGENT_BROWSER_SESSION", session);
    let mut state = DaemonState::new();
    let resp = execute_command(
        &json!({
            "id": format!("{}-launch", session),
            "action": "launch",
            "cdpUrl": ws_url,
            "pinTab": true
        }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({ "id": format!("{}-nav", session), "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    state
}

async fn current_url(state: &mut DaemonState, id: &str) -> Value {
    execute_command(&json!({ "id": id, "action": "url" }), state).await
}

fn load_binding(session: &str, expect: &str) -> super::tab_binding::TabBinding {
    super::tab_binding::load(session)
        .expect("binding file should be readable")
        .expect(expect)
}

#[tokio::test]
#[ignore]
async fn e2e_pin_tab_rebinds_after_daemon_restart() {
    let (guard, _dir) = binding_test_env();
    let (mut host, ws_url) = launch_binding_host(&guard).await;

    let url_a = "data:text/html,session-a-page";
    let url_b = "data:text/html,session-b-page";

    // Session A binds its own tab (pin mode creates a fresh tab on attach).
    let state_a = attach_pinned_session(&guard, "e2e-bind-a", &ws_url, url_a).await;
    let binding_a = load_binding("e2e-bind-a", "session A binding should persist");
    assert!(binding_a.pinned, "binding should record pinned=true");
    assert_eq!(
        binding_a.url, "",
        "opaque URL payloads must not be persisted in diagnostic state"
    );

    // Session B binds its own tab and navigates last, so B's tab is the most
    // recently active one (the tab a naive re-attach would adopt).
    let mut state_b = attach_pinned_session(&guard, "e2e-bind-b", &ws_url, url_b).await;
    let binding_b = load_binding("e2e-bind-b", "session B binding should persist");
    assert_ne!(
        binding_a.target_id, binding_b.target_id,
        "sessions must bind distinct tabs"
    );

    // Simulate session A's daemon dying (idle timeout, crash, kill): drop the
    // state without a clean close. The binding file survives.
    drop(state_a);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A restarted daemon for session A must re-bind to A's original tab by
    // targetId, not adopt B's (most recently active) tab.
    guard.set("AGENT_BROWSER_SESSION", "e2e-bind-a");
    let mut state_a2 = DaemonState::new();
    assert!(
        state_a2.pin_tab,
        "pin-tab should be sticky via the persisted binding"
    );
    let resp = execute_command(
        &json!({ "id": "a2-launch", "action": "launch", "cdpUrl": ws_url }),
        &mut state_a2,
    )
    .await;
    assert_success(&resp);

    let resp = current_url(&mut state_a2, "a2-url").await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["url"],
        url_a,
        "restarted session A must operate on A's original tab"
    );
    let binding_a2 = load_binding("e2e-bind-a", "binding should still exist");
    assert_eq!(
        binding_a2.target_id, binding_a.target_id,
        "re-attach must keep the original bound target"
    );
    assert_eq!(
        binding_a2.url, "",
        "re-attach must not reintroduce the opaque URL payload"
    );

    // Lifecycle-dependent commands must work on the restored tab: attach
    // only enables the CDP domains on the first target, so the restore path
    // must enable them on the bound tab too or navigate hangs waiting for
    // Page lifecycle events (timeout guards against the hang regression).
    let url_a2 = "data:text/html,session-a-page-2";
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        execute_command(
            &json!({ "id": "a2-nav", "action": "navigate", "url": url_a2 }),
            &mut state_a2,
        ),
    )
    .await
    .expect("navigate on the restored tab must not hang");
    assert_success(&resp);
    assert_eq!(get_data(&resp)["targetId"], binding_a.target_id);

    // Session B is unaffected.
    let resp = current_url(&mut state_b, "b-url").await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["url"], url_b);

    let resp = execute_command(&json!({ "id": "host-99", "action": "close" }), &mut host).await;
    assert_success(&resp);
}

// Real-Chrome smoke test: a foreign tab opened via `window.open` inside the
// pinned session's own page must never steal the active tab or overwrite the
// pin binding. NOTE: this does NOT specifically guard finding #3 (the
// `Target.attachedToTarget`-before-`Target.targetCreated` race) — empirically
// in this environment Chrome always delivers `Target.targetCreated` first for
// a `window.open()` popup, so this only exercises the `new_targets` drain
// path in actions.rs (~line 1082), which was never the buggy branch. The
// deterministic, race-independent regression coverage for finding #3 itself
// lives at the unit level: `BrowserManager::register_discovered_page` (the
// single decision point both drain handlers call) is exercised directly by
// `test_register_discovered_page_untracked_target_does_not_steal_pinned_tab`
// in browser.rs, which fails if that function's internal `add_page` vs.
// `add_page_without_activation` choice regresses. This e2e test is kept as a
// general non-regression smoke check against real Chrome, not as #3 proof.
#[tokio::test]
#[ignore]
async fn e2e_auto_attached_foreign_tab_does_not_steal_pinned_tab() {
    let (guard, _dir) = binding_test_env();
    let (mut host, ws_url) = launch_binding_host(&guard).await;

    let url_a = "data:text/html,pinned-session-a";
    let mut state_a = attach_pinned_session(&guard, "e2e-foreign-a", &ws_url, url_a).await;
    let binding_before = load_binding("e2e-foreign-a", "session A binding should persist");

    // Open a foreign tab in the shared browser. This is not an agent command
    // (`tab new`), it is a plain popup — the same shape as a human opening a
    // tab or a page calling `window.open`.
    let resp = execute_command(
        &json!({
            "id": "foreign-open",
            "action": "evaluate",
            "script": "window.open('data:text/html,foreign-popup'); 'opened'"
        }),
        &mut state_a,
    )
    .await;
    assert_success(&resp);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let resp = current_url(&mut state_a, "a-url-after-foreign").await;
    assert_success(&resp);

    let mgr = state_a.browser.as_ref().expect("browser should be running");
    let page_count_before = 1; // the pinned session's own bound tab
    assert!(
        mgr.page_count() > page_count_before,
        "the foreign popup must still register in tab list (got {} pages)",
        mgr.page_count()
    );
    assert_eq!(
        get_data(&resp)["url"],
        url_a,
        "the pinned session's active tab must not be stolen by the foreign popup"
    );
    assert_eq!(
        mgr.bound_target_id(),
        Some(binding_before.target_id.as_str()),
        "the pin binding must not be overwritten by the foreign popup"
    );

    let resp = execute_command(&json!({ "id": "host-99", "action": "close" }), &mut host).await;
    assert_success(&resp);
}

// SCRATCH (finding #3): explicit agent commands must still end up active
// even under the same auto-attach shared browser, proving the fix does not
// regress `tab new`.
#[tokio::test]
#[ignore]
async fn e2e_tab_new_still_activates_under_auto_attach() {
    let (guard, _dir) = binding_test_env();
    let (mut host, ws_url) = launch_binding_host(&guard).await;

    let url_a = "data:text/html,pinned-session-a";
    let mut state_a = attach_pinned_session(&guard, "e2e-tabnew-a", &ws_url, url_a).await;

    let resp = execute_command(
        &json!({
            "id": "tab-new",
            "action": "tab_new",
            "url": "data:text/html,agent-opened-tab"
        }),
        &mut state_a,
    )
    .await;
    assert_success(&resp);

    let resp = current_url(&mut state_a, "a-url-after-tab-new").await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["url"],
        "data:text/html,agent-opened-tab",
        "an explicit `tab new` must activate the new tab it created"
    );

    let resp = execute_command(&json!({ "id": "host-99", "action": "close" }), &mut host).await;
    assert_success(&resp);
}

#[tokio::test]
#[ignore]
async fn e2e_pin_tab_gone_error_and_recovery() {
    let (guard, _dir) = binding_test_env();
    let (mut host, ws_url) = launch_binding_host(&guard).await;

    let url_a = "data:text/html,gone-session-a";
    let url_b = "data:text/html,gone-session-b";

    let mut state_a = attach_pinned_session(&guard, "e2e-gone-a", &ws_url, url_a).await;
    let binding_a = load_binding("e2e-gone-a", "session A binding should persist");
    let mut state_b = attach_pinned_session(&guard, "e2e-gone-b", &ws_url, url_b).await;

    // Session B closes A's tab by targetId (targetIds are accepted anywhere a
    // tab ref is accepted and are stable across daemons).
    let resp = execute_command(
        &json!({ "id": "b-close-a", "action": "tab_close", "tabId": binding_a.target_id }),
        &mut state_b,
    )
    .await;
    assert_success(&resp);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Session A must now fail loudly with a machine-readable tab_gone error
    // instead of silently adopting a neighboring tab.
    let resp = current_url(&mut state_a, "a-url-gone").await;
    assert_eq!(resp["success"], false);
    assert_eq!(
        resp["code"],
        "tab_gone",
        "response should carry code=tab_gone, got: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );
    let err = resp["error"].as_str().unwrap_or("");
    assert!(
        err.starts_with(super::browser::TAB_GONE_PREFIX),
        "error should start with the tab_gone prefix, got: {}",
        err
    );
    assert_eq!(resp["data"]["targetId"], binding_a.target_id);
    assert!(
        resp["data"].get("lastUrl").is_none(),
        "opaque URLs must not be exposed in structured diagnostics: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );

    // Recovery commands still work in the gone state: tab_list is allowed,
    // and tab_new binds a fresh tab.
    let resp = execute_command(
        &json!({ "id": "a-list", "action": "tab_list" }),
        &mut state_a,
    )
    .await;
    assert_success(&resp);

    let url_a2 = "data:text/html,recovered-session-a";
    let resp = execute_command(
        &json!({ "id": "a-new", "action": "tab_new", "url": url_a2 }),
        &mut state_a,
    )
    .await;
    assert_success(&resp);

    let resp = current_url(&mut state_a, "a-url-recovered").await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["url"], url_a2);

    let binding_a2 = load_binding("e2e-gone-a", "binding should be rewritten");
    assert_ne!(
        binding_a2.target_id, binding_a.target_id,
        "recovery must bind a new target"
    );
    assert!(binding_a2.pinned, "recovered binding stays pinned");

    // Session B keeps working on its own tab throughout.
    let resp = current_url(&mut state_b, "b-url-after").await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["url"], url_b);

    let resp = execute_command(&json!({ "id": "host-99", "action": "close" }), &mut host).await;
    assert_success(&resp);
}

/// A multiselect locator miss must surface the anchored "No element found"
/// guidance, not a raw "Evaluation error: ...". Guards the handler wiring end to
/// end: a unit test of the mapping alone stays green if the handler stops routing
/// misses through it. Force-red: drop the sentinel miss handling in
/// handle_multiselect and this assertion fails on the raw evaluate error.
#[tokio::test]
#[ignore]
async fn e2e_multiselect_miss_surfaces_anchored_error() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // about:blank has no #picker, so the selector misses.
    let resp = execute_command(
        &json!({ "id": "2", "action": "multiselect", "selector": "#picker", "values": ["a"] }),
        &mut state,
    )
    .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "multiselect on a missing selector should error: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );
    let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("");
    // Assert the full anchored shape, not just the guidance suffix: the miss must
    // carry "No element found", retain the "#picker" selector detail, and end with
    // the locator-miss guidance. A generic suffix-only check would pass even if the
    // detail were dropped or a different classifier produced the guidance.
    assert!(
        err.starts_with("No element found") && err.contains("#picker"),
        "miss should keep the anchored shape and selector detail, got: {err}"
    );
    assert!(
        err.contains("Verify the selector, role, or name is correct"),
        "miss should surface the anchored locator-miss guidance, got: {err}"
    );

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

/// An earlier valid ARIA token in a multi-token role attribute is the operative
/// role, so `role="mark none"` is a mark and must not answer `find role none`.
/// Force-red: drop `mark` from the presentational VALID_ROLES set and the query
/// matches this element, so the miss assertion fails.
#[tokio::test]
#[ignore]
async fn e2e_presentational_role_respects_earlier_operative_token() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let html = "<html><body><span role='mark none'>marked</span></body></html>";
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "getbyrole", "role": "none", "subaction": "text" }),
        &mut state,
    )
    .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "operative role is `mark`, so `find role none` must not match: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

/// `find role directory` must match an explicit `role="directory"` element but
/// not an ordinary list. Chrome collapses `directory` into the `list` AX role,
/// so this goes through the DOM-attribute path, not the AX tree. Force-red:
/// route `directory` back through the AX tree and the explicit element is missed
/// (its AX role is `list`), so the found-text assertion fails.
#[tokio::test]
#[ignore]
async fn e2e_find_role_directory_matches_only_explicit_attribute() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // A plain list must not be matched by `find role directory`.
    let plain = "data:text/html;base64,".to_string()
        + &STANDARD.encode("<html><body><ul><li>item</li></ul></body></html>");
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": plain }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({ "id": "3", "action": "getbyrole", "role": "directory", "subaction": "text" }),
        &mut state,
    )
    .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "a plain <ul> must not match `find role directory`: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );

    // An explicit role="directory" element is found.
    let explicit = "data:text/html;base64,".to_string()
        + &STANDARD.encode("<html><body><div role='directory'>DIR</div></body></html>");
    let resp = execute_command(
        &json!({ "id": "4", "action": "navigate", "url": explicit }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({ "id": "5", "action": "getbyrole", "role": "directory", "subaction": "text" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["text"], "DIR");

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

/// The presentational DOM lookup must honor the selected frame, not always the
/// top document. Force-red: revert the frame dispatch in the presentational path
/// (search the top document only) and the in-frame element is missed.
#[tokio::test]
#[ignore]
async fn e2e_presentational_role_honors_selected_frame() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Top document has no role="none"; the same-origin iframe does. `srcdoc`
    // inherits the parent origin, so the child is same-origin (the path this fix
    // targets) without needing a server; a `data:` child would be cross-origin.
    let outer = "<body><iframe id='f' srcdoc=\"<div role='none'>INSIDE</div>\"></iframe></body>";
    let url = format!("data:text/html;base64,{}", STANDARD.encode(outer));
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Before selecting the frame, the top document has no match.
    let resp = execute_command(
        &json!({ "id": "3", "action": "getbyrole", "role": "none", "subaction": "text" }),
        &mut state,
    )
    .await;
    assert_eq!(
        resp.get("success").and_then(|v| v.as_bool()),
        Some(false),
        "top document has no role=none: {}",
        serde_json::to_string_pretty(&resp).unwrap_or_default()
    );

    // Select the frame; the presentational lookup must now find the frame element.
    let resp = execute_command(
        &json!({ "id": "4", "action": "frame", "selector": "#f" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    let resp = execute_command(
        &json!({ "id": "5", "action": "getbyrole", "role": "none", "subaction": "text" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(get_data(&resp)["text"], "INSIDE");

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

/// ARIA presentational-roles conflict resolution: role="none" on a focusable
/// element (or one with global ARIA props) is ignored, so `find role none` must
/// skip it but still match a truly presentational element. Force-red: drop the
/// conflict check and the `<button role="none">` is matched.
#[tokio::test]
#[ignore]
async fn e2e_presentational_role_respects_conflict_resolution() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // Each non-matching element triggers conflict resolution a different way:
    // native focusable, explicit tabindex=-1 (programmatically focusable), and a
    // global ARIA property. Only the last, truly presentational div must match.
    let html = concat!(
        "<body>",
        "<button role='none'>NativeFocusable</button>",
        "<div role='none' tabindex='-1'>TabindexFocusable</div>",
        "<div role='none' aria-label='named'>GlobalAria</div>",
        "<div role='none'>Plain</div>",
        "</body>"
    );
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    // The focusable button keeps its implicit role, so the match must be the div.
    let resp = execute_command(
        &json!({ "id": "3", "action": "getbyrole", "role": "none", "subaction": "text" }),
        &mut state,
    )
    .await;
    assert_success(&resp);
    assert_eq!(
        get_data(&resp)["text"],
        "Plain",
        "role=none on a focusable button must be ignored (conflict resolution)"
    );

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

/// `find role document` must match the page root. Chrome exposes it as
/// `RootWebArea`; without the normalization to `document` the query misses.
/// End-to-end guard for that mapping (the unit test only checks the string).
#[tokio::test]
#[ignore]
async fn e2e_find_role_document_matches_root() {
    let mut state = DaemonState::new();

    let resp = execute_command(
        &json!({ "id": "1", "action": "launch", "headless": true }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let url = format!(
        "data:text/html;base64,{}",
        STANDARD.encode("<title>T</title><body>hi</body>")
    );
    let resp = execute_command(
        &json!({ "id": "2", "action": "navigate", "url": url }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let resp = execute_command(
        &json!({ "id": "3", "action": "getbyrole", "role": "document", "subaction": "text" }),
        &mut state,
    )
    .await;
    assert_success(&resp);

    let _ = execute_command(&json!({ "id": "99", "action": "close" }), &mut state).await;
}

#[tokio::test]
#[ignore]
async fn e2e_mouse_interpolation_starts_at_last_element_interaction() {
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(
            &json!({"id": "1", "action": "launch", "headless": true}),
            &mut state,
        )
        .await,
    );
    assert_success(&execute_command(&json!({"id": "2", "action": "setcontent", "html": "<button id='b' style='position:absolute;left:400px;top:300px;width:100px;height:40px'>Target</button><input id='c' type='checkbox' style='position:absolute;left:200px;top:200px'>"}), &mut state).await);
    for action in ["click", "hover", "dblclick", "check", "uncheck"] {
        let selector = if matches!(action, "check" | "uncheck") {
            "#c"
        } else {
            "#b"
        };
        assert_success(
            &execute_command(
                &json!({"id": "3", "action": action, "selector": selector}),
                &mut state,
            )
            .await,
        );
        let start = (state.mouse_state.x, state.mouse_state.y);
        assert!(start.0 >= 200.0 && start.1 >= 200.0, "{action}: {start:?}");
        assert_success(&execute_command(&json!({"id": "4", "action": "evaluate", "script": "window.moves=[];document.onmousemove=e=>moves.push([e.clientX,e.clientY]);"}), &mut state).await);
        assert_success(&execute_command(&json!({"id": "5", "action": "mousemove", "x": start.0 + 100.0, "y": start.1, "steps": 2}), &mut state).await);
        let result = execute_command(
            &json!({"id": "6", "action": "evaluate", "script": "moves"}),
            &mut state,
        )
        .await;
        assert_success(&result);
        let moves = get_data(&result)["result"].as_array().unwrap();
        assert_eq!(moves.len(), 2, "{action}: {moves:?}");
        assert!((moves[0][0].as_f64().unwrap() - (start.0 + 50.0)).abs() <= 1.0);
        assert!((moves[1][0].as_f64().unwrap() - (start.0 + 100.0)).abs() <= 1.0);
    }
    assert_success(&execute_command(&json!({"id": "99", "action": "close"}), &mut state).await);
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_dispatches_the_checked_fractional_point() {
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({"action":"launch", "headless":true}), &mut state).await,
    );
    let html = r#"<!doctype html><html><body>
<script>window.clicks = {target:0, cover:0}</script>
<button id="target" style="position:absolute;left:19.4px;top:20px;width:100px;height:30px;padding:0;border:0"
    onclick="window.clicks.target++">Fractional target</button>
<div id="cover" style="position:absolute;left:70.3px;top:20px;width:30px;height:30px"
    onclick="window.clicks.cover++">Cover</div>
</body></html>"#;
    assert_success(
        &execute_command(
            &json!({"action":"navigate", "url":format!("data:text/html;base64,{}", STANDARD.encode(html))}),
            &mut state,
        )
        .await,
    );
    // CDP's integer hit-test point hits the button, while the fractional box
    // center hits the neighboring overlay. Input must use the checked point.
    assert_evaluate(
        &mut state,
        "geometry",
        "(() => { const rect = document.getElementById('target').getBoundingClientRect(); const x = rect.x + rect.width / 2; const y = rect.y + rect.height / 2; return [document.elementFromPoint(x, y).id, document.elementFromPoint(Math.round(x), Math.round(y)).id]; })()",
        json!(["cover", "target"]),
    )
    .await;
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    let reference = get_data(&snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Fractional target")
        .expect("fractional target must be exposed by the Rust snapshot")
        .0
        .clone();
    let clicked = execute_command(
        &json!({"action":"click", "selector":format!("@{reference}")}),
        &mut state,
    )
    .await;
    let first_counters = execute_command(
        &json!({"action":"evaluate", "script":"window.clicks"}),
        &mut state,
    )
    .await;
    // An overlay that also covers the chosen integer point must still block.
    assert_evaluate(
        &mut state,
        "cover-target",
        "Object.assign(document.getElementById('cover').style, {left:'19px', width:'101px'}); true",
        json!(true),
    )
    .await;
    let covered = execute_command(
        &json!({"action":"click", "selector":format!("@{reference}")}),
        &mut state,
    )
    .await;
    let final_counters = execute_command(
        &json!({"action":"evaluate", "script":"window.clicks"}),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);

    assert_success(&clicked);
    assert_success(&first_counters);
    assert_eq!(
        get_data(&first_counters)["result"],
        json!({"target":1,"cover":0}),
        "the dispatched point must match the point accepted by interception"
    );
    assert_eq!(
        covered["success"], false,
        "covering overlay must reject: {covered}"
    );
    assert!(covered["error"]
        .as_str()
        .unwrap_or_default()
        .contains("covered by <div#cover>"));
    assert_success(&final_counters);
    assert_eq!(
        get_data(&final_counters)["result"],
        json!({"target":1,"cover":0})
    );
}

fn reference_named(snapshot: &Value, name: &str) -> String {
    get_data(snapshot)["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == name)
        .unwrap_or_else(|| panic!("missing snapshot reference for {name}: {snapshot}"))
        .0
        .clone()
}

async fn reference_action(
    state: &mut DaemonState,
    snapshot: &Value,
    name: &str,
    action: &str,
) -> Value {
    execute_command(
        &json!({"action":action, "selector":format!("@{}", reference_named(snapshot, name))}),
        state,
    )
    .await
}

async fn reference_relationship_page(state: &mut DaemonState, html: &str) -> Value {
    assert_success(&execute_command(&json!({"action":"launch", "headless":true}), state).await);
    assert_success(&execute_command(&json!({"action":"navigate", "url":format!("data:text/html;base64,{}", STANDARD.encode(html))}), state).await);
    let snapshot = execute_command(&json!({"action":"snapshot"}), state).await;
    assert_success(&snapshot);
    snapshot
}

fn serve_reference_relationship_pages(
    listener: tokio::net::TcpListener,
    parent: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let parent = parent.clone();
            tokio::spawn(async move {
                let mut buf = [0; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let child = r#"<!doctype html><body style="margin:0"><button style="position:absolute;inset:0;width:100%;height:100%" onclick="top.postMessage({kind:'click',name:window.name},'*')">Embedded button</button><script>top.postMessage({kind:'ready',name:window.name},'*')</script>"#;
                let body = if request.starts_with("GET /child ") {
                    child
                } else {
                    &parent
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nOrigin-Agent-Cluster: ?0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    })
}

async fn frame_relationship_fixture(
    topology: &str,
) -> (
    DaemonState,
    Value,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let child_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let child_port = child_listener.local_addr().unwrap().port();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let child_url = match topology {
        "same-origin" => format!("http://localhost:{port}/child"),
        "same-session" => format!("http://localhost:{child_port}/child"),
        "oopif" => format!("http://127.0.0.1:{child_port}/child"),
        _ => panic!("unknown topology {topology}"),
    };
    let html = format!(
        r#"<!doctype html><body style="margin:0">
<script>
window.hits = {{region:0,card:0,custom:0,ad:0,covered:0,overlay:0}}; window.ready = new Set();
addEventListener('message', e => {{
    const frame = document.querySelector('iframe[name="' + e.data.name + '"]');
    if (!frame || frame.contentWindow !== e.source) return;
    if (e.data.kind === 'ready') ready.add(e.data.name);
    if (e.data.kind === 'click') hits[e.data.name]++;
}});
customElements.define('fancy-frame', class FancyFrame extends HTMLIFrameElement {{}}, {{extends:'iframe'}});
</script>
<div role="region" aria-label="Embed region" style="position:absolute;left:20px;top:20px;width:200px;height:80px">
<iframe name="region" title="Region frame" src="{child_url}" style="width:100%;height:100%;border:0"></iframe></div>
<div role="button" aria-label="Frame card" tabindex="0" style="position:absolute;left:20px;top:120px;width:200px;height:80px">
<iframe name="card" title="Card frame" src="{child_url}" style="width:100%;height:100%;border:0"></iframe></div>
<iframe is="fancy-frame" name="custom" title="Custom frame" src="{child_url}" style="position:absolute;left:20px;top:220px;width:200px;height:80px;border:0"></iframe>
<button aria-label="Frame covered target" onclick="hits.covered++" style="position:absolute;left:20px;top:320px;width:200px;height:80px">Covered</button>
<iframe id="ad-frame" name="ad" title="Ad frame" src="{child_url}" style="position:absolute;left:20px;top:320px;width:200px;height:80px;border:0"></iframe>
</body>"#
    );
    let child_server = serve_reference_relationship_pages(child_listener, String::new());
    let parent_server = serve_reference_relationship_pages(listener, html);
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({"action":"launch", "headless":true}), &mut state).await,
    );
    assert_success(
        &execute_command(
            &json!({"action":"navigate", "url":format!("http://localhost:{port}/")}),
            &mut state,
        )
        .await,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let ready = execute_command(
                &json!({"action":"evaluate", "script":"window.ready.size === 4"}),
                &mut state,
            )
            .await;
            assert_success(&ready);
            if get_data(&ready)["result"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("frame relationship fixtures did not load");
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    assert_eq!(
        state.iframe_sessions.len(),
        if topology == "oopif" { 4 } else { 0 },
        "unexpected fixture topology: {topology}"
    );
    assert_evaluate(
        &mut state,
        "custom-owner",
        "document.querySelector('iframe[is]').constructor.name",
        json!("FancyFrame"),
    )
    .await;
    (state, snapshot, parent_server, child_server)
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_frame_containers() {
    let mut outcomes = Vec::new();
    for topology in ["same-origin", "same-session", "oopif"] {
        let (mut state, snapshot, parent_server, child_server) =
            frame_relationship_fixture(topology).await;
        for name in ["Embed region", "Frame card"] {
            for action in ["hover", "click"] {
                outcomes.push((
                    format!("{topology}: {action} {name}"),
                    reference_action(&mut state, &snapshot, name, action).await,
                ));
            }
        }
        let counts = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve(window.hits), 50))"}), &mut state).await;
        assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
        parent_server.abort();
        child_server.abort();
        outcomes.push((format!("{topology}: counters"), json!({"success": get_data(&counts)["result"] == json!({"region":1,"card":1,"custom":0,"ad":0,"covered":0,"overlay":0}), "counters":counts})));
    }
    for (case, outcome) in outcomes {
        assert_eq!(outcome["success"], true, "{case}: {outcome}");
    }
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_custom_frame_owner() {
    let (mut state, snapshot, parent_server, child_server) =
        frame_relationship_fixture("same-session").await;
    let click = reference_action(&mut state, &snapshot, "Custom frame", "click").await;
    assert_success(&execute_command(&json!({"action":"evaluate", "script":"const overlay = document.createElement('div'); overlay.id = 'custom-overlay'; overlay.style = 'position:absolute;left:20px;top:220px;width:200px;height:80px;z-index:10'; overlay.onclick = () => hits.overlay++; document.body.append(overlay);"}), &mut state).await);
    let covered = reference_action(&mut state, &snapshot, "Custom frame", "click").await;
    let counts = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve(window.hits), 50))"}), &mut state).await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    parent_server.abort();
    child_server.abort();
    assert_success(&click);
    assert_eq!(
        covered["success"], false,
        "custom iframe must reject unrelated overlay: {covered}"
    );
    assert!(
        covered["error"]
            .as_str()
            .unwrap_or_default()
            .contains("covered by <div#custom-overlay>"),
        "{covered}"
    );
    assert_eq!(
        get_data(&counts)["result"],
        json!({"region":0,"card":0,"custom":1,"ad":0,"covered":0,"overlay":0})
    );
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_reports_frame_owner_blockers() {
    let mut outcomes = Vec::new();
    for topology in ["same-origin", "same-session", "oopif"] {
        let (mut state, snapshot, parent_server, child_server) =
            frame_relationship_fixture(topology).await;
        let covered =
            reference_action(&mut state, &snapshot, "Frame covered target", "click").await;
        let counts = execute_command(
            &json!({"action":"evaluate", "script":"window.hits"}),
            &mut state,
        )
        .await;
        assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
        parent_server.abort();
        child_server.abort();
        outcomes.push((topology, covered, counts));
    }
    for (topology, covered, counts) in outcomes {
        assert_eq!(covered["success"], false, "{topology}: {covered}");
        assert!(
            covered["error"]
                .as_str()
                .unwrap_or_default()
                .contains("covered by <iframe#ad-frame>"),
            "{topology} must describe an addressable owner: {covered}"
        );
        assert_eq!(
            get_data(&counts)["result"],
            json!({"region":0,"card":0,"custom":0,"ad":0,"covered":0,"overlay":0})
        );
    }
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_slotted_shadow_controls() {
    let html = r#"<!doctype html><body><script>window.hits = {nestedOpen:0,nestedClosed:0,topOpen:0,topClosed:0};</script>
<div id="outer"></div><script>
const root = document.getElementById('outer').attachShadow({mode:'open'});
for (const [index, [name, mode, nested]] of [['nestedOpen','open',true],['nestedClosed','closed',true],['topOpen','open',false],['topClosed','closed',false]].entries()) {
    const inner = document.createElement('div'); inner.id = 'inner-' + name;
    inner.style = 'position:absolute;left:20px;top:' + (20 + index * 80) + 'px;width:200px;height:50px';
    inner.innerHTML = '<span style="display:block;width:200px;height:50px">Slotted ' + name + ' target</span>';
    const shadow = inner.attachShadow({mode});
    shadow.innerHTML = '<button style="padding:0;width:200px;height:50px"><slot></slot></button>';
    shadow.querySelector('button').onclick = () => hits[name]++;
    (nested ? root : document.body).append(inner);
}
</script></body>"#;
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, html).await;
    let mut clicks = Vec::new();
    for name in ["nestedOpen", "nestedClosed", "topOpen", "topClosed"] {
        clicks.push(
            reference_action(
                &mut state,
                &snapshot,
                &format!("Slotted {name} target"),
                "click",
            )
            .await,
        );
    }
    let counts = execute_command(
        &json!({"action":"evaluate", "script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    for click in clicks {
        assert_success(&click);
    }
    assert_eq!(
        get_data(&counts)["result"],
        json!({"nestedOpen":1,"nestedClosed":1,"topOpen":1,"topClosed":1})
    );
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_shadow_component_controls() {
    let html = r#"<!doctype html><body><script>
window.hits = {open:0,closed:0,ripple:0}; window.checked = {};
for (const [index, mode] of ['open','closed'].entries()) {
    const host = document.createElement('div');
    host.style = 'position:absolute;left:20px;top:' + (20 + index * 80) + 'px;width:60px;height:30px';
    const root = host.attachShadow({mode});
    root.innerHTML = '<input type="checkbox" aria-label="' + mode + ' toggle" style="position:absolute;inset:0;margin:0;width:60px;height:30px;opacity:.01"><span class="track" style="position:absolute;inset:0;background:#ccc"></span>';
    const input = root.querySelector('input');
    Object.defineProperty(checked, mode, {enumerable:true,get:() => input.checked});
    host.addEventListener('click', event => { if (event.composedPath()[0] !== input) input.checked = !input.checked; hits[mode]++; });
    document.body.append(host);
}
const rippleHost = document.createElement('div');
rippleHost.style = 'position:absolute;left:20px;top:180px;width:160px;height:40px';
const rippleRoot = rippleHost.attachShadow({mode:'open'});
rippleRoot.innerHTML = '<button style="width:160px;height:40px">Ripple target</button><div id="ripple" style="position:absolute;inset:0"></div>';
rippleRoot.getElementById('ripple').attachShadow({mode:'closed'}).innerHTML = '<div class="ripple-surface" style="position:absolute;inset:0;background:#ccc"></div>';
rippleHost.addEventListener('click', () => hits.ripple++);
document.body.append(rippleHost);
</script></body>"#;
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, html).await;
    let mut outcomes = Vec::new();
    for mode in ["open", "closed"] {
        for (action, checked) in [("check", true), ("uncheck", false)] {
            let result =
                reference_action(&mut state, &snapshot, &format!("{mode} toggle"), action).await;
            let state_result = execute_command(
                &json!({"action":"evaluate", "script":format!("window.checked.{mode}")}),
                &mut state,
            )
            .await;
            outcomes.push((mode, action, result, state_result, checked));
        }
    }
    let ripple = reference_action(&mut state, &snapshot, "Ripple target", "click").await;
    let counts = execute_command(
        &json!({"action":"evaluate", "script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    for (mode, action, outcome, state_result, checked) in outcomes {
        assert_eq!(outcome["success"], true, "{mode} {action}: {outcome}");
        assert_eq!(
            get_data(&state_result)["result"],
            checked,
            "{mode} {action} did not change the actual control"
        );
    }
    assert_success(&ripple);
    assert_eq!(
        get_data(&counts)["result"],
        json!({"open":2,"closed":2,"ripple":1})
    );
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_reports_closed_shadow_blockers() {
    let html = r#"<!doctype html><body><script>window.hits = {target:0,shadow:0,overlay:0};</script>
<button style="position:absolute;left:20px;top:20px;width:200px;height:50px" onclick="hits.target++">Covered light target</button>
<div id="target-host" style="position:absolute;left:20px;top:100px;width:200px;height:50px"></div>
<consent-banner id="consent" style="position:absolute;left:0;top:0;width:300px;height:180px;display:block;z-index:10"></consent-banner>
<script>
document.getElementById('target-host').attachShadow({mode:'open'}).innerHTML = '<button style="width:200px;height:50px" onclick="window.hits.shadow++">Covered shadow target</button>';
const consent = document.getElementById('consent');
consent.attachShadow({mode:'closed'}).innerHTML = '<div class="panel inner" style="position:absolute;inset:0;background:#ccc"></div>';
consent.onclick = () => hits.overlay++;
</script></body>"#;
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, html).await;
    let light = reference_action(&mut state, &snapshot, "Covered light target", "click").await;
    let shadow = reference_action(&mut state, &snapshot, "Covered shadow target", "click").await;
    let counts = execute_command(
        &json!({"action":"evaluate", "script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    for covered in [light, shadow] {
        assert_eq!(
            covered["success"], false,
            "unrelated shadow overlay must block: {covered}"
        );
        assert!(
            covered["error"]
                .as_str()
                .unwrap_or_default()
                .contains("covered by <consent-banner#consent>"),
            "closed-root blocker must describe the addressable host: {covered}"
        );
    }
    assert_eq!(
        get_data(&counts)["result"],
        json!({"target":0,"shadow":0,"overlay":0})
    );
}

/// A common outer shadow host does not make separate components related.
#[tokio::test]
#[ignore]
async fn e2e_reference_click_rejects_sibling_shadow_components() {
    let html = r#"<!doctype html><body style="margin:0"><script>
window.hits = Array.from({length:8}, () => ({target:0,overlay:0}));
window.overlays = [];
let index = 0;
for (const shellMode of ['open','closed']) {
    for (const targetMode of ['open','closed']) {
        for (const overlayMode of ['open','closed']) {
            const i = index++;
            const shell = document.createElement('app-shell');
            shell.style = 'position:absolute;left:' + (20 + Math.floor(i / 4) * 240) + 'px;top:' + (20 + (i % 4) * 90) + 'px;width:200px;height:50px';
            const shellRoot = shell.attachShadow({mode:shellMode});
            const component = document.createElement('my-button');
            component.style = 'position:absolute;inset:0';
            const root = component.attachShadow({mode:targetMode});
            root.innerHTML = '<button style="position:absolute;inset:0;width:200px;height:50px">Shell target ' + i + '</button><span class="internal-visual" style="position:absolute;inset:0;background:#ddd"></span>';
            component.onclick = () => hits[i].target++;
            const overlay = document.createElement('cookie-dialog');
            overlay.id = 'dialog-' + i;
            overlay.style = 'position:absolute;inset:0;z-index:10';
            overlay.attachShadow({mode:overlayMode}).innerHTML = '<div class="panel" style="position:absolute;inset:0;background:#bbb">Consent</div>';
            overlay.onclick = () => hits[i].overlay++;
            overlays.push(overlay);
            shellRoot.append(component, overlay);
            document.body.append(shell);
        }
    }
}
</script></body>"#;
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, html).await;
    let mut covered = Vec::new();
    for index in 0..8 {
        covered.push(
            reference_action(
                &mut state,
                &snapshot,
                &format!("Shell target {index}"),
                "click",
            )
            .await,
        );
    }
    let covered_counts = execute_command(
        &json!({"action":"evaluate", "script":"window.hits"}),
        &mut state,
    )
    .await;
    // The same refs must still allow the target component's own visual once
    // the separate overlay component has gone away.
    assert_evaluate(
        &mut state,
        "remove-sibling-components",
        "window.overlays.forEach(overlay => overlay.remove()); true",
        json!(true),
    )
    .await;
    let mut internal = Vec::new();
    for index in 0..8 {
        internal.push(
            reference_action(
                &mut state,
                &snapshot,
                &format!("Shell target {index}"),
                "click",
            )
            .await,
        );
    }
    let internal_counts = execute_command(
        &json!({"action":"evaluate", "script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    for (index, response) in covered.iter().enumerate() {
        assert_eq!(
            response["success"], false,
            "separate component {index} must intercept: {response}"
        );
        assert!(
            response["error"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!("covered by <cookie-dialog#dialog-{index}>")),
            "blocker must be visible from the target's tree scope: {response}"
        );
    }
    assert_success(&covered_counts);
    assert_eq!(
        get_data(&covered_counts)["result"],
        json!(vec![json!({"target":0,"overlay":0}); 8])
    );
    for response in internal {
        assert_success(&response);
    }
    assert_success(&internal_counts);
    assert_eq!(
        get_data(&internal_counts)["result"],
        json!(vec![json!({"target":1,"overlay":0}); 8])
    );
}

/// The target can be assigned into the hit's closed root, so slot recovery
/// must inspect the hit's roots as well as the target's roots.
#[tokio::test]
#[ignore]
async fn e2e_reference_click_slotted_target_internal_visuals() {
    let html = r#"<!doctype html><body style="margin:0"><script>
window.hits = [0,0]; window.controls = [];
for (const [index, mode] of ['open','closed'].entries()) {
    const field = document.createElement('fancy-field');
    field.style = 'position:absolute;left:20px;top:' + (20 + index * 80) + 'px;width:200px;height:40px';
    const input = document.createElement('input');
    input.type = 'checkbox'; input.setAttribute('aria-label', mode + ' slotted toggle');
    input.style = 'position:absolute;inset:0;margin:0;width:200px;height:40px';
    field.append(input);
    const root = field.attachShadow({mode});
    root.innerHTML = '<slot></slot><span class="decor" style="position:absolute;inset:0;background:#ccc"></span>';
    field.onclick = event => {
        if (event.composedPath()[0] !== input) input.checked = !input.checked;
        hits[index]++;
    };
    controls.push(input);
    document.body.append(field);
}
</script></body>"#;
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, html).await;
    assert_evaluate(
        &mut state,
        "slot-visibility",
        "window.controls.map(input => input.assignedSlot !== null)",
        json!([true, false]),
    )
    .await;
    let mut outcomes = Vec::new();
    for (index, mode) in ["open", "closed"].into_iter().enumerate() {
        for (action, checked) in [("check", true), ("uncheck", false)] {
            let result = reference_action(
                &mut state,
                &snapshot,
                &format!("{mode} slotted toggle"),
                action,
            )
            .await;
            let checked_result = execute_command(
                &json!({"action":"evaluate", "script":format!("window.controls[{index}].checked")}),
                &mut state,
            )
            .await;
            outcomes.push((mode, action, result, checked_result, checked));
        }
    }
    let counts = execute_command(
        &json!({"action":"evaluate", "script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    for (mode, action, response, checked_result, checked) in outcomes {
        assert_eq!(response["success"], true, "{mode} {action}: {response}");
        assert_success(&checked_result);
        assert_eq!(get_data(&checked_result)["result"], checked);
    }
    assert_success(&counts);
    assert_eq!(get_data(&counts)["result"], json!([2, 2]));
}

fn serve_frame_blocker_pages(
    listener: tokio::net::TcpListener,
    pages: Arc<std::collections::HashMap<&'static str, String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let pages = Arc::clone(&pages);
            tokio::spawn(async move {
                let mut buf = [0; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = pages.get(path).map(String::as_str).unwrap_or("");
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nOrigin-Agent-Cluster: ?0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    })
}

fn reference_frame_path(tree: &Value, frame_id: &str) -> Option<Vec<String>> {
    let id = tree["frame"]["id"].as_str()?;
    if id == frame_id {
        return Some(vec![id.to_string()]);
    }
    tree["childFrames"].as_array()?.iter().find_map(|child| {
        let mut path = reference_frame_path(child, frame_id)?;
        path.insert(0, id.to_string());
        Some(path)
    })
}

/// The hit belongs to a different branch under an embedded common document.
/// The blocker must name that branch's owner, including when the hit is nested
/// another level deeper, while leaving both click counters unchanged.
async fn assert_reference_click_cross_branch_blocker(topology: &str, cousins: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let other_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other_port = other_listener.local_addr().unwrap().port();
    let origin = format!("http://localhost:{port}");
    let other_origin = if topology == "same-origin" {
        origin.clone()
    } else {
        format!("http://localhost:{other_port}")
    };
    let root = r#"<!doctype html><body style="margin:0"><script>
window.hits = {target:0,overlay:0}; window.ready = {};
addEventListener('message', e => {
    if (!['target','overlay'].includes(e.data.name)) return;
    const common = document.getElementById('common-wrapper').contentWindow;
    const branch = common.frames[e.data.name === 'target' ? 0 : 1];
    const expected = __COUSINS__ ? branch.frames[0] : branch;
    if (e.source !== expected) return;
    if (e.data.kind === 'ready') ready[e.data.name] = e.data;
    if (e.data.kind === 'click') hits[e.data.name]++;
});
</script><iframe id="common-wrapper" title="Common wrapper" src="/common" style="position:absolute;left:40px;top:40px;width:600px;height:240px;border:0"></iframe>"#.replace("__COUSINS__", if cousins { "true" } else { "false" });
    let target_url = format!(
        "{origin}/{}",
        if cousins {
            "target-branch"
        } else {
            "target-leaf"
        }
    );
    let overlay_url = format!(
        "{other_origin}/{}",
        if cousins {
            "overlay-branch"
        } else {
            "overlay-leaf"
        }
    );
    let common = format!(
        r#"<!doctype html><body style="margin:0">
<iframe id="target-branch" title="Target branch" src="{target_url}" style="position:absolute;left:20px;top:20px;width:240px;height:120px;border:0"></iframe>
<iframe id="covering-branch" title="Covering branch" src="{overlay_url}" style="position:absolute;left:320px;top:20px;width:240px;height:120px;border:0;z-index:10"></iframe>"#
    );
    let mut pages = std::collections::HashMap::from([("/", root), ("/common", common)]);
    for name in ["target", "overlay"] {
        let label = if name == "target" {
            "Cross branch target"
        } else {
            "Cross branch overlay"
        };
        let leaf = format!(
            r#"<!doctype html><body style="margin:0"><button id="{name}-internal" style="position:absolute;inset:0;width:100%;height:100%" onclick="top.postMessage({{kind:'click',name:'{name}'}},'*')">{label}</button><script>top.postMessage({{kind:'ready',name:'{name}',origin:location.origin,parentAccessible:!!frameElement}},'*')</script>"#
        );
        // Cross-origin cousins reverse the origins again at the leaf, proving
        // the diagnostic chooses the common document's owner, not this owner.
        let leaf_origin = if name == "target" {
            &other_origin
        } else {
            &origin
        };
        let branch = format!(
            r#"<!doctype html><body style="margin:0"><iframe id="{name}-leaf" title="Nested {name}" src="{leaf_origin}/{name}-leaf" style="position:absolute;inset:0;width:100%;height:100%;border:0"></iframe>"#
        );
        if name == "target" {
            pages.insert("/target-leaf", leaf);
            pages.insert("/target-branch", branch);
        } else {
            pages.insert("/overlay-leaf", leaf);
            pages.insert("/overlay-branch", branch);
        }
    }
    let pages = Arc::new(pages);
    let server = serve_frame_blocker_pages(listener, Arc::clone(&pages));
    let other_server = serve_frame_blocker_pages(other_listener, pages);
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({"action":"launch", "headless":true}), &mut state).await,
    );
    assert_success(
        &execute_command(
            &json!({"action":"navigate", "url":format!("{origin}/")}),
            &mut state,
        )
        .await,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let ready = execute_command(
                &json!({"action":"evaluate", "script":"!!(ready.target && ready.overlay)"}),
                &mut state,
            )
            .await;
            assert_success(&ready);
            if get_data(&ready)["result"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("cross-branch frame fixtures did not load");
    assert_evaluate(&mut state, "verify-origins", "[ready.target.origin === ready.overlay.origin, ready.target.parentAccessible, ready.overlay.parentAccessible]", json!([topology == "same-origin", topology == "same-origin" || !cousins, topology == "same-origin"])).await;
    // The default snapshot expands only one iframe level. Select each leaf
    // through the real frame command to obtain its own Rust snapshot refs.
    assert_success(
        &execute_command(
            &json!({"action":"frame", "url":"/overlay-leaf"}),
            &mut state,
        )
        .await,
    );
    let overlay_snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&overlay_snapshot);
    let overlay = state
        .ref_map
        .get(&reference_named(&overlay_snapshot, "Cross branch overlay"))
        .unwrap()
        .clone();
    assert_success(
        &execute_command(&json!({"action":"frame", "url":"/target-leaf"}), &mut state).await,
    );
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    assert!(
        state.iframe_sessions.is_empty(),
        "{topology} must use one CDP session"
    );
    let target = state
        .ref_map
        .get(&reference_named(&snapshot, "Cross branch target"))
        .unwrap()
        .clone();
    assert_eq!(state.active_frame_id, target.frame_id);
    assert_success(&execute_command(&json!({"action":"mainframe"}), &mut state).await);
    {
        let browser = state.browser.as_ref().unwrap();
        let session = browser.active_session_id().unwrap();
        let tree = browser
            .client
            .send_command("Page.getFrameTree", None, Some(session))
            .await
            .unwrap();
        let target_path =
            reference_frame_path(&tree["frameTree"], target.frame_id.as_deref().unwrap()).unwrap();
        let overlay_path =
            reference_frame_path(&tree["frameTree"], overlay.frame_id.as_deref().unwrap()).unwrap();
        assert_eq!(target_path.len(), if cousins { 4 } else { 3 });
        assert_eq!(overlay_path.len(), target_path.len());
        assert_eq!(
            target_path[..2],
            overlay_path[..2],
            "common ancestor must be an embedded frame"
        );
        assert_ne!(
            target_path[2], overlay_path[2],
            "target and overlay must occupy separate branches"
        );
    }
    assert_evaluate(
        &mut state,
        "uncovered-geometry",
        "document.getElementById('common-wrapper').contentDocument.elementFromPoint(140,80).id",
        json!("target-branch"),
    )
    .await;
    let uncovered = reference_action(&mut state, &snapshot, "Cross branch target", "click").await;
    let before = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve({...hits}), 50))"}), &mut state).await;
    assert_evaluate(&mut state, "cover-target", "const common = document.getElementById('common-wrapper').contentDocument; common.getElementById('covering-branch').style.left = '20px'; common.elementFromPoint(140,80).id", json!("covering-branch")).await;
    {
        let browser = state.browser.as_ref().unwrap();
        let session = browser.active_session_id().unwrap();
        let model = browser
            .client
            .send_command(
                "DOM.getBoxModel",
                Some(json!({"backendNodeId":target.backend_node_id.unwrap()})),
                Some(session),
            )
            .await
            .unwrap();
        let quad = model["model"]["border"].as_array().unwrap();
        let x = (quad[0].as_f64().unwrap() + quad[4].as_f64().unwrap()) / 2.0;
        let y = (quad[1].as_f64().unwrap() + quad[5].as_f64().unwrap()) / 2.0;
        let hit = browser
            .client
            .send_command(
                "DOM.getNodeForLocation",
                Some(json!({"x":x.round() as i64, "y":y.round() as i64})),
                Some(session),
            )
            .await
            .unwrap();
        assert_eq!(
            hit["frameId"],
            overlay.frame_id.as_deref().unwrap(),
            "native hit must enter the covering branch"
        );
        assert_eq!(
            hit["backendNodeId"],
            overlay.backend_node_id.unwrap(),
            "fixture must hit the internal overlay button, not a frame border"
        );
    }
    let covered = reference_action(&mut state, &snapshot, "Cross branch target", "click").await;
    let after = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve({...hits}), 50))"}), &mut state).await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    server.abort();
    other_server.abort();
    assert_success(&uncovered);
    assert_success(&before);
    assert_success(&after);
    assert_eq!(get_data(&before)["result"], json!({"target":1,"overlay":0}));
    assert_eq!(
        covered["success"], false,
        "{topology}, cousins={cousins}: {covered}"
    );
    assert!(covered["error"].as_str().unwrap_or_default().contains("covered by <iframe#covering-branch>"), "{topology}, cousins={cousins}: must name the hit-side owner in the lowest common ancestor, got {covered}");
    assert_eq!(
        get_data(&after)["result"],
        get_data(&before)["result"],
        "a rejected click must reach neither handler"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_reports_sibling_frame_blockers() {
    for topology in ["same-origin", "same-session"] {
        assert_reference_click_cross_branch_blocker(topology, false).await;
    }
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_reports_cousin_frame_blockers() {
    for topology in ["same-origin", "same-session"] {
        assert_reference_click_cross_branch_blocker(topology, true).await;
    }
}

/// A target assigned to a closed component still belongs to that component,
/// even when only a sibling overlay's deep node is available to JavaScript.
#[tokio::test]
#[ignore]
async fn e2e_reference_click_rejects_overlays_above_slotted_components() {
    let html = r#"<!doctype html><body style="margin:0"><script>
window.hits = Array.from({length:4}, () => ({target:0,overlay:0}));
window.controls = []; window.slottables = []; window.overlays = [];
let index = 0;
for (const mode of ['open','closed']) {
    for (const wrapped of [false,true]) {
        const i = index++;
        const shell = document.createElement('app-shell');
        shell.style = 'position:absolute;left:20px;top:' + (20 + i * 80) + 'px;width:200px;height:40px';
        const shellRoot = shell.attachShadow({mode:'open'});
        const field = document.createElement('fancy-field');
        field.style = 'position:absolute;inset:0';
        const input = document.createElement('input');
        input.type = 'checkbox';
        input.setAttribute('aria-label', 'Protected slotted toggle ' + i);
        input.style = 'position:absolute;inset:0;margin:0;width:200px;height:40px';
        if (wrapped) {
            const wrapper = document.createElement('div');
            wrapper.style = 'position:absolute;inset:0';
            wrapper.append(input);
            field.append(wrapper);
            slottables.push(wrapper);
        } else {
            field.append(input);
            slottables.push(input);
        }
        const root = field.attachShadow({mode});
        root.innerHTML = '<slot></slot><span class="decor" style="position:absolute;inset:0;background:#ddd"></span>';
        field.onclick = event => {
            if (event.composedPath()[0] !== input) input.checked = !input.checked;
            hits[i].target++;
        };
        const overlay = document.createElement('cookie-dialog');
        overlay.id = 'slotted-dialog-' + i;
        overlay.style = 'position:absolute;inset:0;z-index:10';
        overlay.attachShadow({mode:'closed'}).innerHTML = '<div class="panel" style="position:absolute;inset:0;background:#bbb">Consent</div>';
        overlay.onclick = () => hits[i].overlay++;
        controls.push(input); overlays.push(overlay);
        shellRoot.append(field, overlay);
        document.body.append(shell);
    }
}
</script></body>"#;
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, html).await;
    assert_evaluate(
        &mut state,
        "hidden-component-slots",
        "window.slottables.map(node => node.assignedSlot !== null)",
        json!([true, true, false, false]),
    )
    .await;
    let mut covered = Vec::new();
    for index in 0..4 {
        covered.push(
            reference_action(
                &mut state,
                &snapshot,
                &format!("Protected slotted toggle {index}"),
                "click",
            )
            .await,
        );
    }
    let covered_counts = execute_command(
        &json!({"action":"evaluate", "script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_evaluate(
        &mut state,
        "remove-slotted-component-overlays",
        "window.overlays.forEach(overlay => overlay.remove()); true",
        json!(true),
    )
    .await;
    let mut outcomes = Vec::new();
    for index in 0..4 {
        for (action, checked) in [("check", true), ("uncheck", false)] {
            let response = reference_action(
                &mut state,
                &snapshot,
                &format!("Protected slotted toggle {index}"),
                action,
            )
            .await;
            let checked_result = execute_command(
                &json!({"action":"evaluate", "script":format!("window.controls[{index}].checked")}),
                &mut state,
            )
            .await;
            outcomes.push((index, action, response, checked_result, checked));
        }
    }
    let final_counts = execute_command(
        &json!({"action":"evaluate", "script":"window.hits"}),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    for (index, response) in covered.iter().enumerate() {
        assert_eq!(
            response["success"], false,
            "overlay above slotted component {index} must intercept: {response}"
        );
        assert!(
            response["error"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!(
                    "covered by <cookie-dialog#slotted-dialog-{index}>"
                )),
            "blocker must name the separate component: {response}"
        );
    }
    assert_success(&covered_counts);
    assert_eq!(
        get_data(&covered_counts)["result"],
        json!(vec![json!({"target":0,"overlay":0}); 4])
    );
    for (index, action, response, checked_result, checked) in outcomes {
        assert_eq!(
            response["success"], true,
            "component {index} {action}: {response}"
        );
        assert_success(&checked_result);
        assert_eq!(get_data(&checked_result)["result"], checked);
    }
    assert_success(&final_counts);
    assert_eq!(
        get_data(&final_counts)["result"],
        json!(vec![json!({"target":2,"overlay":0}); 4])
    );
}

/// Built-in UA slots must not replace the nearest author component, and
/// walking through them must not make a sibling overlay component related.
async fn assert_reference_click_builtin_component_visuals(kind: &str) {
    let html = r#"<!doctype html><body style="margin:0"><script>
window.hits = [{target:0,overlay:0},{target:0,overlay:0}];
window.controls = []; window.overlays = [];
const kind = 'BUILTIN_KIND';
for (const [i, mode] of ['open','closed'].entries()) {
    const shell = document.createElement('app-shell');
    shell.style = 'display:block;position:absolute;left:20px;top:' + (20 + i * 120) + 'px;width:240px;height:80px';
    const shellRoot = shell.attachShadow({mode});
    const component = document.createElement('fancy-field');
    component.style = 'display:block;position:absolute;inset:0';
    const root = component.attachShadow({mode});
    const inputHtml = '<input type="checkbox" aria-label="' + mode + ' ' + kind + ' toggle" style="width:40px;height:40px;margin:0">';
    root.innerHTML = (kind === 'details'
        ? '<details open><summary>Question</summary>' + inputHtml + '</details>'
        : '<details><summary>' + inputHtml + '</summary>Body</details>')
        + '<span class="visual" style="position:absolute;inset:0;background:#ddd"></span>';
    const input = root.querySelector('input');
    component.onclick = event => {
        if (event.composedPath()[0] !== input) input.checked = !input.checked;
        hits[i].target++;
    };
    const overlay = document.createElement('cookie-dialog');
    overlay.id = 'builtin-dialog-' + i;
    overlay.style = 'display:block;position:absolute;inset:0;z-index:10';
    overlay.attachShadow({mode:'closed'}).innerHTML = '<div class="panel" style="position:absolute;inset:0;background:#bbb">Consent</div>';
    overlay.onclick = () => hits[i].overlay++;
    controls.push(input); overlays.push(overlay);
    shellRoot.append(component, overlay);
    document.body.append(shell);
}
</script></body>"#.replace("BUILTIN_KIND", kind);
    let mut state = DaemonState::new();
    let snapshot = reference_relationship_page(&mut state, &html).await;
    assert_evaluate(
        &mut state,
        "builtin-component-geometry",
        "window.controls.every(input => { const r = input.getBoundingClientRect(); const v = input.getRootNode().querySelector('.visual').getBoundingClientRect(); return r.width === 40 && r.height === 40 && r.left >= v.left && r.right <= v.right && r.top >= v.top && r.bottom <= v.bottom && r.top >= 0 && r.bottom < innerHeight; })",
        json!(true),
    )
    .await;
    let mut covered = Vec::new();
    for mode in ["open", "closed"] {
        covered.push(
            reference_action(
                &mut state,
                &snapshot,
                &format!("{mode} {kind} toggle"),
                "check",
            )
            .await,
        );
    }
    let covered_state = execute_command(
        &json!({"action":"evaluate", "script":"({hits, checked:controls.map(input => input.checked)})"}),
        &mut state,
    )
    .await;
    assert_evaluate(
        &mut state,
        "remove-builtin-component-overlays",
        "window.overlays.forEach(overlay => overlay.remove()); true",
        json!(true),
    )
    .await;
    let mut outcomes = Vec::new();
    for (index, mode) in ["open", "closed"].iter().enumerate() {
        for (action, checked) in [("check", true), ("uncheck", false)] {
            let response = reference_action(
                &mut state,
                &snapshot,
                &format!("{mode} {kind} toggle"),
                action,
            )
            .await;
            let checked_result = execute_command(
                &json!({"action":"evaluate", "script":format!("window.controls[{index}].checked")}),
                &mut state,
            )
            .await;
            outcomes.push((mode, action, response, checked_result, checked));
        }
    }
    let final_state = execute_command(
        &json!({"action":"evaluate", "script":"({hits, checked:controls.map(input => input.checked)})"}),
        &mut state,
    )
    .await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    for (index, response) in covered.iter().enumerate() {
        assert_eq!(
            response["success"], false,
            "separate overlay above {kind} component {index} must intercept: {response}"
        );
        assert!(
            response["error"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!(
                    "covered by <cookie-dialog#builtin-dialog-{index}>"
                )),
            "blocker must name the separate component: {response}"
        );
    }
    assert_success(&covered_state);
    assert_eq!(
        get_data(&covered_state)["result"],
        json!({"hits":[{"target":0,"overlay":0},{"target":0,"overlay":0}],"checked":[false,false]})
    );
    for (mode, action, response, checked_result, checked) in outcomes {
        assert_eq!(
            response["success"], true,
            "{mode} {kind} component {action}: {response}"
        );
        assert_success(&checked_result);
        assert_eq!(get_data(&checked_result)["result"], checked);
    }
    assert_success(&final_state);
    assert_eq!(
        get_data(&final_state)["result"],
        json!({"hits":[{"target":2,"overlay":0},{"target":2,"overlay":0}],"checked":[false,false]})
    );
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_details_component_visuals() {
    assert_reference_click_builtin_component_visuals("details").await;
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_summary_component_visuals() {
    assert_reference_click_builtin_component_visuals("summary").await;
}

/// A cross-document hit must be described in the scopes of the target-side
/// frame owner, even when both branches share a closed shell below the top frame.
async fn assert_reference_click_shadow_frame_blocker(
    topology: &str,
    mode: &str,
    frame_overlay: bool,
    cousins: bool,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let other_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other_port = other_listener.local_addr().unwrap().port();
    let origin = format!("http://localhost:{port}");
    let leaf_origin = if topology == "same-origin" {
        origin.clone()
    } else {
        format!("http://localhost:{other_port}")
    };
    let root = r#"<!doctype html><body style="margin:0"><script>
window.hits = {target:0,overlay:0}; window.ready = {};
addEventListener('message', e => {
    if (!e.data || !['target','overlay'].includes(e.data.name)) return;
    const common = document.getElementById('common-wrapper').contentWindow;
    const branch = common.fixtureFrames[e.data.name];
    const expected = common.cousins ? branch.contentWindow.frames[0] : branch.contentWindow;
    if (e.source !== expected) return;
    if (e.data.kind === 'ready') ready[e.data.name] = e.data;
    if (e.data.kind === 'click') hits[e.data.name]++;
});
</script><iframe id="common-wrapper" title="Shared shell document" src="/common" style="position:absolute;left:40px;top:40px;width:560px;height:200px;border:0"></iframe>"#.to_string();
    let target_url = if cousins {
        format!("{origin}/target-branch")
    } else {
        format!("{leaf_origin}/target-leaf")
    };
    let overlay_url = if cousins {
        format!("{origin}/overlay-branch")
    } else {
        format!("{leaf_origin}/overlay-leaf")
    };
    let overlay = if frame_overlay {
        format!(
            r#"<iframe id="chat-frame" title="Chat frame" src="{overlay_url}" style="position:absolute;left:300px;top:20px;width:200px;height:100px;border:0;z-index:10"></iframe>"#
        )
    } else {
        r#"<cookie-dialog id="consent" style="display:block;position:absolute;left:300px;top:20px;width:200px;height:100px;z-index:10"></cookie-dialog>"#.to_string()
    };
    let common = format!(
        r#"<!doctype html><body style="margin:0"><app-shell id="shell" style="display:block;position:absolute;inset:0"></app-shell><script>
window.cousins = {cousins};
window.fixtureRoot = document.getElementById('shell').attachShadow({{mode:'{mode}'}});
fixtureRoot.innerHTML = '<iframe id="target-frame" title="Target frame" src="{target_url}" style="position:absolute;left:20px;top:20px;width:200px;height:100px;border:0"></iframe>{overlay}';
window.fixtureFrames = {{target:fixtureRoot.getElementById('target-frame'),overlay:fixtureRoot.getElementById('chat-frame')}};
window.covering = fixtureRoot.getElementById('{overlay_id}');
if (!{frame_overlay}) {{
    covering.attachShadow({{mode:'closed'}}).innerHTML = '<div id="consent-panel" style="position:absolute;inset:0;background:#ddd"></div>';
    covering.onclick = () => top.hits.overlay++;
}}
</script>"#,
        overlay_id = if frame_overlay {
            "chat-frame"
        } else {
            "consent"
        },
    );
    let mut pages = std::collections::HashMap::from([("/", root), ("/common", common)]);
    for name in ["target", "overlay"] {
        let label = if name == "target" {
            "Shared shell target"
        } else {
            "Shared shell chat"
        };
        let leaf = format!(
            r#"<!doctype html><body style="margin:0"><button id="{name}-internal" style="position:absolute;inset:0;width:100%;height:100%;margin:0;padding:0;border:0" onclick="top.postMessage({{kind:'click',name:'{name}'}},'*')">{label}</button><script>top.postMessage({{kind:'ready',name:'{name}',origin:location.origin,parentAccessible:!!frameElement}},'*')</script>"#
        );
        let branch = format!(
            r#"<!doctype html><body style="margin:0"><iframe id="{name}-leaf" title="Nested {name}" src="{leaf_origin}/{name}-leaf" style="position:absolute;inset:0;width:100%;height:100%;border:0"></iframe>"#
        );
        if name == "target" {
            pages.insert("/target-leaf", leaf);
            pages.insert("/target-branch", branch);
        } else {
            pages.insert("/overlay-leaf", leaf);
            pages.insert("/overlay-branch", branch);
        }
    }
    let pages = Arc::new(pages);
    let server = serve_frame_blocker_pages(listener, Arc::clone(&pages));
    let other_server = serve_frame_blocker_pages(other_listener, pages);
    let mut state = DaemonState::new();
    assert_success(
        &execute_command(&json!({"action":"launch", "headless":true}), &mut state).await,
    );
    assert_success(
        &execute_command(
            &json!({"action":"navigate", "url":format!("{origin}/")}),
            &mut state,
        )
        .await,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let ready = execute_command(&json!({"action":"evaluate", "script":format!("!!(ready.target && ({} || ready.overlay))", !frame_overlay)}), &mut state).await;
            assert_success(&ready);
            if get_data(&ready)["result"] == true { break; }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }).await.expect("shared shadow shell frames did not load");
    assert_evaluate(&mut state, "verify-shared-shell-topology", "[ready.target.origin === location.origin, ready.target.parentAccessible, document.getElementById('common-wrapper').contentDocument.getElementById('shell').shadowRoot !== null]", json!([topology == "same-origin", topology == "same-origin", mode == "open"])).await;
    let overlay_entry = if frame_overlay {
        assert_success(
            &execute_command(
                &json!({"action":"frame", "url":"/overlay-leaf"}),
                &mut state,
            )
            .await,
        );
        let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
        assert_success(&snapshot);
        Some(
            state
                .ref_map
                .get(&reference_named(&snapshot, "Shared shell chat"))
                .unwrap()
                .clone(),
        )
    } else {
        None
    };
    // Selecting the leaf uses the real frame command and snapshot ref path;
    // default snapshots do not expand every nested iframe automatically.
    assert_success(
        &execute_command(&json!({"action":"frame", "url":"/target-leaf"}), &mut state).await,
    );
    let snapshot = execute_command(&json!({"action":"snapshot"}), &mut state).await;
    assert_success(&snapshot);
    let target = state
        .ref_map
        .get(&reference_named(&snapshot, "Shared shell target"))
        .unwrap()
        .clone();
    assert_eq!(state.active_frame_id, target.frame_id);
    assert!(
        state.iframe_sessions.is_empty(),
        "{topology} must share a CDP session"
    );
    assert_success(&execute_command(&json!({"action":"mainframe"}), &mut state).await);
    let common_frame = {
        let browser = state.browser.as_ref().unwrap();
        let session = browser.active_session_id().unwrap();
        let tree = browser
            .client
            .send_command("Page.getFrameTree", None, Some(session))
            .await
            .unwrap();
        let target_path =
            reference_frame_path(&tree["frameTree"], target.frame_id.as_deref().unwrap()).unwrap();
        assert_eq!(target_path.len(), if cousins { 4 } else { 3 });
        if let Some(overlay) = &overlay_entry {
            let overlay_path =
                reference_frame_path(&tree["frameTree"], overlay.frame_id.as_deref().unwrap())
                    .unwrap();
            assert_eq!(target_path.len(), overlay_path.len());
            assert_eq!(target_path[..2], overlay_path[..2]);
            assert_ne!(
                target_path[2], overlay_path[2],
                "the hit must occupy a separate frame branch"
            );
        }
        target_path[1].clone()
    };
    assert_evaluate(&mut state, "uncovered-shared-shell", "document.getElementById('common-wrapper').contentWindow.fixtureRoot.elementFromPoint(120,70).id", json!("target-frame")).await;
    let uncovered = reference_action(&mut state, &snapshot, "Shared shell target", "click").await;
    let before = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve({...hits}), 50))"}), &mut state).await;
    assert_evaluate(&mut state, "cover-shared-shell-target", "const common = document.getElementById('common-wrapper').contentWindow; common.covering.style.left = '20px'; common.fixtureRoot.elementFromPoint(120,70).id", json!(if frame_overlay { "chat-frame" } else { "consent" })).await;
    {
        let browser = state.browser.as_ref().unwrap();
        let session = browser.active_session_id().unwrap();
        let model = browser
            .client
            .send_command(
                "DOM.getBoxModel",
                Some(json!({"backendNodeId":target.backend_node_id.unwrap()})),
                Some(session),
            )
            .await
            .unwrap();
        let quad = model["model"]["border"].as_array().unwrap();
        let x = (quad[0].as_f64().unwrap() + quad[4].as_f64().unwrap()) / 2.0;
        let y = (quad[1].as_f64().unwrap() + quad[5].as_f64().unwrap()) / 2.0;
        let hit = browser
            .client
            .send_command(
                "DOM.getNodeForLocation",
                Some(json!({"x":x.round() as i64,"y":y.round() as i64})),
                Some(session),
            )
            .await
            .unwrap();
        if let Some(overlay) = &overlay_entry {
            assert_eq!(
                hit["frameId"],
                overlay.frame_id.as_deref().unwrap(),
                "native hit must enter the separate covering branch"
            );
            assert_eq!(
                hit["backendNodeId"],
                overlay.backend_node_id.unwrap(),
                "native hit must reach the internal chat button"
            );
        } else {
            assert_eq!(
                hit["frameId"], common_frame,
                "component hit must belong to the embedded ancestor document"
            );
            let node = browser
                .client
                .send_command(
                    "DOM.describeNode",
                    Some(json!({"backendNodeId":hit["backendNodeId"],"depth":0})),
                    Some(session),
                )
                .await
                .unwrap();
            assert!(
                node["node"]["attributes"]
                    .as_array()
                    .unwrap()
                    .chunks(2)
                    .any(|pair| pair == [json!("id"), json!("consent-panel")]),
                "native hit must enter the dialog's closed root: {node}"
            );
        }
    }
    let covered = reference_action(&mut state, &snapshot, "Shared shell target", "click").await;
    let after = execute_command(&json!({"action":"evaluate", "script":"new Promise(resolve => setTimeout(() => resolve({...hits}), 50))"}), &mut state).await;
    assert_success(&execute_command(&json!({"action":"close"}), &mut state).await);
    server.abort();
    other_server.abort();
    assert_success(&uncovered);
    assert_success(&before);
    assert_success(&after);
    assert_eq!(get_data(&before)["result"], json!({"target":1,"overlay":0}));
    assert_eq!(
        covered["success"], false,
        "{topology} {mode}, frame_overlay={frame_overlay}, cousins={cousins}: {covered}"
    );
    let blocker = if frame_overlay {
        "iframe#chat-frame"
    } else {
        "cookie-dialog#consent"
    };
    assert!(covered["error"].as_str().unwrap_or_default().contains(&format!("covered by <{blocker}>")), "{topology} {mode}, cousins={cousins}: must name the addressable covering element, got {covered}");
    assert_eq!(
        get_data(&after)["result"],
        get_data(&before)["result"],
        "rejection must dispatch to neither target nor blocker"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_reports_frame_blockers_in_shared_shadow_shells() {
    for topology in ["same-origin", "same-session"] {
        for mode in ["open", "closed"] {
            for cousins in [false, true] {
                assert_reference_click_shadow_frame_blocker(topology, mode, true, cousins).await;
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn e2e_reference_click_reports_ancestor_blockers_in_shared_shadow_shells() {
    for topology in ["same-origin", "same-session"] {
        for mode in ["open", "closed"] {
            assert_reference_click_shadow_frame_blocker(topology, mode, false, false).await;
        }
    }
}
