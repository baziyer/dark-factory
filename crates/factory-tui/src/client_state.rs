//! The one piece of client-side state that survives a restart: which
//! project was focused last (`p`, `[`/`]`, or `Enter` on a FORTRESS
//! station), so the next `factory-tui` opens where the operator left off
//! unless `--project` says otherwise. Lives at
//! `$DARK_FACTORY_HOME/factory-tui.json`; nothing else reads it.

use std::{fs, path::PathBuf};

use factory_core::ProjectId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    focused_project: Option<ProjectId>,
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
