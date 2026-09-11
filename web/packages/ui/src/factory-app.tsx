"use client";

import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode, type SyntheticEvent } from "react";
import { browserEndpoint, FactoryAppController, type FactoryAppSnapshot, type FactoryAppStatus, type FactoryTerminalView } from "./factory-app-controller.js";
import { FactoryConsole, type ConsoleDetail, type ConsoleView } from "./factory-console.js";
import { TaskConversation, type AgentPanelView } from "./console-sidebar.js";
import { primaryAgent } from "./console-view.js";
import { XtermTerminal } from "./xterm-terminal.js";

const INITIAL_SNAPSHOT: FactoryAppSnapshot = { status: "idle" };

export type FactoryAppProps = {
  /** Receives the finite connection lifecycle exposed by the owned controller. */
  onStatusChange?: (status: FactoryAppStatus) => void;
  /** Optional loopback port for an isolated development listener. */
  browserPort?: number;
};

/** Complete browser application lifecycle; hosts only render this component. */
export function FactoryApp({ onStatusChange, browserPort }: FactoryAppProps = {}) {
  const [snapshot, setSnapshot] = useState<FactoryAppSnapshot>(INITIAL_SNAPSHOT);
  const [view, setView] = useState<ConsoleView>("floor");
  const [detail, setDetail] = useState<ConsoleDetail>("needs-you");
  const [agentPanel, setAgentPanel] = useState<AgentPanelView>("terminal");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const browser = useMemo(() => browserEndpoint(browserPort), [browserPort]);
  const owner = useRef<FactoryAppController | undefined>(undefined);
  const defaultedController = useRef<FactoryAppController | undefined>(undefined);
  const statusChange = useRef(onStatusChange);
  statusChange.current = onStatusChange;

  useEffect(() => {
    const controller = new FactoryAppController({
      origin: window.location.origin,
      location: window.location,
      history: window.history,
      onChange: setSnapshot,
      onStatusChange: (status) => statusChange.current?.(status),
      browser,
    });
    owner.current = controller;
    controller.start();
    return () => {
      if (owner.current === controller) owner.current = undefined;
      controller.close();
    };
  }, [browser]);

  // The primary overseer is useful immediately, but only once per owned
  // controller: closing a pane remains the operator's choice.
  useEffect(() => {
    const controller = owner.current;
    if (controller === undefined || defaultedController.current === controller || snapshot.status !== "ready" || snapshot.selectedAgent !== undefined || snapshot.state === undefined) return;
    const agent = primaryAgent(snapshot.state);
    if (agent === undefined) return;
    defaultedController.current = controller;
    controller.selectAgent(agent);
  }, [snapshot]);

  const selectedAgentID = snapshot.selectedAgent?.id;
  const previousSelectedAgentID = useRef<string | undefined>(selectedAgentID);
  useEffect(() => {
    if (previousSelectedAgentID.current === undefined && selectedAgentID !== undefined) setDetail("agent");
    previousSelectedAgentID.current = selectedAgentID;
  }, [selectedAgentID]);

  // A lone decision has no competing context. Open it once; a collapsed item
  // remains collapsed until the factory asks a different question.
  const openedHumanRequest = useRef<string | undefined>(undefined);
  const loneHumanRequest = snapshot.state !== undefined && snapshot.state.humanRequests.size === 1 ? snapshot.state.humanRequests.values().next().value : undefined;
  useEffect(() => {
    const controller = owner.current;
    const request = loneHumanRequest;
    if (controller === undefined || snapshot.status !== "ready" || snapshot.selectedHumanRequest !== undefined || request === undefined || openedHumanRequest.current === request.id) return;
    openedHumanRequest.current = request.id;
    setDetail("needs-you");
    void controller.selectHumanRequest(request);
  }, [snapshot.status, snapshot.selectedHumanRequest, loneHumanRequest]);

  // The floor's rooms are regenerable, so they are fetched when the floor is
  // shown, whenever a fresh session becomes ready, and whenever the set of
  // projects changes under them.
  const projectKey = snapshot.state === undefined ? "" : [...snapshot.state.projects.keys()].join(" ");
  useEffect(() => {
    if (view === "floor" && snapshot.status === "ready") owner.current?.loadTopology();
  }, [view, snapshot.status, projectKey]);

  // Where the running agents are working is live, not regenerable: it is polled
  // for as long as the floor is on screen and stopped the moment it is not.
  useEffect(() => {
    if (view !== "floor" || snapshot.status !== "ready") return;
    const controller = owner.current;
    controller?.watchRunPaths(true);
    return () => controller?.watchRunPaths(false);
  }, [view, snapshot.status]);

  const controller = owner.current;
  const agentTerminal = controller === undefined || snapshot.selectedAgent === undefined ? undefined : snapshot.terminal;
  const terminal = agentTerminal === undefined || controller === undefined ? undefined : (
    <TerminalPanel
      terminal={agentTerminal}
    >
      <TerminalContent terminal={agentTerminal} controller={controller} />
    </TerminalPanel>
  );
  return (
    <FactoryConsole
      {...snapshot}
      address={browser.host}
      view={view}
      onView={setView}
      detail={detail}
      onDetail={setDetail}
      agentPanel={agentPanel}
      onAgentPanel={setAgentPanel}
      settingsOpen={settingsOpen}
      onToggleSettings={() => setSettingsOpen((open) => !open)}
      onSelectAgent={(agent) => { setDetail("agent"); setAgentPanel("terminal"); owner.current?.selectAgent(agent); }}
      onSaveAgentConfig={(config) => { void owner.current?.updateAgentConfig(config); }}
      onEditTask={(task, change) => owner.current?.editTask(task, change) ?? Promise.resolve(false)}
      onLoadTaskDetail={(task, peerOffset, expectedHead) => owner.current?.taskDetail(task, peerOffset, expectedHead) ?? Promise.reject(new Error("closed"))}
      onLoadTaskHistory={(task) => owner.current?.taskHistory(task) ?? Promise.reject(new Error("closed"))}
      onLoadTaskList={(agentId, cursor) => owner.current?.taskList(agentId, cursor) ?? Promise.reject(new Error("closed"))}
      onOpenTerminalForHumanRequest={(request) => { setDetail("agent"); setAgentPanel("terminal"); owner.current?.openTerminalForHumanRequest(request); }}
      onSelectHumanRequest={(request) => { setDetail("needs-you"); void owner.current?.selectHumanRequest(request); }}
      onHumanReplyChange={(reply) => owner.current?.setHumanReply(reply)}
      onReplyHumanRequest={() => { void owner.current?.replyHumanRequest(); }}
      onCancelHumanRequest={() => { void owner.current?.cancelHumanRequest(); }}
      onCloseHumanRequest={() => owner.current?.clearHumanRequest()}
      onLoadAccounts={() => { void owner.current?.loadAccounts(); }}
      onLinkAccount={(login, label) => { void owner.current?.linkAccount({ provider: login.provider, home: login.home, label }); }}
      onUpdateAccount={(account, change) => { void owner.current?.updateAccount({ accountId: account.id, expectedRevision: account.revision, ...change }); }}
      onInviteRemote={() => { void owner.current?.inviteRemote(); }}
      onDismissRemoteInvite={() => owner.current?.dismissRemoteInvite()}
      terminalContent={terminal}
    />
  );
}

/** The selected agent's terminal is a sidebar, not a replacement screen. */
export function TerminalPanel({
  terminal,
  children,
}: {
  terminal: FactoryTerminalView;
  children: ReactNode;
}) {
  return (
    <section className="dfFactoryConsole__terminalPanel" aria-label={`Agent console for ${terminal.agentName}`}>
      {terminal.taskTitle === undefined ? null : <p className="dfFactoryConsole__terminalAgent">{terminal.taskTitle}</p>}
      {terminal.error === undefined ? null : (
        <p className="dfFactoryConsole__terminalError" role="alert">
          {terminal.phase === "ready" && !terminal.writable && terminal.error.code === "stale"
            ? "TERMINAL OPEN ELSEWHERE"
            : "TERMINAL UNAVAILABLE"}
        </p>
      )}
      {!terminal.resets ? null : (
        <p className="dfFactoryConsole__terminalReset" role="status">
          Earlier output is no longer retained; showing new output.
        </p>
      )}
      {terminal.taskTitle !== undefined && terminal.paused ? <p className="dfFactoryConsole__instructionState">QUEUE PAUSED</p> : null}
      {children}
    </section>
  );
}

/** Chooses the one safe surface for the selected agent's durable state. */
export function TerminalContent({
  terminal,
  controller,
}: {
  terminal: FactoryTerminalView;
  controller: FactoryAppController;
}) {
  const terminalHost = terminal.hasOutputSurface || (terminal.taskTitle !== undefined && !terminal.finishing)
    ? <TerminalHost key={`${terminal.agentId}:${terminal.surfaceVersion}`} controller={controller} surfaceVersion={terminal.surfaceVersion} />
    : undefined;
  if (terminal.finishing) return <>{terminalHost}<p className="dfFactoryConsole__instructionState">FINISHING</p></>;
  if (terminal.taskTitle !== undefined) return <>{terminalHost}<AgentTaskTools terminal={terminal} controller={controller} /></>;
  return <>{terminalHost}<AgentIdleTools terminal={terminal} controller={controller} /></>;
}

function AgentTaskTools({ terminal, controller }: { terminal: FactoryTerminalView; controller: FactoryAppController }) {
  return (
    <>
      <AgentSteering terminal={terminal} controller={controller} />
      <AgentInstruction terminal={terminal} mode="queue" onDraftChange={(instruction) => controller.setAgentInstructionDraft(instruction)} onSubmit={(instruction, mode) => controller.enqueueAgentInstruction(instruction, mode)} />
    </>
  );
}

function AgentIdleTools({ terminal, controller }: { terminal: FactoryTerminalView; controller: FactoryAppController }) {
  const mode = terminal.paused || terminal.queued ? "queue" : "now";
  return (
    <>
      <AgentInstruction terminal={terminal} mode={mode} onDraftChange={(instruction) => controller.setAgentInstructionDraft(instruction)} onSubmit={(instruction, mode) => controller.enqueueAgentInstruction(instruction, mode)} />
      {terminal.history === undefined && !terminal.historyPending ? null : <TaskHistory terminal={terminal} onRefresh={() => controller.loadTaskHistory()} onLoadConversation={() => controller.loadTaskDetail()} onLoadOlderConversation={() => controller.loadOlderTaskConversation()} />}
    </>
  );
}

function AgentSteering({ terminal, controller }: { terminal: FactoryTerminalView; controller: FactoryAppController }) {
  const pending = terminal.controlPending !== undefined;
  const unknown = terminal.controlStatus === "delivery_unknown" || terminal.controlError?.code === "connection";
  const refused = terminal.controlStatus === "rejected" || (terminal.controlError !== undefined && ["invalid_request", "unauthorized", "stale", "too_large", "rate_limited", "not_found", "unsupported"].includes(terminal.controlError.code));
  const status = terminal.controlStatus === "stopping"
    ? "STOPPING CURRENT WORK"
    : terminal.controlStatus === "queued"
      ? "REPLACEMENT QUEUED"
      : refused
        ? "CONTROL NOT SENT"
        : unknown
          ? "DELIVERY COULD NOT BE CONFIRMED — CHECK TERMINAL/HISTORY BEFORE SENDING AGAIN"
          : undefined;
  if (!terminal.controlReady) return <p className="dfFactoryConsole__instructionState">{terminal.finishing ? "FINISHING" : "STARTING"}</p>;
  return (
    <section className="dfFactoryConsole__steering" aria-label={`Controls for ${terminal.agentName}`}>
      <div className="dfFactoryConsole__instructionActions">
        {status === undefined ? null : <span role={unknown || terminal.controlStatus === "rejected" ? "alert" : "status"}>{status}</span>}
        <button type="button" disabled={pending} onClick={() => { void controller.controlAgent("interrupt"); }}>INTERRUPT</button>
        <button type="button" disabled={pending} onClick={() => { void controller.controlAgent("stop"); }}>STOP CURRENT</button>
      </div>
      <TaskHistory terminal={terminal} onRefresh={() => controller.loadTaskHistory()} onLoadConversation={() => controller.loadTaskDetail()} onLoadOlderConversation={() => controller.loadOlderTaskConversation()} />
    </section>
  );
}

function TaskHistory({ terminal, onRefresh, onLoadConversation, onLoadOlderConversation }: { terminal: FactoryTerminalView; onRefresh: () => void; onLoadConversation: () => void; onLoadOlderConversation: () => void }) {
  const history = terminal.history;
  return (
    <details className="dfFactoryConsole__history" aria-label="Task control history">
      <summary>HISTORY</summary>
      <div className="dfFactoryConsole__historyHeading"><button type="button" disabled={terminal.historyPending} onClick={onRefresh}>{terminal.historyPending ? "LOADING" : "REFRESH"}</button><button type="button" disabled={terminal.taskDetailPending} onClick={onLoadConversation}>{terminal.taskDetailPending ? "LOADING" : "VIEW CONVERSATION"}</button></div>
      {history === undefined || history.entries.length === 0 ? <p className="dfFactoryConsole__instructionState">{terminal.historyPending ? "LOADING RECEIPTS" : "NO DURABLE CONTROLS YET"}</p> : (
        <ol>
          {history.entries.map((entry) => <li key={entry.operationId}><strong>{entry.kind.toUpperCase()} · {entry.status.toUpperCase()}</strong><span>{entry.actor}{entry.body === "" ? "" : ` · ${entry.body}`}</span></li>)}
        </ol>
      )}
		{terminal.taskDetailError === undefined ? null : <p role="alert">THE FACTORY REFUSED THIS HISTORY</p>}
		{terminal.taskDetail === undefined ? null : <TaskConversation brief={terminal.taskDetail} pending={terminal.taskDetailPending} onOlder={terminal.taskDetail.nextPeerOffset === undefined ? undefined : onLoadOlderConversation} />}
    </details>
  );
}

export function AgentInstruction({
  terminal,
  mode = "now",
  onDraftChange,
  onSubmit,
}: {
  terminal: FactoryTerminalView;
  mode?: "now" | "queue";
  onDraftChange?: (instruction: string) => void;
  onSubmit: (instruction: string, mode?: "now" | "queue") => Promise<boolean>;
}) {
  const [localInstruction, setLocalInstruction] = useState("");
  const instruction = onDraftChange === undefined ? localInstruction : terminal.instructionDraft ?? "";
  const setInstruction = (value: string) => {
    if (onDraftChange === undefined) setLocalInstruction(value);
    else onDraftChange(value);
  };
  const submit = async (event?: SyntheticEvent) => {
    event?.preventDefault();
    if ((mode === "now" && terminal.paused) || terminal.instructionPending || instruction.trim().length === 0) return;
    if (await onSubmit(instruction, mode)) setInstruction("");
  };
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") void submit(event);
  };
  if (mode === "now" && terminal.paused) return <p className="dfFactoryConsole__instructionState">PAUSED</p>;
  if (mode === "now" && terminal.queued) return <p className="dfFactoryConsole__instructionState">QUEUED · WAITING FOR CAPACITY</p>;
  const errorCopy = terminal.instructionError === undefined
    ? undefined
    : ["invalid_request", "unauthorized", "stale", "too_large", "rate_limited", "not_found", "crypto_unavailable", "unsupported"].includes(terminal.instructionError.code)
      ? "NOT SENT"
      : "SEND NOT CONFIRMED — CHECK TASKS BEFORE RETRYING";
  return (
    <form className={`dfFactoryConsole__instruction${mode === "queue" ? " dfFactoryConsole__instruction--queue" : ""}`} onSubmit={(event) => { void submit(event); }}>
      <label className="dfFactoryConsole__visuallyHidden" htmlFor={`df-instruction-${terminal.agentId}-${mode}`}>
        {mode === "queue" ? `Queue follow-up work for ${terminal.agentName}` : `Instruction for ${terminal.agentName}`}
      </label>
      <textarea
        id={`df-instruction-${terminal.agentId}-${mode}`}
        value={instruction}
        rows={3}
        autoFocus={mode === "now"}
        disabled={terminal.instructionPending}
        placeholder={mode === "queue" ? "Add follow-up work…" : "Add an instruction…"}
        onChange={(event) => setInstruction(event.target.value)}
        onKeyDown={onKeyDown}
      />
      <div className="dfFactoryConsole__instructionActions">
        {errorCopy === undefined ? null : <span role="alert">{errorCopy}</span>}
        <button type="submit" disabled={terminal.instructionPending || instruction.trim().length === 0}>
          {terminal.instructionPending ? "SENDING" : mode === "queue" ? "ADD TO QUEUE" : "START"}
        </button>
      </div>
    </form>
  );
}

function TerminalHost({ controller, surfaceVersion }: { controller: FactoryAppController; surfaceVersion: number }) {
  const token = useRef<object>({});
  useEffect(() => {
    controller.beginTerminalSurface(token.current, surfaceVersion);
  }, [controller, surfaceVersion]);
  return (
    <XtermTerminal
      onSurface={(surface) => {
        if (surface === undefined) controller.endTerminalSurface(token.current, surfaceVersion);
        else {
          controller.beginTerminalSurface(token.current, surfaceVersion);
          controller.setTerminalSurface(token.current, surface, surfaceVersion);
        }
      }}
      onError={() => controller.terminalError(token.current, surfaceVersion)}
      onData={(value) => controller.sendTerminalText(token.current, value, surfaceVersion)}
      onBinary={(value) => controller.sendTerminalBinary(token.current, value, surfaceVersion)}
      onResize={(rows, cols) => controller.resizeTerminal(token.current, rows, cols, surfaceVersion)}
    />
  );
}
