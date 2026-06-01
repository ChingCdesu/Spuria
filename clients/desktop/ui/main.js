// Spuria desktop frontend. Uses the global Tauri API (withGlobalTauri = true).
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// Per-connection fields persisted for convenience (NOT the global settings,
// which live in settings.json via the backend).
const CONN_FIELDS = ["device-override", "peer", "listen", "rdp"];
let role = "controller";

// ---------- view switching ----------
function showView(view) {
  $("view-connect").classList.toggle("hidden", view !== "connect");
  $("view-settings").classList.toggle("hidden", view !== "settings");
  document.querySelectorAll(".nav").forEach((b) => b.classList.toggle("active", b.dataset.view === view));
}
document.querySelectorAll(".nav").forEach((b) => (b.onclick = () => showView(b.dataset.view)));

// ---------- connect-form persistence ----------
function loadConnFields() {
  for (const f of CONN_FIELDS) {
    const v = localStorage.getItem("spuria." + f);
    if (v !== null) $(f).value = v;
  }
  $("force-relay").checked = localStorage.getItem("spuria.force-relay") === "1";
  setRole(localStorage.getItem("spuria.role") || "controller");
}
function saveConnFields() {
  for (const f of CONN_FIELDS) localStorage.setItem("spuria." + f, $(f).value);
  localStorage.setItem("spuria.force-relay", $("force-relay").checked ? "1" : "0");
  localStorage.setItem("spuria.role", role);
}
function setRole(r) {
  role = r;
  document.querySelectorAll(".role").forEach((b) => b.classList.toggle("active", b.dataset.role === r));
  document.querySelectorAll(".controller-only").forEach((e) => e.classList.toggle("hidden", r !== "controller"));
  document.querySelectorAll(".host-only").forEach((e) => e.classList.toggle("hidden", r !== "host"));
}
document.querySelectorAll(".role").forEach((b) => (b.onclick = () => { setRole(b.dataset.role); saveConnFields(); }));

// ---------- global settings (persisted on disk by the backend) ----------
async function loadSettings() {
  try {
    const s = await invoke("get_settings");
    $("server").value = s.server || "";
    $("reflect").value = s.reflect || "";
    $("secret").value = s.secret || "";
    $("not-configured").classList.toggle("hidden", !!(s.server && s.server.trim()));
  } catch (e) {
    log("failed to load settings: " + e, "err");
  }
}
$("save-settings").onclick = async () => {
  try {
    await invoke("save_settings", {
      settings: {
        server: $("server").value.trim(),
        reflect: $("reflect").value.trim(),
        secret: $("secret").value,
      },
    });
    $("settings-status").textContent = "saved ✓";
    $("not-configured").classList.toggle("hidden", !!$("server").value.trim());
    setTimeout(() => ($("settings-status").textContent = ""), 2000);
  } catch (e) {
    $("settings-status").textContent = "save failed: " + e;
  }
};

// ---------- status + log ----------
function setState(text, cls) {
  const el = $("state");
  el.textContent = text;
  el.className = "state " + cls;
}
function log(text, cls = "") {
  const row = document.createElement("div");
  row.className = "row";
  row.innerHTML = `<span class="t">${new Date().toLocaleTimeString()}</span><span class="${cls}"></span>`;
  row.lastChild.textContent = text;
  $("log").appendChild(row);
  $("log").scrollTop = $("log").scrollHeight;
}
$("clear-log").onclick = () => ($("log").innerHTML = "");

// ---------- client events from the backend ----------
function describe(ev) {
  switch (ev.kind) {
    case "registered": return ["registered as " + ev.device_id, "ok"];
    case "session_started": return ["session " + ev.session_id + " with peer " + ev.peer_id, ""];
    case "tunnel_up": return ["tunnel up via " + ev.path.toUpperCase(), "ok"];
    case "rdp_ready":
      showReady(ev.listen_addr);
      return ["RDP ready — connect your client to " + ev.listen_addr, "ok"];
    case "host_bridging": return ["bridging to local RDP at " + ev.rdp_addr, "ok"];
    case "session_ended": return ["session ended" + (ev.error ? ": " + ev.error : ""), ev.error ? "warn" : ""];
    case "error": return ["error: " + ev.message, "err"];
    default: return [JSON.stringify(ev), ""];
  }
}
function showReady(addr) {
  const r = $("ready");
  r.classList.remove("hidden");
  r.innerHTML = `Tunnel ready. Point your RDP client (mstsc) at <span class="mono">${addr}</span>`;
}

listen("client-event", (e) => {
  const [text, cls] = describe(e.payload);
  log(text, cls);
  if (e.payload.kind === "tunnel_up") setState("connected (" + e.payload.path + ")", "up");
  if (e.payload.kind === "error") setState("error", "error");
});
listen("client-error", (e) => { log("fatal: " + e.payload, "err"); setState("error", "error"); });
listen("client-stopped", () => { setState("idle", "idle"); connected(false); });

// ---------- connect / disconnect ----------
function connected(on) {
  $("connect").disabled = on;
  $("disconnect").disabled = !on;
}

$("connect").onclick = async () => {
  saveConnFields();
  $("ready").classList.add("hidden");
  setState("connecting…", "connecting");
  connected(true);
  try {
    const id = await invoke("connect", {
      opts: {
        role,
        device_id: $("device-override").value.trim() || null,
        peer_id: role === "controller" ? $("peer").value.trim() || null : null,
        listen: $("listen").value.trim() || null,
        rdp: $("rdp").value.trim() || null,
        force_relay: $("force-relay").checked,
      },
    });
    $("device-id").textContent = id;
    log("connecting as " + id + " (" + role + ")");
  } catch (e) {
    log("connect failed: " + e, "err");
    setState("error", "error");
    connected(false);
    if (String(e).includes("Settings")) showView("settings");
  }
};

$("disconnect").onclick = async () => {
  await invoke("disconnect");
  setState("idle", "idle");
  connected(false);
  log("disconnected");
};

$("check-update").onclick = async () => {
  log("checking for updates…");
  try { log(await invoke("check_update"), "ok"); }
  catch (e) { log("update check failed: " + e, "warn"); }
};

// ---------- init ----------
loadConnFields();
loadSettings();
invoke("ensure_device_id").then((id) => ($("device-id").textContent = id)).catch(() => {});
