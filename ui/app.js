// Frontend for mouser.
//
// Talks to the Rust side through four Tauri commands. Polling `snapshot`
// rather than subscribing to events is deliberate: the snapshot is small,
// the backend has to pump captured input on every call anyway, and one
// coherent object per poll cannot render a half-applied state transition.

const POLL_MS = 100;

const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);

const el = (id) => document.getElementById(id);

/** Last sequence number rendered, so the log only appends new lines. */
let lastSeq = 0;
/** Last rendered snapshot, used to skip DOM writes that would not change. */
let previous = {};

async function refresh() {
  let snap;
  try {
    snap = await invoke("snapshot");
  } catch (err) {
    // The backend is gone; stop polling rather than filling the console with
    // identical failures.
    console.error("snapshot failed", err);
    clearInterval(timer);
    return;
  }

  const state = snap.remote_active
    ? "remote"
    : snap.peer_addr
      ? "local"
      : "solo";
  setText("owner", `${state} ${state === "remote" ? "*" : state === "local" ? "+" : "-"}`);
  el("owner").dataset.state = state;

  setText("device", snap.device_name);
  setText(
    "role",
    snap.role === "host"
      ? "host  (waits for the other machine)"
      : `client -> ${snap.peer_addr ?? "?"}`,
  );
  setText(
    "link",
    snap.peer_addr
      ? snap.round_trip_ms != null
        ? `${snap.peer_addr}  ${snap.round_trip_ms.toFixed(0)}ms`
        : snap.peer_addr
      : "not connected",
  );
  setText("peer", snap.peer_name || "-");
  setText("fingerprint", snap.fingerprint ? `[${snap.fingerprint}]` : "-");

  el("bind-row").hidden = !snap.listening;
  setText("bind", snap.bind_addr);

  if (snap.edge !== previous.edge) {
    for (const button of document.querySelectorAll("[data-edge]")) {
      button.setAttribute("aria-pressed", String(button.dataset.edge === snap.edge));
    }
  }

  if (snap.has_secret !== previous.has_secret) {
    setText("pair", snap.has_secret ? "change" : "pair");
  }

  appendLog(snap.log, snap.next_seq);
  previous = snap;
}

/** Write only when the value changed, to avoid needless layout work. */
function setText(id, value) {
  const node = el(id);
  const text = value ?? "";
  if (node.textContent !== text) {
    node.textContent = text;
  }
}

/** Append only lines newer than the last one rendered. */
function appendLog(lines, nextSeq) {
  if (nextSeq === lastSeq) {
    return;
  }

  const list = el("log");
  const wasAtBottom =
    list.scrollHeight - list.scrollTop - list.clientHeight < 24;

  for (const line of lines) {
    if (line.seq <= lastSeq) {
      continue;
    }
    const item = document.createElement("li");
    item.className = line.level;
    item.textContent = line.text;
    list.append(item);
  }

  lastSeq = nextSeq;

  // Cap the DOM so a long session does not grow the log without limit.
  while (list.childElementCount > 250) {
    list.firstElementChild.remove();
  }

  if (wasAtBottom) {
    list.scrollTop = list.scrollHeight;
  }
}

for (const button of document.querySelectorAll("[data-edge]")) {
  button.addEventListener("click", async () => {
    try {
      await invoke("set_edge", { edge: button.dataset.edge });
      previous = {};
      await refresh();
    } catch (err) {
      console.error("set_edge failed", err);
    }
  });
}

el("pair").addEventListener("click", async () => {
  const input = el("secret");
  const secret = input.value.trim();
  if (!secret) {
    input.focus();
    return;
  }
  try {
    await invoke("set_secret", { secret });
    // Clear immediately: the secret is write-only and should not linger on
    // screen or in the DOM.
    input.value = "";
    input.blur();
    previous = {};
    await refresh();
  } catch (err) {
    console.error("set_secret failed", err);
  }
});

// Enter submits from the field, matching what a terminal user expects.
el("secret").addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    el("pair").click();
  }
});

el("quit").addEventListener("click", () => {
  invoke("quit");
});

const timer = setInterval(refresh, POLL_MS);
refresh();