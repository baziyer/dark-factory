import { useEffect, useRef, useState, type FormEvent } from "react";
import type { AgentItem, SpriteAppearance } from "@dark-factory/client";
import { AgentSprite } from "./factory-scene.js";
import { automaticAppearance, randomAppearance, resolvedAppearance, spriteOptions } from "./appearance.js";

const groups = ["skin", "hair", "hair_colour", "face", "outfit", "clothes_colour", "shoes", "tool", "headwear"] as const;
const labels = { skin: "SKIN TONE", hair: "HAIR STYLE", hair_colour: "HAIR COLOUR", face: "FACE DETAIL", outfit: "CLOTHING STYLE", clothes_colour: "CLOTHING COLOUR", shoes: "SHOES", tool: "TOOL", headwear: "HEADWEAR" } as const;
const activities = ["waiting", "busy", "needs-you", "idle"] as const;

export function SpriteEditor({ agent, pending, onSave, onClose }: {
  agent: AgentItem;
  pending: boolean;
  onSave: (appearance: SpriteAppearance) => Promise<boolean>;
  onClose: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [draft, setDraft] = useState(() => resolvedAppearance(agent));
  const [activity, setActivity] = useState<(typeof activities)[number]>("waiting");
  const [submitting, setSubmitting] = useState(false);
  useEffect(() => { dialog.current?.showModal(); }, []);
  const close = () => dialog.current?.close();
  const submit = async (event: FormEvent) => { event.preventDefault(); setSubmitting(true); if (await onSave(draft.automatic ? { ...draft, skin: 0, hair: 0, hair_colour: 0, face: 0, outfit: 0, clothes_colour: 0, shoes: 0, tool: 0, headwear: 0 } : draft)) close(); else setSubmitting(false); };
  return <dialog ref={dialog} className="dfConsoleDialog dfSpriteEditor" aria-label={`Edit appearance for ${agent.name}`} onClose={onClose} onClick={(event) => { if (event.target === dialog.current) close(); }}>
    <form className="dfConsoleSidebar__panel" onSubmit={submit}>
      <div className="dfConsoleSidebar__heading"><h2>EDIT APPEARANCE · {agent.name}</h2><button type="button" onClick={close}>CLOSE</button></div>
      <div className="dfSpriteEditor__preview">
        <AgentSprite agent={{ ...agent, appearance: draft }} activity={activity} />
        <div className="dfConsoleViewToggle" role="group" aria-label="Preview activity">
          {activities.map((value) => <button key={value} type="button" aria-pressed={activity === value} onClick={() => setActivity(value)}>{value.replace("needs-you", "ALERT")}</button>)}
        </div>
      </div>
      <div className="dfSpriteEditor__groups">
        {groups.map((group) => <label key={group}><span>{labels[group]}</span><select value={draft[group]} onChange={(event) => setDraft({ ...draft, automatic: false, [group]: Number(event.target.value) })}>
          {spriteOptions[group].map((option, index) => <option key={option.name} value={index}>{option.label}</option>)}
        </select></label>)}
      </div>
      <div className="dfSpriteEditor__actions">
        <button type="button" onClick={() => setDraft(randomAppearance())}>RANDOMISE</button>
        <button type="button" aria-pressed={draft.automatic} onClick={() => setDraft(automaticAppearance(agent))}>AUTOMATIC</button>
        <button type="submit" disabled={pending || submitting}>{pending || submitting ? "SAVING" : "SAVE"}</button>
      </div>
    </form>
  </dialog>;
}
