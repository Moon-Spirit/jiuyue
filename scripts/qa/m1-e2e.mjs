// JiuYue M1 end-to-end evidence tool (manual-QA surface artifact).
// Drives the REAL backend over HTTP + WebSocket exactly like two clients would:
//   register A+B -> create conversation -> live send/ack/receive -> duplicate
//   client_msg_id dedupe -> offline gap-fill via sync.req/sync.res.
// Every wire frame is printed as JSONL to stdout (the captured artifact);
// exit code 0 means all scenario assertions passed.
//
// Usage:  node scripts/qa/m1-e2e.mjs [baseUrl]   (default http://127.0.0.1:8080)

const BASE = process.argv[2] ?? "http://127.0.0.1:8080";
const WS_BASE = BASE.replace(/^http/, "ws");
const log = (...parts) =>
  console.log(JSON.stringify({ ts: new Date().toISOString(), ...parts }));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function api(method, path, { token, body } = {}) {
  const res = await fetch(BASE + path, {
    method,
    headers: {
      "content-type": "application/json",
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const json = await res.json().catch(() => ({}));
  if (!res.ok)
    throw new Error(
      `${method} ${path} -> ${res.status} ${JSON.stringify(json)}`,
    );
  return json;
}

function connectWs(ticket, label) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(
      `${WS_BASE}/ws?ticket=${encodeURIComponent(ticket)}&platform=web`,
    );
    const inbox = [];
    const waiters = [];
    ws.addEventListener("open", () => {
      log(dir(label), "ws.open");
      resolve(handle);
    });
    ws.addEventListener("error", (e) =>
      reject(new Error(`ws error: ${e.message ?? "unknown"}`)),
    );
    ws.addEventListener("message", (ev) => {
      const frame = JSON.parse(String(ev.data));
      inbox.push(frame);
      log(dir(label), "ws.recv", frame);
      for (let i = waiters.length - 1; i >= 0; i--) {
        const w = waiters[i];
        if (w.pred(frame)) {
          waiters.splice(i, 1);
          w.resolve(frame);
        }
      }
    });
    ws.addEventListener("close", () => {
      handle.closed = true;
      log(dir(label), "ws.close");
    });

    const handle = {
      closed: false,
      send(frame) {
        ws.send(JSON.stringify(frame));
        log(dir(label), "ws.send", frame);
      },
      close() {
        try {
          ws.close();
        } catch {
          /* already dead */
        }
      },
      /** Resolves the next frame matching pred; rejects after timeoutMs. */
      next(pred, timeoutMs = 8000) {
        const hit = inbox.find(pred);
        if (hit) return Promise.resolve(hit);
        return new Promise((resolve, reject) => {
          const timer = setTimeout(
            () => reject(new Error(`timeout waiting for frame (${label})`)),
            timeoutMs,
          );
          waiters.push({
            pred,
            resolve: (f) => {
              clearTimeout(timer);
              resolve(f);
            },
          });
        });
      },
      drain() {
        inbox.length = 0;
      },
    };
    return handle;
  });
}

function dir(label) {
  return { dir: label };
}

const msgSend = (conversationId, clientMsgId, body) => ({
  v: 1,
  t: "msg.send",
  d: { conversation_id: conversationId, client_msg_id: clientMsgId, body },
});
const syncReq = (cursors) => ({ v: 1, t: "sync.req", d: { cursors } });

async function registerUser(tag, epoch) {
  const username = `qa_${tag}_${epoch}`;
  const target = `+8613${String(epoch).slice(-9)}${tag === "a" ? "1" : "2"}`;
  await api("POST", "/api/auth/request-code", {
    body: { channel: "phone", target },
  });
  const reg = await api("POST", "/api/auth/register", {
    body: {
      channel: "phone",
      target,
      code: "000000",
      username,
      password: "qa-password-123",
    },
  });
  log("http", "registered", {
    tag,
    user_id: reg.user_id,
    username: reg.username,
  });
  return {
    username,
    userId: reg.user_id,
    access: reg.access_token,
    refresh: reg.refresh_token,
  };
}

async function main() {
  const failures = [];
  const check = (name, ok, detail) => {
    log("assert", name, { ok, detail });
    if (!ok) failures.push(name);
  };
  const epoch = Date.now();

  // -- accounts ------------------------------------------------------------
  const a = await registerUser("a", epoch);
  const b = await registerUser("b", epoch);

  // -- conversation --------------------------------------------------------
  const conv = await api("POST", "/api/conversations", {
    token: a.access,
    body: { peer_username: b.username },
  });
  log("http", "conversation created", conv);
  const convId = conv.conversation_id;
  check(
    "conversation.created",
    Number.isFinite(convId) && convId > 0,
    String(convId),
  );

  // -- sockets -------------------------------------------------------------
  const ticketA = await api("POST", "/api/auth/ws-ticket", { token: a.access });
  const ticketB = await api("POST", "/api/auth/ws-ticket", { token: b.access });
  const sockA = await connectWs(ticketA.ticket, "A");
  const sockB = await connectWs(ticketB.ticket, "B");

  // -- live send / ack / receive ------------------------------------------
  const cmid1 = crypto.randomUUID();
  sockA.send(msgSend(convId, cmid1, "你好，来自 A 的第一条消息"));
  const ack1 = await sockA.next(
    (f) => f.t === "msg.ack" && f.d.client_msg_id === cmid1,
  );
  check(
    "send.acked_with_seq",
    ack1.d.duplicate === false && ack1.d.seq === 1,
    JSON.stringify(ack1.d),
  );
  const got1 = await sockB.next(
    (f) => f.t === "msg.new" && f.d.message_id === ack1.d.message_id,
  );
  check(
    "receive.live_delivery",
    got1.d.body.includes("第一条") && got1.d.seq === 1,
    JSON.stringify(got1.d),
  );

  // -- idempotent resend ---------------------------------------------------
  sockA.send(msgSend(convId, cmid1, "重复内容应被去重"));
  const ackDup = await sockA.next(
    (f) =>
      f.t === "msg.ack" &&
      f.d.client_msg_id === cmid1 &&
      f.d.duplicate === true,
  );
  check(
    "dedupe.same_ids_on_duplicate",
    ackDup.d.message_id === ack1.d.message_id && ackDup.d.seq === ack1.d.seq,
    JSON.stringify(ackDup.d),
  );

  // -- offline gap fill ----------------------------------------------------
  sockB.close();
  await sleep(300);
  const ids = [crypto.randomUUID(), crypto.randomUUID()];
  const seqsSeen = [];
  for (let i = 0; i < ids.length; i++) {
    sockA.send(msgSend(convId, ids[i], `离线消息 #${i + 1}`));
    const ack = await sockA.next(
      (f) => f.t === "msg.ack" && f.d.client_msg_id === ids[i],
    );
    seqsSeen.push(ack.d.seq);
  }
  check(
    "offline.send_confirmed_while_b_away",
    seqsSeen.length === 2,
    seqsSeen.join(","),
  );

  const ticketB2 = await api("POST", "/api/auth/ws-ticket", {
    token: b.access,
  });
  const sockB2 = await connectWs(ticketB2.ticket, "B2");
  sockB2.send(syncReq([{ conversation_id: convId, last_delivered_seq: 1 }]));
  const sync = await sockB2.next((f) => f.t === "sync.res", 10000);
  const filledSeqs = sync.d.messages.map((m) => m.seq).sort((x, y) => x - y);
  check(
    "sync.fills_exact_gap",
    sync.d.complete === true &&
      filledSeqs.length === 2 &&
      filledSeqs.every((s) => s > 1),
    `got [${filledSeqs}]`,
  );
  check(
    "sync.bodies_plaintext_over_wire",
    sync.d.messages.every((m) => m.body.startsWith("离线消息")),
    "bodies readable",
  );

  // -- teardown ------------------------------------------------------------
  sockA.close();
  sockB2.close();
  log("summary", failures.length === 0 ? "PASS" : "FAIL", { failures });
  process.exit(failures.length === 0 ? 0 : 1);
}

main().catch((err) => {
  console.error(JSON.stringify({ fatal: String(err) }));
  process.exit(2);
});
