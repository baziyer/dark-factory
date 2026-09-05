import type { ReactNode } from "react";
import type { AgentItem, TaskItem } from "@dark-factory/client";
import { BROWSER_HOST, type FactoryAgentSelection, type FactoryAppSnapshot, type FactoryHumanRequestView } from "./factory-app-controller.js";
import { AgentList, FactoryFloor, StageMeter } from "./console-screens.js";
import { AgentPanel, HumanRequestPanel, SettingsPanel, type AgentConfigEdit, type TaskEdit } from "./console-sidebar.js";
import { RemoteInvitePanel } from "./remote-invite.js";
import { factoryCounters, stageOfTask } from "./console-view.js";

export type ConsoleView = "floor" | "agents";

/** The right column stays short: the rest is one click away in the sidebar. */
const RIGHT_COLUMN_LIMIT = 8;

export type FactoryConsoleProps = FactoryAppSnapshot & {
  view?: ConsoleView;
  onView?: (view: ConsoleView) => void;
  settingsOpen?: boolean;
  onToggleSettings?: () => void;
  selectedAgent?: FactoryAgentSelection;
  onSelectAgent?: (agent: AgentItem) => void;
  onCloseAgent?: () => void;
  onOpenAgentTerminal?: () => void;
  onSaveAgentConfig?: (config: AgentConfigEdit) => void;
  onEditTask?: (task: TaskItem, change: TaskEdit) => void;
  onOpenTerminalForHumanRequest?: (request: FactoryHumanRequestView["request"]) => void;
  onSelectHumanRequest?: (request: FactoryHumanRequestView["request"]) => void;
  onHumanReplyChange?: (reply: string) => void;
  onReplyHumanRequest?: () => void;
  onCancelHumanRequest?: () => void;
  onCloseHumanRequest?: () => void;
  onInviteRemote?: () => void;
  onDismissRemoteInvite?: () => void;
  /** The loopback address this console is served from. */
  address?: string;
  /** Overrides the pairing surface the settings panel mounts by default. */
  pairing?: ReactNode;
  /** The agent's enqueue composer, shown under the sidebar queue. */
  instructionContent?: ReactNode;
  /** The live terminal; when present it is the whole sidebar. */
  terminalContent?: ReactNode;
};

const STATUS_LABELS: Record<FactoryAppSnapshot["status"], string> = {
  idle: "IDLE",
  connecting: "CONNECTING",
  authenticating: "AUTHENTICATING",
  syncing: "SYNCING",
  ready: "READY",
  closed: "CLOSED",
};

const ERROR_LABELS = new Map<string, string>([
  ["connection", "Connection unavailable."],
  ["closed", "Connection closed."],
  ["pairing_required", "Pair this browser client before connecting."],
  ["pairing_uncertain", "Pairing result is uncertain. Pair this browser client again."],
  ["storage_unavailable", "Browser key storage is unavailable."],
  ["crypto_unavailable", "Browser cryptography is unavailable."],
  ["malformed", "The server sent an invalid frame."],
  ["oversized", "The server frame exceeded the protocol limit."],
  ["wrong_direction", "The server sent an invalid frame direction."],
  ["unauthorized", "This browser client is not authorized."],
  ["invalid_request", "The request was rejected."],
  ["rate_limited", "The request was rate limited."],
  ["not_found", "The requested item was not found."],
  ["stale", "The requested state is stale."],
  ["too_large", "The request was too large."],
  ["internal", "The server could not complete the request."],
]);

/** One screen: the floor or the agents on the left, what needs you on the right. */
export function FactoryConsole({
  status,
  state,
  error,
  topology,
  edit,
  view = "floor",
  onView,
  settingsOpen,
  onToggleSettings,
  selectedHumanRequest,
  selectedAgent,
  onSelectAgent,
  onCloseAgent,
  onOpenAgentTerminal,
  onSaveAgentConfig,
  onEditTask,
  onOpenTerminalForHumanRequest,
  onSelectHumanRequest,
  onHumanReplyChange,
  onReplyHumanRequest,
  onCancelHumanRequest,
  onCloseHumanRequest,
  remoteInviteAllowed,
  remoteInvite,
  remoteInviteError,
  onInviteRemote,
  onDismissRemoteInvite,
  address = BROWSER_HOST,
  pairing,
  instructionContent,
  terminalContent,
}: FactoryConsoleProps) {
  const ready = status === "ready";
  const counters = factoryCounters(state);
  const agent = selectedAgent === undefined ? undefined : state?.agents.get(selectedAgent.id);
  const sidebar = terminalContent ?? (
    selectedHumanRequest !== undefined ? (
      <HumanRequestPanel
        selected={selectedHumanRequest}
        project={projectLabel(state?.projects, selectedHumanRequest.request.project_id)}
        agent={entityLabel(state?.agents, selectedHumanRequest.request.agent_id, "AGENT")}
        task={entityLabel(state?.tasks, selectedHumanRequest.request.task_id, "TASK")}
        onReplyChange={onHumanReplyChange}
        onReply={onReplyHumanRequest}
        onCancel={onCancelHumanRequest}
        onClose={onCloseHumanRequest}
        onOpenTerminal={onOpenTerminalForHumanRequest}
        terminalReady={ready}
      />
    ) : agent !== undefined ? (
      <AgentPanel
        agent={agent}
        state={state}
        edit={edit}
        ready={ready}
        onSaveConfig={onSaveAgentConfig}
        onEditTask={onEditTask}
        onOpenTerminal={onOpenAgentTerminal}
        onClose={onCloseAgent}
      >
        {instructionContent}
      </AgentPanel>
    ) : settingsOpen === true ? (
      <SettingsPanel
        state={state}
        address={address}
        pairing={pairing ?? (!remoteInviteAllowed ? undefined : (
          <RemoteInvitePanel invite={remoteInvite} error={remoteInviteError} onInvite={onInviteRemote} onDismiss={onDismissRemoteInvite} />
        ))}
        onClose={onToggleSettings}
      />
    ) : undefined
  );

  return (
    <div className="dfConsoleShell">
      <main className="dfFactoryConsole" aria-label="Factory operator console">
        <header className="dfFactoryConsole__header">
          <div>
            <p className="dfFactoryConsole__eyebrow">OPERATOR VIEW</p>
            <h1>FACTORY</h1>
          </div>
          <dl className="dfConsoleBar__counters" aria-label="Factory counters">
            <Counter label="ACTIVE RUNS" value={state === undefined ? "—" : `${state.factory.active_runs}/${state.factory.capacity}`} />
            <Counter label="QUEUED" value={`${counters.queued ?? "—"}`} />
            <Counter label="NEEDS YOU" value={`${counters.needsYou ?? "—"}`} alert={(counters.needsYou ?? 0) > 0} />
          </dl>
          <div className="dfConsoleBar__actions">
            <div className="dfConsoleBar__toggle" role="group" aria-label="Left view">
              {(["floor", "agents"] as const).map((option) => (
                <button
                  key={option}
                  type="button"
                  aria-pressed={view === option}
                  disabled={onView === undefined}
                  onClick={() => onView?.(option)}
                >
                  {option.toUpperCase()}
                </button>
              ))}
            </div>
            <button type="button" aria-pressed={settingsOpen === true} disabled={onToggleSettings === undefined} onClick={onToggleSettings}>SETTINGS</button>
          </div>
          <div
            className={`dfFactoryConsole__connection${ready ? " dfFactoryConsole__visuallyHidden" : ""}`}
            aria-label={`Connection status: ${STATUS_LABELS[status]}`}
          >
            <p className="dfFactoryConsole__status" role="status" aria-live="polite" aria-atomic="true">
              <span className={`dfFactoryConsole__statusDot dfFactoryConsole__statusDot--${status}`} aria-hidden="true" />
              {STATUS_LABELS[status]}
            </p>
          </div>
        </header>

        {error === undefined ? null : (
          <p className="dfFactoryConsole__error" role="alert">
            {ERROR_LABELS.get(error.code) ?? "The connection could not continue."}
          </p>
        )}

        <div className={`dfConsoleLayout${sidebar === undefined ? "" : " dfConsoleLayout--narrow"}`}>
          <section className="dfConsoleLayout__left dfFactoryConsole__section" aria-label={view === "floor" ? "Factory floor" : "Agents"}>
            {view === "floor"
              ? <FactoryFloor state={state} topology={topology} onSelectAgent={ready ? onSelectAgent : undefined} />
              : <AgentList state={state} selectedAgentId={selectedAgent?.id} ready={ready} onSelectAgent={onSelectAgent} />}
          </section>

          {sidebar !== undefined ? null : (
            <div className="dfConsoleLayout__right">
              <NeedsYouColumn
                state={state}
                status={status}
                selectedHumanRequestId={selectedHumanRequest?.request.id}
                onSelectHumanRequest={onSelectHumanRequest}
              />
              <QueueColumn state={state} />
            </div>
          )}
        </div>
      </main>
      {sidebar === undefined ? null : <aside className="dfConsoleSidebar" aria-label="Selected detail">{sidebar}</aside>}
    </div>
  );
}

function Counter({ label, value, alert }: { label: string; value: string; alert?: boolean }) {
  return (
    <div className={alert === true ? "dfConsoleBar__counter dfConsoleBar__counter--alert" : "dfConsoleBar__counter"}>
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}

function NeedsYouColumn({
  state,
  status,
  selectedHumanRequestId,
  onSelectHumanRequest,
}: Pick<FactoryConsoleProps, "state" | "status" | "onSelectHumanRequest"> & { selectedHumanRequestId?: string }) {
  const requests = state === undefined ? undefined : [...state.humanRequests.values()];
  return (
    <section className="dfFactoryConsole__section" aria-label="NEEDS YOU">
      <div className="dfFactoryConsole__sectionHeading">
        <h2>NEEDS YOU</h2>
        <span>{requests?.length ?? "—"} {requests?.length === 1 ? "ITEM" : "ITEMS"}</span>
      </div>
      {requests === undefined ? <p className="dfFactoryConsole__empty">WAITING FOR SNAPSHOT</p>
        : requests.length === 0 ? <p className="dfFactoryConsole__empty">all quiet — nothing needs you</p> : (
          <ul className="dfFactoryConsole__list dfFactoryConsole__list--requests">
            {requests.slice(0, RIGHT_COLUMN_LIMIT).map((request) => {
              const selected = selectedHumanRequestId === request.id;
              const copy = humanRequestStatusCopy(request.status);
              return (
                <li className="dfFactoryConsole__card" key={request.id}>
                  <div className="dfFactoryConsole__cardTitle">
                    <strong>{entityLabel(state?.agents, request.agent_id, "AGENT")} asks</strong>
                    <span>{copy.label}</span>
                  </div>
                  <p>{projectLabel(state?.projects, request.project_id)} · {entityLabel(state?.tasks, request.task_id, "TASK")}</p>
                  <small>{copy.description}</small>
                  {onSelectHumanRequest === undefined ? null : (
                    <button type="button" aria-pressed={selected} disabled={selected || status !== "ready"} onClick={() => onSelectHumanRequest(request)}>
                      {selected ? "OPEN" : "VIEW"}
                    </button>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      <Overflow shown={Math.min(requests?.length ?? 0, RIGHT_COLUMN_LIMIT)} total={requests?.length ?? 0} />
    </section>
  );
}

function QueueColumn({ state }: Pick<FactoryConsoleProps, "state">) {
  const tasks = state === undefined ? undefined : [...state.tasks.values()]
    .filter((task) => task.status === "queued" || task.status === "running")
    .sort((left, right) => (left.status === right.status ? right.priority - left.priority : left.status === "running" ? -1 : 1));
  return (
    <section className="dfFactoryConsole__section" aria-label="Queue">
      <div className="dfFactoryConsole__sectionHeading">
        <h2>QUEUE</h2>
        <span>{tasks === undefined ? "— queued" : `${tasks.length} open`}</span>
      </div>
      {tasks === undefined ? <p className="dfFactoryConsole__empty">waiting for snapshot</p>
        : tasks.length === 0 ? <p className="dfFactoryConsole__empty">the queue is empty</p> : (
          <ul className="dfConsoleRows">
            {tasks.slice(0, RIGHT_COLUMN_LIMIT).map((task) => (
              <li key={task.id}>
                <div className="dfConsoleRow">
                  <span className="dfConsoleRow__glyph" aria-hidden="true">{task.status === "running" ? "░" : "▒"}</span>
                  <span className="dfConsoleRow__title">{task.title}</span>
                  <span className="dfConsoleRow__agent">
                    {task.status === "running" ? entityLabel(state?.agents, task.assigned_agent_id, "AGENT") : `priority ${task.priority}`}
                  </span>
                  <StageMeter stage={stageOfTask(task)} />
                </div>
              </li>
            ))}
          </ul>
        )}
      <Overflow shown={Math.min(tasks?.length ?? 0, RIGHT_COLUMN_LIMIT)} total={tasks?.length ?? 0} />
    </section>
  );
}

function Overflow({ shown, total }: { shown: number; total: number }) {
  if (total <= shown) return null;
  return <p className="dfFactoryConsole__empty">+{total - shown} more</p>;
}

function projectLabel(projects: ReadonlyMap<string, { name: string }> | undefined, projectID: string): string {
  return projects?.get(projectID)?.name ?? `PROJECT ${shortID(projectID)}`;
}

function entityLabel(entities: ReadonlyMap<string, { name?: string; title?: string }> | undefined, id: string, fallback: string): string {
  const entity = entities?.get(id);
  return entity?.name ?? entity?.title ?? `${fallback} ${shortID(id)}`;
}

function shortID(value: string): string {
  return value.slice(0, 8);
}

function humanRequestStatusCopy(status: FactoryHumanRequestView["request"]["status"]): Readonly<{ label: string; description: string }> {
  switch (status) {
    case "open":
      return { label: "OPEN", description: "Awaiting your answer" };
    case "delivering":
      return { label: "DELIVERING", description: "Answer delivery in progress" };
    case "delivery_unknown":
      return { label: "DELIVERY UNKNOWN", description: "Answer delivery could not be confirmed" };
  }
}
