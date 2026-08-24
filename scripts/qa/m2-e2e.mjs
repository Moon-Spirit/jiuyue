// JiuYue M2 end-to-end evidence: read receipts + typing relay + recall
// tombstone over the REAL backend. Exit 0 = all assertions pass.
const BASE = process.argv[2] ?? "http://127.0.0.1:8080";
const WS_BASE = BASE.replace(/^http/, "ws");
const log = (...p) =>
  console.log(JSON.stringify({ ts: new Date().toISOString(), ...p }));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function api(m, p, tok, body) {
  const r = await fetch(BASE + p, {
    method: m,
    headers: {
      "content-type": "application/json",
      ...(tok ? { authorization: `Bearer ${tok}` } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(`${p} ${r.status} ${JSON.stringify(j)}`);
  return j;
}
function connect(ticket, label) {
  return new Promise((res, rej) => {
    const ws = new WebSocket(
      `${WS_BASE}/ws?ticket=${encodeURIComponent(ticket)}&platform=web`,
    );
    const inbox = [];
    const waiters = [];
    ws.addEventListener("open", () => res(h));
    ws.addEventListener("message", (ev) => {
      const f = JSON.parse(String(ev.data));
      inbox.push(f);
      log({ dir: label, recv: f });
      for (let i = waiters.length - 1; i >= 0; i--)
        if (waiters[i].pred(f)) {
          const w = waiters.splice(i, 1)[0];
          w.res(f);
        }
    });
    const h = {
      send: (f) => ws.send(JSON.stringify(f)),
      next: (pred, ms = 8000) =>
        new Promise((res2, rej2) => {
          const hit = inbox.find(pred);
          if (hit) return res2(hit);
          const t = setTimeout(() => rej2(new Error(`timeout ${label}`)), ms);
          waiters.push({
            pred,
            res: (f) => {
              clearTimeout(t);
              res2(f);
            },
          });
        }),
      drain: () => (inbox.length = 0),
      close: () => ws.close(),
    };
  });
}

async function main() {
  const fails = [];
  const check = (n, ok, d) => {
    log({ assert: n, ok, detail: d });
    if (!ok) fails.push(n);
  };
  const epoch = Date.now();
  const mk = async (tag) => {
    const target = `+86166${String(epoch).slice(-9)}${tag === "a" ? "1" : "2"}`;
    await api("POST", "/api/auth/request-code", null, {
      channel: "phone",
      target,
    });
    return api("POST", "/api/auth/register", null, {
      channel: "phone",
      target,
      code: "000000",
      username: `m2_${tag}_${epoch}`,
      password: "m2-pass-123",
    });
  };
  const A = await mk("a");
  const B = await mk("b");
  const conv = await api("POST", "/api/conversations", A.access_token, {
    peer_username: B.username,
  });
  const cid = conv.conversation_id;

  const tA = await api("POST", "/api/auth/ws-ticket", A.access_token);
  const tB = await api("POST", "/api/auth/ws-ticket", B.access_token);
  const a = await connect(tA.ticket, "A");
  const b = await connect(tB.ticket, "B");

  // typing relay: A starts -> B receives typing from A's user id
  a.send({ v: 1, t: "typing", d: { conversation_id: cid, state: "start" } });
  const ty = await b.next((f) => f.t === "typing");
  check(
    "typing.relayed_to_peer",
    ty.d.state === "start" && ty.d.user_id === A.user_id,
    JSON.stringify(ty.d),
  );

  // send one message A->B
  const cmid = crypto.randomUUID();
  a.send({
    v: 1,
    t: "msg.send",
    d: {
      conversation_id: cid,
      client_msg_id: cmid,
      body: "read-receipt probe",
    },
  });
  const ack = await a.next(
    (f) => f.t === "msg.ack" && f.d.client_msg_id === cmid,
  );
  const got = await b.next(
    (f) => f.t === "msg.new" && f.d.message_id === ack.d.message_id,
  );

  // B reads -> A receives read.receipt
  b.send({
    v: 1,
    t: "read.update",
    d: { conversation_id: cid, last_read_seq: got.d.seq },
  });
  const rr = await a.next((f) => f.t === "read.receipt");
  check(
    "readreceipt.peer_notified",
    rr.d.user_id === B.user_id && rr.d.last_read_seq >= got.d.seq,
    JSON.stringify(rr.d),
  );

  // A recalls within window -> both get msg.recalled; sync serves tombstone
  a.send({
    v: 1,
    t: "msg.recall",
    d: { conversation_id: cid, message_id: got.d.message_id },
  });
  const recA = await a.next((f) => f.t === "msg.recalled");
  const recB = await b.next((f) => f.t === "msg.recalled");
  check(
    "recall.broadcast_both_sides",
    recA.d.message_id === got.d.message_id &&
      recB.d.message_id === got.d.message_id,
    "",
  );

  // fresh socket for B: sync must serve tombstone without body
  b.close();
  await sleep(200);
  const tB2 = await api("POST", "/api/auth/ws-ticket", B.access_token);
  const b2 = await connect(tB2.ticket, "B2");
  b2.send({
    v: 1,
    t: "sync.req",
    d: { cursors: [{ conversation_id: cid, last_delivered_seq: 0 }] },
  });
  const sy = await b2.next((f) => f.t === "sync.res", 10000);
  const recalledMsg = sy.d.messages.find(
    (m) => m.message_id === got.d.message_id,
  );
  check(
    "recall.sync_serves_tombstone_no_body",
    !!recalledMsg && recalledMsg.recalled === true && recalledMsg.body === "",
    JSON.stringify(recalledMsg ?? {}).slice(0, 160),
  );
  const normal = sy.d.messages.find(
    (m) => !m.recalled && m.message_id !== got.d.message_id,
  );
  void normal;

  // Non-sender recall attempt: policy denial reaches ONLY the attempter as
  // an error frame (exhaustively covered by m2_flow.rs); the observable
  // network semantic asserted here is that NO tombstone broadcast occurs.
  const cmid2 = crypto.randomUUID();
  a.send({
    v: 1,
    t: "msg.send",
    d: { conversation_id: cid, client_msg_id: cmid2, body: "stay" },
  });
  const ack2 = await a.next((f) => f.t === "msg.ack" && f.d.client_msg_id === cmid2);
  b2.send({
    v: 1,
    t: "msg.recall",
    d: { conversation_id: cid, message_id: ack2.d.message_id },
  });
  let hijacked = false;
  await a.next(
    (f) => f.t === "msg.recalled" && f.d.message_id === ack2.d.message_id,
    1500,
  ).then(
    () => { hijacked = true; },
    () => {},
  );
  check(
    "recall.non_sender_not_broadcast",
    !hijacked,
    "message survives peer recall attempt",
  );

  a.close();
  b2.close();
  log({ summary: fails.length === 0 ? "PASS" : "FAIL", fails });
  process.exit(fails.length === 0 ? 0 : 1);
}
main().catch((e) => {
  console.error(JSON.stringify({ fatal: String(e) }));
  process.exit(2);
});
