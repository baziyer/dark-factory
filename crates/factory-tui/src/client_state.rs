//! The one piece of client-side state that survives a restart: which
//! project was focused last (whichever way it changed — `p`, `Enter` on a
//! FORTRESS station, or `Tab`/`j`/`k`/`g`/`G` crossing into another
//! project's agent), so the next `factory-tui` opens where the operator
//! left off unless `--project` says otherwise. Lives at
//! `$DARK_FACTORY_HOME/factory-tui.json`, so it belongs to that home's
//! daemon; a board pointed at some other daemon with an explicit `--socket`
//! neither reads nor writes it (see `main.rs`). Nothing else reads it.

use std::{fs, path::PathBuf};

use factory_core::{AgentId, ProjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    focused_project: Option<ProjectId>,
}

/// Navigation restored after the updater replaces the TUI process image.
/// Typing mode is deliberately not retained: the replacement returns keyboard
/// control to the board on the same screen and selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RelaunchState {
    pub focused_project: Option<ProjectId>,
    pub selected_agent: Option<AgentId>,
    pub agent_view: bool,
    pub terminal_maximized: bool,
}

pub fn encode_relaunch(state: &RelaunchState) -> Result<String, String> {
    serde_json::to_string(state).map_err(|error| error.to_string())
}

pub fn decode_relaunch(value: &str) -> Result<RelaunchState, String> {
    serde_json::from_str(value).map_err(|error| format!("invalid --resume-state: {error}"))
}

fn path() -> Option<PathBuf> {
    factory_core::paths::dark_factory_home()
        .ok()
        .map(|home| home.join("factory-tui.json"))
}

/// The project focused when the last board exited, if any was saved.
#[must_use]
pub fn load_focused_project() -> Option<ProjectId> {
    let bytes = fs::read(path()?).ok()?;
    serde_json::from_slice::<State>(&bytes)
        .ok()?
        .focused_project
}

/// Best effort; a home that doesn't exist yet just means nothing is remembered.
pub fn save_focused_project(project_id: &ProjectId) {
    let Some(path) = path() else {
        return;
    };
    let state = State {
        focused_project: Some(project_id.clone()),
    };
    let Ok(bytes) = serde_json::to_vec(&state) else {
        return;
    };
    let temp = path.with_extension("json.tmp");
    if fs::write(&temp, bytes).is_ok() {
        let _ = fs::rename(temp, path);
    }
}
