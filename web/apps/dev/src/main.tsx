import { useState } from "react";
import { createRoot } from "react-dom/client";
import { FactoryApp, FactoryConsole, type FactoryConsoleProps } from "@dark-factory/ui";
import "@dark-factory/ui/styles.css";
import "./styles.css";
import { fixtureState, fixtureTopology } from "../../../fixtures/state.mjs";

// Fixture tour: sample data, no daemon, no authority. Reply/cancel and edit
// handlers are deliberately absent so one-shot actions cannot pretend to
// succeed.
function FixtureTour() {
  const [view, setView] = useState<FactoryConsoleProps["view"]>("floor");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [selectedAgent, setSelectedAgent] = useState<FactoryConsoleProps["selectedAgent"]>();
  const [selectedHumanRequest, setSelectedHumanRequest] = useState<FactoryConsoleProps["selectedHumanRequest"]>();
  return (
    <>
      <p className="devFixtureBanner" role="note">
        FIXTURE TOUR — sample data, no daemon. Actions that need the factory are inert here.
      </p>
      <FactoryConsole
        status="ready"
        state={fixtureState}
        topology={fixtureTopology}
        view={view}
        onView={setView}
        settingsOpen={settingsOpen}
        onToggleSettings={() => {
          setSelectedAgent(undefined);
          setSelectedHumanRequest(undefined);
          setSettingsOpen((open) => !open);
        }}
        selectedAgent={selectedAgent}
        selectedHumanRequest={selectedHumanRequest}
        onSelectAgent={(agent) => {
          setSettingsOpen(false);
          setSelectedHumanRequest(undefined);
          setSelectedAgent({ id: agent.id, name: agent.name, revision: agent.revision });
        }}
        onCloseAgent={() => setSelectedAgent(undefined)}
        onSelectHumanRequest={(request) => {
          setSettingsOpen(false);
          setSelectedHumanRequest({
            request,
            phase: "ready",
            question: "Should the migration also cover the users table? The plan only names accounts.",
            canReply: false,
            canCancel: false,
            replyMaxBytes: request.reply_max_bytes,
            reply: "",
          });
        }}
        onCloseHumanRequest={() => setSelectedHumanRequest(undefined)}
      />
    </>
  );
}

const root = createRoot(document.getElementById("root")!);
if (new URLSearchParams(window.location.search).has("fixture")) {
  root.render(<FixtureTour />);
} else {
  root.render(<FactoryApp />);
}
