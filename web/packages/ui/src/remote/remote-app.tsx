"use client";

import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  consumeInvitation,
  createRemoteManager,
  createRemoteStore,
  parseInvitation,
  type HumanRequestDetail,
  type HumanRequestItem,
  type PushSubscribeBody,
  type RemoteFactoryView,
  type RemoteInvitation,
  type RemoteManager,
  type RemoteManagerOptions,
  type RemoteStore,
  type StateView,
} from "@dark-factory/client";
import { AgentStrip, QueueScreen, StageMeter } from "../console-screens.js";
import { stageOfTask } from "../console-view.js";
import {
  FACTORY_UNREACHABLE,
  INVITATION_SPENT,
  INVITATION_UNREADABLE,
  REMOTE_STATUS_GLYPH,
  REQUEST_CLOSED,
  REQUEST_UNAVAILABLE,
  remoteActionable,
  remoteDeliveryNotice,
  remoteFactoryBanner,
  remoteOpenRequests,
  remotePairFailure,
  remoteProjectGroups,
  shortRemoteID,
} from "./remote-view.js";

export type RemoteAppProps = {
  /** Durable bindings; the IndexedDB store when the host supplies none. */
  store?: RemoteStore;
  /** The seam a test drives the whole console through, without a relay. */
  managerFactory?: (options: RemoteManagerOptions) => RemoteManager;
  location?: Pick<Location, "hash" | "pathname" | "search" | "origin">;
  history?: Pick<History, "replaceState" | "state">;
  navigator?: Pick<Navigator, "onLine">;
  /** Turns this device's push subscription on; the browser implementation when the host supplies none. */
  subscribePush?: () => Promise<PushSubscribeBody>;
};

const ALERTS_NEED_INSTALL = "On iPhone, add this page to the Home Screen first: Share, then Add to Home Screen, and open Dark Factory from there.";
const ALERTS_REFUSED = "ALERTS WERE REFUSED BY THIS BROWSER. ALLOW NOTIFICATIONS FOR THIS SITE TO TURN THEM ON.";

/**
 * The browser half of alerts: permission, then one push subscription made with
 * a key pair this device mints. The device keeps the private key too, because
 * every factory it pairs with must be able to sign for the one subscription a
 * push service will give it.
 */
async function browserPushSubscription(): Promise<PushSubscribeBody> {
  if (!("PushManager" in globalThis) || globalThis.navigator?.serviceWorker === undefined) throw new Error(ALERTS_NEED_INSTALL);
  if (await Notification.requestPermission() !== "granted") throw new Error(ALERTS_REFUSED);
  const registration = await navigator.serviceWorker.ready;
  const stale = await registration.pushManager.getSubscription();
  if (stale !== null) await stale.unsubscribe();
  const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign"]);
  const publicKey = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
  const privateKey = new Uint8Array(await crypto.subtle.exportKey("pkcs8", pair.privateKey));
  const subscription = await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: publicKey });
  const encode = (bytes: Uint8Array) => btoa(String.fromCharCode(...bytes)).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
  return { endpoint: subscription.endpoint, public_key: encode(publicKey), private_key: encode(privateKey) };
}

type Pairing =
  | Readonly<{ phase: "idle" }>
  | Readonly<{ phase: "pairing" }>
  | Readonly<{ phase: "failed"; copy: string }>;

type Detail = {
  nodeId: string;
  label: string;
  request: HumanRequestItem;
  phase: "loading" | "ready" | "replying" | "cancelling" | "ended";
  detail?: HumanRequestDetail;
  reply: string;
  notice?: string;
  token: number;
};

type Confirm = Readonly<{ kind: "factory"; nodeId: string }> | Readonly<{ kind: "device" }>;

const INVITATION_KEY = /(?:^|[?#&])df_remote(?:[&=]|$)/i;
const CANCEL_PHRASE = "CANCEL RUN";
const IDLE: Pairing = { phase: "idle" };

/**
 * The phone-shaped remote console. It owns one RemoteManager and renders the
 * factories that manager holds; every effect it offers is one-shot, and there
 * is no terminal here at all — a phone answers questions, it does not drive a
 * TUI.
 */
export function RemoteApp(props: RemoteAppProps = {}) {
  const [, setVersion] = useState(0);
  const [pairing, setPairing] = useState<Pairing>(IDLE);
  const [pasting, setPasting] = useState(false);
  const [link, setLink] = useState("");
  const [detail, setDetailState] = useState<Detail | undefined>(undefined);
  const [confirm, setConfirm] = useState<Confirm | undefined>(undefined);
  const [cancelPhrase, setCancelPhrase] = useState<string | undefined>(undefined);
  const [alerts, setAlerts] = useState<{ phase: "idle" | "working" | "failed"; copy?: string }>({ phase: "idle" });
  const [online, setOnline] = useState(() => (props.navigator ?? globalThis.navigator)?.onLine !== false);
  const manager = useRef<RemoteManager | undefined>(undefined);
  const token = useRef(0);
  // Read and cleared once per mount, not once per effect run.
  const arrival = useRef<{ attempted: boolean; invitation: RemoteInvitation | null } | undefined>(undefined);
  // The selection is read back inside one-shot handlers, so it must be current
  // in the same tick a control is used, not on the next render.
  const selection = useRef<Detail | undefined>(undefined);

  const bump = () => { if (manager.current !== undefined) setVersion((value) => value + 1); };
  const putDetail = (next: Detail | undefined) => {
    selection.current = next;
    if (manager.current !== undefined) setDetailState(next);
  };

  useEffect(() => {
    // A host that states the device's connectivity owns it; only when none is
    // given does this listen to the browser itself.
    const stated = props.navigator;
    if (stated !== undefined) { setOnline(stated.onLine !== false); return; }
    const update = () => setOnline(globalThis.navigator?.onLine !== false);
    update();
    globalThis.addEventListener("online", update);
    globalThis.addEventListener("offline", update);
    return () => {
      globalThis.removeEventListener("online", update);
      globalThis.removeEventListener("offline", update);
    };
  }, [props.navigator]);

  useEffect(() => {
    const where = props.location ?? window.location;
    const past = props.history ?? window.history;
    const build = props.managerFactory ?? createRemoteManager;
    const built = build({
      store: props.store ?? createRemoteStore(),
      origin: where.origin,
      onChange: bump,
    });
    manager.current = built;
    bump();
    // Read-and-clear before anything can await: a one-shot invitation must not
    // survive in the address bar for a reload to replay. StrictMode runs this
    // effect twice, so the fragment is spent by the mount, not by a run that is
    // about to be thrown away.
    if (arrival.current === undefined) arrival.current = { attempted: invitationArrived(where.hash), invitation: consumeInvitation(where, past) };
    const { attempted, invitation } = arrival.current;
    void (async () => {
      try { await built.start(); } catch { /* an unreadable store is an empty device, not a crash */ }
      // Identity, never a shared flag: only the run that still owns the manager
      // may act on it.
      if (manager.current !== built) return;
      bump();
      if (invitation !== null) await pairWith(built, invitation);
      else if (attempted) setPairing({ phase: "failed", copy: INVITATION_SPENT });
    })();
    return () => {
      if (manager.current === built) manager.current = undefined;
      built.close();
    };
  }, []);

  const owner = manager.current;
  const factories = owner?.factories() ?? [];
  const needsYou = owner?.needsYou() ?? [];
  const selectedId = owner?.selected();
  const selected = factories.find((factory) => factory.nodeId === selectedId) ?? factories[0];
  const working = detail !== undefined && busy(detail);
  const byNode = new Map(factories.map((factory) => [factory.nodeId, factory] as const));
  const actionable = (nodeId: string) => remoteActionable(byNode.get(nodeId)?.status, online);

  async function pairWith(target: RemoteManager, invitation: RemoteInvitation): Promise<void> {
    setPairing({ phase: "pairing" });
    try {
      await target.pair(invitation);
      if (manager.current !== target) return;
      setPairing(IDLE);
      setPasting(false);
      setLink("");
    } catch (error) {
      if (manager.current !== target) return;
      setPairing({ phase: "failed", copy: remotePairFailure(error) });
    }
    bump();
  }

  const pairPasted = () => {
    const target = manager.current;
    if (target === undefined || pairing.phase === "pairing") return;
    let invitation: RemoteInvitation;
    try { invitation = parseInvitation(link); } catch { setPairing({ phase: "failed", copy: INVITATION_UNREADABLE }); return; }
    void pairWith(target, invitation);
  };

  const select = (nodeId: string) => {
    try { manager.current?.select(nodeId); } catch { /* a binding can be forgotten between render and tap */ }
    bump();
  };

  /**
   * Reads the private detail for one question at one exact revision. It is the
   * only place detail is ever fetched, so a reply, a cancellation and a first
   * open all converge on the same fresh authority.
   */
  const load = async (next: Detail, notice: string | undefined, absent = REQUEST_UNAVAILABLE): Promise<void> => {
    putDetail({ ...next, notice });
    const session = manager.current?.client(next.nodeId)?.session;
    if (session === undefined) {
      putDetail({ ...next, phase: "ended", detail: undefined, notice: notice ?? FACTORY_UNREACHABLE });
      return;
    }
    try {
      const fetched = await session.getHumanRequestDetail({
        requestId: next.request.id,
        expectedRevision: next.request.revision,
      });
      if (selection.current?.token !== next.token) return;
      putDetail({ ...next, phase: "ready", detail: fetched, reply: "", notice });
    } catch {
      if (selection.current?.token !== next.token) return;
      putDetail({ ...next, phase: "ended", detail: undefined, notice: notice ?? absent });
    }
  };

  const open = (nodeId: string, label: string, request: HumanRequestItem) => {
    // Two ANSWER controls can be on screen for one question, and the daemon
    // allows one human operation per request at a time: a tap while a read or
    // an effect is still in flight would race it, and the loser reads as a
    // failure — or worse, moves the detail out from under an answer coming back.
    const current = selection.current;
    if (!actionable(nodeId) || (current !== undefined && busy(current))) return;
    setCancelPhrase(undefined);
    void load({ nodeId, label, request, phase: "loading", reply: "", token: ++token.current }, undefined);
  };

  /**
   * After a one-shot effect the answer is whatever the factory now says, so the
   * detail is read again — once. Nothing here ever repeats the effect itself.
   */
  const refresh = (after: Detail, notice: string | undefined) => {
    const current = manager.current?.factories().find((factory) => factory.nodeId === after.nodeId);
    const request = current?.state?.humanRequests.get(after.request.id);
    if (request === undefined) {
      // Whatever notice the effect left stands: the request may be missing
      // because the factory closed it, or because there is no snapshot to look
      // in at all, and this cannot tell those apart.
      putDetail({ ...after, phase: "ended", detail: undefined, notice: notice ?? REQUEST_CLOSED });
      return;
    }
    // An effect that landed moves the question's revision on, so a refused
    // re-read here means it is answered, not that anything went wrong.
    void load({ ...after, request, phase: "loading", reply: "", detail: undefined }, notice, REQUEST_CLOSED);
  };

  const reply = () => {
    const current = selection.current;
    const detailValue = current?.detail;
    if (current === undefined || current.phase !== "ready" || detailValue === undefined) return;
    if (!actionable(current.nodeId) || current.reply.trim().length === 0) return;
    const session = manager.current?.client(current.nodeId)?.session;
    if (session === undefined) { putDetail({ ...current, phase: "ended", detail: undefined, notice: FACTORY_UNREACHABLE }); return; }
    const answer = current.reply;
    putDetail({ ...current, phase: "replying", notice: undefined });
    void (async () => {
      let notice: string | undefined;
      try { await session.replyHumanRequest(detailValue, answer); } catch (error) { notice = remoteDeliveryNotice(error); }
      if (selection.current?.token !== current.token) return;
      setCancelPhrase(undefined);
      refresh(current, notice);
    })();
  };

  const changeReply = (value: string) => {
    const current = selection.current;
    const currentDetail = current?.detail;
    if (current === undefined || current.phase !== "ready" || currentDetail === undefined || !currentDetail.canReply || !actionable(current.nodeId)) return;
    if (new TextEncoder().encode(value).length > currentDetail.replyMaxBytes) return;
    putDetail({ ...current, reply: value });
  };

  const cancelRun = () => {
    const current = selection.current;
    const descriptor = current?.detail?.cancelRun;
    if (current === undefined || current.phase !== "ready" || descriptor === undefined || descriptor === null) return;
    if (!actionable(current.nodeId) || cancelPhrase?.trim().toUpperCase() !== CANCEL_PHRASE) return;
    const session = manager.current?.client(current.nodeId)?.session;
    if (session === undefined) { putDetail({ ...current, phase: "ended", detail: undefined, notice: FACTORY_UNREACHABLE }); return; }
    putDetail({ ...current, phase: "cancelling", notice: undefined });
    void (async () => {
      let notice: string | undefined;
      try { await session.cancelHumanRequest(descriptor); } catch (error) { notice = remoteDeliveryNotice(error); }
      if (selection.current?.token !== current.token) return;
      setCancelPhrase(undefined);
      refresh(current, notice);
    })();
  };

  const forget = (nodeId: string) => {
    setConfirm(undefined);
    if (selection.current?.nodeId === nodeId) putDetail(undefined);
    void (async () => {
      try { await manager.current?.forget(nodeId); } catch { /* the binding is gone either way */ }
      bump();
    })();
  };

  const enableAlerts = () => {
    const target = manager.current;
    if (target === undefined || alerts.phase === "working") return;
    setAlerts({ phase: "working" });
    void (async () => {
      try {
        const subscription = await (props.subscribePush ?? browserPushSubscription)();
        if (manager.current !== target) return;
        await target.setPush(subscription);
        setAlerts({ phase: "idle" });
      } catch (error) {
        if (manager.current !== target) return;
        setAlerts({ phase: "failed", copy: error instanceof Error && error.message.length > 0 ? error.message : ALERTS_REFUSED });
      }
      bump();
    })();
  };

  const forgetDevice = () => {
    setConfirm(undefined);
    putDetail(undefined);
    void (async () => {
      try { await manager.current?.forgetDevice(); } catch { /* the bindings are gone either way */ }
      bump();
    })();
  };

  return (
    <div className="dfConsoleShell dfRemote">
      <main className="dfFactoryConsole dfRemote__main" aria-label="Factory remote console">
        <header className="dfFactoryConsole__header">
          <div>
            <p className="dfFactoryConsole__eyebrow">REMOTE</p>
            <h1>FACTORIES</h1>
          </div>
        </header>

        {online ? null : (
          <p className="dfRemote__banner dfRemote__banner--device" role="status">DEVICE OFFLINE</p>
        )}

        <section className="dfFactoryConsole__section dfRemote__pair" aria-label="Pair a factory">
          <div className="dfFactoryConsole__sectionHeading">
            <h2>PAIR A FACTORY</h2>
            <span>{factories.length} PAIRED</span>
          </div>
          {pairing.phase === "pairing" ? (
            <p className="dfRemote__pairing" role="status">PAIRING FACTORY…</p>
          ) : null}
          {pairing.phase === "failed" ? (
            <p className="dfRemote__pairError" role="alert">{pairing.copy}</p>
          ) : null}
          {pasting ? (
            <div className="dfRemote__paste">
              <label htmlFor="dfRemoteInvitation">INVITATION LINK</label>
              <textarea
                id="dfRemoteInvitation"
                className="dfRemote__link"
                value={link}
                disabled={!online || pairing.phase === "pairing"}
                onChange={(event) => setLink(event.currentTarget.value)}
              />
              <div className="dfRemote__actions">
                <button
                  type="button"
                  className="dfRemote__pairAction"
                  disabled={!online || pairing.phase === "pairing" || link.trim().length === 0}
                  onClick={pairPasted}
                >
                  PAIR
                </button>
                <button
                  type="button"
                  className="dfRemote__pasteCancel"
                  disabled={pairing.phase === "pairing"}
                  onClick={() => { setPasting(false); setLink(""); }}
                >
                  NOT NOW
                </button>
              </div>
            </div>
          ) : (
            <button
              type="button"
              className="dfRemote__pasteOpen"
              disabled={!online || pairing.phase === "pairing"}
              onClick={() => { setPasting(true); setPairing(IDLE); }}
            >
              PAIR A FACTORY
            </button>
          )}
        </section>

        {factories.length === 0 ? (
          <section className="dfFactoryConsole__section dfRemote__none" aria-label="No factories">
            <div className="dfFactoryConsole__sectionHeading"><h2>NO FACTORY ON THIS DEVICE</h2></div>
            <p className="dfRemote__prose">
              On a paired desktop, open the factory console and choose PAIR A PHONE, then scan the
              code or open its link on this phone.
            </p>
            <p className="dfRemote__prose">
              You can also paste that link above. The link works once and only on the device that
              opens it.
            </p>
            <p className="dfRemote__prose">{ALERTS_NEED_INSTALL}</p>
          </section>
        ) : (
          <nav className="dfRemote__switcher" aria-label="Factories on this device">
            <ul className="dfRemote__factories">
              {factories.map((factory) => {
                const waiting = needsYou.filter((item) => item.nodeId === factory.nodeId).length;
                return (
                  <li key={factory.nodeId}>
                    <button
                      type="button"
                      className={`dfRemote__factory dfRemote__factory--${factory.status}${waiting > 0 ? " dfRemote__factory--waiting" : ""}`}
                      aria-pressed={factory.nodeId === selected?.nodeId}
                      aria-label={`${factory.label}: ${factory.status}, ${waiting} needs you`}
                      disabled={!online}
                      onClick={() => select(factory.nodeId)}
                    >
                      <span className="dfRemote__factoryGlyph" aria-hidden="true">{REMOTE_STATUS_GLYPH[factory.status]}</span>
                      <span className="dfRemote__factoryLabel">{factory.label}</span>
                      <span className="dfRemote__factoryStatus">{factory.status}</span>
                      <span className="dfRemote__factoryCount">{waiting} NEEDS YOU</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          </nav>
        )}

        {factories.length === 0 ? null : (
          <section className="dfFactoryConsole__section dfRemote__alerts" aria-label="Alerts">
            <div className="dfFactoryConsole__sectionHeading">
              <h2>ALERTS</h2>
              <span>{owner?.push() === undefined ? "OFF" : "ON"}</span>
            </div>
            {alerts.phase === "failed" ? <p className="dfRemote__pairError" role="alert">{alerts.copy}</p> : null}
            {owner?.push() === undefined ? (
              <>
                <p className="dfRemote__prose">Get a notification on this device when a factory needs you.</p>
                <button type="button" className="dfRemote__alertsOn" disabled={!online || alerts.phase === "working"} onClick={enableAlerts}>
                  {alerts.phase === "working" ? "TURNING ON…" : "ENABLE ALERTS"}
                </button>
              </>
            ) : (
              <p className="dfRemote__prose">This device is woken when any paired factory needs you.</p>
            )}
          </section>
        )}

        {factories.length === 0 ? null : (
          <section className="dfFactoryConsole__section dfRemote__needsYou" aria-label="NEEDS YOU">
            <div className="dfFactoryConsole__sectionHeading">
              <h2>NEEDS YOU</h2>
              <span>{needsYou.length} {needsYou.length === 1 ? "QUESTION" : "QUESTIONS"}</span>
            </div>
            {needsYou.length === 0 ? (
              <p className="dfFactoryConsole__empty">all quiet — nothing needs you</p>
            ) : (
              <ul className="dfFactoryConsole__list">
                {needsYou.map((item) => (
                  <li className="dfFactoryConsole__card dfRemote__question" key={`${item.nodeId}:${item.request.id}`}>
                    <div className="dfFactoryConsole__cardTitle">
                      <strong>{entityName(byNode.get(item.nodeId)?.state?.agents, item.request.agent_id, "AGENT")} asks</strong>
                      <span className="dfRemote__tag">{item.label}</span>
                    </div>
                    <p>
                      {entityName(byNode.get(item.nodeId)?.state?.projects, item.request.project_id, "project")} · TASK {shortRemoteID(item.request.task_id)}
                    </p>
                    <button
                      type="button"
                      className="dfRemote__answer"
                      aria-pressed={detail !== undefined && detail.nodeId === item.nodeId && detail.request.id === item.request.id}
                      disabled={!actionable(item.nodeId) || working}
                      onClick={() => open(item.nodeId, item.label, item.request)}
                    >
                      ANSWER
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </section>
        )}

        {detail === undefined ? null : (
          <article className="dfFactoryConsole__section dfRemote__detail" aria-label="Selected question" aria-live="polite">
            <div className="dfFactoryConsole__sectionHeading">
              <h2>{detail.label}</h2>
              <span>{detail.phase === "replying" ? "REPLYING" : detail.phase === "cancelling" ? "CANCELLING" : detail.phase.toUpperCase()}</span>
            </div>
            {detail.notice === undefined ? null : (
              <p className="dfRemote__notice" role="status">{detail.notice}</p>
            )}
            {detail.phase === "loading" ? <p className="dfFactoryConsole__empty">LOADING THE QUESTION…</p> : null}
            {detail.detail === undefined ? null : (
              <>
                <p className="dfRemote__questionText">{detail.detail.question}</p>
                {detail.detail.options.length === 0 ? null : <div className="dfFactoryConsole__answerOptions" role="group" aria-label="Suggested answers">
                  {detail.detail.options.map((option, index) => <button type="button" key={option} disabled={busy(detail) || !detail.detail?.canReply || !actionable(detail.nodeId)} onClick={() => changeReply(option)}>{option}{index === 0 ? " · RECOMMENDED" : ""}</button>)}
                </div>}
                {detail.detail.canReply ? (
                  <div className="dfRemote__reply">
                    <label htmlFor="dfRemoteReply">YOUR ANSWER</label>
                    <textarea
                      id="dfRemoteReply"
                      className="dfRemote__replyText"
                      value={detail.reply}
                      maxLength={detail.detail.replyMaxBytes}
                      disabled={busy(detail) || !actionable(detail.nodeId)}
                      onChange={(event) => changeReply(event.currentTarget.value)}
                    />
                    <button
                      type="button"
                      className="dfRemote__replyAction"
                      disabled={busy(detail) || !actionable(detail.nodeId) || detail.reply.trim().length === 0}
                      onClick={reply}
                    >
                      {detail.phase === "replying" ? "REPLYING…" : "REPLY"}
                    </button>
                  </div>
                ) : <p className="dfFactoryConsole__empty">{detail.request.status === "open" ? "THIS OPEN DECISION IS READ-ONLY IN THIS VIEW." : `THIS DECISION IS ${detail.request.status.replaceAll("_", " ").toUpperCase()}.`}</p>}
                {detail.detail.cancelRun === null ? null : cancelPhrase === undefined ? (
                  <button
                    type="button"
                    className="dfRemote__cancelOpen"
                    disabled={busy(detail) || !actionable(detail.nodeId)}
                    onClick={() => setCancelPhrase("")}
                  >
                    CANCEL RUN
                  </button>
                ) : (
                  <div className="dfRemote__cancel">
                    <label htmlFor="dfRemoteCancel">TYPE {CANCEL_PHRASE} TO STOP THIS RUN</label>
                    <input
                      id="dfRemoteCancel"
                      className="dfRemote__cancelText"
                      value={cancelPhrase}
                      disabled={busy(detail) || !actionable(detail.nodeId)}
                      onChange={(event) => setCancelPhrase(event.currentTarget.value)}
                    />
                    <div className="dfRemote__actions">
                      <button
                        type="button"
                        className="dfRemote__cancelAction"
                        disabled={busy(detail) || !actionable(detail.nodeId) || cancelPhrase.trim().toUpperCase() !== CANCEL_PHRASE}
                        onClick={cancelRun}
                      >
                        {detail.phase === "cancelling" ? "CANCELLING…" : CANCEL_PHRASE}
                      </button>
                      <button
                        type="button"
                        className="dfRemote__cancelKeep"
                        disabled={busy(detail)}
                        onClick={() => setCancelPhrase(undefined)}
                      >
                        KEEP RUNNING
                      </button>
                    </div>
                  </div>
                )}
              </>
            )}
            <button
              type="button"
              className="dfRemote__close"
              disabled={busy(detail)}
              onClick={() => { setCancelPhrase(undefined); putDetail(undefined); }}
            >
              CLOSE
            </button>
          </article>
        )}

        {selected === undefined ? null : (
          <FactoryPanel
            factory={selected}
            online={online}
            working={working}
            selectedRequestId={detail !== undefined && detail.nodeId === selected.nodeId ? detail.request.id : undefined}
            confirming={confirm?.kind === "factory" && confirm.nodeId === selected.nodeId}
            onOpenRequest={(request) => open(selected.nodeId, selected.label, request)}
            onConfirmForget={() => setConfirm({ kind: "factory", nodeId: selected.nodeId })}
            onKeepFactory={() => setConfirm(undefined)}
            onForget={() => forget(selected.nodeId)}
          />
        )}

        {factories.length === 0 ? null : (
          <footer className="dfRemote__factoryFooter">
            <ConfirmAction
              className="dfRemote__forgetDevice"
              label="FORGET THIS DEVICE"
              confirmLabel="FORGET EVERYTHING"
              open={confirm?.kind === "device"}
              disabled={false}
              onOpen={() => setConfirm({ kind: "device" })}
              onKeep={() => setConfirm(undefined)}
              onConfirm={forgetDevice}
            />
          </footer>
        )}
      </main>
    </div>
  );
}

/** The client scrubs an encoded fragment too, so this reads the same forms. */
function invitationArrived(hash: string): boolean {
  let decoded = hash;
  try { decoded = decodeURIComponent(hash); } catch { /* a malformed attempt still matches its raw key */ }
  return INVITATION_KEY.test(hash) || INVITATION_KEY.test(decoded);
}

/** A read or a one-shot effect this console is already waiting on. */
function busy(detail: Detail): boolean {
  return detail.phase === "loading" || detail.phase === "replying" || detail.phase === "cancelling";
}

/** One factory's whole public picture: projects, agents, queue, questions. */
function FactoryPanel({
  factory,
  online,
  working,
  selectedRequestId,
  confirming,
  onOpenRequest,
  onConfirmForget,
  onKeepFactory,
  onForget,
}: {
  factory: RemoteFactoryView;
  online: boolean;
  working: boolean;
  selectedRequestId: string | undefined;
  confirming: boolean;
  onOpenRequest: (request: HumanRequestItem) => void;
  onConfirmForget: () => void;
  onKeepFactory: () => void;
  onForget: () => void;
}) {
  const banner = remoteFactoryBanner(factory.status);
  const ready = remoteActionable(factory.status, online);
  const open = remoteOpenRequests(factory.state);
  return (
    <div className="dfRemote__panel">
      {banner === undefined ? null : (
        <p className={`dfRemote__banner dfRemote__banner--${factory.status === "offline" || factory.status === "connecting" ? "offline" : factory.status}`} role="status">
          {banner}
        </p>
      )}

      <AgentStrip state={factory.state} ready={ready} />

      <div className="dfFactoryConsole__columns">
        <ProjectsSection state={factory.state} />
        <QueueScreen state={factory.state} />
      </div>

      <section className="dfFactoryConsole__section dfRemote__factoryQuestions" aria-label="Questions from this factory">
        <div className="dfFactoryConsole__sectionHeading">
          <h2>OPEN QUESTIONS</h2>
          <span>{factory.state === undefined ? "—" : open.length} OPEN</span>
        </div>
        {factory.state === undefined ? (
          <p className="dfFactoryConsole__empty">waiting for the factory</p>
        ) : open.length === 0 ? (
          <p className="dfFactoryConsole__empty">nothing open here</p>
        ) : (
          <ul className="dfFactoryConsole__list">
            {open.map((request) => (
              <li className="dfFactoryConsole__card" key={request.id}>
                <div className="dfFactoryConsole__cardTitle">
                  <strong>{entityName(factory.state?.agents, request.agent_id, "AGENT")} asks</strong>
                  <span className="dfRemote__tag">{entityName(factory.state?.projects, request.project_id, "project")}</span>
                </div>
                <p>TASK {shortRemoteID(request.task_id)}</p>
                <button
                  type="button"
                  className="dfRemote__answer"
                  aria-pressed={selectedRequestId === request.id}
                  disabled={!ready || working}
                  onClick={() => onOpenRequest(request)}
                >
                  ANSWER
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      <div className="dfRemote__factoryFooter">
        <ConfirmAction
          className="dfRemote__forgetFactory"
          label="FORGET THIS FACTORY"
          confirmLabel={`FORGET ${factory.label}`}
          open={confirming}
          disabled={false}
          onOpen={onConfirmForget}
          onKeep={onKeepFactory}
          onConfirm={onForget}
        />
      </div>
    </div>
  );
}

/** Work grouped by the project that owns it, with the console's stage meter. */
function ProjectsSection({ state }: { state: StateView | undefined }) {
  if (state === undefined) {
    return (
      <section className="dfFactoryConsole__section dfRemote__projects" aria-label="Projects">
        <div className="dfFactoryConsole__sectionHeading"><h2>PROJECTS</h2><span>—</span></div>
        <p className="dfFactoryConsole__empty">waiting for the factory</p>
      </section>
    );
  }
  const groups = remoteProjectGroups(state);
  return (
    <section className="dfFactoryConsole__section dfRemote__projects" aria-label="Projects">
      <div className="dfFactoryConsole__sectionHeading">
        <h2>PROJECTS</h2>
        <span>{groups.length} {groups.length === 1 ? "PROJECT" : "PROJECTS"}</span>
      </div>
      {groups.length === 0 ? <p className="dfFactoryConsole__empty">no projects yet</p> : (
        <ul className="dfRemote__projectList">
          {groups.map((group) => (
            <li key={group.id} className="dfRemote__project">
              <div className="dfRemote__projectHeading">
                <strong>{group.name}</strong>
                <span>{group.tasks.length} {group.tasks.length === 1 ? "task" : "tasks"}</span>
              </div>
              {group.tasks.length === 0 ? (
                <p className="dfFactoryConsole__empty">no tasks yet</p>
              ) : (
                <ul className="dfConsoleRows">
                  {group.tasks.map((task) => {
                    const stage = stageOfTask(task);
                    return (
                      <li key={task.id}>
                        <div className="dfConsoleRow">
                          <span className="dfConsoleRow__title">{task.title}</span>
                          <span className="dfConsoleRow__agent">{entityName(state.agents, task.assigned_agent_id, "agent")}</span>
                          <StageMeter stage={stage} />
                        </div>
                      </li>
                    );
                  })}
                </ul>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

/** Destructive actions are two taps and never a browser dialog. */
function ConfirmAction({
  className,
  label,
  confirmLabel,
  open,
  disabled,
  onOpen,
  onKeep,
  onConfirm,
}: {
  className: string;
  label: string;
  confirmLabel: string;
  open: boolean;
  disabled: boolean;
  onOpen: () => void;
  onKeep: () => void;
  onConfirm: () => void;
}): ReactNode {
  if (!open) {
    return (
      <button type="button" className={className} disabled={disabled} onClick={onOpen}>
        {label}
      </button>
    );
  }
  return (
    <div className="dfRemote__actions">
      <button type="button" className={`${className} ${className}--confirm`} onClick={onConfirm}>
        {confirmLabel}
      </button>
      <button type="button" className="dfRemote__keep" onClick={onKeep}>KEEP</button>
    </div>
  );
}

function entityName(
  entities: ReadonlyMap<string, { name?: string; title?: string }> | undefined,
  id: string,
  fallback: string,
): string {
  const entity = entities?.get(id);
  return entity?.name ?? entity?.title ?? `${fallback} ${shortRemoteID(id)}`;
}
