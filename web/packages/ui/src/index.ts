export { FactoryApp } from "./factory-app.js";
export type { FactoryAppProps } from "./factory-app.js";
export type { FactoryAppStatus } from "./factory-app-controller.js";
export { FactoryConsole } from "./factory-console.js";
export type { ConsoleView, FactoryConsoleProps } from "./factory-console.js";
export { AgentStrip, QueueScreen, StageMeter } from "./console-screens.js";
export { RemoteApp } from "./remote/remote-app.js";
export type { RemoteAppProps } from "./remote/remote-app.js";
export {
  STAGE_SEQUENCE,
  agentActivity,
  agentCurrentTask,
  agentGlyph,
  factoryCounters,
  floorScene,
  orderTasksForHome,
  stageMeterFill,
  stageOfTask,
} from "./console-view.js";
export type {
  AgentActivity,
  FactoryCounters,
  FloorScene,
  TaskStage,
} from "./console-view.js";
