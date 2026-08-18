//! Tests for `Board`'s data/derived-view methods (fleet snapshot application, session-vs-run
//! precedence, terminal-attach targets, route/queue/capacity derivation). Key-handling tests live
//! next to `keymap()` in `keymap.rs`.

use super::*;
use crate::test_fixtures::{agent, project, run, session, task};
use factory_core::attention::Attention;
use factory_core::status::{AgentStatus, FleetStatus, ProjectStatus, WorktreeStatus};
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
fn later_fleet_status_replaces_clean_worktree_state_without_a_reconnect() {
    let snapshot = agent("alice", "a", AgentRole::Worker, None);
    let mut b = board();
    let mut status = FleetStatus {
        generated_at_ms: 0,
        auto_mode: false,
        live_session_cap: 4,
        live_sessions: 0,
        projects: vec![ProjectStatus {
            project: project("a", 0),
            agents: vec![AgentStatus {
                agent: snapshot.clone(),
                worktree: Some(WorktreeStatus {
                    path: "/work/alice".into(),
                    branch: Some("agent/alice".into()),
                    changed_files: 0,
                    dirty: false,
                    error: None,
                }),
                session: None,
                current_run: None,
                queue_depth: 0,
                queue: Vec::new(),
                inbox_pending: 0,
                attention: Attention::Routine,
                attention_inferred: true,
            }],
            unassigned_queue_depth: 0,
            unassigned_queue: Vec::new(),
        }],
        attention: Vec::new(),
    };

    b.apply_fleet_status(status.clone());
    assert!(!b.worktree_for(&snapshot.id).unwrap().dirty);

    let refreshed = status.projects[0].agents[0].worktree.as_mut().unwrap();
    refreshed.changed_files = 2;
    refreshed.dirty = true;
    b.apply_fleet_status(status);

    assert_eq!(b.live_session_cap, Some(4));
    let worktree = b.worktree_for(&snapshot.id).unwrap();
    assert!(worktree.dirty);
    assert_eq!(worktree.changed_files, 2);
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
        sequence: 0,
        occurred_at_ms: 0,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    });
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 1,
        event: FactoryEvent::SessionChanged {
            session: session("sess-1", "alice", "a", SessionState::Working),
        },
    });
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 2,
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
        sequence: 0,
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

// -- state/attention precedence ----------------------------------------------------------------

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

    let rated_attention = b.agent_attention(agent);
    assert_eq!(rated_attention.value, Attention::Routine);
    assert!(!rated_attention.inferred);
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

// -- route kind ---------------------------------------------------------------------------------

#[test]
fn route_kind_is_transient_while_working() {
    let mut b = board();
    let mut alice = agent("alice", "a", AgentRole::Worker, None);
    alice.current_session_id = Some(SessionId::try_from("sess-1").unwrap());
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![alice],
        Vec::new(),
        Vec::new(),
        vec![session("sess-1", "alice", "a", SessionState::Working)],
    );
    let agent = b.agents.get(&AgentId::try_from("alice").unwrap()).unwrap();
    assert_eq!(b.route_kind_for(agent), RouteKind::Transient);
}

#[test]
fn route_kind_is_durable_when_idle_but_ever_assigned() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        vec![task("t1", "a", TaskStatus::Succeeded, Some("alice"), 0)],
        Vec::new(),
        Vec::new(),
    );
    let agent = b.agents.get(&AgentId::try_from("alice").unwrap()).unwrap();
    assert_eq!(b.route_kind_for(agent), RouteKind::Durable);
}

#[test]
fn route_kind_is_none_when_never_assigned_and_idle() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![agent("alice", "a", AgentRole::Worker, None)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let agent = b.agents.get(&AgentId::try_from("alice").unwrap()).unwrap();
    assert_eq!(b.route_kind_for(agent), RouteKind::None);
}

// -- terminal targets -----------------------------------------------------------------------

#[test]
fn terminal_targets_are_capped_at_four_and_only_live_sessions() {
    let mut b = board();
    let mut agents = Vec::new();
    let mut sessions = Vec::new();
    for i in 0..6 {
        let id = format!("agent{i}");
        let mut a = agent(&id, "a", AgentRole::Worker, None);
        let session_id = format!("sess{i}");
        a.current_session_id = Some(SessionId::try_from(session_id.clone()).unwrap());
        agents.push(a);
        // Half the sessions are stopped (not live).
        let state = if i % 2 == 0 {
            SessionState::Working
        } else {
            SessionState::Stopped
        };
        sessions.push(session(&session_id, &id, "a", state));
    }
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        agents,
        Vec::new(),
        Vec::new(),
        sessions,
    );
    let targets = b.terminal_targets();
    assert!(
        targets.len() <= 4,
        "expected at most 4 panes, got {}",
        targets.len()
    );
    for session_id in &targets {
        assert!(
            b.sessions.get(session_id).unwrap().state.is_live(),
            "only live sessions should be tiled"
        );
    }
}

#[test]
fn terminal_targets_are_scoped_to_the_focused_project() {
    let mut b = board();
    let mut alice = agent("alice", "a", AgentRole::Worker, None);
    alice.current_session_id = Some(SessionId::try_from("sess-a").unwrap());
    let mut bob = agent("bob", "b", AgentRole::Worker, None);
    bob.current_session_id = Some(SessionId::try_from("sess-b").unwrap());
    b.apply_fleet_snapshot(
        vec![project("a", 0), project("b", 1)],
        vec![alice, bob],
        Vec::new(),
        Vec::new(),
        vec![
            session("sess-a", "alice", "a", SessionState::Working),
            session("sess-b", "bob", "b", SessionState::Working),
        ],
    );
    b.focus_project(ProjectId::try_from("a").unwrap());
    let targets = b.terminal_targets();
    assert_eq!(targets, vec![SessionId::try_from("sess-a").unwrap()]);
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
fn desired_sessions_is_empty_outside_terminals_and_focus() {
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
    b.view = View::Fortress;
    assert!(b.desired_sessions().is_empty());
    b.view = View::Workshop;
    assert!(b.desired_sessions().is_empty());
    b.view = View::Terminals;
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

#[test]
fn visible_agent_tree_filters_to_needs_operator_when_attention_filter_is_on() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![
            agent("orch", "a", AgentRole::Orchestrator, None),
            agent("alice", "a", AgentRole::Worker, None),
        ],
        Vec::new(),
        vec![run("alice", "a", RunStatus::Failed, 0)],
        Vec::new(),
    );
    let project_id = ProjectId::try_from("a").unwrap();
    b.attention_filter = true;
    let tree = b.visible_agent_tree(&project_id);
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].0.as_str(), "alice");
}

#[test]
fn queued_and_idle_capacity_counts() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        vec![
            agent("orch", "a", AgentRole::Orchestrator, None),
            agent("alice", "a", AgentRole::Worker, None),
            agent("bob", "a", AgentRole::Worker, None),
        ],
        vec![
            task("t1", "a", TaskStatus::Queued, None, 0),
            task("t2", "a", TaskStatus::Queued, None, 1),
            task("t3", "a", TaskStatus::Succeeded, None, 2),
        ],
        Vec::new(),
        Vec::new(),
    );
    let project_id = ProjectId::try_from("a").unwrap();
    assert_eq!(b.queued_task_count(&project_id), 2);
    // Both alice and bob are idle (no run/session); orchestrator is excluded from capacity.
    assert_eq!(b.idle_capacity_count(&project_id), 2);
}

// -- task detail (lazy GetTask fetch) ------------------------------------------------------

#[test]
fn task_detail_needs_fetch_is_true_for_a_never_fetched_task() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        Vec::new(),
        vec![task("t1", "a", TaskStatus::Queued, None, 0)],
        Vec::new(),
        Vec::new(),
    );
    assert!(b.task_detail_needs_fetch(&TaskId::try_from("t1").unwrap()));
}

#[test]
fn task_detail_needs_fetch_is_false_for_an_unknown_task() {
    let b = board();
    assert!(!b.task_detail_needs_fetch(&TaskId::try_from("nonexistent").unwrap()));
}

#[test]
fn begin_task_detail_fetch_marks_pending_and_dedupes_in_flight_requests() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        Vec::new(),
        vec![task("t1", "a", TaskStatus::Queued, None, 0)],
        Vec::new(),
        Vec::new(),
    );
    let id = TaskId::try_from("t1").unwrap();

    let project_id = b.begin_task_detail_fetch(&id);
    assert_eq!(project_id.map(|p| p.to_string()), Some("a".to_owned()));
    assert!(b.is_task_detail_pending(&id));

    // A second call before the response lands is a no-op - one already in flight.
    assert_eq!(b.begin_task_detail_fetch(&id), None);
}

#[test]
fn apply_task_detail_result_merges_detail_and_marks_it_synced() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        Vec::new(),
        vec![task("t1", "a", TaskStatus::Queued, None, 0)],
        Vec::new(),
        Vec::new(),
    );
    let id = TaskId::try_from("t1").unwrap();
    assert!(b.begin_task_detail_fetch(&id).is_some());

    let mut fetched = task("t1", "a", TaskStatus::Queued, None, 0);
    fetched.body = "do the thing".to_owned();
    fetched.result = Some("done".to_owned());
    b.apply_task_detail_result(id.clone(), Ok(LocalResponse::Task { task: fetched }));

    assert!(!b.is_task_detail_pending(&id));
    assert!(!b.task_detail_needs_fetch(&id));
    let cached = b.tasks.get(&id).unwrap();
    assert_eq!(cached.body, "do the thing");
    assert_eq!(cached.result.as_deref(), Some("done"));
}

/// The bug this track fixes: `TaskChanged` only ever carries the durable snapshot (see
/// `apply_event`'s doc comment), so a task that completes after its detail was already fetched
/// once needs a *fresh* fetch to pick up its `result` - `updated_at_ms` moving past what was
/// last synced is exactly the signal `task_detail_needs_fetch` watches for.
#[test]
fn a_task_changed_event_past_the_synced_version_requires_a_fresh_fetch() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        Vec::new(),
        vec![task("t1", "a", TaskStatus::Queued, None, 0)],
        Vec::new(),
        Vec::new(),
    );
    let id = TaskId::try_from("t1").unwrap();
    assert!(b.begin_task_detail_fetch(&id).is_some());
    let mut fetched = task("t1", "a", TaskStatus::Queued, None, 0);
    fetched.body = "hello".to_owned();
    b.apply_task_detail_result(id.clone(), Ok(LocalResponse::Task { task: fetched }));
    assert!(!b.task_detail_needs_fetch(&id));

    let mut newer = task("t1", "a", TaskStatus::Succeeded, None, 0).snapshot;
    newer.updated_at_ms = 1;
    b.apply_event(EventEnvelope {
        protocol_version: 1,
        sequence: 1,
        occurred_at_ms: 1,
        event: FactoryEvent::TaskChanged { task: newer },
    });

    assert_eq!(
        b.tasks.get(&id).unwrap().body,
        "hello",
        "old detail preserved meanwhile - see apply_event's TaskChanged arm"
    );
    assert!(
        b.task_detail_needs_fetch(&id),
        "should read as stale once the snapshot moved past the last synced version"
    );
}

#[test]
fn apply_task_detail_result_clears_pending_on_failure_so_it_can_retry() {
    let mut b = board();
    b.apply_fleet_snapshot(
        vec![project("a", 0)],
        Vec::new(),
        vec![task("t1", "a", TaskStatus::Queued, None, 0)],
        Vec::new(),
        Vec::new(),
    );
    let id = TaskId::try_from("t1").unwrap();
    assert!(b.begin_task_detail_fetch(&id).is_some());
    assert!(b.is_task_detail_pending(&id));

    b.apply_task_detail_result(id.clone(), Err("timed out".to_owned()));

    assert!(!b.is_task_detail_pending(&id));
    assert!(
        b.task_detail_needs_fetch(&id),
        "still needs a fetch - the first one failed"
    );
    assert!(
        b.begin_task_detail_fetch(&id).is_some(),
        "a failed fetch must be retryable, not stuck pending forever"
    );
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
