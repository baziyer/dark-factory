import type { SpriteAppearance } from "@dark-factory/client";
import { spriteAtlas, spriteOptions } from "./sprites/sprites.generated.js";
import type { SceneWorker } from "./scene.js";

export { spriteOptions };
export type { SpriteAppearance };

export const appearanceFields = ["skin", "hair", "hair_colour", "face", "outfit", "clothes_colour", "shoes", "tool", "headwear"] as const;

function identity(id: string): number {
  let hash = 2166136261;
  for (let i = 0; i < id.length; i++) hash = Math.imul(hash ^ id.charCodeAt(i), 16777619) >>> 0;
  return hash % 4;
}

export function automaticAppearance(worker: Pick<SceneWorker, "id" | "role">): SpriteAppearance {
  const slot = identity(worker.id);
  return {
    automatic: true,
    skin: slot < 2 ? 1 : 2,
    hair: slot,
    hair_colour: slot,
    face: 0,
    outfit: slot,
    clothes_colour: slot,
    shoes: slot,
    tool: worker.role === "orchestrator" ? 1 : 0,
    headwear: worker.role === "orchestrator" ? 1 : 0,
  };
}

export function resolvedAppearance(worker: Pick<SceneWorker, "id" | "role" | "appearance">): SpriteAppearance {
  const value = worker.appearance;
  if (value === undefined || value.automatic) return automaticAppearance(worker);
  return Object.fromEntries([
    ["automatic", false],
    ...appearanceFields.map((field) => [field, Number.isInteger(value[field]) && value[field] >= 0 && value[field] < spriteOptions[field].length ? value[field] : 0]),
  ]) as SpriteAppearance;
}

/** The aligned atlas layers for one worker; combinations grow additively. */
export function workerFrames(worker: SceneWorker): readonly string[] {
  const appearance = resolvedAppearance(worker);
  const role = worker.role === "orchestrator" ? "overseer" : "worker";
  const provider = worker.provider === "claude_code" || worker.provider === "codex" ? worker.provider : "shell";
  const activity = ["busy", "waiting", "needs-you", "idle"].includes(worker.activity) ? worker.activity : "idle";
  const frames = [
    `person.skin.${appearance.skin}.${activity}`,
    `person.outfit.${appearance.outfit}.${appearance.clothes_colour}.${activity}`,
    `person.hair.${appearance.hair}.${appearance.hair_colour}.${activity}`,
    `person.face.${appearance.face}.${activity}`,
    `person.shoes.${appearance.shoes}.${activity}`,
    `person.tool.${appearance.tool}.${activity}`,
    `person.headwear.${appearance.headwear}.${activity}`,
    `person.system.${role}.${provider}.${activity}`,
  ];
  return frames;
}

export function randomAppearance(random = Math.random): SpriteAppearance {
  return Object.fromEntries([
    ["automatic", false],
    ...appearanceFields.map((field) => [field, Math.floor(random() * spriteOptions[field].length)]),
  ]) as SpriteAppearance;
}
