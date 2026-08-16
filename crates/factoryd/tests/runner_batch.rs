use factory_core::{
    AgentRole, FactoryEvent, ProjectId, Provider, RunId, RunStatus, RunnerInstanceId, TaskStatus,
    runner::{OutputStream, RUNNER_PROTOCOL_VERSION, RunnerEvent, RunnerEventEnvelope},
};
use factoryd::store::{
    IngestDisposition, MAX_RUNNER_BATCH_EVENTS, NewAgent, NewProject, NewTask, RunReservation,
    RunnerEventEffects, RunnerEventInput, Store, StoreError, TerminalOutcome,
};

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

fn event(sequence: i64, event: RunnerEvent) -> RunnerEventEnvelope {
    RunnerEventEnvelope {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        sequence,
        occurred_at_ms: 1_000 + sequence,
        event,
    }
}

fn input(event: RunnerEventEnvelope) -> RunnerEventInput {
    RunnerEventInput {
        event,
        effects: RunnerEventEffects {
            confirmed_provider_session_id: None,
            terminal_outcome: None,
        },
    }
}

fn fixture() -> (Store, RunId, RunnerInstanceId) {
    let mut store = Store::open_in_memory().unwrap();
    store
        .create_project(
            NewProject {
                id: id("project"),
                name: "Project".into(),
                root: "/work/project".into(),
            },
            1,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: id("task"),
                project_id: id("project"),
                parent_task_id: None,
                title: "Task".into(),
                body: "private task body sentinel".into(),
                priority: 0,
            },
            2,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: id("agent"),
                project_id: id("project"),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
            },
            3,
        )
        .unwrap();

    let run_id: RunId = id("run");
    let runner_instance_id: RunnerInstanceId = id("instance");
    store
        .reserve_task_run(
            RunReservation {
                project_id: id("project"),
                task_id: id("task"),
                agent_id: id("agent"),
                expected_provider: Provider::Codex,
                run_id: run_id.clone(),
                parent_run_id: None,
                worktree: "/work/project".into(),
                fresh_provider_session_id: None,
                runner_instance_id: runner_instance_id.clone(),
                runner_runtime: "/private/runners/run".into(),
            },
            1,
            4,
        )
        .unwrap();
    (store, run_id, runner_instance_id)
}

#[test]
fn contiguous_batch_commits_session_cursor_and_terminal_state_atomically() {
    let (mut store, run_id, runner_instance_id) = fixture();
    let head_before = store.latest_event_sequence().unwrap();
    let private_output = "private provider output sentinel";
    let items = vec![
        input(event(1, RunnerEvent::Started { child_pid: 41 })),
        RunnerEventInput {
            event: event(
                2,
                RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: private_output.into(),
                    lossy: false,
                },
            ),
            effects: RunnerEventEffects {
                confirmed_provider_session_id: Some("01a007ea-f53a-76c2-9a54-d44b3fce0fb7".into()),
                terminal_outcome: None,
            },
        },
        input(event(
            3,
            RunnerEvent::Output {
                stream: OutputStream::Stderr,
                text: "private diagnostic sentinel".into(),
                lossy: false,
            },
        )),
        RunnerEventInput {
            event: event(
                4,
                RunnerEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                },
            ),
            effects: RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Succeeded {
                    result: Some("Completed factory result".into()),
                }),
            },
        },
    ];

    let result = store
        .ingest_runner_events(&run_id, &runner_instance_id, items, 10)
        .unwrap();
    assert_eq!(result.disposition, IngestDisposition::Recorded);
    assert_eq!(result.events.len(), 4);
    assert!(matches!(
        result.events[0].event,
        FactoryEvent::RunChanged { ref run } if run.status == RunStatus::Running
    ));
    assert!(matches!(
        result.events.last().unwrap().event,
        FactoryEvent::RunChanged { ref run } if run.status == RunStatus::Succeeded
    ));
    assert_eq!(
        result
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        (head_before + 1..=head_before + 4).collect::<Vec<_>>()
    );

    let recovery = store.recoverable_runs().unwrap();
    assert_eq!(recovery.len(), 1);
    assert_eq!(recovery[0].run.status, RunStatus::Succeeded);
    assert_eq!(recovery[0].terminal_runner_sequence, Some(4));
    assert_eq!(recovery[0].target.last_committed_runner_sequence, 4);
    assert_eq!(
        store
            .list_tasks(&id::<ProjectId>("project"), None, 10)
            .unwrap()[0]
            .snapshot
            .status,
        TaskStatus::Succeeded
    );
    assert_eq!(
        store
            .list_tasks(&id::<ProjectId>("project"), None, 10)
            .unwrap()[0]
            .result
            .as_deref(),
        Some("Completed factory result")
    );
    let snapshot = store
        .webhook_snapshot(
            &id::<ProjectId>("project"),
            &id::<factory_core::AgentId>("agent"),
            20,
        )
        .unwrap();
    assert_eq!(
        snapshot.tasks[0].result.as_deref(),
        Some("Completed factory result")
    );

    let serialized = serde_json::to_string(&store.events_after(0, 100).unwrap()).unwrap();
    assert!(!serialized.contains(private_output));
    assert!(!serialized.contains("private diagnostic sentinel"));
    assert!(!serialized.contains("private task body sentinel"));
    assert!(!serialized.contains("01a007ea-f53a-76c2-9a54-d44b3fce0fb7"));
    assert!(!serialized.contains("instance"));
}

#[test]
fn invalid_late_event_rolls_back_the_whole_batch() {
    let (mut store, run_id, runner_instance_id) = fixture();
    let head_before = store.latest_event_sequence().unwrap();
    let items = vec![
        input(event(1, RunnerEvent::Started { child_pid: 41 })),
        RunnerEventInput {
            event: event(
                2,
                RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "session".into(),
                    lossy: false,
                },
            ),
            effects: RunnerEventEffects {
                confirmed_provider_session_id: Some("01a007ea-f53a-76c2-9a54-d44b3fce0fb7".into()),
                terminal_outcome: None,
            },
        },
        RunnerEventInput {
            event: event(
                3,
                RunnerEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                },
            ),
            effects: RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Succeeded {
                    result: Some("stable result".into()),
                }),
            },
        },
        input(event(
            4,
            RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text: "impossible post-terminal output".into(),
                lossy: false,
            },
        )),
    ];

    assert!(matches!(
        store.ingest_runner_events(&run_id, &runner_instance_id, items, 10),
        Err(StoreError::RunnerAlreadyTerminal)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head_before);
    let target = store.execution_target(&run_id).unwrap();
    assert_eq!(target.last_committed_runner_sequence, 0);
    assert_eq!(target.provider_session_id, None);
    let recovery = store.recoverable_runs().unwrap();
    assert_eq!(recovery[0].run.status, RunStatus::Starting);
    assert_eq!(recovery[0].terminal_runner_sequence, None);
}

#[test]
fn duplicate_prefix_and_new_suffix_have_one_explicit_result() {
    let (mut store, run_id, runner_instance_id) = fixture();
    let started = input(event(1, RunnerEvent::Started { child_pid: 41 }));
    store
        .ingest_runner_events(&run_id, &runner_instance_id, vec![started], 5)
        .unwrap();

    let items = vec![
        input(event(1, RunnerEvent::Started { child_pid: 41 })),
        RunnerEventInput {
            event: event(
                2,
                RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "session".into(),
                    lossy: false,
                },
            ),
            effects: RunnerEventEffects {
                confirmed_provider_session_id: Some("01a007ea-f53a-76c2-9a54-d44b3fce0fb7".into()),
                terminal_outcome: None,
            },
        },
        RunnerEventInput {
            event: event(
                3,
                RunnerEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                },
            ),
            effects: RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Succeeded {
                    result: Some("stable result".into()),
                }),
            },
        },
    ];
    let result = store
        .ingest_runner_events(&run_id, &runner_instance_id, items, 6)
        .unwrap();
    assert_eq!(result.disposition, IngestDisposition::Recorded);
    assert_eq!(
        store
            .execution_target(&run_id)
            .unwrap()
            .last_committed_runner_sequence,
        3
    );

    let all_duplicate = vec![
        input(event(1, RunnerEvent::Started { child_pid: 41 })),
        RunnerEventInput {
            event: event(
                2,
                RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "session".into(),
                    lossy: false,
                },
            ),
            effects: RunnerEventEffects {
                confirmed_provider_session_id: Some("01a007ea-f53a-76c2-9a54-d44b3fce0fb7".into()),
                terminal_outcome: None,
            },
        },
        RunnerEventInput {
            event: event(
                3,
                RunnerEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                },
            ),
            effects: RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Succeeded {
                    result: Some("stable result".into()),
                }),
            },
        },
    ];
    assert_eq!(
        store
            .ingest_runner_events(&run_id, &runner_instance_id, all_duplicate, 7)
            .unwrap()
            .disposition,
        IngestDisposition::Duplicate
    );

    let conflicting_terminal = RunnerEventInput {
        event: event(
            3,
            RunnerEvent::Exited {
                exit_code: Some(0),
                signal: None,
            },
        ),
        effects: RunnerEventEffects {
            confirmed_provider_session_id: None,
            terminal_outcome: Some(TerminalOutcome::Succeeded {
                result: Some("changed replay result".into()),
            }),
        },
    };
    assert!(matches!(
        store.ingest_runner_event(
            &run_id,
            &runner_instance_id,
            &conflicting_terminal.event,
            conflicting_terminal.effects,
            8,
        ),
        Err(StoreError::InvalidTerminalOutcome)
    ));
}

#[test]
fn empty_and_oversized_batches_are_rejected_without_a_transaction() {
    let (mut store, run_id, runner_instance_id) = fixture();
    assert!(matches!(
        store.ingest_runner_events(&run_id, &runner_instance_id, Vec::new(), 5),
        Err(StoreError::InvalidRunnerBatchSize)
    ));

    let too_many = (0..=MAX_RUNNER_BATCH_EVENTS)
        .map(|index| {
            input(event(
                i64::try_from(index + 1).unwrap(),
                RunnerEvent::OutputTruncated { limit_bytes: 1 },
            ))
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        store.ingest_runner_events(&run_id, &runner_instance_id, too_many, 5),
        Err(StoreError::InvalidRunnerBatchSize)
    ));
    assert_eq!(
        store
            .execution_target(&run_id)
            .unwrap()
            .last_committed_runner_sequence,
        0
    );
}

#[test]
fn a_duplicate_after_a_new_event_is_not_a_contiguous_replay() {
    let (mut store, run_id, runner_instance_id) = fixture();
    let head_before = store.latest_event_sequence().unwrap();
    let items = vec![
        input(event(1, RunnerEvent::Started { child_pid: 41 })),
        input(event(1, RunnerEvent::Started { child_pid: 41 })),
    ];

    assert!(matches!(
        store.ingest_runner_events(&run_id, &runner_instance_id, items, 5),
        Err(StoreError::RunnerSequenceGap {
            expected: 2,
            found: 1
        })
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head_before);
    assert_eq!(
        store
            .execution_target(&run_id)
            .unwrap()
            .last_committed_runner_sequence,
        0
    );
}
