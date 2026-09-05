import type { AgentItem, StateView, TopologyView } from "@dark-factory/client";
import {
  STAGE_SEQUENCE,
  agentActivity,
  agentCurrentTask,
  agentGlyph,
  factoryCounters,
  floorScene,
  stageMeterFill,
  stageOfTask,
  type TaskStage,
} from "./console-view.js";
import { FactoryScene } from "./factory-scene/factory-scene.js";

function shortID(value: string): string {
  return value.slice(0, 8);
}

function projectLabel(state: StateView | undefined, projectID: string): string {
  return state?.projects.get(projectID)?.name ?? `project ${shortID(projectID)}`;
}

/** The load-bearing cross-screen status bar: agents plus served counters. */
export function AgentStrip({
  state,
  selectedAgentId,
  ready,
  onSelectAgent,
}: {
  state: StateView | undefined;
  selectedAgentId?: string;
  ready: boolean;
  onSelectAgent?: (agent: AgentItem) => void;
}) {
  const counters = factoryCounters(state);
  return (
    <nav className="dfConsoleStrip" aria-label="Agents and factory counters">
      <ul className="dfConsoleStrip__agents">
        {state === undefined ? (
          <li className="dfConsoleStrip__empty">waiting for snapshot</li>
        ) : state.agents.size === 0 ? (
          <li className="dfConsoleStrip__empty">no agents</li>
        ) : (
          [...state.agents.values()].map((agent) => {
            const activity = agentActivity(agent, state);
            const task = agentCurrentTask(agent, state);
            const phase =
              activity === "busy" && task !== undefined ? stageOfTask(task) : activity;
            const cell = (
              <>
                <span className="dfConsoleStrip__glyph" aria-hidden="true">
                  {agentGlyph(agent)}
                </span>
                <span className="dfConsoleStrip__agentName">{agent.name}</span>
                <span className="dfConsoleStrip__agentPhase">
                  {activity === "needs-you" ? "! needs you" : phase}
                </span>
              </>
            );
            const className = `dfConsoleStrip__agent dfConsoleStrip__agent--${activity}`;
            return (
              <li key={agent.id}>
                {onSelectAgent === undefined ? (
                  <span className={className} aria-label={`${agent.name}: ${phase}`}>
                    {cell}
                  </span>
                ) : (
                  <button
                    type="button"
                    className={className}
                    aria-pressed={selectedAgentId === agent.id}
                    aria-label={`${agent.name}: ${phase}`}
                    disabled={!ready}
                    onClick={() => onSelectAgent(agent)}
                  >
                    {cell}
                  </button>
                )}
              </li>
            );
          })
        )}
      </ul>
      <div className="dfConsoleStrip__counters">
        <Counter glyph="▒" label={`${counters.queued ?? "—"} queued`} />
        <Counter glyph="!" label={`${counters.needsYou ?? "—"} NEEDS YOU`} alert={(counters.needsYou ?? 0) > 0} />
      </div>
    </nav>
  );
}

function Counter({ glyph, label, alert }: { glyph: string; label: string; alert?: boolean }) {
  return (
    <span className={`dfConsoleStrip__counter${alert === true ? " dfConsoleStrip__counter--alert" : ""}`}>
      <span aria-hidden="true">{glyph}</span> {label}
    </span>
  );
}

/** Segments fill only from durable task status. */
export function StageMeter({ stage }: { stage: TaskStage }) {
  const filled = stageMeterFill(stage);
  return (
    <span className="dfStageMeter" role="img" aria-label={`stage: ${stage}`}>
      {STAGE_SEQUENCE.map((name, index) => (
        <span
          key={name}
          className={`dfStageMeter__segment${index < filled ? " dfStageMeter__segment--filled" : ""}`}
          aria-hidden="true"
        />
      ))}
      <span
        className={`dfStageMeter__terminal${stage === "blocked" ? " dfStageMeter__terminal--blocked" : ""}${stage === "done" ? " dfStageMeter__terminal--done" : ""}${stage === "failed" ? " dfStageMeter__terminal--failed" : ""}`}
        aria-hidden="true"
      >
        {stage === "blocked" ? "!" : stage === "done" ? "✓" : stage === "failed" ? "×" : ""}
      </span>
    </span>
  );
}

/** The floor: the served topology with every agent standing in its room. */
export function FactoryFloor({
  state,
  topology,
  onSelectAgent,
}: {
  state: StateView | undefined;
  topology: TopologyView | undefined;
  onSelectAgent?: (agent: AgentItem) => void;
}) {
  const scene = floorScene(state, topology);
  return (
    <FactoryScene
      topology={scene.topology}
      workers={scene.workers}
      workItems={scene.workItems}
      onSelectWorker={onSelectAgent === undefined || state === undefined ? undefined : (workerID) => {
        const agent = state.agents.get(workerID);
        if (agent !== undefined) onSelectAgent(agent);
      }}
    />
  );
}

/** Rank is the served role: an orchestrator oversees, a worker builds. */
export function rankLabel(role: AgentItem["role"]): string {
  return role === "orchestrator" ? "OVERSEER" : "WORKER";
}

/** Agents grouped by rank, oversight first. */
export function AgentList({
  state,
  selectedAgentId,
  ready,
  onSelectAgent,
}: {
  state: StateView | undefined;
  selectedAgentId?: string;
  ready: boolean;
  onSelectAgent?: (agent: AgentItem) => void;
}) {
  if (state === undefined) return <p className="dfFactoryConsole__empty">waiting for the factory</p>;
  const agents = [...state.agents.values()];
  if (agents.length === 0) return <p className="dfFactoryConsole__empty">no agents</p>;
  return (
    <div className="dfAgentList">
      {(["orchestrator", "worker"] as const).map((role) => {
        const members = agents.filter((agent) => agent.role === role);
        if (members.length === 0) return null;
        return (
          <section key={role} aria-label={rankLabel(role)}>
            <div className="dfFactoryConsole__sectionHeading">
              <h2>{rankLabel(role)}</h2>
              <span>{members.length}</span>
            </div>
            <ul className="dfConsoleRows">
              {members.map((agent) => (
                <li key={agent.id}>
                  <AgentRow
                    agent={agent}
                    state={state}
                    selected={selectedAgentId === agent.id}
                    ready={ready}
                    onSelectAgent={onSelectAgent}
                  />
                </li>
              ))}
            </ul>
          </section>
        );
      })}
    </div>
  );
}

function AgentRow({
  agent,
  state,
  selected,
  ready,
  onSelectAgent,
}: {
  agent: AgentItem;
  state: StateView;
  selected: boolean;
  ready: boolean;
  onSelectAgent?: (agent: AgentItem) => void;
}) {
  const activity = agentActivity(agent, state);
  const task = agentCurrentTask(agent, state);
  let queued = 0;
  for (const item of state.tasks.values()) if (item.assigned_agent_id === agent.id && item.status === "queued") queued += 1;
  const label = `${agent.name}: ${activity}`;
  const cells = (
    <>
      <span className="dfConsoleRow__glyph" aria-hidden="true">{agentGlyph(agent)}</span>
      <span className="dfConsoleRow__title">{agent.name}</span>
      <span className="dfAgentList__provider">{agent.provider}</span>
      <span className="dfAgentList__activity">{activity === "needs-you" ? "! needs you" : activity}</span>
      <span className="dfConsoleRow__agent">{task?.title ?? "no current task"}</span>
      <span className="dfAgentList__count">{queued} queued</span>
    </>
  );
  if (onSelectAgent === undefined) return <span className="dfConsoleRow" aria-label={label}>{cells}</span>;
  return (
    <button
      type="button"
      className="dfConsoleRow dfAgentList__row"
      aria-label={label}
      aria-pressed={selected}
      disabled={!ready}
      onClick={() => onSelectAgent(agent)}
    >
      {cells}
    </button>
  );
}

/** Queue: the durable queued tasks, with no invented mutation controls. */
export function QueueScreen({ state }: { state: StateView | undefined }) {
  const queued =
    state === undefined
      ? undefined
      : [...state.tasks.values()].filter((task) => stageOfTask(task) === "queued");
  return (
    <section className="dfFactoryConsole__section" aria-label="Queue">
      <div className="dfFactoryConsole__sectionHeading">
        <h2>QUEUE</h2>
        <span>{queued === undefined ? "— queued" : `${queued.length} queued`}</span>
      </div>
      {queued === undefined ? (
        <p className="dfFactoryConsole__empty">waiting for snapshot</p>
      ) : queued.length === 0 ? (
        <p className="dfFactoryConsole__empty">the queue is empty</p>
      ) : (
        <ul className="dfConsoleRows">
          {queued.map((task) => (
            <li key={task.id}>
              <div className="dfConsoleRow">
                <span className="dfConsoleRow__glyph" aria-hidden="true">
                  ▒
                </span>
                <span className="dfConsoleRow__title">{task.title}</span>
                <span className="dfConsoleRow__agent">
                  priority {task.priority} · {projectLabel(state, task.project_id)}
                </span>
                <StageMeter stage="queued" />
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
