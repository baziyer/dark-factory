"use client";

import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode, type SyntheticEvent } from "react";
import { FactoryAppController, type FactoryAppSnapshot, type FactoryAppStatus, type FactoryTerminalView } from "./factory-app-controller.js";
import { FactoryConsole, type ConsoleView } from "./factory-console.js";
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

  // The floor's rooms are regenerable, so they are fetched when the floor is
  // shown, whenever a fresh session becomes ready, and whenever the set of
  // projects changes under them.
  const projectKey = snapshot.state === undefined ? "" : [...snapshot.state.projects.keys()].join(" ");
  useEffect(() => {
    if (view === "floor" && snapshot.status === "ready") owner.current?.loadTopology();
  }, [view, snapshot.status, projectKey]);

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
  // An agent with no running task has no terminal to show, only the composer
  // that gives it its next task; the sidebar puts that under its queue.
  const instruction = agentTerminal === undefined || controller === undefined || agentTerminal.taskTitle !== undefined ? undefined : (
    <TerminalContent terminal={agentTerminal} controller={controller} />
  );

  const openSidebar = (open: () => void) => {
    setSettingsOpen(false);
    setTerminalOpen(false);
    open();
  };

  return (
    <FactoryConsole
      {...snapshot}
      view={view}
      onView={setView}
      settingsOpen={settingsOpen}
      onToggleSettings={() => {
        setTerminalOpen(false);
        owner.current?.clearAgentTerminal();
        owner.current?.clearHumanRequest();
        setSettingsOpen((open) => !open);
      }}
      onSelectAgent={(agent) => openSidebar(() => { owner.current?.clearHumanRequest(); owner.current?.selectAgent(agent); })}
      onCloseAgent={() => { setTerminalOpen(false); owner.current?.clearAgentTerminal(); }}
      onOpenAgentTerminal={() => setTerminalOpen(true)}
      onSaveAgentConfig={(config) => { void owner.current?.updateAgentConfig(config); }}
      onEditTask={(task, change) => { void owner.current?.editTask(task, change); }}
      onOpenTerminalForHumanRequest={(request) => { setTerminalOpen(true); owner.current?.openTerminalForHumanRequest(request); }}
      onSelectHumanRequest={(request) => openSidebar(() => { void owner.current?.selectHumanRequest(request); })}
      onHumanReplyChange={(reply) => owner.current?.setHumanReply(reply)}
      onReplyHumanRequest={() => { void owner.current?.replyHumanRequest(); }}
      onCancelHumanRequest={() => { void owner.current?.cancelHumanRequest(); }}
      onCloseHumanRequest={() => owner.current?.clearHumanRequest()}
      instructionContent={instruction}
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
          Earlier output is no longer retained; showing what the factory still holds.
        </p>
      )}
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
  if (terminal.taskTitle === undefined) {
    return (
      <AgentInstruction
        key={terminal.agentId}
        terminal={terminal}
        onSubmit={(instruction) => controller.enqueueAgentInstruction(instruction)}
      />
    );
  }
  if (terminal.finishing) return <p className="dfFactoryConsole__instructionState">FINISHING</p>;
  return <TerminalHost key={`${terminal.agentId}:${terminal.surfaceVersion}`} controller={controller} surfaceVersion={terminal.surfaceVersion} />;
}

export function AgentInstruction({
  terminal,
  onSubmit,
}: {
  terminal: FactoryTerminalView;
  onSubmit: (instruction: string) => Promise<boolean>;
}) {
  const [instruction, setInstruction] = useState("");
  const submit = async (event?: SyntheticEvent) => {
    event?.preventDefault();
    if (terminal.paused || terminal.instructionPending || instruction.trim().length === 0) return;
    if (await onSubmit(instruction)) setInstruction("");
  };
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") void submit(event);
  };
  if (terminal.paused) return <p className="dfFactoryConsole__instructionState">PAUSED</p>;
  if (terminal.queued) return <p className="dfFactoryConsole__instructionState">QUEUED</p>;
  const errorCopy = terminal.instructionError === undefined
    ? undefined
    : ["invalid_request", "unauthorized", "stale", "too_large", "rate_limited", "not_found", "crypto_unavailable"].includes(terminal.instructionError.code)
      ? "NOT SENT"
      : "SEND NOT CONFIRMED — CHECK TASKS BEFORE RETRYING";
  return (
    <form className="dfFactoryConsole__instruction" onSubmit={(event) => { void submit(event); }}>
      <label className="dfFactoryConsole__visuallyHidden" htmlFor={`df-instruction-${terminal.agentId}`}>
        Instruction for {terminal.agentName}
      </label>
      <textarea
        id={`df-instruction-${terminal.agentId}`}
        value={instruction}
        autoFocus
        disabled={terminal.instructionPending}
        placeholder="Add an instruction…"
        onChange={(event) => setInstruction(event.target.value)}
        onKeyDown={onKeyDown}
      />
      <div className="dfFactoryConsole__instructionActions">
        {errorCopy === undefined ? null : <span role="alert">{errorCopy}</span>}
        <button type="submit" disabled={terminal.instructionPending || instruction.trim().length === 0}>
          {terminal.instructionPending ? "SENDING" : "SEND"}
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
