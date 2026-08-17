//! Tests for `Board`'s data/derived-view methods (fleet snapshot application, session-vs-run
//! precedence, terminal-attach targets, route/queue/capacity derivation). Key-handling tests live
//! next to `keymap()` in `keymap.rs`.

use super::*;
use crate::test_fixtures::{agent, project, run, task};
use factory_core::{ObserverHealth, Provider, RunStatus, SessionSnapshot, SessionState};

fn session(id: &str, agent_id: &str, project: &str, state: SessionState) -> SessionSnapshot {
    SessionSnapshot {
        id: SessionId::try_from(id).unwrap(),
        project_id: ProjectId::try_from(project).unwrap(),
        agent_id: AgentId::try_from(agent_id).unwrap(),
        provider: Provider::ClaudeCode,
        state,
        state_since_ms: 0,
        worktree: "/work".into(),
        provider_session_id: None,
        current_run_id: None,
        activity: None,
        activity_inferred: false,
        last_hook_event: None,
        last_hook_at_ms: None,
        wait_reason: None,
        observer_health: ObserverHealth::Unknown,
        observer_health_since_ms: 0,
        started_at_ms: 0,
        updated_at_ms: 0,
        ended_at_ms: None,
        exit_code: None,
        exit_signal: None,
    }
}

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
