import { useState, type FormEvent, type ReactNode } from "react";
import type { AgentItem, StateView, TaskItem } from "@dark-factory/client";
import type { FactoryEditView, FactoryHumanRequestView } from "./factory-app-controller.js";
import { rankLabel } from "./console-screens.js";
import { agentActivity, agentCurrentTask } from "./console-view.js";

/** Only the controls the operator actually changed; the rest are left alone. */
export type AgentConfigEdit = Readonly<{ model?: string; reasoningEffort?: string; paused?: boolean }>;

export type TaskEdit = Readonly<{ title?: string; priority?: number; assignedAgentId?: string; cancel?: boolean }>;

const EDIT_ERRORS = new Map<string, string>([
  ["stale", "SOMEONE ELSE CHANGED THIS — REOPEN IT AND TRY AGAIN"],
  ["invalid_request", "THE FACTORY REFUSED THIS EDIT"],
  ["not_found", "THIS NO LONGER EXISTS"],
  ["too_large", "TOO LONG"],
  ["rate_limited", "TOO MANY EDITS AT ONCE"],
  ["unauthorized", "THIS BROWSER MAY NOT EDIT"],
]);

function editErrorCopy(edit: FactoryEditView | undefined): string | undefined {
  if (edit?.error === undefined) return undefined;
  return EDIT_ERRORS.get(edit.error.code) ?? "THE EDIT DID NOT COMPLETE";
}

/** One agent: what it is doing, how it is configured, and what it owes. */
export function AgentPanel({
  agent,
  state,
  edit,
  ready,
  onSaveConfig,
  onEditTask,
  onOpenTerminal,
  onClose,
  children,
}: {
  agent: AgentItem;
  state: StateView | undefined;
  edit?: FactoryEditView;
  ready: boolean;
  onSaveConfig?: (config: AgentConfigEdit) => void;
  onEditTask?: (task: TaskItem, change: TaskEdit) => void;
  onOpenTerminal?: () => void;
  onClose?: () => void;
  /** The instruction composer the terminal view owns for an idle agent. */
  children?: ReactNode;
}) {
  const activity = state === undefined ? "idle" : agentActivity(agent, state);
  const current = state === undefined ? undefined : agentCurrentTask(agent, state);
  const queued = state === undefined ? [] : [...state.tasks.values()]
    .filter((task) => task.assigned_agent_id === agent.id && task.status === "queued")
    .sort((left, right) => right.priority - left.priority);
  const peers = state === undefined ? [] : [...state.agents.values()].filter((peer) => peer.project_id === agent.project_id);
  const errorCopy = editErrorCopy(edit);
  // A form remounts when the served value moves under it and when its own
  // edit is refused, so a rejected change reverts instead of being resent on
  // the next blur. A refusal never changes the revision, so it needs its own
  // token, and only the refused form's: a refused task edit must not throw
  // away what the operator has typed into the config form.
  const formKey = (id: string, revision: bigint) =>
    `${id}:${revision}:${errorCopy !== undefined && edit?.target === id ? "refused" : ""}`;
  return (
    <section className="dfConsoleSidebar__panel" aria-label={`Agent ${agent.name}`}>
      <div className="dfConsoleSidebar__heading">
        <div>
          <p className="dfFactoryConsole__eyebrow">{rankLabel(agent.role)} · {agent.provider}</p>
          <h2>{agent.name}</h2>
        </div>
        {onClose === undefined ? null : <button type="button" onClick={onClose}>CLOSE</button>}
      </div>

      <p className="dfConsoleSidebar__status">{activity === "needs-you" ? "! needs you" : activity}</p>

      <div className="dfConsoleSidebar__section" aria-label="NOW">
        <h3>NOW</h3>
        <p className="dfConsoleSidebar__now">{current?.title ?? "idle"}</p>
      </div>

      {errorCopy === undefined ? null : <p className="dfFactoryConsole__terminalError" role="alert">{errorCopy}</p>}

      <AgentConfig key={formKey(agent.id, agent.revision)} agent={agent} pending={edit?.pending === true} ready={ready} onSave={onSaveConfig} />

      <div className="dfConsoleSidebar__section" aria-label="Agent queue">
        <h3>QUEUE</h3>
        {queued.length === 0 ? <p className="dfFactoryConsole__empty">nothing queued</p> : (
          <ul className="dfFactoryConsole__list">
            {queued.map((task, index) => (
              <QueuedTask
                key={formKey(task.id, task.revision)}
                task={task}
                above={queued[index - 1]}
                below={queued[index + 1]}
                peers={peers}
                pending={edit?.pending === true}
                ready={ready}
                onEditTask={onEditTask}
              />
            ))}
          </ul>
        )}
        {children}
      </div>

      {onOpenTerminal === undefined ? null : (
        <button type="button" className="dfConsoleSidebar__terminal" disabled={!ready} onClick={onOpenTerminal}>OPEN TERMINAL</button>
      )}
    </section>
  );
}

function AgentConfig({
  agent,
  pending,
  ready,
  onSave,
}: {
  agent: AgentItem;
  pending: boolean;
  ready: boolean;
  onSave?: (config: AgentConfigEdit) => void;
}) {
  const [model, setModel] = useState(agent.model);
  const [reasoningEffort, setReasoningEffort] = useState(agent.reasoning_effort);
  const [paused, setPaused] = useState(agent.paused);
  if (onSave === undefined) return null;
  // Sending a control the operator did not touch would make the daemon
  // revalidate it, so a stored pair it no longer accepts could not be paused.
  const submit = (event: FormEvent) => {
    event.preventDefault();
    onSave({
      ...(model === agent.model ? {} : { model }),
      ...(reasoningEffort === agent.reasoning_effort ? {} : { reasoningEffort }),
      ...(paused === agent.paused ? {} : { paused }),
    });
  };
  return (
    <form className="dfConsoleSidebar__section dfConsoleSidebar__config" aria-label="Agent configuration" onSubmit={submit}>
      <h3>CONFIG</h3>
      <label htmlFor={`df-model-${agent.id}`}>MODEL</label>
      <input id={`df-model-${agent.id}`} value={model} disabled={pending} onChange={(event) => setModel(event.currentTarget.value)} />
      <label htmlFor={`df-effort-${agent.id}`}>REASONING EFFORT</label>
      <input id={`df-effort-${agent.id}`} value={reasoningEffort} disabled={pending} onChange={(event) => setReasoningEffort(event.currentTarget.value)} />
      <label className="dfConsoleSidebar__toggle" htmlFor={`df-paused-${agent.id}`}>
        <input id={`df-paused-${agent.id}`} type="checkbox" checked={paused} disabled={pending} onChange={(event) => setPaused(event.currentTarget.checked)} />
        PAUSED
      </label>
      <button type="submit" disabled={pending || !ready}>{pending ? "SAVING" : "SAVE"}</button>
    </form>
  );
}

/**
 * Reorder is expressed in the durable priority the daemon already orders by:
 * one step up is the neighbour above's priority plus one, one step down is the
 * neighbour below's minus one.
 */
function QueuedTask({
  task,
  above,
  below,
  peers,
  pending,
  ready,
  onEditTask,
}: {
  task: TaskItem;
  above?: TaskItem;
  below?: TaskItem;
  peers: readonly AgentItem[];
  pending: boolean;
  ready: boolean;
  onEditTask?: (task: TaskItem, change: TaskEdit) => void;
}) {
  const [title, setTitle] = useState(task.title);
  const disabled = pending || !ready || onEditTask === undefined;
  if (onEditTask === undefined) {
    return <li className="dfConsoleSidebar__task"><p className="dfConsoleRow__title">{task.title}</p></li>;
  }
  return (
    <li className="dfConsoleSidebar__task">
      <label className="dfFactoryConsole__visuallyHidden" htmlFor={`df-title-${task.id}`}>Title for {task.title}</label>
      <input
        id={`df-title-${task.id}`}
        value={title}
        disabled={disabled}
        onChange={(event) => setTitle(event.currentTarget.value)}
        onBlur={() => { if (title !== task.title && title.trim().length > 0) onEditTask(task, { title }); }}
      />
      <div className="dfConsoleSidebar__taskActions">
        <button type="button" aria-label={`Move ${task.title} up`} disabled={disabled || above === undefined} onClick={() => { if (above !== undefined) onEditTask(task, { priority: above.priority + 1 }); }}>▲</button>
        <button type="button" aria-label={`Move ${task.title} down`} disabled={disabled || below === undefined} onClick={() => { if (below !== undefined) onEditTask(task, { priority: below.priority - 1 }); }}>▼</button>
        <label className="dfFactoryConsole__visuallyHidden" htmlFor={`df-assign-${task.id}`}>Agent for {task.title}</label>
        <select
          id={`df-assign-${task.id}`}
          value={task.assigned_agent_id}
          disabled={disabled}
          onChange={(event) => onEditTask(task, { assignedAgentId: event.currentTarget.value })}
        >
          {peers.map((peer) => <option key={peer.id} value={peer.id}>{peer.name}</option>)}
        </select>
        <button type="button" disabled={disabled} onClick={() => onEditTask(task, { cancel: true })}>CANCEL</button>
      </div>
    </li>
  );
}

/** The whole-factory readout, the address it is served from, and pairing. */
export function SettingsPanel({
  state,
  address,
  pairing,
  onClose,
}: {
  state: StateView | undefined;
  address: string;
  /** A self-contained "PAIR A PHONE" surface mounts here. */
  pairing?: ReactNode;
  onClose?: () => void;
}) {
  return (
    <section className="dfConsoleSidebar__panel" aria-label="Settings">
      <div className="dfConsoleSidebar__heading">
        <h2>SETTINGS</h2>
        {onClose === undefined ? null : <button type="button" onClick={onClose}>CLOSE</button>}
      </div>
      <div className="dfConsoleSidebar__section" aria-label="BUILDING">
        <h3>BUILDING</h3>
        {state === undefined ? <p className="dfFactoryConsole__empty">BUILDING STATE UNAVAILABLE</p> : (
          <dl className="dfFactoryConsole__metrics">
            <div><dt>DISPATCH</dt><dd>{state.factory.dispatch_enabled ? "ENABLED" : "PAUSED"}</dd></div>
            <div><dt>CAPACITY</dt><dd>{String(state.factory.capacity)}</dd></div>
            <div><dt>ACTIVE RUNS</dt><dd>{String(state.factory.active_runs)}</dd></div>
            <div><dt>REVISION</dt><dd>{state.factory.revision.toString()}</dd></div>
          </dl>
        )}
      </div>
      <div className="dfConsoleSidebar__section" aria-label="This factory">
        <h3>THIS FACTORY</h3>
        <p className="dfConsoleSidebar__address">{address}</p>
      </div>
      <div className="dfConsoleSidebar__section" aria-label="PAIRING">
        <h3>PAIRING</h3>
        {pairing ?? <p className="dfFactoryConsole__empty">phone pairing arrives here</p>}
      </div>
    </section>
  );
}

/** The decision card: one question, its answer, and the two exits. */
export function HumanRequestPanel({
  selected,
  project,
  agent,
  task,
  onReplyChange,
  onReply,
  onCancel,
  onClose,
  onOpenTerminal,
  terminalReady,
}: {
  selected: FactoryHumanRequestView;
  project: string;
  agent: string;
  task: string;
  onReplyChange?: (reply: string) => void;
  onReply?: () => void;
  onCancel?: () => void;
  onClose?: () => void;
  onOpenTerminal?: (request: FactoryHumanRequestView["request"]) => void;
  terminalReady: boolean;
}) {
  const busy = selected.phase === "replying" || selected.phase === "cancelling";
  const submit = (event: FormEvent) => { event.preventDefault(); onReply?.(); };
  return (
    <article className="dfConsoleSidebar__panel dfFactoryConsole__humanRequest" aria-label="Selected question" aria-live="polite">
      <div className="dfConsoleSidebar__heading">
        <div>
          <p className="dfFactoryConsole__eyebrow">{project} · {task}</p>
          <h2>{agent} needs you</h2>
        </div>
        <button type="button" disabled={busy || onClose === undefined} onClick={onClose}>CLOSE</button>
      </div>
      <p className="dfConsoleSidebar__status">{selected.phase === "replying" ? "ANSWERING" : selected.phase.toUpperCase()}</p>
      {selected.phase === "loading" ? <p className="dfFactoryConsole__empty">LOADING THE QUESTION…</p> : (
        <>
          <p className="dfFactoryConsole__question">{selected.question}</p>
          {selected.canReply ? (
            <form className="dfFactoryConsole__reply" aria-label="Answer this question" onSubmit={submit}>
              <label htmlFor="dfHumanRequestReply">YOUR ANSWER</label>
              <textarea
                id="dfHumanRequestReply"
                value={selected.reply}
                maxLength={selected.replyMaxBytes}
                disabled={busy || onReplyChange === undefined}
                onChange={(event) => onReplyChange?.(event.currentTarget.value)}
              />
              <button type="submit" disabled={busy || onReply === undefined}>{selected.phase === "replying" ? "ANSWERING…" : "ANSWER"}</button>
            </form>
          ) : null}
          <div className="dfFactoryConsole__humanActions">
            {selected.canCancel ? <button type="button" disabled={busy || onCancel === undefined} onClick={onCancel}>Stop</button> : null}
            {onOpenTerminal === undefined ? null : <button type="button" disabled={busy || !terminalReady} onClick={() => onOpenTerminal(selected.request)}>OPEN TERMINAL</button>}
          </div>
        </>
      )}
    </article>
  );
}
