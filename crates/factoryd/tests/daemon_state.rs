use factory_core::ProjectId;
use factoryd::{
    daemon_state::DaemonState,
    store::{NewProject, Store},
};

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_state_is_visible_before_events_are_published_in_order() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let database = directory.path().join("factory.db");
    let state = DaemonState::new(Store::open(&database).unwrap());
    let mut observer = state.subscribe();
    let observed_database = database.clone();

    let observation = tokio::spawn(async move {
        let first = observer.recv().await.unwrap();
        let independent = Store::open(observed_database).unwrap();
        let durable_head = independent.latest_event_sequence().unwrap();
        let projects = independent.list_projects(None, 10).unwrap();
        let second = observer.recv().await.unwrap();
        (
            first.sequence,
            second.sequence,
            durable_head,
            projects.len(),
        )
    });

    let returned = state
        .commit_and_publish(|store| {
            let (_, first) = store.create_project(
                NewProject {
                    id: project_id("project-one"),
                    name: "Project one".into(),
                    root: "/one".into(),
                },
                1,
            )?;
            let (_, second) = store.create_project(
                NewProject {
                    id: project_id("project-two"),
                    name: "Project two".into(),
                    root: "/two".into(),
                },
                2,
            )?;
            Ok(("committed", vec![first, second]))
        })
        .await
        .unwrap();

    assert_eq!(returned, "committed");
    assert_eq!(observation.await.unwrap(), (1, 2, 2, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_commits_publish_in_durable_sequence() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let database = directory.path().join("factory.db");
    let state = DaemonState::new(Store::open(database).unwrap());
    let mut observer = state.subscribe();
    let mut commits = tokio::task::JoinSet::new();

    for index in 0..64 {
        let state = state.clone();
        commits.spawn(async move {
            state
                .commit_and_publish(move |store| {
                    let (_, event) = store.create_project(
                        NewProject {
                            id: project_id(&format!("project-{index}")),
                            name: format!("Project {index}"),
                            root: format!("/{index}"),
                        },
                        index,
                    )?;
                    Ok(((), vec![event]))
                })
                .await
        });
    }

    while let Some(commit) = commits.join_next().await {
        commit.unwrap().unwrap();
    }
    for expected in 1..=64 {
        assert_eq!(observer.recv().await.unwrap().sequence, expected);
    }
}
