//! Mouse routing for board controls drawn in the latest frame.

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use factory_core::status::AttentionItem;
use factory_core::{AgentId, TaskId};

use crate::model::View;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    View(View),
    Help,
    Detach,
    Update,
    Agent(AgentId),
    Task(TaskId),
    Attention(AttentionItem),
    AttentionChoice(AttentionItem, usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Region {
    area: Rect,
    target: Target,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HitMap {
    regions: Vec<Region>,
}

impl HitMap {
    pub fn add(&mut self, area: Rect, target: Target) {
        if area.width > 0 && area.height > 0 {
            self.regions.push(Region { area, target });
        }
    }

    pub fn add_row(&mut self, area: Rect, row: usize, target: Target) {
        let Ok(row) = u16::try_from(row) else {
            return;
        };
        if row < area.height {
            self.add(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                target,
            );
        }
    }

    #[must_use]
    pub fn target_at(&self, column: u16, row: u16) -> Option<Target> {
        self.regions
            .iter()
            .rev()
            .find(|region| contains(region.area, column, row))
            .map(|region| region.target.clone())
    }
}

#[must_use]
pub fn route(event: MouseEvent, hits: &HitMap) -> Option<Target> {
    matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
        .then(|| hits.target_at(event.column, event.row))
        .flatten()
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_left_click_target_wins() {
        let mut hits = HitMap::default();
        hits.add(Rect::new(0, 0, 5, 2), Target::Help);
        hits.add(Rect::new(1, 0, 2, 1), Target::Detach);
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        };
        assert_eq!(route(event, &hits), Some(Target::Detach));
    }
}
