import type { ReactNode } from "react";
import type { AccountItem, AgentItem, TaskHistoryView, TaskItem, TaskListView } from "@dark-factory/client";
import { BROWSER_HOST, type FactoryAgentSelection, type FactoryAppSnapshot, type FactoryHumanRequestView } from "./factory-app-controller.js";
import { AgentList, FactoryFloor } from "./console-screens.js";
import { AgentPanel, HumanRequestPanel, QueuePanel, SettingsDialog, editErrorCopy, type AgentConfigEdit, type AgentPanelView, type DiscoveredAccount, type TaskEdit, type TaskBrief } from "./console-sidebar.js";
import { RemoteInvitePanel } from "./remote-invite.js";
import { factoryCounters, stageOfTask } from "./console-view.js";

export type ConsoleView = "floor" | "agents";
export type ConsoleDetail = "needs-you" | "queue" | "agent";

export type FactoryConsoleProps = FactoryAppSnapshot & {
  view?: ConsoleView;
  onView?: (view: ConsoleView) => void;
  detail?: ConsoleDetail;
  onDetail?: (detail: ConsoleDetail) => void;
  agentPanel?: AgentPanelView;
  onAgentPanel?: (panel: AgentPanelView) => void;
  settingsOpen?: boolean;
  onToggleSettings?: () => void;
  selectedAgent?: FactoryAgentSelection;
  onSelectAgent?: (agent: AgentItem) => void;
  onSaveAgentConfig?: (config: AgentConfigEdit) => void;
  onEditTask?: (task: TaskItem, change: TaskEdit) => Promise<boolean>;
  onLoadTaskDetail?: (task: TaskItem, peerOffset?: bigint, expectedHead?: bigint) => Promise<TaskBrief>;
  onLoadTaskHistory?: (task: TaskItem) => Promise<TaskHistoryView>;
  onLoadTaskList?: (agentId: string, cursor?: { beforeUpdatedAtMs?: bigint; beforeTaskId?: string }) => Promise<TaskListView>;
  onOpenTerminalForHumanRequest?: (request: FactoryHumanRequestView["request"]) => void;
  onSelectHumanRequest?: (request: FactoryHumanRequestView["request"]) => void;
  onHumanReplyChange?: (reply: string) => void;
  onReplyHumanRequest?: () => void;
  onCancelHumanRequest?: () => void;
  onCloseHumanRequest?: () => void;
  onInviteRemote?: () => void;
  onDismissRemoteInvite?: () => void;
  onLoadAccounts?: () => void;
  onLinkAccount?: (login: DiscoveredAccount, label: string) => void;
  onUpdateAccount?: (account: AccountItem, change: { label?: string; remove?: boolean }) => void;
  /** The loopback address this console is served from. */
  address?: string;
  /** Overrides the pairing surface the settings modal mounts by default. */
  pairing?: ReactNode;
  /** The selected agent's mounted terminal and durable composer. */
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
  ["unsupported", "The factory does not support this request yet."],
]);

/** One screen: a factory or agent list beside one operator detail panel. */
export function FactoryConsole({
  status,
  state,
  error,
  topologies,
  runPaths,
  lastRunPaths,
  edit,
  view = "floor",
  onView,
  detail,
  onDetail,
  agentPanel,
  onAgentPanel,
  settingsOpen,
  onToggleSettings,
  selectedHumanRequest,
  selectedAgent,
  onSelectAgent,
  onSaveAgentConfig,
  onEditTask,
  onLoadTaskDetail,
  onLoadTaskHistory,
  onLoadTaskList,
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
  accounts,
  accountsPending,
  accountsError,
  onLoadAccounts,
  onLinkAccount,
  onUpdateAccount,
  address = BROWSER_HOST,
  pairing,
  terminalContent,
}: FactoryConsoleProps) {
  const ready = status === "ready";
  const counters = factoryCounters(state);
  const agent = selectedAgent === undefined ? undefined : state?.agents.get(selectedAgent.id);
  const selectedDetail = detail ?? (selectedAgent === undefined ? "needs-you" : "agent");
  const editError = editErrorCopy(edit);

  return (
    <div className="dfConsoleShell">
      <main className="dfFactoryConsole" aria-label="Factory operator console">
        <header className="dfFactoryConsole__header">
          <div>
            <p className="dfFactoryConsole__eyebrow">OPERATOR VIEW</p>
            <h1>DARK FACTORY</h1>
          </div>
          <dl className="dfConsoleBar__counters" aria-label="Factory counters">
            <Counter label="ACTIVE RUNS" value={state === undefined ? "—" : String(state.factory.active_runs)} />
          </dl>
          <div className="dfConsoleBar__actions">
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

        <div className="dfConsoleLayout">
          <section className="dfConsoleLayout__left dfFactoryConsole__section" aria-label={view === "floor" ? "Factory floor" : "Agents"}>
            <div className="dfFactoryConsole__sectionHeading">
              <h2>{view === "floor" ? "FACTORY FLOOR" : "AGENTS"}</h2>
              <div className="dfConsoleViewToggle" role="group" aria-label="Left view">
                {(["floor", "agents"] as const).map((option) => (
                  <button
                    key={option}
                    type="button"
                    aria-pressed={view === option}
                    disabled={!ready || onView === undefined}
                    onClick={() => onView?.(option)}
                  >
                    {option === "floor" ? "FACTORY" : "AGENTS"}
                  </button>
                ))}
              </div>
            </div>
            {view === "floor"
              ? <FactoryFloor selectedAgentId={selectedDetail === "agent" ? selectedAgent?.id : undefined} state={state} topologies={topologies} runPaths={runPaths} lastRunPaths={lastRunPaths} onSelectAgent={ready ? onSelectAgent : undefined} />
              : <AgentList state={state} selectedAgentId={selectedAgent?.id} ready={ready} onSelectAgent={ready ? onSelectAgent : undefined} />}
          </section>

          <aside className="dfConsoleSidebar" aria-label="Selected detail">
            <div className="dfConsoleViewToggle" role="group" aria-label="Right panel">
              <button type="button" aria-pressed={selectedDetail === "needs-you"} disabled={!ready || onDetail === undefined} onClick={() => onDetail?.("needs-you")}>NEEDS YOU <span>{counters.needsYou ?? "—"}</span></button>
              <button type="button" aria-pressed={selectedDetail === "queue"} disabled={!ready || onDetail === undefined} onClick={() => onDetail?.("queue")}>QUEUE <span>{counters.queued ?? "—"}</span></button>
              <button type="button" aria-pressed={selectedDetail === "agent"} disabled={!ready || onDetail === undefined} onClick={() => onDetail?.("agent")}>AGENT</button>
            </div>
            {editError === undefined ? null : <p className="dfFactoryConsole__terminalError" role="alert">{editError}</p>}
            <div hidden={selectedDetail !== "needs-you"}>
              <NeedsYouColumn
                state={state}
                status={status}
                selectedHumanRequest={selectedHumanRequest}
                onSelectHumanRequest={onSelectHumanRequest}
                onCloseHumanRequest={onCloseHumanRequest}
                selectedContent={selectedHumanRequest === undefined ? null : <HumanRequestPanel
                  selected={selectedHumanRequest}
                  onReplyChange={onHumanReplyChange}
                  onReply={onReplyHumanRequest}
                  onCancel={onCancelHumanRequest}
                  onClose={onCloseHumanRequest}
                  onOpenTerminal={onOpenTerminalForHumanRequest}
                  terminalReady={ready}
                />}
              />
            </div>
            <div hidden={selectedDetail !== "queue"}>
              <QueuePanel
                state={state}
                edit={edit}
                ready={ready}
                onEditTask={onEditTask}
                onLoadTaskDetail={onLoadTaskDetail}
              />
            </div>
            <div hidden={selectedDetail !== "agent"}>
              {agent === undefined ? <p className="dfFactoryConsole__empty">SELECT AN AGENT TO OPEN CONTROLS</p> : <AgentPanel
                key={agent.id}
                agent={agent}
                state={state}
                edit={edit}
                ready={ready}
                onSaveConfig={onSaveAgentConfig}
                onEditTask={onEditTask}
                onLoadTaskDetail={onLoadTaskDetail}
                onLoadTaskHistory={onLoadTaskHistory}
                onLoadTaskList={onLoadTaskList}
                terminalContent={terminalContent}
                panel={agentPanel}
                onPanel={onAgentPanel}
              />}
            </div>
          </aside>
        </div>
      </main>
      {settingsOpen !== true ? null : (
        <SettingsDialog
          state={state}
          address={address}
          accounts={accounts}
          accountsPending={accountsPending}
          accountsError={accountsError}
          onLoadAccounts={onLoadAccounts}
          onLinkAccount={onLinkAccount}
          onUpdateAccount={onUpdateAccount}
          pairing={pairing ?? (!remoteInviteAllowed ? undefined : (
            <RemoteInvitePanel invite={remoteInvite} error={remoteInviteError} onInvite={onInviteRemote} onDismiss={onDismissRemoteInvite} />
          ))}
          onClose={onToggleSettings}
        />
      )}
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
  selectedHumanRequest,
  onSelectHumanRequest,
  onCloseHumanRequest,
  selectedContent,
}: Pick<FactoryConsoleProps, "state" | "status" | "selectedHumanRequest" | "onSelectHumanRequest" | "onCloseHumanRequest"> & { selectedContent?: ReactNode }) {
  const requests = state === undefined ? undefined : [...state.humanRequests.values()];
  const busy = selectedHumanRequest?.phase === "replying" || selectedHumanRequest?.phase === "cancelling";
  return (
    <section className="dfConsoleSidebar__panel" aria-label="NEEDS YOU">
      <div className="dfFactoryConsole__sectionHeading">
        <h2>NEEDS YOU</h2>
        <span>{requests?.length ?? "—"} {requests?.length === 1 ? "ITEM" : "ITEMS"}</span>
      </div>
      {requests === undefined ? <p className="dfFactoryConsole__empty">WAITING FOR SNAPSHOT</p>
        : requests.length === 0 ? <p className="dfFactoryConsole__empty">all quiet — nothing needs you</p> : (
          <ul className="dfConsoleItems">
            {requests.map((request) => {
              const selected = selectedHumanRequest?.request.id === request.id;
              const label = request.status.replaceAll("_", " ").toUpperCase();
              const disabled = status !== "ready" || busy || (selected ? onCloseHumanRequest === undefined : onSelectHumanRequest === undefined);
              return (
                <li key={request.id}>
                  <details className="dfConsoleItem" open={selected}>
                    <summary className="dfConsoleItem__summary" aria-disabled={disabled} onClick={(event) => {
                      event.preventDefault();
                      if (disabled) return;
                      if (selected) onCloseHumanRequest?.();
                      else onSelectHumanRequest?.(request);
                    }}>
                      <strong>{entityLabel(state?.agents, request.agent_id, "AGENT")} asks</strong>
                      <span className="dfConsoleItem__meta">{label} · {projectLabel(state?.projects, request.project_id)} · {entityLabel(state?.tasks, request.task_id, "TASK")}</span>
                    </summary>
                    {selected ? <div className="dfConsoleItem__detail">{selectedContent}</div> : null}
                  </details>
                </li>
              );
            })}
          </ul>
        )}
    </section>
  );
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
