"use client";

import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode, type SyntheticEvent } from "react";
import { FactoryAppController, type FactoryAppSnapshot, type FactoryAppStatus, type FactoryTerminalView } from "./factory-app-controller.js";
import { FactoryConsole, type ConsoleView } from "./factory-console.js";
import { TaskConversation } from "./console-sidebar.js";
import { primaryAgent } from "./console-view.js";
import { XtermTerminal } from "./xterm-terminal.js";

const INITIAL_SNAPSHOT: FactoryAppSnapshot = { status: "idle" };

export type FactoryAppProps = {
  /** Receives the finite connection lifecycle exposed by the owned controller. */
  onStatusChange?: (status: FactoryAppStatus) => void;
};

/** Complete browser application lifecycle; hosts only render this component. */
export function FactoryApp({ onStatusChange }: FactoryAppProps = {}) {
  const [snapshot, setSnapshot] = useState<FactoryAppSnapshot>(INITIAL_SNAPSHOT);
  const [view, setView] = useState<ConsoleView>("floor");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [terminalOpen, setTerminalOpen] = useState(false);
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
    });
    owner.current = controller;
    controller.start();
    return () => {
      if (owner.current === controller) owner.current = undefined;
      controller.close();
    };
  }, []);

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
  const terminal = agentTerminal === undefined || controller === undefined || !terminalOpen ? undefined : (
    <TerminalPanel
      terminal={agentTerminal}
      onClose={() => { setTerminalOpen(false); controller.closeAgentTerminal(); }}
    >
      <TerminalContent terminal={agentTerminal} controller={controller} />
    </TerminalPanel>
  );
  // The sidebar always owns a durable task composer. While a run is live its
  // explicit action is queueing follow-up work; terminal keystrokes stay raw.
  const instruction = agentTerminal === undefined || controller === undefined ? undefined : (
    agentTerminal.taskTitle === undefined ? (
      <AgentIdleTools terminal={agentTerminal} controller={controller} />
    ) : <AgentTaskTools terminal={agentTerminal} controller={controller} />
  );

  const openSidebar = (open: () => void) => {
    setTerminalOpen(false);
    open();
  };

  return (
    <FactoryConsole
      {...snapshot}
      view={view}
      onView={setView}
      settingsOpen={settingsOpen}
      onToggleSettings={() => setSettingsOpen((open) => !open)}
      onSelectAgent={(agent) => openSidebar(() => { owner.current?.clearHumanRequest(); owner.current?.selectAgent(agent); })}
      onCloseAgent={() => { setTerminalOpen(false); owner.current?.clearAgentTerminal(); }}
      onOpenAgentTerminal={() => setTerminalOpen(true)}
      onSaveAgentConfig={(config) => { void owner.current?.updateAgentConfig(config); }}
      onEditTask={(task, change) => owner.current?.editTask(task, change) ?? Promise.resolve(false)}
      onLoadTaskDetail={(task, peerOffset, expectedHead) => owner.current?.taskDetail(task, peerOffset, expectedHead) ?? Promise.reject(new Error("closed"))}
      onOpenTerminalForHumanRequest={(request) => { setTerminalOpen(true); owner.current?.openTerminalForHumanRequest(request); }}
      onSelectHumanRequest={(request) => openSidebar(() => { void owner.current?.selectHumanRequest(request); })}
      onHumanReplyChange={(reply) => owner.current?.setHumanReply(reply)}
      onReplyHumanRequest={() => { void owner.current?.replyHumanRequest(); }}
      onCancelHumanRequest={() => { void owner.current?.cancelHumanRequest(); }}
      onCloseHumanRequest={() => owner.current?.clearHumanRequest()}
      instructionContent={instruction}
      onLoadAccounts={() => { void owner.current?.loadAccounts(); }}
      onLinkAccount={(login, label) => { void owner.current?.linkAccount({ provider: login.provider, home: login.home, label }); }}
      onInviteRemote={() => { void owner.current?.inviteRemote(); }}
      onDismissRemoteInvite={() => owner.current?.dismissRemoteInvite()}
      terminalContent={terminal}
    />
  );
}

/** The selected agent's terminal is a sidebar, not a replacement screen. */
export function TerminalPanel({
  terminal,
  onClose,
  children,
}: {
  terminal: FactoryTerminalView;
  onClose: () => void;
  children: ReactNode;
}) {
  return (
    <section className="dfFactoryConsole__terminalPanel" aria-label={`Agent console for ${terminal.agentName}`}>
      <div className="dfFactoryConsole__terminalHeading">
        <p className="dfFactoryConsole__terminalAgent">
          {terminal.agentName}{terminal.taskTitle === undefined ? "" : ` · ${terminal.taskTitle}`}
        </p>
        <button
          type="button"
          disabled={terminal.phase === "closing" || terminal.phase === "closed"}
          onClick={onClose}
          title="close this agent console; running work continues"
        >
          CLOSE
        </button>
      </div>
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
  const [instruction, setInstruction] = useState("");
  const pending = terminal.controlPending !== undefined;
  const submit = async (action: "message" | "replace") => {
    if (pending || !terminal.controlReady || instruction.trim() === "") return;
    if (await controller.controlAgent(action, instruction)) setInstruction("");
  };
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
      <label className="dfFactoryConsole__visuallyHidden" htmlFor={`df-steer-${terminal.agentId}`}>Message the current session for {terminal.agentName}</label>
      <textarea id={`df-steer-${terminal.agentId}`} rows={2} value={instruction} disabled={pending} placeholder="Message the current session…" onChange={(event) => setInstruction(event.target.value)} />
      <div className="dfFactoryConsole__instructionActions">
        {status === undefined ? null : <span role={unknown || terminal.controlStatus === "rejected" ? "alert" : "status"}>{status}</span>}
        <button type="button" disabled={pending || instruction.trim() === ""} onClick={() => { void submit("message"); }}>{pending ? "SENDING" : "MESSAGE"}</button>
        <button type="button" disabled={pending} onClick={() => { void controller.controlAgent("interrupt"); }}>INTERRUPT</button>
        <button type="button" disabled={pending} onClick={() => { void controller.controlAgent("stop"); }}>STOP CURRENT</button>
        <button type="button" disabled={pending || instruction.trim() === ""} onClick={() => { void submit("replace"); }}>STOP CURRENT / START NEW</button>
      </div>
      <TaskHistory terminal={terminal} onRefresh={() => controller.loadTaskHistory()} onLoadConversation={() => controller.loadTaskDetail()} onLoadOlderConversation={() => controller.loadOlderTaskConversation()} />
    </section>
  );
}

function TaskHistory({ terminal, onRefresh, onLoadConversation, onLoadOlderConversation }: { terminal: FactoryTerminalView; onRefresh: () => void; onLoadConversation: () => void; onLoadOlderConversation: () => void }) {
  const history = terminal.history;
  return (
    <section className="dfFactoryConsole__history" aria-label="Task control history">
      <div className="dfFactoryConsole__historyHeading"><h3>HISTORY</h3><button type="button" disabled={terminal.historyPending} onClick={onRefresh}>{terminal.historyPending ? "LOADING" : "REFRESH"}</button><button type="button" disabled={terminal.taskDetailPending} onClick={onLoadConversation}>{terminal.taskDetailPending ? "LOADING" : "VIEW CONVERSATION"}</button></div>
      {history === undefined || history.entries.length === 0 ? <p className="dfFactoryConsole__instructionState">{terminal.historyPending ? "LOADING RECEIPTS" : "NO DURABLE CONTROLS YET"}</p> : (
        <ol>
          {history.entries.map((entry) => <li key={entry.operationId}><strong>{entry.kind.toUpperCase()} · {entry.status.toUpperCase()}</strong><span>{entry.actor}{entry.body === "" ? "" : ` · ${entry.body}`}</span></li>)}
        </ol>
      )}
		{terminal.taskDetailError === undefined ? null : <p role="alert">THE FACTORY REFUSED THIS HISTORY</p>}
		{terminal.taskDetail === undefined ? null : <TaskConversation brief={terminal.taskDetail} pending={terminal.taskDetailPending} onOlder={terminal.taskDetail.nextPeerOffset === undefined ? undefined : onLoadOlderConversation} />}
    </section>
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
