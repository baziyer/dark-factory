import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import type { AccountItem, AgentItem, StateView, TaskItem, TaskPeerQuestion } from "@dark-factory/client";
import type { FactoryEditView, FactoryHumanRequestView } from "./factory-app-controller.js";
import { rankLabel } from "./console-screens.js";
import { agentStatus, agentCurrentTask, orderTasksForHome } from "./console-view.js";

/** Only the controls the operator actually changed; the rest are left alone. */
export type AgentConfigEdit = Readonly<{ model?: string; reasoningEffort?: string; accountId?: string; paused?: boolean; idlePolicy?: "wait" | "standing_instruction"; idleAfterSeconds?: number; idleInstruction?: string; idleRunBudget?: number }>;

/** One discovered login and the account row it is linked to, if any. */
export type DiscoveredAccount = Readonly<{
  provider: "claude_code" | "codex";
  home: string;
  label: string;
  email: string;
  organization: string;
  default_model: string;
  default_reasoning_effort: string;
  linked_id: string;
}>;

export type TaskEdit = Readonly<{ title?: string; body?: string; priority?: number; assignedAgentId?: string; cancel?: boolean }>;
export type TaskBrief = Readonly<{ taskId: string; revision: bigint; head: bigint; instruction: string; feedback: string; peerQuestions: readonly TaskPeerQuestion[]; nextPeerOffset?: bigint }>;
export type AgentPanelView = "terminal" | "config";

/** One private peer-conversation page, shared by queued and completed work. */
export function TaskConversation({ brief, onOlder, pending = false }: { brief: TaskBrief; onOlder?: () => void; pending?: boolean }) {
  return <section aria-label="Task conversation"><h3>CONVERSATION</h3>{brief.peerQuestions.length === 0 ? <p>NO PEER QUESTIONS</p> : <ol>{brief.peerQuestions.map((question) => <li key={question.id}><strong>QUESTION · {question.source_task_id} → {question.target_task_id}</strong><span>{question.question}</span><small>RECIPIENT DELIVERY · {question.recipient_delivery_state.toUpperCase()}</small>{question.answer === undefined || question.answer === "" ? null : <><span>ANSWER · {question.answer}</span><small>ANSWER DELIVERY · {question.answer_delivery_state.toUpperCase()}</small></>}</li>)}</ol>}{brief.nextPeerOffset === undefined || onOlder === undefined ? null : <button type="button" disabled={pending} onClick={onOlder}>OLDER CONVERSATION</button>}</section>;
}

const EDIT_ERRORS = new Map<string, string>([
  ["stale", "SOMEONE ELSE CHANGED THIS — REOPEN IT AND TRY AGAIN"],
  ["invalid_request", "THE FACTORY REFUSED THIS EDIT"],
  ["not_found", "THIS NO LONGER EXISTS"],
  ["too_large", "TOO LONG"],
  ["rate_limited", "TOO MANY EDITS AT ONCE"],
  ["unauthorized", "THIS BROWSER MAY NOT EDIT"],
  ["unsupported", "THE FACTORY DOES NOT SUPPORT THIS YET"],
]);

export function editErrorCopy(edit: FactoryEditView | undefined): string | undefined {
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
  onLoadTaskDetail,
  terminalContent,
  panel: panelProp,
  onPanel,
}: {
  agent: AgentItem;
  state: StateView | undefined;
  edit?: FactoryEditView;
  ready: boolean;
  onSaveConfig?: (config: AgentConfigEdit) => void;
  onEditTask?: (task: TaskItem, change: TaskEdit) => Promise<boolean>;
  onLoadTaskDetail?: (task: TaskItem, peerOffset?: bigint, expectedHead?: bigint) => Promise<TaskBrief>;
  /** The selected agent's mounted terminal and durable composer. */
  terminalContent?: ReactNode;
  panel?: AgentPanelView;
  onPanel?: (panel: AgentPanelView) => void;
}) {
  const activity = state === undefined ? "ready" : agentStatus(agent, state);
  const current = state === undefined ? undefined : agentCurrentTask(agent, state);
  const queued = state === undefined ? [] : [...state.tasks.values()]
    .filter((task) => task.assigned_agent_id === agent.id && task.status === "queued")
    .sort((left, right) => right.priority - left.priority);
  const historyTasks = state === undefined ? [] : orderTasksForHome(state)
    .filter((task) => task.assigned_agent_id === agent.id && task.status !== "queued");
  const [conversation, setConversation] = useState<{ task: TaskItem; brief: TaskBrief }>();
  const [conversationError, setConversationError] = useState(false);
  const [conversationPending, setConversationPending] = useState(false);
  const [historyTaskID, setHistoryTaskID] = useState<string>();
  const [localPanel, setLocalPanel] = useState<AgentPanelView>("terminal");
  const panel = panelProp ?? localPanel;
  const selectPanel = onPanel ?? setLocalPanel;
  useEffect(() => { setConversation(undefined); setConversationError(false); setHistoryTaskID(undefined); }, [agent.id]);
  const historyTask = historyTasks.find((task) => task.id === historyTaskID) ?? historyTasks[0];
  const errorCopy = edit?.target === agent.id ? editErrorCopy(edit) : undefined;
  const queueHint = agent.paused
    ? "QUEUE PAUSED"
    : current === undefined && queued.length > 0
      ? "QUEUED · WAITING FOR CAPACITY"
      : undefined;
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
          <p className="dfFactoryConsole__eyebrow">{rankLabel(agent.role)} · {agent.provider}{agent.effective_model === "" ? "" : ` · ${agent.effective_model}`}</p>
          <h2>{agent.name}</h2>
        </div>
      </div>

      <p className="dfConsoleSidebar__status">{activity === "needs-you" ? "! needs you" : activity}</p>
      {queueHint === undefined ? null : <p className="dfConsoleSidebar__inherit">{queueHint}</p>}

      <div className="dfConsoleViewToggle" role="group" aria-label="Agent controls">
        <button type="button" aria-pressed={panel === "terminal"} onClick={() => selectPanel("terminal")}>TERMINAL</button>
        <button type="button" aria-pressed={panel === "config"} onClick={() => selectPanel("config")}>CONFIG</button>
      </div>
      <section className="dfConsoleSidebar__section dfConsoleSidebar__terminalSlot" aria-label="Terminal" hidden={panel !== "terminal"}>
        {terminalContent ?? <p className="dfFactoryConsole__empty">OPENING TERMINAL</p>}
      </section>

      <section className="dfConsoleSidebar__section" aria-label="Agent configuration" hidden={panel !== "config"}>
        <AgentConfig key={formKey(agent.id, agent.revision)} agent={agent} accounts={state === undefined ? [] : [...state.accounts.values()]} pending={edit?.pending === true} ready={ready} onSave={onSaveConfig} />
      </section>

      {historyTask === undefined || onLoadTaskDetail === undefined ? null : <div className="dfConsoleSidebar__section" aria-label="Task history">
        <h3>TASK HISTORY</h3>
        <label className="dfFactoryConsole__visuallyHidden" htmlFor={`df-history-${agent.id}`}>TASK HISTORY</label>
        <select id={`df-history-${agent.id}`} value={historyTask.id} disabled={conversationPending} onChange={(event) => { setHistoryTaskID(event.currentTarget.value); setConversation(undefined); setConversationError(false); }}>{historyTasks.map((task) => <option key={task.id} value={task.id}>{task.title} · {task.status.toUpperCase()} · {task.id.slice(0, 8)}</option>)}</select>
        <button type="button" disabled={conversationPending} onClick={async () => { setConversationPending(true); try { setConversation({ task: historyTask, brief: await onLoadTaskDetail(historyTask) }); setConversationError(false); } catch { setConversationError(true); } finally { setConversationPending(false); } }}>VIEW TASK</button>
        {conversationError ? <p role="alert">THE FACTORY REFUSED THIS HISTORY</p> : null}
        {conversation === undefined ? null : <>{conversation.brief.instruction === "" ? null : <><h4>ORIGINAL INSTRUCTION</h4><pre className="dfConsoleSidebar__feedback">{conversation.brief.instruction}</pre></>}{conversation.brief.feedback === "" ? null : <><h4>RETAINED REVIEW FEEDBACK</h4><pre className="dfConsoleSidebar__feedback">{conversation.brief.feedback}</pre></>}<TaskConversation brief={conversation.brief} pending={conversationPending} onOlder={conversation.brief.nextPeerOffset === undefined ? undefined : () => { void (async () => { setConversationPending(true); try { setConversation({ task: conversation.task, brief: await onLoadTaskDetail(conversation.task, conversation.brief.nextPeerOffset, conversation.brief.head) }); setConversationError(false); } catch { setConversationError(true); } finally { setConversationPending(false); } })(); }} /></>}
      </div>}
    </section>
  );
}

/** The single editable queue, grouped only to retain each agent's priority order. */
export function QueuePanel({
  state,
  edit,
  ready,
  onEditTask,
  onLoadTaskDetail,
}: {
  state: StateView | undefined;
  edit?: FactoryEditView;
  ready: boolean;
  onEditTask?: (task: TaskItem, change: TaskEdit) => Promise<boolean>;
  onLoadTaskDetail?: (task: TaskItem, peerOffset?: bigint, expectedHead?: bigint) => Promise<TaskBrief>;
}) {
  const agents = state === undefined ? [] : [...state.agents.values()];
  const tasks = state === undefined ? [] : [...state.tasks.values()];
  const running = tasks.filter((task) => task.status === "running");
  const queued = agents.flatMap((agent) => {
    const assigned = tasks
      .filter((task) => task.assigned_agent_id === agent.id && task.status === "queued")
      .sort((left, right) => right.priority - left.priority);
    return assigned.length === 0 ? [] : [{ agent, tasks: assigned }];
  });
  return <section className="dfConsoleSidebar__panel" aria-label="Queue">
    <div className="dfConsoleSidebar__heading"><h2>QUEUE</h2></div>
    {state === undefined ? <p className="dfFactoryConsole__empty">WAITING FOR SNAPSHOT</p>
      : <>
        {running.length === 0 ? null : <section className="dfConsoleSidebar__section" aria-label="Running tasks">
          <h3>RUNNING</h3>
          <ul className="dfConsoleRows">{running.map((task) => <li key={task.id}><div className="dfConsoleRow">
            <span className="dfConsoleRow__title">{task.title}</span>
            <span className="dfConsoleRow__agent">{agents.find((agent) => agent.id === task.assigned_agent_id)?.name ?? "AGENT"}</span>
          </div></li>)}</ul>
        </section>}
        {queued.length === 0 ? <p className="dfFactoryConsole__empty">THE QUEUE IS EMPTY</p> : queued.map(({ agent, tasks }) => {
          const peers = agents.filter((peer) => peer.project_id === agent.project_id);
          return <section className="dfConsoleSidebar__section" key={agent.id} aria-label={`Queue for ${agent.name}`}>
            <h3>{agent.name}</h3>
            <ul className="dfFactoryConsole__list">{tasks.map((task, index) => <QueuedTask
              key={task.id}
              task={task}
              above={tasks[index - 1]}
              below={tasks[index + 1]}
              peers={peers}
              pending={edit?.pending === true}
              ready={ready}
              onEditTask={onEditTask}
              onLoadTaskDetail={onLoadTaskDetail}
            />)}</ul>
          </section>;
        })}
      </>}
  </section>;
}

/**
 * The two inputs hold the agent's OWN override, so empty means inherit and the
 * placeholder shows what the run will use instead. This says where that came
 * from, because "inherited" is useless without the file that decided it.
 */
function modelSourceCaption(agent: AgentItem): string {
  if (agent.model_source === "agent") return "set on this agent";
  if (agent.model_source === "") return "CLI default (not visible to the factory)";
  return `inherited from ${agent.model_source}`;
}

function AgentConfig({
  agent,
  accounts,
  pending,
  ready,
  onSave,
}: {
  agent: AgentItem;
  accounts: readonly AccountItem[];
  pending: boolean;
  ready: boolean;
  onSave?: (config: AgentConfigEdit) => void;
}) {
  const [model, setModel] = useState(agent.model);
  const [reasoningEffort, setReasoningEffort] = useState(agent.reasoning_effort);
  const [accountId, setAccountId] = useState(agent.account_id);
  const [paused, setPaused] = useState(agent.paused);
  const [idlePolicy, setIdlePolicy] = useState(agent.idle_policy);
  const [idleAfterSeconds, setIdleAfterSeconds] = useState(String(agent.idle_after_seconds));
  const [idleInstruction, setIdleInstruction] = useState(agent.idle_instruction);
  const [idleRunBudget, setIdleRunBudget] = useState(String(agent.idle_run_budget));
  const [budgetTyped, setBudgetTyped] = useState(false);
  if (onSave === undefined) return null;
  // Sending a control the operator did not touch would make the daemon
  // revalidate it, so a stored pair it no longer accepts could not be paused.
  const idleAfter = Math.max(0, Math.floor(Number(idleAfterSeconds) || 0));
  const idleBudget = Math.max(0, Math.floor(Number(idleRunBudget) || 0));
  const standing = idlePolicy === "standing_instruction";
  const supervising = agent.role === "orchestrator";
  // A standing instruction is one rule, not three controls: the daemon
  // refuses a wait, text or budget it cannot run, so the form sends the whole
  // rule whenever any part of it moved, and will not submit one it can see
  // is incomplete. The budget travels only when it was typed (even the same
  // number) or the rule is new, since a budget the daemon receives starts the
  // used count again; an edit to the wait or the text leaves the count alone.
  const ruleMoved = idlePolicy !== agent.idle_policy || idleAfter !== agent.idle_after_seconds || idleInstruction !== agent.idle_instruction || idleBudget !== agent.idle_run_budget || budgetTyped;
  const ruleIncomplete = standing && (idleAfter < 1 || idleInstruction.trim() === "" || idleBudget < 1);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (ruleIncomplete) return;
    onSave({
      ...(model === agent.model ? {} : { model }),
      ...(reasoningEffort === agent.reasoning_effort ? {} : { reasoningEffort }),
      ...(accountId === agent.account_id ? {} : { accountId }),
      ...(paused === agent.paused ? {} : { paused }),
      ...(!ruleMoved ? {} : standing ? { idlePolicy, idleAfterSeconds: idleAfter, idleInstruction, ...(budgetTyped || idlePolicy !== agent.idle_policy ? { idleRunBudget: idleBudget } : {}) } : { idlePolicy }),
    });
  };
  return (
    <form className="dfConsoleSidebar__section dfConsoleSidebar__config" aria-label="Agent configuration" onSubmit={submit}>
      {agent.provider === "shell" ? <p className="dfConsoleSidebar__inherit">shell has no model</p> : (
        <>
          <label htmlFor={`df-model-${agent.id}`}>MODEL</label>
          <input id={`df-model-${agent.id}`} value={model} placeholder={agent.effective_model} disabled={pending} onChange={(event) => setModel(event.currentTarget.value)} />
          <label htmlFor={`df-effort-${agent.id}`}>REASONING EFFORT</label>
          <input id={`df-effort-${agent.id}`} value={reasoningEffort} placeholder={agent.effective_reasoning_effort} disabled={pending} onChange={(event) => setReasoningEffort(event.currentTarget.value)} />
          <p className="dfConsoleSidebar__inherit">{modelSourceCaption(agent)}</p>
          <label htmlFor={`df-account-${agent.id}`}>ACCOUNT</label>
          <select id={`df-account-${agent.id}`} value={accountId} disabled={pending} onChange={(event) => setAccountId(event.currentTarget.value)}>
            <option value="">provider default</option>
            {accounts.filter((account) => account.provider === agent.provider).map((account) => (
              <option key={account.id} value={account.id}>{account.label}</option>
            ))}
          </select>
        </>
      )}
      <label className="dfConsoleSidebar__toggle" htmlFor={`df-paused-${agent.id}`}>
        <input id={`df-paused-${agent.id}`} type="checkbox" checked={paused} disabled={pending} onChange={(event) => setPaused(event.currentTarget.checked)} />
        PAUSED
      </label>
      <h3>{supervising ? "SUPERVISION" : "RULES"}</h3>
      <label htmlFor={`df-idle-${agent.id}`}>{supervising ? "WHEN WORK CHANGES" : "WHEN READY"}</label>
      <select id={`df-idle-${agent.id}`} value={idlePolicy} disabled={pending} onChange={(event) => setIdlePolicy(event.currentTarget.value as typeof idlePolicy)}>
        <option value="wait">wait for work</option>
        <option value="standing_instruction">{supervising ? "supervise worker activity" : "run a standing instruction"}</option>
      </select>
      {idlePolicy === "standing_instruction" ? (
        <>
          <label htmlFor={`df-idle-after-${agent.id}`}>{supervising ? "COOLDOWN SECONDS" : "AFTER SECONDS READY"}</label>
          <input id={`df-idle-after-${agent.id}`} inputMode="numeric" value={idleAfterSeconds} disabled={pending} onChange={(event) => setIdleAfterSeconds(event.currentTarget.value)} />
          <label htmlFor={`df-idle-instruction-${agent.id}`}>INSTRUCTION</label>
          <textarea id={`df-idle-instruction-${agent.id}`} rows={3} value={idleInstruction} disabled={pending} onChange={(event) => setIdleInstruction(event.currentTarget.value)} />
          <label htmlFor={`df-idle-budget-${agent.id}`}>RUN BUDGET</label>
          <input id={`df-idle-budget-${agent.id}`} inputMode="numeric" value={idleRunBudget} disabled={pending} onChange={(event) => { setBudgetTyped(true); setIdleRunBudget(event.currentTarget.value); }} />
          <p className="dfConsoleSidebar__inherit">{agent.idle_runs_used} of {agent.idle_run_budget} idle runs used · type the budget again to start the count again</p>
          {supervising ? <p className="dfConsoleSidebar__inherit">initial inspection, then worker events</p> : null}
          {ruleIncomplete ? <p className="dfConsoleSidebar__inherit">a standing instruction needs at least a second, text and a budget of one run</p> : null}
        </>
      ) : null}
      <button type="submit" disabled={pending || !ready || ruleIncomplete}>{pending ? "SAVING" : "SAVE"}</button>
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
  onLoadTaskDetail,
}: {
  task: TaskItem;
  above?: TaskItem;
  below?: TaskItem;
  peers: readonly AgentItem[];
  pending: boolean;
  ready: boolean;
  onEditTask?: (task: TaskItem, change: TaskEdit) => Promise<boolean>;
  onLoadTaskDetail?: (task: TaskItem, peerOffset?: bigint, expectedHead?: bigint) => Promise<TaskBrief>;
}) {
  const [brief, setBrief] = useState<TaskBrief>();
  const [title, setTitle] = useState(task.title);
  const [instruction, setInstruction] = useState("");
  const [loading, setLoading] = useState(false);
  const [detailError, setDetailError] = useState(false);
  const [open, setOpen] = useState(false);
  const [openedRevision, setOpenedRevision] = useState<bigint>();
  const disabled = pending || !ready || onEditTask === undefined;
  const stale = open && openedRevision !== task.revision;
  const load = async (peerOffset?: bigint, expectedHead?: bigint) => {
    if (onLoadTaskDetail === undefined) return;
    setLoading(true);
    setDetailError(false);
    try {
      const loaded = await onLoadTaskDetail(task, peerOffset, expectedHead);
      setBrief(loaded);
      if (peerOffset === undefined) {
        setTitle(task.title);
        setInstruction(loaded.instruction);
        setOpenedRevision(task.revision);
        setOpen(true);
      }
    } catch {
      setDetailError(true);
    } finally {
      setLoading(false);
    }
  };
  if (onEditTask === undefined) {
    return <li className="dfConsoleSidebar__task"><p className="dfConsoleRow__title">{task.title}</p></li>;
  }
  return (
    <li className="dfConsoleSidebar__task">
      {open ? <>
        <label htmlFor={`df-title-${task.id}`}>TITLE</label>
        <input id={`df-title-${task.id}`} value={title} disabled={disabled || loading || stale} onChange={(event) => setTitle(event.currentTarget.value)} />
        <label htmlFor={`df-instruction-${task.id}`}>INSTRUCTION</label>
        <textarea id={`df-instruction-${task.id}`} rows={4} value={instruction} disabled={disabled || loading || stale} onChange={(event) => setInstruction(event.currentTarget.value)} />
        {brief?.feedback === "" || brief === undefined ? null : <><label>RETAINED REVIEW FEEDBACK</label><pre className="dfConsoleSidebar__feedback">{brief.feedback}</pre></>}
		{brief === undefined ? null : <TaskConversation brief={brief} pending={loading} onOlder={brief.nextPeerOffset === undefined ? undefined : () => { void load(brief.nextPeerOffset, brief.head); }} />}
		{stale ? <p role="alert">TASK CHANGED — REOPEN BRIEF TO SAVE</p> : null}
        <div className="dfConsoleSidebar__taskActions">
          <button type="button" disabled={disabled || loading || stale || title.trim() === ""} onClick={async () => { if (await onEditTask(task, { title, body: instruction })) setOpen(false); }}>{pending ? "SAVING" : "SAVE BRIEF"}</button>
          {stale ? <button type="button" disabled={loading} onClick={() => { void load(); }}>REOPEN BRIEF</button> : null}
          <button type="button" disabled={loading} onClick={() => setOpen(false)}>DISCARD</button>
        </div>
      </> : <><p className="dfConsoleRow__title">{task.title}</p><button type="button" disabled={disabled || onLoadTaskDetail === undefined} onClick={() => { void load(); }}>{loading ? "LOADING BRIEF" : "EDIT BRIEF"}</button></>}
      {detailError ? <p role="alert">{open ? "COULD NOT LOAD DETAILS. SAVE OR DISCARD YOUR DRAFT, THEN REOPEN TO RETRY." : "COULD NOT LOAD DETAILS. REOPEN THE BRIEF TO RETRY."}</p> : null}
      <div className="dfConsoleSidebar__taskActions">
        <button type="button" aria-label={`Move ${task.title} up`} disabled={disabled || above === undefined} onClick={() => { if (above !== undefined) void onEditTask(task, { priority: above.priority + 1 }); }}>▲</button>
        <button type="button" aria-label={`Move ${task.title} down`} disabled={disabled || below === undefined} onClick={() => { if (below !== undefined) void onEditTask(task, { priority: below.priority - 1 }); }}>▼</button>
        <label className="dfFactoryConsole__visuallyHidden" htmlFor={`df-assign-${task.id}`}>Agent for {task.title}</label>
        <select
          id={`df-assign-${task.id}`}
          value={task.assigned_agent_id}
          disabled={disabled}
          onChange={(event) => { void onEditTask(task, { assignedAgentId: event.currentTarget.value }); }}
        >
          {peers.map((peer) => <option key={peer.id} value={peer.id}>{peer.name}</option>)}
        </select>
        <button type="button" disabled={disabled} onClick={() => { void onEditTask(task, { cancel: true }); }}>CANCEL</button>
      </div>
    </li>
  );
}

/**
 * The whole-factory readout, the address it is served from, and pairing, over
 * the console rather than squeezing it: <dialog> owns ESC, the backdrop, the
 * focus trap and focus return, so every exit goes through close().
 */
export function SettingsDialog({
  state,
  address,
  accounts,
  accountsPending,
  accountsError,
  onLoadAccounts,
  onLinkAccount,
  pairing,
  onClose,
}: {
  state: StateView | undefined;
  address: string;
  /** The logins the daemon found, once it has been asked. */
  accounts?: readonly DiscoveredAccount[];
  accountsPending?: boolean;
  accountsError?: string;
  onLoadAccounts?: () => void;
  onLinkAccount?: (login: DiscoveredAccount, label: string) => void;
  /** A self-contained "PAIR A PHONE" surface mounts here. */
  pairing?: ReactNode;
  onClose?: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  // Discovery is an observation of the daemon's machine, so it is asked for
  // when the dialog opens rather than carried in the durable snapshot.
  const load = useRef(onLoadAccounts);
  load.current = onLoadAccounts;
  useEffect(() => { load.current?.(); }, []);
  const close = () => dialog.current?.close();
  return (
    <dialog
      className="dfConsoleDialog"
      ref={dialog}
      aria-label="Settings"
      onClose={onClose}
      onClick={(event) => { if (event.target === dialog.current) close(); }}
    >
      <div className="dfConsoleSidebar__panel">
        <div className="dfConsoleSidebar__heading">
          <h2>SETTINGS</h2>
          {onClose === undefined ? null : <button type="button" onClick={close}>CLOSE</button>}
        </div>
        <div className="dfConsoleSidebar__section" aria-label="BUILDING">
          <h3>BUILDING</h3>
          {state === undefined ? <p className="dfFactoryConsole__empty">BUILDING STATE UNAVAILABLE</p> : (
            <dl className="dfFactoryConsole__metrics">
              <div><dt>DISPATCH</dt><dd>{state.factory.dispatch_enabled ? "ENABLED" : "PAUSED"}</dd></div>
              <div><dt>WORKER SLOTS</dt><dd>{String(state.factory.capacity)}</dd></div>
              <div><dt>ACTIVE RUNS</dt><dd>{`${state.factory.active_runs} TOTAL`}</dd></div>
              <div><dt>REVISION</dt><dd>{state.factory.revision.toString()}</dd></div>
            </dl>
          )}
        </div>
        <div className="dfConsoleSidebar__section" aria-label="This factory">
          <h3>THIS FACTORY</h3>
          <p className="dfConsoleSidebar__address">{address}</p>
        </div>
        <AccountsSection
          state={state}
          accounts={accounts}
          pending={accountsPending === true}
          error={accountsError}
          onLink={onLinkAccount}
        />
        <div className="dfConsoleSidebar__section" aria-label="PAIRING">
          <h3>PAIRING</h3>
          {pairing ?? <p className="dfFactoryConsole__empty">phone pairing arrives here</p>}
        </div>
      </div>
    </dialog>
  );
}

/**
 * The provider logins on this machine. Linking registers one that already
 * exists; signing a CLI in is that CLI's own job, so there is no button for it.
 */
function AccountsSection({
  state,
  accounts,
  pending,
  error,
  onLink,
}: {
  state: StateView | undefined;
  accounts?: readonly DiscoveredAccount[];
  pending: boolean;
  error?: string;
  onLink?: (login: DiscoveredAccount, label: string) => void;
}) {
  const [labels, setLabels] = useState<Record<string, string>>({});
  const linked = state === undefined ? [] : [...state.accounts.values()];
  const unlinked = (accounts ?? []).filter((login) => login.linked_id === "");
  return (
    <div className="dfConsoleSidebar__section" aria-label="ACCOUNTS">
      <h3>ACCOUNTS</h3>
      {error === undefined ? null : <p className="dfFactoryConsole__terminalError" role="alert">{EDIT_ERRORS.get(error) ?? "THE FACTORY REFUSED THIS"}</p>}
      {linked.length === 0 ? <p className="dfFactoryConsole__empty">NO ACCOUNTS LINKED</p> : (
        <ul className="dfFactoryConsole__list">
          {linked.map((account) => {
            const login = (accounts ?? []).find((candidate) => candidate.linked_id === account.id);
            const identity = [login?.email, login?.organization, login?.default_model].filter((part) => part !== undefined && part !== "").join(" · ");
            return (
              <li key={account.id} className="dfConsoleSidebar__account">
                <p className="dfConsoleRow__title">{account.label} · {account.provider}</p>
                <p className="dfFactoryConsole__eyebrow">{identity === "" ? account.home : identity}</p>
              </li>
            );
          })}
        </ul>
      )}
      <h3>DISCOVERED</h3>
      {accounts === undefined ? <p className="dfFactoryConsole__empty">{pending ? "LOOKING" : "NOT LOOKED YET"}</p>
        : unlinked.length === 0 ? <p className="dfFactoryConsole__empty">EVERY LOGIN IS LINKED</p> : (
        <ul className="dfFactoryConsole__list">
          {unlinked.map((login) => (
            <li key={login.home} className="dfConsoleSidebar__account">
              <p className="dfConsoleRow__title">{login.home}</p>
              <p className="dfFactoryConsole__eyebrow">{[login.provider, login.email, login.default_model].filter((part) => part !== "").join(" · ")}</p>
              <div className="dfConsoleSidebar__taskActions">
                <label className="dfFactoryConsole__visuallyHidden" htmlFor={`df-account-label-${login.home}`}>Label for {login.home}</label>
                <input
                  id={`df-account-label-${login.home}`}
                  value={labels[login.home] ?? login.label}
                  disabled={pending || onLink === undefined}
                  onChange={(event) => { const value = event.currentTarget.value; setLabels((current) => ({ ...current, [login.home]: value })); }}
                />
                <button
                  type="button"
                  disabled={pending || onLink === undefined || (labels[login.home] ?? login.label).trim() === ""}
                  onClick={() => onLink?.(login, labels[login.home] ?? login.label)}
                >
                  LINK
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
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
