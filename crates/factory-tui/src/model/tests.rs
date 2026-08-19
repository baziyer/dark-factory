//! Tests for `Board`'s data/derived-view methods (fleet snapshot application, session-vs-run
//! precedence, terminal-attach targets, route/queue/capacity derivation). Key-handling tests live
//! next to `keymap()` in `keymap.rs`.

use super::*;
use crate::test_fixtures::{agent, project, run, session};
use factory_core::{RunStatus, SessionState};

fn board() -> Board {
    Board::new(false, 0, crate::theme::FORTRESS)
}

// -- fleet snapshot / focus --------------------------------------------------------------------

#[test]
fn apply_fleet_snapshot_focuses_the_oldest_project_by_default() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("b", 10), project("a", 0)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(b.focused_project.unwrap().as_str(), "a");
}

#[test]
fn focus_project_only_succeeds_for_a_known_project() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    b.focus_project(ProjectId::try_from("nonexistent").unwrap());
    assert_eq!(
        b.focused_project.unwrap().as_str(),
        "a",
        "unknown project should not steal focus"
    );
    b.focused_project = None;
    b.focus_project(ProjectId::try_from("a").unwrap());
    assert_eq!(b.focused_project.unwrap().as_str(), "a");
}

#[test]
fn project_deleted_event_clears_scoped_state_and_refocuses() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0), project("b", 1)],
        vec![agent("orch", "a", AgentRole::Orchestrator, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(b.focused_project.as_ref().unwrap().as_str(), "a");
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 0,
        event: FactoryEvent::ProjectDeleted {
            project_id: ProjectId::try_from("a").unwrap(),
        },
    });
    assert!(b.agents.is_empty());
    assert_eq!(b.focused_project.as_ref().unwrap().as_str(), "b");
}

#[test]
fn fleet_snapshot_prunes_removed_agents_and_resets_reused_ids() {
    let mut b = board();
    let alice = AgentId::try_from("alice").unwrap();
    let bob = AgentId::try_from("bob").unwrap();
    b.apply_fleet_snapshot(
        vec![project("a", 0), project("b", 1)],
        vec![
            agent("alice", "a", AgentRole::Worker, None),
            agent("bob", "b", AgentRole::Worker, None),
        ],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    for (sequence, session_id, agent_id, project_id) in
        [(1, "sess-a", "alice", "a"), (2, "sess-b", "bob", "b")]
    {
        b.apply_event(EventEnvelope {
            protocol_version: 1,
            sequence,
            occurred_at_ms: 0,
            event: FactoryEvent::SessionChanged {
                session: session(session_id, agent_id, project_id, SessionState::Working),
            },
        });
    }
    b.apply_fleet_snapshot(
        vec![project("b", 1)],
        vec![agent("alice", "b", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert!(
        !b.activity.contains_key(&bob),
        "removed agents must be pruned"
    );
    assert!(
        !b.activity.contains_key(&alice),
        "an id reused by another project must not inherit old history"
    );

    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 3,
        occurred_at_ms: 0,
        event: FactoryEvent::SessionChanged {
            session: session("sess-new", "alice", "b", SessionState::Working),
        },
    });
    assert_eq!(b.activity[&alice].counts(), vec![1]);
}

#[test]
fn fleet_snapshot_resets_same_project_agent_generation() {
    let alice = AgentId::try_from("alice").unwrap();
    let mut old_alice = agent("alice", "a", AgentRole::Worker, None);
    old_alice.created_at_ms = 100;
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![old_alice],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 100,
        event: FactoryEvent::SessionChanged {
            session: session("sess-old", "alice", "a", SessionState::Working),
        },
    });

    let mut recreated_alice = agent("alice", "a", AgentRole::Worker, None);
    recreated_alice.created_at_ms = 200;
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![recreated_alice],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert!(
        !b.activity.contains_key(&alice),
        "a recreated agent must not inherit its predecessor's same-project history"
    );

    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 2,
        occurred_at_ms: 200,
        event: FactoryEvent::SessionChanged {
            session: session("sess-new", "alice", "a", SessionState::Working),
        },
    });
    assert_eq!(b.activity[&alice].counts(), vec![1]);
}

#[test]
fn snapshot_then_replay_rejects_predecessor_generation_activity() {
    let alice = AgentId::try_from("alice").unwrap();
    let mut current_alice = agent("alice", "a", AgentRole::Worker, None);
    current_alice.created_at_ms = 5_000;
    let mut b = Board::new(false, 10_000, crate::theme::FORTRESS);
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![current_alice],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );

    b.apply_replay(vec![EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 1_000,
        event: FactoryEvent::SessionChanged {
            session: session("sess-old", "alice", "a", SessionState::Working),
        },
    }]);
    assert!(
        !b.activity.contains_key(&alice),
        "predecessor replay activity must not be assigned to the current generation"
    );

    b.apply_replay(vec![EventEnvelope {
        protocol_version: 1,
        sequence: 2,
        occurred_at_ms: 5_000,
        event: FactoryEvent::SessionChanged {
            session: session("sess-current", "alice", "a", SessionState::Working),
        },
    }]);
    assert_eq!(b.activity[&alice].counts().iter().sum::<u64>(), 1);
}

#[test]
fn project_deleted_removes_replayed_project_activity() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0), project("b", 1)],
        vec![
            agent("alice", "a", AgentRole::Worker, None),
            agent("bob", "b", AgentRole::Worker, None),
        ],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    b.apply_replay(vec![
        EventEnvelope {
            protocol_version: 1,
            sequence: 1,
            occurred_at_ms: 0,
            event: FactoryEvent::SessionChanged {
                session: session("sess-a", "alice", "a", SessionState::Working),
            },
        },
        EventEnvelope {
            protocol_version: 1,
            sequence: 2,
            occurred_at_ms: 0,
            event: FactoryEvent::SessionChanged {
                session: session("sess-b", "bob", "b", SessionState::Working),
            },
        },
        EventEnvelope {
            protocol_version: 1,
            sequence: 3,
            occurred_at_ms: 1,
            event: FactoryEvent::ProjectDeleted {
                project_id: ProjectId::try_from("a").unwrap(),
            },
        },
    ]);
    assert!(
        !b.activity
            .contains_key(&AgentId::try_from("alice").unwrap()),
        "project deletion must remove every owned activity series"
    );
    assert_eq!(
        b.activity[&AgentId::try_from("bob").unwrap()].counts(),
        vec![1]
    );
}

#[test]
fn session_changed_event_updates_the_sessions_map() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 0,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    });
    assert_eq!(
        b.sessions
            .get(&SessionId::try_from("sess-1").unwrap())
            .unwrap()
            .state,
        SessionState::Working
    );
}

#[test]
fn budget_event_updates_the_effective_pause_projection() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let agent_id = AgentId::try_from("alice").unwrap();
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 0,
        event: FactoryEvent::AgentBudgetChanged {
            project_id: ProjectId::try_from("a").unwrap(),
            agent_id: agent_id.clone(),
            budget: factory_core::AgentBudget {
                exhausted: true,
                ..Default::default()
            },
            action: "denied".into(),
            paused: true,
            pause_reasons: vec![factory_core::status::AgentPauseReason::BudgetExhausted],
        },
    });
    assert!(b.agents[&agent_id].paused);
}

// -- announcement dedup ---------------------------------------------------------------------

/// The bug: the daemon emits one `SessionChanged` per hook, most of which don't change `state`
/// at all — the first dogfood run saw 65 announcements in a few minutes, almost all "session
/// working" repeated by hooks that touched the row without moving it. Feeding the same state N
/// times must announce exactly once.
#[test]
fn session_changed_events_with_unchanged_state_announce_only_once() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    for i in 0..5 {
        b.apply_event(EventEnvelope {
            protocol_version: 1,
            sequence: i,
            occurred_at_ms: i,
            event: FactoryEvent::SessionChanged {
                session: session("sess-1", "alice", "a", SessionState::Working),
            },
        });
    }
    assert_eq!(b.announcements.len(), 1);
}

/// A real transition — the state actually changing — must still announce, even right after a
/// run of hook-only updates that didn't.
#[test]
fn session_changed_event_with_a_new_state_announces_again() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 0,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    });
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 2,
        occurred_at_ms: 1,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    });
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 3,
        occurred_at_ms: 2,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::WaitingForInput),
        },
    });
    assert_eq!(b.announcements.len(), 2);
}

/// The very first `SessionChanged` seen for a session (nothing recorded yet, `None`) always
/// announces — dedup only ever suppresses a *repeat* of a known state.
#[test]
fn first_session_changed_event_for_a_session_always_announces() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 0,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Starting),
        },
    });
    assert_eq!(b.announcements.len(), 1);
}

// -- connect-time replay (#67 backfilled announcements, #70 backfilled sparklines) ----------

#[test]
fn apply_replay_adds_announcements_in_time_order_regardless_of_batch_order() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    // Fed newest-first on purpose — `apply_replay` must not just trust whatever order the batch
    // arrived in.
    b.apply_replay(vec![
        EventEnvelope {
            protocol_version: 1,
            sequence: 2,
            occurred_at_ms: 200,
            event: FactoryEvent::SessionChanged {
                session: session("sess-1", "alice", "a", SessionState::WaitingForInput),
            },
        },
        EventEnvelope {
            protocol_version: 1,
            sequence: 1,
            occurred_at_ms: 100,
            event: FactoryEvent::SessionChanged {
                session: session("sess-1", "alice", "a", SessionState::Working),
            },
        },
    ]);
    let texts: Vec<&str> = b.announcements.iter().map(|a| a.text.as_str()).collect();
    assert_eq!(texts.len(), 2);
    assert!(
        texts[0].contains("working"),
        "oldest event first: {texts:?}"
    );
    assert!(
        texts[1].contains("waiting for input"),
        "newest event last: {texts:?}"
    );
}

#[test]
fn apply_replay_does_not_repeat_unchanged_session_states_within_the_batch() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let events = (0..5)
        .map(|i| EventEnvelope {
            protocol_version: 1,
            sequence: i,
            occurred_at_ms: i,
            event: FactoryEvent::SessionChanged {
                session: session("sess-1", "alice", "a", SessionState::Working),
            },
        })
        .collect();
    b.apply_replay(events);
    assert_eq!(
        b.announcements.len(),
        1,
        "same dedupe as the live path: a repeated unchanged state announces once"
    );
}

#[test]
fn apply_replay_then_a_live_redelivery_of_the_same_event_does_not_duplicate() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let event = EventEnvelope {
        protocol_version: 1,
        sequence: 7,
        occurred_at_ms: 700,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    };
    b.apply_replay(vec![event.clone()]);
    assert_eq!(b.announcements.len(), 1);
    // The live subscription starts right where the replay left off, but must stay correct even if
    // it (or a later reconnect) redelivers an event the replay already announced.
    b.apply_event(event);
    assert_eq!(
        b.announcements.len(),
        1,
        "the same event id (sequence) must never announce twice"
    );
    assert_eq!(
        b.activity[&AgentId::try_from("alice").unwrap()]
            .counts()
            .iter()
            .sum::<u64>(),
        1,
        "the same event id must not inflate the activity signal"
    );
}

#[test]
fn apply_replay_feeds_the_activity_sparkline() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let alice = AgentId::try_from("alice").unwrap();
    assert!(
        !b.activity.contains_key(&alice),
        "no activity before any event arrives"
    );
    b.apply_replay(vec![EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 0,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    }]);
    let counts = b.activity.get(&alice).unwrap().counts();
    assert_eq!(
        counts.iter().sum::<u64>(),
        1,
        "the sparkline's data source must be fed from replayed events too (#70)"
    );
}

#[test]
fn successive_durable_activity_events_are_visible_in_short_horizon_buckets() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let event = |sequence, occurred_at_ms| EventEnvelope {
        protocol_version: 1,
        sequence,
        occurred_at_ms,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    };
    b.apply_event(event(1, 0));
    b.tick(4_999);
    b.apply_event(event(2, 4_999));
    b.tick(5_000);
    b.apply_event(event(3, 5_000));
    assert_eq!(
        b.activity[&AgentId::try_from("alice").unwrap()].counts(),
        vec![2, 1],
        "durable events should update adjacent five-second buckets immediately"
    );
    b.tick(65_000);
    assert_eq!(
        b.activity[&AgentId::try_from("alice").unwrap()].counts(),
        vec![0],
        "a silent agent should age out without invented activity"
    );
}

#[test]
fn replayed_activity_is_aged_against_the_current_board_clock() {
    let mut b = Board::new(false, 10_000, crate::theme::FORTRESS);
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    b.apply_replay(vec![EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 0,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    }]);
    assert_eq!(
        b.activity[&AgentId::try_from("alice").unwrap()].counts(),
        vec![1, 0, 0],
        "replay should preserve recent history instead of making old activity current"
    );
}

#[test]
fn future_dated_activity_is_clamped_and_expires_from_board_time() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 60_000,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    });
    assert_eq!(
        b.activity[&AgentId::try_from("alice").unwrap()].counts(),
        vec![1],
        "future skew should be clamped into the current board bucket"
    );
    b.tick(60_000);
    assert_eq!(
        b.activity[&AgentId::try_from("alice").unwrap()].counts(),
        vec![0],
        "future activity must age out on board time instead of pinning a live bar"
    );
}

#[test]
fn apply_replay_drops_activity_for_an_agent_deleted_during_the_replay_window() {
    let mut b = board();
    let alice = AgentId::try_from("alice").unwrap();
    b.apply_replay(vec![
        EventEnvelope {
            protocol_version: 1,
            sequence: 1,
            occurred_at_ms: 0,
            event: FactoryEvent::SessionChanged {
                session: session("sess-1", "alice", "a", SessionState::Working),
            },
        },
        EventEnvelope {
            protocol_version: 1,
            sequence: 2,
            occurred_at_ms: 1,
            event: FactoryEvent::AgentDeleted {
                project_id: ProjectId::try_from("a").unwrap(),
                agent_id: alice.clone(),
            },
        },
    ]);
    assert!(
        !b.activity.contains_key(&alice),
        "a deleted agent must not leave an orphaned activity series"
    );
}

// -- state precedence --------------------------------------------------------------------------

#[test]
fn session_state_wins_over_run_status_when_both_exist() {
    let mut b = board();
    let mut alice = agent("alice", "a", AgentRole::Worker, None);
    alice.current_session_id = Some(SessionId::try_from("sess-1").unwrap());
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![alice],
        Vec::new(),
        vec![run("alice", "a", RunStatus::Failed, 0)],
        vec![session("sess-1", "alice", "a", SessionState::Working)],
    );
    let agent = b.agents.get(&AgentId::try_from("alice").unwrap()).unwrap();
    let rated_state = b.agent_state(agent);
    assert_eq!(
        rated_state.value,
        AgentState::Working,
        "session says working, not failed"
    );
    assert!(
        !rated_state.inferred,
        "observed from a session, not inferred"
    );
}

#[test]
fn falls_back_to_run_inference_when_no_session_exists() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        vec![run("alice", "a", RunStatus::Failed, 0)],
        Vec::new(),
    );
    let agent = b.agents.get(&AgentId::try_from("alice").unwrap()).unwrap();
    let rated = b.agent_state(agent);
    assert_eq!(rated.value, AgentState::Failed);
    assert!(
        rated.inferred,
        "no session backing this, should be marked inferred"
    );
}

#[test]
fn stale_session_state_wins_even_over_a_healthier_looking_run() {
    // A session that ended in Failed should NOT be masked by an old successful run.
    let mut b = board();
    let mut alice = agent("alice", "a", AgentRole::Worker, None);
    alice.current_session_id = Some(SessionId::try_from("sess-1").unwrap());
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![alice],
        Vec::new(),
        vec![run("alice", "a", RunStatus::Succeeded, 0)],
        vec![session("sess-1", "alice", "a", SessionState::Failed)],
    );
    let agent = b.agents.get(&AgentId::try_from("alice").unwrap()).unwrap();
    assert_eq!(b.agent_state(agent).value, AgentState::Failed);
}

#[test]
fn focus_target_follows_the_selected_agent() {
    let mut b = board();
    let mut alice = agent("alice", "a", AgentRole::Worker, None);
    alice.current_session_id = Some(SessionId::try_from("sess-a").unwrap());
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![alice],
        Vec::new(),
        Vec::new(),
        vec![session("sess-a", "alice", "a", SessionState::Working)],
    );
    assert_eq!(
        b.focus_target(),
        None,
        "no target until an agent is selected"
    );
    b.selected_agent = Some(AgentId::try_from("alice").unwrap());
    assert_eq!(
        b.focus_target(),
        Some(SessionId::try_from("sess-a").unwrap())
    );
}

#[test]
fn desired_sessions_attaches_only_the_open_agent() {
    let mut b = board();
    let mut alice = agent("alice", "a", AgentRole::Worker, None);
    alice.current_session_id = Some(SessionId::try_from("sess-a").unwrap());
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![alice],
        Vec::new(),
        Vec::new(),
        vec![session("sess-a", "alice", "a", SessionState::Working)],
    );
    b.view = View::Building;
    assert!(b.desired_sessions().is_empty());
    b.selected_agent = Some(AgentId::try_from("alice").unwrap());
    b.view = View::Agent;
    assert_eq!(
        b.desired_sessions(),
        vec![SessionId::try_from("sess-a").unwrap()]
    );
}

// -- agent tree / queue / capacity -----------------------------------------------------------

#[test]
fn agent_tree_orders_orchestrator_then_workers_then_subagents_with_depth() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![
            agent("orch", "a", AgentRole::Orchestrator, None),
            agent("worker", "a", AgentRole::Worker, None),
            agent("sub", "a", AgentRole::Worker, Some("worker")),
        ],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let project_id = ProjectId::try_from("a").unwrap();
    let tree = b.agent_tree(&project_id);
    let ids: Vec<&str> = tree.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, vec!["orch", "worker", "sub"]);
    assert_eq!(tree[0].1, 0);
    assert_eq!(tree[1].1, 0);
    assert_eq!(tree[2].1, 1);
}

// -- status line: bounded status message -----------------------------------------------------

#[test]
fn a_long_status_message_is_truncated_and_the_hint_stays_intact() {
    let mut b = board();
    let long_error = "x".repeat(500);

    b.note_error(long_error.clone());

    let status_text = b.status.as_ref().unwrap().text.clone();
    assert!(
        status_text.chars().count() <= STATUS_TEXT_MAX_CHARS,
        "stored status text must be bounded, got {} chars",
        status_text.chars().count()
    );
    assert_ne!(
        status_text, long_error,
        "the message must actually be cut, not just short-circuit on `<=`"
    );

    let line = b.status_line_text();
    assert!(
        line.contains(&b.help_text()),
        "the full hint must still be present alongside a long status message: {line}"
    );
    assert!(
        line.chars().count() < long_error.chars().count(),
        "the combined line must be far shorter than the original 500-char message: {line}"
    );
}

#[test]
fn normal_footer_status_omits_the_old_action_catalog() {
    let mut b = board();
    assert_eq!(b.help_text(), "BOARD");

    b.view = View::Agent;
    assert_eq!(b.help_text(), "BOARD");
    for old_action in ["j/k", "Enter", "needs-you", "type"] {
        assert!(!b.status_line_text().contains(old_action));
    }
}

#[test]
fn daemon_version_mismatch_is_actionable_and_same_version_is_quiet() {
    let mut board = board();
    board.set_daemon_version(env!("CARGO_PKG_VERSION"));
    assert_eq!(board.version_mismatch(), None);

    board.set_daemon_version("0.0.1");
    assert_eq!(
        board.version_mismatch().as_deref(),
        Some("STALE TUI v0.2.4 / active runtime v0.0.1 — detach + relaunch")
    );

    board.set_daemon_version("");
    assert_eq!(
        board.version_mismatch().as_deref(),
        Some("STALE TUI — daemon version unknown; detach + relaunch")
    );
}

#[test]
fn activity_label_names_recent_stale_and_missing_activity() {
    let mut board = Board::new(false, 120_000, crate::theme::FORTRESS);
    let mut alice = agent("alice", "a", AgentRole::Worker, None);
    alice.current_session_id = Some(SessionId::try_from("sess-1").unwrap());
    let mut recent = session("sess-1", "alice", "a", SessionState::Working);
    recent.activity = Some("tool: Read".to_owned());
    recent.last_hook_at_ms = Some(110_000);
    board.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![alice.clone()],
        Vec::new(),
        Vec::new(),
        vec![recent],
    );
    let alice_id = alice.id.clone();
    assert_eq!(
        board.activity_label(board.agents.get(&alice_id).unwrap()),
        "tool: Read 10s ago"
    );

    let mut stale = session("sess-1", "alice", "a", SessionState::Working);
    stale.activity = Some("tool: Read".to_owned());
    stale.last_hook_at_ms = Some(0);
    board.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![board.agents.get(&alice_id).unwrap().clone()],
        Vec::new(),
        Vec::new(),
        vec![stale],
    );
    assert_eq!(
        board.activity_label(board.agents.get(&alice_id).unwrap()),
        "stale activity 2m ago"
    );

    board.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![board.agents.get(&alice_id).unwrap().clone()],
        Vec::new(),
        Vec::new(),
        vec![session("sess-1", "alice", "a", SessionState::Idle)],
    );
    assert_eq!(
        board.activity_label(board.agents.get(&alice_id).unwrap()),
        "no recent activity"
    );

    let mut event_session = session("sess-2", "alice", "a", SessionState::Working);
    event_session.activity = Some("tool: Write".to_owned());
    event_session.last_hook_at_ms = Some(119_000);
    board.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 119_000,
        event: FactoryEvent::SessionChanged {
            session: event_session,
        },
    });
    assert_eq!(
        board.activity_label(board.agents.get(&alice_id).unwrap()),
        "tool: Write 1s ago",
        "live session events remain visible even before an agent snapshot links the session"
    );
}
