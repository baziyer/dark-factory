use std::{
    cmp::Ordering,
    collections::BTreeMap,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use eframe::egui::{self, Color32, RichText};
use factory_core::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, ProjectId, ProjectSnapshot,
    Provider, RunId, RunSnapshot, TaskDetail, TaskId, TaskStatus,
    local::{
        AgentDetail, AgentMessage, LocalRequest, LocalResponse, MAX_AGENT_PAGE_ITEMS,
        MAX_PROJECT_PAGE_ITEMS, MAX_RUN_PAGE_ITEMS, MAX_TASK_PAGE_ITEMS, RunTerminal, ServerFrame,
    },
};
use uuid::Uuid;

use factoryctl::Client;

const RECENT_EVENT_LIMIT: usize = 100;

pub fn run(client: Client) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Dark Factory")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([920.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Dark Factory",
        options,
        Box::new(move |context| Ok(Box::new(FactoryApp::new(context, client)))),
    )
    .map_err(|error| format!("native UI failed: {error}"))
}

enum UiMessage {
    Snapshot {
        snapshot: Snapshot,
        event_sequence: Option<i64>,
        refresh_id: u64,
    },
    Event(EventEnvelope),
    TaskDetail(Result<TaskDetail, String>),
    AgentDetail {
        agent_id: AgentId,
        result: Result<AgentDetail, String>,
    },
    AgentProfile {
        agent_id: AgentId,
        result: Result<AgentDetail, String>,
    },
    AgentMessages {
        agent_id: AgentId,
        result: Result<Vec<AgentMessage>, String>,
    },
    AgentMessageSent {
        agent_id: AgentId,
        result: Result<AgentMessage, String>,
    },
    RunTerminal {
        request_id: u64,
        result: Result<RunTerminal, String>,
    },
    SubscriptionCaughtUp,
    Operation {
        result: Result<String, String>,
        assignment_task_id: Option<TaskId>,
    },
    StreamFailed(String),
}

#[derive(Default)]
struct Snapshot {
    projects: Vec<ProjectSnapshot>,
    tasks: Vec<TaskDetail>,
    agents: Vec<AgentSnapshot>,
    runs: Vec<RunSnapshot>,
}

struct FactoryApp {
    client: Client,
    sender: Sender<UiMessage>,
    receiver: Receiver<UiMessage>,
    projects: BTreeMap<ProjectId, ProjectSnapshot>,
    tasks: BTreeMap<TaskId, TaskDetail>,
    agents: BTreeMap<AgentId, AgentSnapshot>,
    agent_details: BTreeMap<AgentId, AgentDetail>,
    agent_messages: BTreeMap<AgentId, Vec<AgentMessage>>,
    runs: BTreeMap<RunId, RunSnapshot>,
    recent: Vec<EventEnvelope>,
    selected_project: Option<ProjectId>,
    show_all_queue: bool,
    show_history: bool,
    selected_task: Option<TaskId>,
    selected_agent: Option<AgentId>,
    terminal_run_id: Option<RunId>,
    terminal: Option<RunTerminal>,
    terminal_request_id: u64,
    connection: ConnectionState,
    notice: Option<String>,
    last_event_sequence: Option<i64>,
    last_snapshot_refresh_id: u64,
    next_refresh_id: u64,
    create_project: Option<ProjectForm>,
    create_agent: Option<AgentForm>,
    create_task: Option<TaskForm>,
    start: StartForm,
    queue_owner_editor: Option<QueueOwnerEditor>,
    agent_profile_draft: Option<AgentProfileDraft>,
    agent_profile_pending: Option<AgentId>,
    agent_profile_error: Option<String>,
    agent_message_draft: String,
    agent_message_pending: bool,
}

struct AgentProfileDraft {
    model: String,
    instructions: String,
    memory: String,
}

#[derive(Clone, Copy)]
enum ConnectionState {
    Loading,
    Live,
    Degraded,
}

struct ProjectForm {
    id: String,
    name: String,
    root: String,
}

struct AgentForm {
    parent: Option<AgentId>,
    role: AgentRole,
    provider: Provider,
    model: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModelOption {
    label: &'static str,
    value: Option<&'static str>,
}

struct TaskForm {
    id: String,
    title: String,
    body: String,
    priority: i32,
}

#[derive(Default)]
struct StartForm {
    agent_id: String,
    worktree: String,
}

struct QueueOwnerEditor {
    task_id: TaskId,
    value: String,
    base_value: String,
    pending_value: Option<String>,
    conflict: bool,
}

impl QueueOwnerEditor {
    fn new(task_id: TaskId, value: String) -> Self {
        Self {
            task_id,
            base_value: value.clone(),
            value,
            pending_value: None,
            conflict: false,
        }
    }

    fn mark_pending(&mut self) {
        self.pending_value = Some(self.value.clone());
    }

    fn clear_pending(&mut self) {
        self.pending_value = None;
    }

    fn sync_from_server(&mut self, value: &str) -> bool {
        if let Some(pending) = self.pending_value.as_deref() {
            if pending == value {
                self.value = value.to_owned();
                self.base_value = value.to_owned();
                self.pending_value = None;
                self.conflict = false;
                return false;
            }
            if self.base_value == value {
                return false;
            }
            self.pending_value = None;
        }
        if self.value == value {
            self.base_value = value.to_owned();
            self.conflict = false;
            return false;
        }
        if self.value == self.base_value {
            self.value = value.to_owned();
            self.base_value = value.to_owned();
            return false;
        }
        if self.base_value != value {
            self.value = value.to_owned();
            self.base_value = value.to_owned();
            self.conflict = true;
            return true;
        }
        false
    }
}

impl FactoryApp {
    fn new(context: &eframe::CreationContext<'_>, client: Client) -> Self {
        context.egui_ctx.set_visuals(egui::Visuals::dark());
        let (sender, receiver) = mpsc::channel();
        spawn_refresh(client.clone(), sender.clone(), context.egui_ctx.clone(), 1);
        spawn_subscription(client.clone(), sender.clone(), context.egui_ctx.clone());
        Self {
            client,
            sender,
            receiver,
            projects: BTreeMap::new(),
            tasks: BTreeMap::new(),
            agents: BTreeMap::new(),
            agent_details: BTreeMap::new(),
            agent_messages: BTreeMap::new(),
            runs: BTreeMap::new(),
            recent: Vec::new(),
            selected_project: None,
            show_all_queue: false,
            show_history: false,
            selected_task: None,
            selected_agent: None,
            terminal_run_id: None,
            terminal: None,
            terminal_request_id: 0,
            connection: ConnectionState::Loading,
            notice: None,
            last_event_sequence: None,
            last_snapshot_refresh_id: 0,
            next_refresh_id: 1,
            create_project: None,
            create_agent: None,
            create_task: None,
            start: StartForm::default(),
            queue_owner_editor: None,
            agent_profile_draft: None,
            agent_profile_pending: None,
            agent_profile_error: None,
            agent_message_draft: String::new(),
            agent_message_pending: false,
        }
    }

    fn receive(&mut self, context: &egui::Context) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                UiMessage::Snapshot {
                    snapshot,
                    event_sequence,
                    refresh_id,
                } => {
                    if !should_apply_snapshot(
                        self.last_snapshot_refresh_id,
                        self.last_event_sequence,
                        refresh_id,
                        event_sequence,
                    ) {
                        continue;
                    }
                    self.merge_snapshot(snapshot);
                    self.last_event_sequence = match (self.last_event_sequence, event_sequence) {
                        (Some(current), Some(incoming)) => Some(current.max(incoming)),
                        (None, incoming) | (incoming, None) => incoming,
                    };
                    self.last_snapshot_refresh_id = refresh_id;
                    if self.selected_project.is_none() {
                        self.selected_project = self.projects.keys().next().cloned();
                    }
                    self.connection = ConnectionState::Live;
                }
                UiMessage::Event(event) => {
                    if self
                        .last_event_sequence
                        .is_some_and(|sequence| event.sequence <= sequence)
                    {
                        continue;
                    }
                    let task_was_known = task_id_from_event(&event)
                        .is_some_and(|task_id| self.tasks.contains_key(task_id));
                    let needs_task_detail = task_event_needs_detail(task_was_known, &event);
                    self.last_event_sequence = Some(event.sequence);
                    let refresh_details = event_requires_detail_refresh(&event);
                    self.apply_event(event);
                    if needs_task_detail {
                        if let Some((project_id, task_id)) = task_event_ids(self.recent.last()) {
                            spawn_task_detail(
                                self.client.clone(),
                                self.sender.clone(),
                                context.clone(),
                                project_id,
                                task_id,
                            );
                        }
                    }
                    if refresh_details {
                        self.refresh(context);
                    }
                }
                UiMessage::TaskDetail(result) => match result {
                    Ok(task) => {
                        let task_id = task.snapshot.id.clone();
                        if let Some(current) = self.tasks.get_mut(&task_id) {
                            merge_task_detail(current, task);
                        } else {
                            self.tasks.insert(task_id, task);
                        }
                    }
                    Err(message) => self.notice = Some(message),
                },
                UiMessage::AgentDetail { agent_id, result } => match result {
                    Ok(detail) => {
                        self.agent_details.insert(agent_id.clone(), detail.clone());
                        if self.selected_agent.as_ref() == Some(&agent_id) {
                            self.agent_profile_draft = Some(agent_profile_draft(&detail));
                            self.agent_profile_error = None;
                        }
                    }
                    Err(message) => {
                        if self.selected_agent.as_ref() == Some(&agent_id) {
                            self.agent_profile_draft = None;
                            self.agent_profile_error = Some(message.clone());
                            self.notice = Some(message);
                        }
                    }
                },
                UiMessage::AgentProfile { agent_id, result } => match result {
                    Ok(detail) => {
                        self.agent_details.insert(agent_id.clone(), detail.clone());
                        if self.selected_agent.as_ref() == Some(&agent_id)
                            && self.agent_profile_pending.as_ref() == Some(&agent_id)
                        {
                            self.agent_profile_draft = Some(agent_profile_draft(&detail));
                            self.agent_profile_pending = None;
                            self.notice = Some(format!("Updated profile for agent {agent_id}"));
                        }
                    }
                    Err(message) => {
                        if self.agent_profile_pending.as_ref() == Some(&agent_id) {
                            self.agent_profile_pending = None;
                            self.notice = Some(message);
                        }
                    }
                },
                UiMessage::AgentMessages { agent_id, result } => match result {
                    Ok(messages) => {
                        self.agent_messages.insert(agent_id, messages);
                    }
                    Err(message) => self.notice = Some(message),
                },
                UiMessage::AgentMessageSent { agent_id, result } => match result {
                    Ok(_) => {
                        self.agent_message_pending = false;
                        self.agent_message_draft.clear();
                        self.load_agent_messages(context, agent_id);
                        self.notice = Some("Message queued for the agent's next task".into());
                    }
                    Err(message) => {
                        self.agent_message_pending = false;
                        self.notice = Some(message);
                    }
                },
                UiMessage::RunTerminal { request_id, result } => match result {
                    Ok(terminal)
                        if should_apply_terminal(
                            request_id,
                            self.terminal_request_id,
                            self.terminal_run_id.as_ref(),
                            &terminal,
                        ) =>
                    {
                        self.terminal = Some(terminal);
                    }
                    Ok(_) => {}
                    Err(message) => self.notice = Some(message),
                },
                UiMessage::SubscriptionCaughtUp => self.refresh(context),
                UiMessage::Operation {
                    result,
                    assignment_task_id,
                } => match result {
                    Ok(message) => {
                        self.notice = Some(message);
                        self.refresh(context);
                    }
                    Err(message) => {
                        let editor_task_id = self
                            .queue_owner_editor
                            .as_ref()
                            .map(|editor| &editor.task_id);
                        if should_clear_queue_owner_pending(
                            editor_task_id,
                            assignment_task_id.as_ref(),
                        ) {
                            if let Some(editor) = self.queue_owner_editor.as_mut() {
                                editor.clear_pending();
                            }
                        }
                        self.notice = Some(message);
                    }
                },
                UiMessage::StreamFailed(message) => {
                    self.connection = ConnectionState::Degraded;
                    self.notice = Some(message);
                }
            }
        }
    }

    fn apply_event(&mut self, envelope: EventEnvelope) {
        match &envelope.event {
            FactoryEvent::ProjectChanged { project } => {
                self.projects.insert(project.id.clone(), project.clone());
            }
            FactoryEvent::TaskChanged { task } => {
                let (body, result) = self.tasks.get(&task.id).map_or_else(
                    || (String::new(), None),
                    |detail| (detail.body.clone(), detail.result.clone()),
                );
                self.tasks.insert(
                    task.id.clone(),
                    TaskDetail {
                        snapshot: task.clone(),
                        body,
                        result,
                    },
                );
            }
            FactoryEvent::AgentChanged { agent } => {
                self.agents.insert(agent.id.clone(), agent.clone());
            }
            FactoryEvent::RunChanged { run } => {
                self.runs.insert(run.id.clone(), run.clone());
            }
            FactoryEvent::TaskDeleted { task_id, .. } => {
                self.tasks.remove(task_id);
            }
            FactoryEvent::AgentDeleted { agent_id, .. } => {
                self.agents.remove(agent_id);
            }
            FactoryEvent::ProjectDeleted { project_id } => {
                self.projects.remove(project_id);
            }
        }
        self.recent.push(envelope);
        if self.recent.len() > RECENT_EVENT_LIMIT {
            self.recent.remove(0);
        }
        self.connection = ConnectionState::Live;
    }

    fn merge_snapshot(&mut self, snapshot: Snapshot) {
        for project in snapshot.projects {
            let replace = self
                .projects
                .get(&project.id)
                .is_none_or(|current| project.updated_at_ms > current.updated_at_ms);
            if replace {
                self.projects.insert(project.id.clone(), project);
            }
        }
        for task in snapshot.tasks {
            if let Some(current) = self.tasks.get_mut(&task.snapshot.id) {
                merge_task_detail(current, task);
            } else {
                self.tasks.insert(task.snapshot.id.clone(), task);
            }
        }
        for agent in snapshot.agents {
            let replace = self
                .agents
                .get(&agent.id)
                .is_none_or(|current| agent.updated_at_ms > current.updated_at_ms);
            if replace {
                self.agents.insert(agent.id.clone(), agent);
            }
        }
        for run in snapshot.runs {
            let replace = self
                .runs
                .get(&run.id)
                .is_none_or(|current| run.updated_at_ms > current.updated_at_ms);
            if replace {
                self.runs.insert(run.id.clone(), run);
            }
        }
    }

    fn refresh(&mut self, context: &egui::Context) {
        self.next_refresh_id = self.next_refresh_id.saturating_add(1);
        spawn_refresh(
            self.client.clone(),
            self.sender.clone(),
            context.clone(),
            self.next_refresh_id,
        );
    }

    fn submit(&self, request: LocalRequest, context: &egui::Context) {
        self.submit_operation(request, context, None);
    }

    fn submit_assignment(&self, request: LocalRequest, task_id: TaskId, context: &egui::Context) {
        self.submit_operation(request, context, Some(task_id));
    }

    fn submit_operation(
        &self,
        request: LocalRequest,
        context: &egui::Context,
        assignment_task_id: Option<TaskId>,
    ) {
        let client = self.client.clone();
        let sender = self.sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result = request_response(&client, request).and_then(operation_message);
            let _ = sender.send(UiMessage::Operation {
                result,
                assignment_task_id,
            });
            context.request_repaint();
        });
    }

    fn project(&self) -> Option<&ProjectSnapshot> {
        self.selected_project
            .as_ref()
            .and_then(|id| self.projects.get(id))
    }

    fn project_agents(&self) -> Vec<&AgentSnapshot> {
        let Some(project) = self.selected_project.as_ref() else {
            return Vec::new();
        };
        self.agents
            .values()
            .filter(|agent| &agent.project_id == project)
            .collect()
    }

    fn project_tasks(&self) -> Vec<&TaskDetail> {
        let Some(project) = self.selected_project.as_ref() else {
            return Vec::new();
        };
        self.tasks
            .values()
            .filter(|task| &task.snapshot.project_id == project)
            .collect()
    }

    fn select_agent(&mut self, context: &egui::Context, agent_id: AgentId) {
        self.selected_agent = Some(agent_id.clone());
        self.selected_task = None;
        self.agent_profile_draft = None;
        self.agent_profile_pending = None;
        self.agent_profile_error = None;
        self.agent_message_draft.clear();
        self.agent_message_pending = false;
        if let Some(detail) = self.agent_details.get(&agent_id) {
            self.agent_profile_draft = Some(agent_profile_draft(detail));
        } else {
            self.load_agent_detail(context, agent_id.clone());
        }
        self.load_agent_messages(context, agent_id.clone());
        if let Some(run_id) = self
            .agents
            .get(&agent_id)
            .and_then(|agent| self.run_for_agent(agent))
            .map(|run| run.id.clone())
        {
            self.load_terminal(context, run_id);
        } else {
            self.terminal_run_id = None;
            self.terminal = None;
        }
    }

    fn load_agent_detail(&mut self, context: &egui::Context, agent_id: AgentId) {
        let Some(project_id) = self
            .agents
            .get(&agent_id)
            .map(|agent| agent.project_id.clone())
        else {
            return;
        };
        spawn_agent_detail(
            self.client.clone(),
            self.sender.clone(),
            context.clone(),
            project_id,
            agent_id,
        );
    }

    fn load_agent_messages(&self, context: &egui::Context, agent_id: AgentId) {
        let Some(project_id) = self
            .agents
            .get(&agent_id)
            .map(|agent| agent.project_id.clone())
        else {
            return;
        };
        spawn_agent_messages(
            self.client.clone(),
            self.sender.clone(),
            context.clone(),
            project_id,
            agent_id,
        );
    }

    fn sync_queue_owner_editor(&mut self, task: &TaskDetail) {
        let value = task
            .snapshot
            .assigned_agent_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let conflict = match self.queue_owner_editor.as_mut() {
            Some(editor) if editor.task_id == task.snapshot.id => editor.sync_from_server(&value),
            _ => {
                self.queue_owner_editor =
                    Some(QueueOwnerEditor::new(task.snapshot.id.clone(), value));
                false
            }
        };
        if conflict {
            self.notice =
                Some("Queue owner changed while you were editing; review and save again.".into());
        }
    }

    fn run_for_agent(&self, agent: &AgentSnapshot) -> Option<&RunSnapshot> {
        agent
            .current_run_id
            .as_ref()
            .and_then(|run| self.runs.get(run))
    }

    fn run_for_task(&self, task: &TaskDetail) -> Option<&RunSnapshot> {
        latest_run_for_task(
            self.runs.values(),
            &task.snapshot.project_id,
            &task.snapshot.id,
        )
    }

    fn top_bar(&mut self, context: &egui::Context) {
        egui::TopBottomPanel::top("top_bar").show(context, |ui| {
            ui.horizontal(|ui| {
                ui.heading("DARK FACTORY");
                ui.separator();
                let (dot, label, color) = match self.connection {
                    ConnectionState::Loading => ("◐", "loading", Color32::YELLOW),
                    ConnectionState::Live => ("●", "live", Color32::LIGHT_GREEN),
                    ConnectionState::Degraded => ("!", "disconnected", Color32::LIGHT_RED),
                };
                ui.label(RichText::new(format!("{dot} {label}")).color(color));
                ui.separator();
                let running = self
                    .runs
                    .values()
                    .filter(|run| !run.status.is_terminal())
                    .count();
                let blocked = self
                    .tasks
                    .values()
                    .filter(|task| task.snapshot.status == TaskStatus::Blocked)
                    .count();
                ui.label(format!("{running} active · {blocked} blocked"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.refresh(context);
                    }
                    ui.menu_button("New", |ui| {
                        if ui
                            .add_enabled(self.selected_project.is_some(), egui::Button::new("Task"))
                            .clicked()
                        {
                            self.create_task = Some(TaskForm {
                                id: short_id("task"),
                                title: String::new(),
                                body: String::new(),
                                priority: 0,
                            });
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                self.selected_project.is_some(),
                                egui::Button::new("Agent"),
                            )
                            .clicked()
                        {
                            self.create_agent = Some(AgentForm {
                                parent: None,
                                role: AgentRole::Worker,
                                provider: Provider::Codex,
                                model: String::new(),
                            });
                            ui.close();
                        }
                        if ui.button("Project").clicked() {
                            self.create_project = Some(ProjectForm {
                                id: short_id("project"),
                                name: String::new(),
                                root: String::new(),
                            });
                            ui.close();
                        }
                    });
                });
            });
            if let Some(notice) = &self.notice {
                ui.label(RichText::new(notice).small().color(Color32::LIGHT_YELLOW));
            }
        });
    }

    fn projects_panel(&mut self, context: &egui::Context) {
        egui::SidePanel::left("projects")
            .resizable(true)
            .default_width(190.0)
            .show(context, |ui| {
                ui.heading("Projects");
                ui.separator();
                if ui
                    .selectable_label(self.show_all_queue, "All queue")
                    .on_hover_text("Scan queued work across every project")
                    .clicked()
                {
                    self.show_all_queue = true;
                    self.selected_task = None;
                    self.selected_agent = None;
                    self.terminal_run_id = None;
                    self.terminal = None;
                }
                ui.separator();
                for project in self.projects.values() {
                    let selected =
                        !self.show_all_queue && self.selected_project.as_ref() == Some(&project.id);
                    if ui
                        .selectable_label(selected, project.name.clone())
                        .on_hover_text(format!("{}\n{}", project.id, project.root))
                        .clicked()
                    {
                        self.show_all_queue = false;
                        self.selected_project = Some(project.id.clone());
                        self.selected_task = None;
                        self.selected_agent = None;
                        self.terminal_run_id = None;
                        self.terminal = None;
                        self.start = StartForm {
                            agent_id: String::new(),
                            worktree: project.root.clone(),
                        };
                    }
                }
            });
    }

    fn inspector(&mut self, context: &egui::Context) {
        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(310.0)
            .show(context, |ui| {
                ui.heading("Inspector");
                ui.separator();
                if let Some(task_id) = self.selected_task.clone() {
                    if let Some(task) = self.tasks.get(&task_id).cloned() {
                        let run = self.run_for_task(&task).cloned();
                        self.task_inspector(ui, context, &task);
                        if let Some(run) = run {
                            self.terminal_panel(ui, context, &run);
                        }
                        return;
                    }
                }
                if let Some(agent_id) = self.selected_agent.clone() {
                    if let Some(agent) = self.agents.get(&agent_id).cloned() {
                        let run = self.run_for_agent(&agent).cloned();
                        self.agent_inspector(ui, context, &agent, run.as_ref());
                        if let Some(run) = run {
                            self.terminal_panel(ui, context, &run);
                        }
                        return;
                    }
                }
                ui.label("Select a task or agent.");
            });
    }

    fn task_inspector(&mut self, ui: &mut egui::Ui, context: &egui::Context, task: &TaskDetail) {
        ui.heading(&task.snapshot.title);
        ui.label(format!("{} · {:?}", task.snapshot.id, task.snapshot.status));
        ui.label(task_assignee_text(task));
        ui.separator();
        ui.collapsing("Instructions", |ui| {
            if task.body.is_empty() {
                ui.label(RichText::new("No instructions loaded.").italics());
            } else {
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        ui.label(&task.body);
                    });
            }
        });
        if let Some(result) = task_result_text(task) {
            ui.collapsing("Result", |ui| {
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        ui.label(result);
                    });
            });
        }
        if matches!(
            task.snapshot.status,
            TaskStatus::Failed | TaskStatus::Cancelled
        ) && ui.button("Retry task").clicked()
        {
            self.submit(
                LocalRequest::RetryTask {
                    project_id: task.snapshot.project_id.clone(),
                    task_id: task.snapshot.id.clone(),
                },
                context,
            );
        }
        if can_edit_task_assignment(task.snapshot.status) {
            self.sync_queue_owner_editor(task);
            ui.separator();
            ui.label(RichText::new("Queue owner").strong());
            let agents = self
                .agents
                .values()
                .filter(|agent| agent.project_id == task.snapshot.project_id)
                .cloned()
                .collect::<Vec<_>>();
            let selected_owner = self
                .queue_owner_editor
                .as_ref()
                .map_or_else(String::new, |editor| editor.value.clone());
            let pending = self
                .queue_owner_editor
                .as_ref()
                .is_some_and(|editor| editor.pending_value.is_some());
            ui.add_enabled_ui(!pending, |ui| {
                egui::ComboBox::from_id_salt(("task-assignment", task.snapshot.id.clone()))
                    .selected_text(if selected_owner.is_empty() {
                        "Operator queue"
                    } else {
                        &selected_owner
                    })
                    .show_ui(ui, |ui| {
                        if let Some(editor) = self.queue_owner_editor.as_mut() {
                            let before = editor.value.clone();
                            ui.selectable_value(&mut editor.value, String::new(), "Operator queue");
                            for agent in &agents {
                                ui.selectable_value(
                                    &mut editor.value,
                                    agent.id.to_string(),
                                    format!("{} · {:?}", agent.id, agent.provider),
                                );
                            }
                            if editor.value != before {
                                editor.conflict = false;
                                editor.pending_value = None;
                            }
                        }
                    });
            });
            let (owner, dirty, conflict) = self.queue_owner_editor.as_ref().map_or_else(
                || (String::new(), false, false),
                |editor| {
                    (
                        editor.value.clone(),
                        editor.value != editor.base_value,
                        editor.conflict,
                    )
                },
            );
            if conflict {
                ui.label(
                    RichText::new("Changed remotely; choose the owner again before saving.")
                        .small()
                        .color(Color32::LIGHT_YELLOW),
                );
            }
            if pending {
                ui.label(RichText::new("Saving queue owner…").small().weak());
            }
            if ui
                .add_enabled(
                    dirty && !conflict && !pending,
                    egui::Button::new("Save queue owner"),
                )
                .clicked()
            {
                if let Some(editor) = self.queue_owner_editor.as_mut() {
                    editor.mark_pending();
                }
                self.submit_assignment(
                    LocalRequest::AssignTask {
                        project_id: task.snapshot.project_id.clone(),
                        task_id: task.snapshot.id.clone(),
                        agent_id: (!owner.is_empty()).then(|| {
                            AgentId::try_from(owner).expect("agent selection comes from a snapshot")
                        }),
                    },
                    task.snapshot.id.clone(),
                    context,
                );
            }
            ui.separator();
            ui.label(RichText::new("Start task").strong());
            let agents = self
                .project_agents()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            egui::ComboBox::from_id_salt("start-agent")
                .selected_text(if self.start.agent_id.is_empty() {
                    "Choose agent"
                } else {
                    &self.start.agent_id
                })
                .show_ui(ui, |ui| {
                    for agent in agents {
                        if agent.current_run_id.is_none() {
                            ui.selectable_value(
                                &mut self.start.agent_id,
                                agent.id.to_string(),
                                format!("{} · {:?}", agent.id, agent.provider),
                            );
                        }
                    }
                });
            if self.start.worktree.is_empty() {
                self.start.worktree = self.project().map_or_else(String::new, |p| p.root.clone());
            }
            ui.text_edit_singleline(&mut self.start.worktree);
            if ui
                .add_enabled(!self.start.agent_id.is_empty(), egui::Button::new("Start"))
                .clicked()
            {
                let request = LocalRequest::StartTask {
                    project_id: task.snapshot.project_id.clone(),
                    task_id: task.snapshot.id.clone(),
                    agent_id: AgentId::try_from(self.start.agent_id.clone())
                        .expect("agent selection comes from a snapshot"),
                    parent_run_id: None,
                    worktree: self.start.worktree.clone(),
                };
                self.submit(request, context);
            }
        }
    }

    fn agent_inspector(
        &mut self,
        ui: &mut egui::Ui,
        context: &egui::Context,
        agent: &AgentSnapshot,
        run: Option<&RunSnapshot>,
    ) {
        agent_inspector_summary(ui, agent, run);
        ui.separator();
        let children = self
            .agents
            .values()
            .filter(|candidate| {
                candidate.project_id == agent.project_id
                    && candidate.parent_agent_id.as_ref() == Some(&agent.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        ui.collapsing(format!("Team · {} child agents", children.len()), |ui| {
            if children.is_empty() {
                ui.label(RichText::new("No child agents configured.").italics());
            }
            for child in children {
                if ui
                    .selectable_label(false, format!("{} · {:?}", child.id, child.provider))
                    .clicked()
                {
                    self.select_agent(context, child.id);
                }
            }
        });
        ui.separator();
        let messages = self
            .agent_messages
            .get(&agent.id)
            .cloned()
            .unwrap_or_default();
        let pending_messages = messages
            .iter()
            .filter(|message| message.delivered_at_ms.is_none())
            .count();
        ui.collapsing(
            format!("Messages · {pending_messages} queued for next task"),
            |ui| {
                if messages.is_empty() {
                    ui.label(RichText::new("No messages yet.").italics());
                } else {
                    egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                        for message in &messages {
                            let status = if message.delivered_at_ms.is_some() {
                                "delivered"
                            } else {
                                "queued"
                            };
                            ui.label(format!("{status}: {}", message.body));
                        }
                    });
                }
                ui.label(
                    RichText::new(
                        "Queued messages are added to the next explicit provider launch; they do not interrupt an active run.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_enabled_ui(!self.agent_message_pending, |ui| {
                    ui.label("Message to agent");
                    ui.add(
                        egui::TextEdit::multiline(&mut self.agent_message_draft)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                });
            },
        );
        let mut send_message = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.agent_message_pending && !self.agent_message_draft.trim().is_empty(),
                    egui::Button::new("Queue message"),
                )
                .clicked()
            {
                send_message = true;
            }
        });
        if send_message {
            self.agent_message_pending = true;
            spawn_agent_message_send(
                self.client.clone(),
                self.sender.clone(),
                context.clone(),
                agent.project_id.clone(),
                agent.id.clone(),
                self.agent_message_draft.trim().to_owned(),
            );
        }
        ui.separator();
        ui.label(RichText::new("Agent profile").strong());
        let Some(draft) = self.agent_profile_draft.as_mut() else {
            ui.label(
                RichText::new(agent_profile_load_message(
                    self.agent_profile_error.as_deref(),
                ))
                .italics(),
            );
            return;
        };
        let pending = self.agent_profile_pending.is_some();
        ui.add_enabled_ui(!pending, |ui| {
            ui.horizontal(|ui| {
                ui.label("Model");
                let options = provider_model_options(agent.provider);
                let selected = options
                    .iter()
                    .find(|option| option.value == (!draft.model.is_empty()).then_some(draft.model.as_str()))
                    .map_or("Choose a model", |option| option.label);
                egui::ComboBox::from_id_salt(("agent-profile-model", agent.id.clone()))
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for option in options {
                            ui.selectable_value(
                                &mut draft.model,
                                option.value.unwrap_or_default().to_owned(),
                                option.label,
                            );
                        }
                    });
            });
            ui.label(
                RichText::new("Models are scoped to the installed provider CLI; Provider default uses its configured default.")
                    .small()
                    .weak(),
            );
            ui.label("Standing guidance");
            ui.label(RichText::new(agent_profile_guidance_help()).small().weak());
            ui.add(
                egui::TextEdit::multiline(&mut draft.instructions)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
            ui.label("Memory");
            ui.add(
                egui::TextEdit::multiline(&mut draft.memory)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
        });
        if pending {
            ui.label(RichText::new("Saving profile…").small().weak());
        }
        if ui
            .add_enabled(!pending, egui::Button::new("Save profile"))
            .clicked()
        {
            let model = (!draft.model.trim().is_empty()).then(|| draft.model.trim().to_owned());
            self.agent_profile_pending = Some(agent.id.clone());
            spawn_agent_profile_update(
                self.client.clone(),
                self.sender.clone(),
                context.clone(),
                LocalRequest::UpdateAgentProfile {
                    project_id: agent.project_id.clone(),
                    agent_id: agent.id.clone(),
                    model,
                    instructions: draft.instructions.clone(),
                    memory: draft.memory.clone(),
                },
            );
        }
    }

    fn terminal_panel(&mut self, ui: &mut egui::Ui, context: &egui::Context, run: &RunSnapshot) {
        ui.separator();
        ui.label(RichText::new("Terminal · local runner").strong());
        ui.label(
            RichText::new("Private bounded output; not part of public events.")
                .small()
                .weak(),
        );
        ui.horizontal(|ui| {
            if ui.button("Refresh terminal").clicked() {
                self.load_terminal(context, run.id.clone());
            }
            if !run.status.is_terminal()
                && ui
                    .button("Stop run")
                    .on_hover_text("Ask this exact runner to stop gracefully")
                    .clicked()
            {
                self.submit(
                    LocalRequest::StopRun {
                        project_id: run.project_id.clone(),
                        run_id: run.id.clone(),
                        grace_ms: 2_000,
                    },
                    context,
                );
            }
        });
        if self.terminal_run_id.as_ref() != Some(&run.id) {
            ui.label("Refresh to load terminal output.");
            return;
        }
        if let Some(terminal) = &self.terminal {
            ui.label(format!(
                "spool sequence {}{}",
                terminal.head_sequence,
                if terminal.truncated {
                    " · tail shown"
                } else {
                    ""
                }
            ));
            egui::ScrollArea::vertical()
                .id_salt("run-terminal")
                .max_height(300.0)
                .show(ui, |ui| {
                    if terminal.output.is_empty() {
                        ui.label(RichText::new("No runner output yet.").italics());
                    } else {
                        ui.label(RichText::new(&terminal.output).monospace());
                    }
                });
        } else {
            ui.label(RichText::new("Loading terminal output…").italics());
        }
    }

    fn all_queue_view(&mut self, context: &egui::Context) {
        egui::CentralPanel::default().show(context, |ui| {
            let queued = self
                .tasks
                .values()
                .filter(|task| task.snapshot.status == TaskStatus::Queued)
                .collect::<Vec<_>>();
            ui.heading("All queue");
            ui.label(format!(
                "{} queued across {} projects",
                queued.len(),
                self.projects.len()
            ));
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("all-queue")
                .show(ui, |ui| {
                    if queued.is_empty() {
                        ui.label(RichText::new("No queued work.").italics());
                    }
                    for task in queued {
                        let project_name =
                            self.projects.get(&task.snapshot.project_id).map_or_else(
                                || task.snapshot.project_id.to_string(),
                                |p| p.name.clone(),
                            );
                        let selected = self.selected_task.as_ref() == Some(&task.snapshot.id);
                        if ui
                            .selectable_label(selected, queue_task_card_text(task, &project_name))
                            .clicked()
                        {
                            self.selected_project = Some(task.snapshot.project_id.clone());
                            self.selected_task = Some(task.snapshot.id.clone());
                            self.selected_agent = None;
                            self.terminal_run_id = None;
                            self.terminal = None;
                            self.start.agent_id = task
                                .snapshot
                                .assigned_agent_id
                                .as_ref()
                                .map_or_else(String::new, ToString::to_string);
                            self.start.worktree = self
                                .projects
                                .get(&task.snapshot.project_id)
                                .map_or_else(String::new, |project| project.root.clone());
                        }
                    }
                });
        });
    }

    fn load_terminal(&mut self, context: &egui::Context, run_id: RunId) {
        let Some(project_id) = self.runs.get(&run_id).map(|run| run.project_id.clone()) else {
            self.notice = Some("run is not present in the current snapshot".into());
            return;
        };
        self.terminal_run_id = Some(run_id.clone());
        self.terminal = None;
        self.terminal_request_id = self.terminal_request_id.saturating_add(1);
        spawn_run_terminal(
            self.client.clone(),
            self.sender.clone(),
            context.clone(),
            project_id,
            run_id,
            self.terminal_request_id,
        );
    }

    fn factory_view(&mut self, context: &egui::Context) {
        if self.show_all_queue {
            self.all_queue_view(context);
            return;
        }
        egui::CentralPanel::default().show(context, |ui| {
            let Some(project) = self.project().cloned() else {
                ui.centered_and_justified(|ui| {
                    ui.label("Create a project to start the factory.");
                });
                return;
            };
            ui.heading(project.name);
            ui.label(RichText::new(&project.root).small().weak());
            let tasks = self
                .project_tasks()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            let completed_count = tasks
                .iter()
                .filter(|task| !task_is_visible(task.snapshot.status, false))
                .count();
            let active_count = tasks.len().saturating_sub(completed_count);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Agents").strong());
                if ui.button("New agent").clicked() {
                    self.create_agent = Some(AgentForm {
                        parent: None,
                        role: AgentRole::Worker,
                        provider: Provider::Codex,
                        model: String::new(),
                    });
                }
            });
            egui::ScrollArea::horizontal()
                .id_salt("agent-strip")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let agents = self
                            .project_agents()
                            .into_iter()
                            .cloned()
                            .collect::<Vec<_>>();
                        for agent in agents {
                            let run = self.run_for_agent(&agent);
                            let text = agent_card_text(&agent, run);
                            if ui
                                .selectable_label(
                                    self.selected_agent.as_ref() == Some(&agent.id),
                                    text,
                                )
                                .clicked()
                            {
                                self.select_agent(context, agent.id);
                            }
                        }
                    });
                });
            ui.separator();
            ui.label(RichText::new("Allocation").strong());
            let unassigned = allocation_counts(&tasks, None);
            ui.collapsing(format!("Allocation · {} unassigned", unassigned.0), |ui| {
                ui.label(
                    RichText::new(
                        "Operator starts v1 work explicitly; scheduling remains roadmap.",
                    )
                    .small()
                    .weak(),
                );
                for agent in self
                    .project_agents()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
                {
                    let counts = allocation_counts(&tasks, Some(&agent.id));
                    ui.collapsing(
                        format!("{} · queued {} · active {}", agent.id, counts.0, counts.1),
                        |ui| {
                            for task in tasks.iter().filter(|task| {
                                task.snapshot.assigned_agent_id.as_ref() == Some(&agent.id)
                                    && task.snapshot.status == TaskStatus::Queued
                            }) {
                                ui.label(task.snapshot.title.clone());
                            }
                            if counts.0 == 0 && counts.1 == 0 {
                                ui.label(RichText::new("No queued or active work.").italics());
                            }
                        },
                    );
                }
                ui.label(format!(
                    "Operator queue · {} queued · {} active",
                    unassigned.0, unassigned.1
                ));
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Active tasks · {active_count}")).strong());
                if ui
                    .button(if self.show_history {
                        format!("Hide history · {completed_count}")
                    } else {
                        format!("History · {completed_count}")
                    })
                    .clicked()
                {
                    self.show_history = !self.show_history;
                }
            });
            let columns = if self.show_history {
                vec![
                    ("QUEUED", TaskColumn::Queued),
                    ("RUNNING", TaskColumn::Running),
                    ("BLOCKED", TaskColumn::Blocked),
                    ("DONE", TaskColumn::Done),
                ]
            } else {
                vec![
                    ("QUEUED", TaskColumn::Queued),
                    ("RUNNING", TaskColumn::Running),
                    ("BLOCKED", TaskColumn::Blocked),
                ]
            };
            let show_history = self.show_history;
            ui.columns(columns.len(), |columns_ui| {
                for (index, (title, status)) in columns.into_iter().enumerate() {
                    columns_ui[index].label(RichText::new(title).strong());
                    columns_ui[index].separator();
                    egui::ScrollArea::vertical()
                        .id_salt(("task-column", index))
                        .auto_shrink([false, false])
                        .show(&mut columns_ui[index], |ui| {
                            for task in tasks.iter().filter(|task| {
                                task_is_visible(task.snapshot.status, show_history)
                                    && status.matches(task.snapshot.status)
                            }) {
                                let selected =
                                    self.selected_task.as_ref() == Some(&task.snapshot.id);
                                if ui
                                    .selectable_label(selected, task_card_text(task))
                                    .clicked()
                                {
                                    let run_id = self.run_for_task(task).map(|run| run.id.clone());
                                    self.selected_task = Some(task.snapshot.id.clone());
                                    self.selected_agent = None;
                                    if let Some(run_id) = run_id {
                                        self.load_terminal(context, run_id);
                                    } else {
                                        self.terminal_run_id = None;
                                        self.terminal = None;
                                    }
                                    self.start.agent_id = task
                                        .snapshot
                                        .assigned_agent_id
                                        .as_ref()
                                        .map_or_else(String::new, ToString::to_string);
                                    self.start.worktree = project.root.clone();
                                }
                            }
                        });
                }
            });
        });
    }

    fn recent_panel(&self, context: &egui::Context) {
        egui::TopBottomPanel::bottom("recent")
            .resizable(true)
            .default_height(120.0)
            .show(context, |ui| {
                ui.label(RichText::new("Recent events").strong());
                ui.collapsing(format!("Activity · {} events", self.recent.len()), |ui| {
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for event in self.recent.iter().rev().take(20).rev() {
                                ui.label(RichText::new(event_summary(event)).monospace().small());
                            }
                        });
                });
            });
    }

    fn dialogs(&mut self, context: &egui::Context) {
        self.project_dialog(context);
        self.agent_dialog(context);
        self.task_dialog(context);
    }

    fn project_dialog(&mut self, context: &egui::Context) {
        let Some(mut form) = self.create_project.take() else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        egui::Window::new("New project")
            .open(&mut open)
            .show(context, |ui| {
                field(ui, "ID", &mut form.id);
                field(ui, "Name", &mut form.name);
                field(ui, "Root", &mut form.root);
                submit = ui.button("Create project").clicked();
            });
        if submit {
            match ProjectId::try_from(form.id.clone()) {
                Ok(id) => self.submit(
                    LocalRequest::CreateProject {
                        id,
                        name: form.name,
                        root: form.root,
                    },
                    context,
                ),
                Err(error) => self.notice = Some(error.to_string()),
            }
        } else if open {
            self.create_project = Some(form);
        }
    }

    fn agent_dialog(&mut self, context: &egui::Context) {
        let Some(mut form) = self.create_agent.take() else {
            return;
        };
        let Some(project_id) = self.selected_project.clone() else {
            return;
        };
        let agents = self
            .project_agents()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let parent_options = parent_agent_options(&agents);
        let mut open = true;
        let mut submit = false;
        egui::Window::new("New agent")
            .open(&mut open)
            .show(context, |ui| {
                ui.label(
                    RichText::new("The agent ID is generated automatically.")
                        .small()
                        .weak(),
                );
                ui.horizontal(|ui| {
                    ui.label("Parent");
                    egui::ComboBox::from_id_salt("new-agent-parent")
                        .selected_text(
                            form.parent
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_else(|| "None".into()),
                        )
                        .show_ui(ui, |ui| {
                            for option in &parent_options {
                                let label = option
                                    .as_ref()
                                    .map(ToString::to_string)
                                    .unwrap_or_else(|| "None".into());
                                ui.selectable_value(&mut form.parent, option.clone(), label);
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Role");
                    ui.selectable_value(&mut form.role, AgentRole::Worker, "worker");
                    ui.selectable_value(&mut form.role, AgentRole::Orchestrator, "orchestrator");
                });
                ui.horizontal(|ui| {
                    ui.label("Provider");
                    ui.selectable_value(&mut form.provider, Provider::Codex, "Codex");
                    ui.selectable_value(&mut form.provider, Provider::ClaudeCode, "Claude");
                });
                let options = provider_model_options(form.provider);
                if !options.iter().any(|option| {
                    option.value == (!form.model.is_empty()).then_some(form.model.as_str())
                }) {
                    form.model.clear();
                }
                ui.horizontal(|ui| {
                    ui.label("Model");
                    let selected = options
                        .iter()
                        .find(|option| {
                            option.value == (!form.model.is_empty()).then_some(form.model.as_str())
                        })
                        .map_or("Choose a model", |option| option.label);
                    egui::ComboBox::from_id_salt("new-agent-model")
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for option in options {
                                ui.selectable_value(
                                    &mut form.model,
                                    option.value.unwrap_or_default().to_owned(),
                                    option.label,
                                );
                            }
                        });
                });
                ui.label(
                    RichText::new("Provider default uses the CLI's configured model.")
                        .small()
                        .weak(),
                );
                submit = ui.button("Create agent").clicked();
            });
        if submit {
            self.submit(
                LocalRequest::CreateAgent {
                    id: AgentId::try_from(short_id("agent")).expect("generated agent ID is valid"),
                    project_id,
                    parent_agent_id: form.parent,
                    role: form.role,
                    provider: form.provider,
                    model: (!form.model.trim().is_empty()).then(|| form.model.trim().to_owned()),
                },
                context,
            );
        } else if open {
            self.create_agent = Some(form);
        }
    }

    fn task_dialog(&mut self, context: &egui::Context) {
        let Some(mut form) = self.create_task.take() else {
            return;
        };
        let Some(project_id) = self.selected_project.clone() else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        egui::Window::new("New task")
            .open(&mut open)
            .default_width(480.0)
            .show(context, |ui| {
                field(ui, "ID", &mut form.id);
                field(ui, "Title", &mut form.title);
                ui.label("Instructions");
                ui.add(egui::TextEdit::multiline(&mut form.body).desired_rows(10));
                ui.add(egui::DragValue::new(&mut form.priority).prefix("priority "));
                submit = ui.button("Create task").clicked();
            });
        if submit {
            match TaskId::try_from(form.id.clone()) {
                Ok(id) => self.submit(
                    LocalRequest::CreateTask {
                        id,
                        project_id,
                        parent_task_id: None,
                        title: form.title,
                        body: form.body,
                        priority: form.priority,
                    },
                    context,
                ),
                Err(error) => self.notice = Some(error.to_string()),
            }
        } else if open {
            self.create_task = Some(form);
        }
    }
}

impl eframe::App for FactoryApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive(context);
        self.top_bar(context);
        self.projects_panel(context);
        self.inspector(context);
        self.recent_panel(context);
        self.factory_view(context);
        self.dialogs(context);
    }
}

#[derive(Clone, Copy)]
enum TaskColumn {
    Queued,
    Running,
    Blocked,
    Done,
}

fn task_is_visible(status: TaskStatus, show_history: bool) -> bool {
    show_history
        || !matches!(
            status,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
        )
}

impl TaskColumn {
    const fn matches(self, status: TaskStatus) -> bool {
        match self {
            Self::Queued => matches!(status, TaskStatus::Queued),
            Self::Running => matches!(status, TaskStatus::Running),
            Self::Blocked => matches!(status, TaskStatus::Blocked),
            Self::Done => matches!(
                status,
                TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
            ),
        }
    }
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.text_edit_singleline(value);
    });
}

fn task_result_text(task: &TaskDetail) -> Option<&str> {
    task.result.as_deref()
}

fn can_edit_task_assignment(status: TaskStatus) -> bool {
    status == TaskStatus::Queued
}

fn should_clear_queue_owner_pending(
    editor_task_id: Option<&TaskId>,
    assignment_task_id: Option<&TaskId>,
) -> bool {
    matches!((editor_task_id, assignment_task_id), (Some(editor), Some(operation)) if editor == operation)
}

fn task_assignee_text(task: &TaskDetail) -> String {
    task.snapshot.assigned_agent_id.as_ref().map_or_else(
        || "unassigned".into(),
        |agent| format!("assigned to {agent}"),
    )
}

fn task_card_text(task: &TaskDetail) -> String {
    format!("{}\n{}", task.snapshot.title, task_assignee_text(task))
}

fn queue_task_card_text(task: &TaskDetail, project_name: &str) -> String {
    format!(
        "{} · {}\n{}",
        project_name,
        task.snapshot.title,
        task_assignee_text(task)
    )
}

fn latest_run_for_task<'a>(
    runs: impl Iterator<Item = &'a RunSnapshot>,
    project_id: &ProjectId,
    task_id: &TaskId,
) -> Option<&'a RunSnapshot> {
    runs.filter(|run| run.project_id == *project_id && run.task_id.as_ref() == Some(task_id))
        .max_by(|left, right| {
            left.updated_at_ms
                .cmp(&right.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        })
}

fn allocation_counts(
    tasks: &[TaskDetail],
    agent_id: Option<&AgentId>,
) -> (usize, usize, usize, usize) {
    let mut counts = (0, 0, 0, 0);
    for task in tasks
        .iter()
        .filter(|task| task.snapshot.assigned_agent_id.as_ref() == agent_id)
    {
        match task.snapshot.status {
            TaskStatus::Queued => counts.0 += 1,
            TaskStatus::Running => counts.1 += 1,
            TaskStatus::Blocked => counts.2 += 1,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled => counts.3 += 1,
        }
    }
    counts
}

fn event_requires_detail_refresh(envelope: &EventEnvelope) -> bool {
    matches!(
        &envelope.event,
        FactoryEvent::TaskChanged { task } if task.status.is_terminal()
    )
}

fn task_event_needs_detail(task_was_known: bool, envelope: &EventEnvelope) -> bool {
    !task_was_known && matches!(envelope.event, FactoryEvent::TaskChanged { .. })
}

fn task_id_from_event(envelope: &EventEnvelope) -> Option<&TaskId> {
    match &envelope.event {
        FactoryEvent::TaskChanged { task } => Some(&task.id),
        _ => None,
    }
}

fn task_event_ids(envelope: Option<&EventEnvelope>) -> Option<(ProjectId, TaskId)> {
    match envelope.map(|envelope| &envelope.event) {
        Some(FactoryEvent::TaskChanged { task }) => {
            Some((task.project_id.clone(), task.id.clone()))
        }
        _ => None,
    }
}

fn should_apply_snapshot(
    current_refresh_id: u64,
    current_event_sequence: Option<i64>,
    incoming_refresh_id: u64,
    incoming_event_sequence: Option<i64>,
) -> bool {
    if incoming_refresh_id < current_refresh_id {
        return false;
    }
    match (current_event_sequence, incoming_event_sequence) {
        (Some(current), Some(incoming)) => incoming >= current,
        _ => true,
    }
}

fn merge_task_detail(current: &mut TaskDetail, incoming: TaskDetail) {
    match incoming
        .snapshot
        .updated_at_ms
        .cmp(&current.snapshot.updated_at_ms)
    {
        Ordering::Greater => *current = incoming,
        Ordering::Equal => {
            if current.body.is_empty() && !incoming.body.is_empty() {
                current.body = incoming.body;
            }
            if current.result.is_none() && incoming.result.is_some() {
                current.result = incoming.result;
            }
        }
        Ordering::Less => {}
    }
}

fn should_apply_terminal(
    request_id: u64,
    latest_request_id: u64,
    selected_run_id: Option<&RunId>,
    incoming: &RunTerminal,
) -> bool {
    request_id == latest_request_id && selected_run_id == Some(&incoming.run_id)
}

fn snapshot_heads_are_stable(before: Option<i64>, after: Option<i64>) -> bool {
    before == after
}

fn short_id(prefix: &str) -> String {
    let uuid = Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &uuid[..8])
}

fn agent_card_text(agent: &AgentSnapshot, run: Option<&RunSnapshot>) -> String {
    let state = run.map_or_else(
        || "idle".to_owned(),
        |run| format!("{:?} · {:?}", run.status, run.observer_health),
    );
    format!("{}\n{:?} · {state}", agent.id, agent.provider)
}

fn agent_inspector_summary(ui: &mut egui::Ui, agent: &AgentSnapshot, run: Option<&RunSnapshot>) {
    ui.heading(agent.id.to_string());
    ui.label(format!("{:?} · {:?}", agent.role, agent.provider));
    ui.separator();
    if let Some(run) = run {
        ui.label(format!("Run: {}", run.id));
        ui.label(format!("Status: {:?}", run.status));
        ui.label(format!("Observer: {:?}", run.observer_health));
        ui.label(format!("Worktree: {}", run.worktree));
        if let Some(task) = &run.task_id {
            ui.label(format!("Task: {task}"));
        }
        if let Some(reason) = run.failure_reason {
            ui.label(format!("Failure: {reason:?}"));
        }
    } else {
        ui.label("IDLE");
    }
}

fn event_summary(event: &EventEnvelope) -> String {
    let detail = match &event.event {
        FactoryEvent::ProjectChanged { project } => format!("project {}", project.id),
        FactoryEvent::TaskChanged { task } => format!("task {} → {:?}", task.id, task.status),
        FactoryEvent::AgentChanged { agent } => format!("agent {}", agent.id),
        FactoryEvent::RunChanged { run } => format!("run {} → {:?}", run.id, run.status),
        FactoryEvent::TaskDeleted { task_id, .. } => format!("task {task_id} deleted"),
        FactoryEvent::AgentDeleted { agent_id, .. } => format!("agent {agent_id} deleted"),
        FactoryEvent::ProjectDeleted { project_id } => format!("project {project_id} deleted"),
    };
    format!("#{:06} {detail}", event.sequence)
}

fn spawn_refresh(
    client: Client,
    sender: Sender<UiMessage>,
    context: egui::Context,
    refresh_id: u64,
) {
    thread::spawn(move || {
        let result = load_consistent_snapshot(&client);
        let message = result.map_or_else(UiMessage::StreamFailed, |(snapshot, event_sequence)| {
            UiMessage::Snapshot {
                snapshot,
                event_sequence,
                refresh_id,
            }
        });
        let _ = sender.send(message);
        context.request_repaint();
    });
}

fn load_consistent_snapshot(client: &Client) -> Result<(Snapshot, Option<i64>), String> {
    for _ in 0..3 {
        let before = load_event_sequence(client)?;
        let snapshot = load_snapshot(client)?;
        let after = load_event_sequence(client)?;
        if snapshot_heads_are_stable(before, after) {
            return Ok((snapshot, after.or(before)));
        }
    }
    Err("daemon state changed while loading the UI snapshot".into())
}

fn spawn_task_detail(
    client: Client,
    sender: Sender<UiMessage>,
    context: egui::Context,
    project_id: ProjectId,
    task_id: TaskId,
) {
    thread::spawn(move || {
        let result = load_task_detail(&client, project_id, task_id);
        let _ = sender.send(UiMessage::TaskDetail(result));
        context.request_repaint();
    });
}

fn spawn_run_terminal(
    client: Client,
    sender: Sender<UiMessage>,
    context: egui::Context,
    project_id: ProjectId,
    run_id: RunId,
    request_id: u64,
) {
    thread::spawn(move || {
        let result = load_run_terminal(&client, project_id, run_id);
        let _ = sender.send(UiMessage::RunTerminal { request_id, result });
        context.request_repaint();
    });
}

fn load_run_terminal(
    client: &Client,
    project_id: ProjectId,
    run_id: RunId,
) -> Result<RunTerminal, String> {
    match request_response_raw(client, LocalRequest::GetRunTerminal { project_id, run_id })? {
        LocalResponse::RunTerminal { terminal } => Ok(terminal),
        LocalResponse::Error { message, .. } => Err(message),
        _ => Err("daemon returned an unexpected terminal response".into()),
    }
}

fn load_task_detail(
    client: &Client,
    project_id: ProjectId,
    task_id: TaskId,
) -> Result<TaskDetail, String> {
    let response = request_response(
        client,
        LocalRequest::GetTask {
            project_id: project_id.clone(),
            task_id: task_id.clone(),
        },
    )?;
    match response {
        LocalResponse::Task { task } => Ok(task),
        _ => Err("daemon returned an unexpected task detail response".into()),
    }
}

fn agent_profile_draft(detail: &AgentDetail) -> AgentProfileDraft {
    AgentProfileDraft {
        model: detail.profile.model.clone().unwrap_or_default(),
        instructions: detail.profile.instructions.clone(),
        memory: detail.profile.memory.clone(),
    }
}

fn provider_model_options(provider: Provider) -> &'static [ModelOption] {
    const CODEX: &[ModelOption] = &[
        ModelOption {
            label: "Provider default",
            value: None,
        },
        ModelOption {
            label: "GPT-5.6 Luna",
            value: Some("gpt-5.6-luna"),
        },
        ModelOption {
            label: "GPT-5 Codex",
            value: Some("gpt-5-codex"),
        },
    ];
    const CLAUDE: &[ModelOption] = &[
        ModelOption {
            label: "Provider default",
            value: None,
        },
        ModelOption {
            label: "Sonnet",
            value: Some("sonnet"),
        },
        ModelOption {
            label: "Opus",
            value: Some("opus"),
        },
        ModelOption {
            label: "Fable",
            value: Some("fable"),
        },
    ];
    match provider {
        Provider::Codex => CODEX,
        Provider::ClaudeCode => CLAUDE,
    }
}

const fn agent_profile_guidance_help() -> &'static str {
    "Persistent guidance included with every new task. Use Message for the next task."
}

fn parent_agent_options(agents: &[AgentSnapshot]) -> Vec<Option<AgentId>> {
    let mut options = vec![None];
    options.extend(agents.iter().map(|agent| Some(agent.id.clone())));
    options
}

fn agent_profile_load_message(error: Option<&str>) -> String {
    error.map_or_else(
        || "Loading profile…".to_owned(),
        |error| format!("Profile unavailable: {error}"),
    )
}

fn spawn_agent_detail(
    client: Client,
    sender: Sender<UiMessage>,
    context: egui::Context,
    project_id: ProjectId,
    agent_id: AgentId,
) {
    thread::spawn(move || {
        let response_agent_id = agent_id.clone();
        let result = request_response(
            &client,
            LocalRequest::GetAgent {
                project_id,
                agent_id,
            },
        )
        .and_then(|response| match response {
            LocalResponse::Agent { agent } => Ok(agent),
            _ => Err("daemon returned an unexpected agent detail response".into()),
        });
        let _ = sender.send(UiMessage::AgentDetail {
            agent_id: response_agent_id,
            result,
        });
        context.request_repaint();
    });
}

fn spawn_agent_profile_update(
    client: Client,
    sender: Sender<UiMessage>,
    context: egui::Context,
    request: LocalRequest,
) {
    thread::spawn(move || {
        let response_agent_id = match &request {
            LocalRequest::UpdateAgentProfile { agent_id, .. } => agent_id.clone(),
            _ => unreachable!("profile update helper receives one request shape"),
        };
        let result = request_response(&client, request).and_then(|response| match response {
            LocalResponse::AgentProfileUpdated { agent } => Ok(agent),
            _ => Err("daemon returned an unexpected agent profile response".into()),
        });
        let _ = sender.send(UiMessage::AgentProfile {
            agent_id: response_agent_id,
            result,
        });
        context.request_repaint();
    });
}

fn spawn_agent_messages(
    client: Client,
    sender: Sender<UiMessage>,
    context: egui::Context,
    project_id: ProjectId,
    agent_id: AgentId,
) {
    thread::spawn(move || {
        let response_agent_id = agent_id.clone();
        let result = request_response(
            &client,
            LocalRequest::ListAgentMessages {
                project_id,
                agent_id,
                after_id: None,
                limit: MAX_AGENT_PAGE_ITEMS,
            },
        )
        .and_then(|response| match response {
            LocalResponse::AgentMessages { messages, .. } => Ok(messages),
            _ => Err("daemon returned an unexpected agent messages response".into()),
        });
        let _ = sender.send(UiMessage::AgentMessages {
            agent_id: response_agent_id,
            result,
        });
        context.request_repaint();
    });
}

fn spawn_agent_message_send(
    client: Client,
    sender: Sender<UiMessage>,
    context: egui::Context,
    project_id: ProjectId,
    agent_id: AgentId,
    body: String,
) {
    thread::spawn(move || {
        let response_agent_id = agent_id.clone();
        let result = request_response(
            &client,
            LocalRequest::SendAgentMessage {
                id: factory_core::MessageId::try_from(short_id("message"))
                    .expect("generated message ID is valid"),
                project_id,
                sender_agent_id: None,
                recipient_agent_id: agent_id,
                body,
            },
        )
        .and_then(|response| match response {
            LocalResponse::AgentMessageSent { message } => Ok(message),
            _ => Err("daemon returned an unexpected agent message response".into()),
        });
        let _ = sender.send(UiMessage::AgentMessageSent {
            agent_id: response_agent_id,
            result,
        });
        context.request_repaint();
    });
}

fn spawn_subscription(client: Client, sender: Sender<UiMessage>, context: egui::Context) {
    thread::spawn(move || {
        let mut after_sequence = 0_i64;
        let mut delay = std::time::Duration::from_millis(250);
        loop {
            let failure = match client.subscribe(after_sequence) {
                Ok(subscription) => {
                    let mut failure = "event stream ended".to_owned();
                    for frame in subscription {
                        match frame {
                            Ok(ServerFrame::Event { event, .. }) => {
                                after_sequence = event.sequence;
                                delay = std::time::Duration::from_millis(250);
                                if sender.send(UiMessage::Event(event)).is_err() {
                                    return;
                                }
                                context.request_repaint();
                            }
                            Ok(ServerFrame::Response {
                                response: LocalResponse::CaughtUp { .. },
                                ..
                            }) => {
                                if sender.send(UiMessage::SubscriptionCaughtUp).is_err() {
                                    return;
                                }
                            }
                            Ok(ServerFrame::Response { .. }) => {}
                            Err(error) => {
                                failure = error.to_string();
                                break;
                            }
                        }
                    }
                    failure
                }
                Err(error) => error.to_string(),
            };
            if sender.send(UiMessage::StreamFailed(failure)).is_err() {
                return;
            }
            context.request_repaint();
            thread::sleep(delay);
            delay = delay
                .checked_mul(2)
                .unwrap_or(std::time::Duration::from_secs(5))
                .min(std::time::Duration::from_secs(5));
        }
    });
}

fn load_snapshot(client: &Client) -> Result<Snapshot, String> {
    let projects = load_projects(client)?;
    let mut snapshot = Snapshot {
        projects,
        ..Snapshot::default()
    };
    for project in &snapshot.projects {
        snapshot.tasks.extend(load_tasks(client, &project.id)?);
        snapshot.agents.extend(load_agents(client, &project.id)?);
        snapshot.runs.extend(load_runs(client, &project.id)?);
    }
    Ok(snapshot)
}

fn load_event_sequence(client: &Client) -> Result<Option<i64>, String> {
    let response = request_response(client, LocalRequest::LatestEventSequence)?;
    match response {
        LocalResponse::EventHead { sequence } => Ok(Some(sequence)),
        _ => Err("daemon returned an unexpected event-head response".into()),
    }
}

fn load_projects(client: &Client) -> Result<Vec<ProjectSnapshot>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        match request_response(
            client,
            LocalRequest::ListProjects {
                after_id: after,
                limit: MAX_PROJECT_PAGE_ITEMS,
            },
        )? {
            LocalResponse::Projects {
                projects,
                next_after_id,
            } => {
                all.extend(projects);
                let Some(next) = next_after_id else { break };
                after = Some(next);
            }
            _ => return Err("daemon returned an unexpected project response".into()),
        }
    }
    Ok(all)
}

fn load_tasks(client: &Client, project_id: &ProjectId) -> Result<Vec<TaskDetail>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        match request_response(
            client,
            LocalRequest::ListTasks {
                project_id: project_id.clone(),
                after_id: after,
                limit: MAX_TASK_PAGE_ITEMS,
            },
        )? {
            LocalResponse::Tasks {
                tasks,
                next_after_id,
            } => {
                all.extend(tasks);
                let Some(next) = next_after_id else { break };
                after = Some(next);
            }
            _ => return Err("daemon returned an unexpected task response".into()),
        }
    }
    Ok(all)
}

fn load_agents(client: &Client, project_id: &ProjectId) -> Result<Vec<AgentSnapshot>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        match request_response(
            client,
            LocalRequest::ListAgents {
                project_id: project_id.clone(),
                after_id: after,
                limit: MAX_AGENT_PAGE_ITEMS,
            },
        )? {
            LocalResponse::Agents {
                agents,
                next_after_id,
            } => {
                all.extend(agents);
                let Some(next) = next_after_id else { break };
                after = Some(next);
            }
            _ => return Err("daemon returned an unexpected agent response".into()),
        }
    }
    Ok(all)
}

fn load_runs(client: &Client, project_id: &ProjectId) -> Result<Vec<RunSnapshot>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        match request_response(
            client,
            LocalRequest::ListRuns {
                project_id: project_id.clone(),
                after_id: after,
                limit: MAX_RUN_PAGE_ITEMS,
            },
        )? {
            LocalResponse::Runs {
                runs,
                next_after_id,
            } => {
                all.extend(runs);
                let Some(next) = next_after_id else { break };
                after = Some(next);
            }
            _ => return Err("daemon returned an unexpected run response".into()),
        }
    }
    Ok(all)
}

fn request_response(client: &Client, request: LocalRequest) -> Result<LocalResponse, String> {
    match request_response_raw(client, request)? {
        LocalResponse::Error { message, .. } => Err(message),
        response => Ok(response),
    }
}

fn request_response_raw(client: &Client, request: LocalRequest) -> Result<LocalResponse, String> {
    match client.request(request).map_err(|error| error.to_string())? {
        ServerFrame::Response { response, .. } => Ok(response),
        ServerFrame::Event { .. } => Err("daemon returned an event instead of a response".into()),
    }
}

fn operation_message(response: LocalResponse) -> Result<String, String> {
    let message = match response {
        LocalResponse::Health => "Daemon is healthy".to_owned(),
        LocalResponse::ProjectCreated { project } => format!("Created project {}", project.id),
        LocalResponse::TaskCreated { task } => format!("Created task {}", task.snapshot.id),
        LocalResponse::TaskRetried { task } => format!("Requeued task {}", task.snapshot.id),
        LocalResponse::TaskAssigned { task } => {
            format!("Updated queue owner for task {}", task.snapshot.id)
        }
        LocalResponse::AgentCreated { agent } => format!("Created agent {}", agent.id),
        LocalResponse::RunAccepted { run_id } => format!("Accepted run {run_id}"),
        LocalResponse::RunStopped { run_id } => format!("Stop requested for run {run_id}"),
        _ => return Err("daemon returned an unexpected operation response".into()),
    };
    Ok(message)
}

#[cfg(test)]
mod tests {
    use factory_core::{
        AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, ObserverHealth, ProjectId,
        Provider, RunId, RunSnapshot, RunStatus, TaskDetail, TaskSnapshot, TaskStatus,
        local::RunTerminal,
    };

    use super::{
        TaskColumn, agent_card_text, agent_profile_guidance_help, agent_profile_load_message,
        allocation_counts, event_requires_detail_refresh, parent_agent_options,
        provider_model_options, task_assignee_text, task_is_visible, task_result_text,
    };

    #[test]
    fn agent_creation_uses_existing_agents_as_parent_choices() {
        let agents = vec![
            AgentSnapshot {
                id: AgentId::try_from("god").unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
                current_run_id: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            AgentSnapshot {
                id: AgentId::try_from("worker").unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_agent_id: Some(AgentId::try_from("god").unwrap()),
                role: AgentRole::Worker,
                provider: Provider::ClaudeCode,
                current_run_id: None,
                created_at_ms: 2,
                updated_at_ms: 2,
            },
        ];
        let options = parent_agent_options(&agents);
        assert_eq!(
            options,
            vec![
                None,
                Some(AgentId::try_from("god").unwrap()),
                Some(AgentId::try_from("worker").unwrap())
            ]
        );
    }

    #[test]
    fn profile_load_errors_replace_the_indefinite_loading_state() {
        assert_eq!(agent_profile_load_message(None), "Loading profile…");
        assert_eq!(
            agent_profile_load_message(Some("request is not valid local protocol JSON")),
            "Profile unavailable: request is not valid local protocol JSON"
        );
    }

    #[test]
    fn model_picker_is_scoped_to_the_selected_provider() {
        let codex = provider_model_options(Provider::Codex);
        let claude = provider_model_options(Provider::ClaudeCode);
        assert_eq!(codex.first().unwrap().value, None);
        assert_eq!(claude.first().unwrap().value, None);
        assert!(
            codex
                .iter()
                .any(|option| option.value == Some("gpt-5.6-luna"))
        );
        assert!(claude.iter().any(|option| option.value == Some("sonnet")));
        assert!(!codex.iter().any(|option| option.value == Some("sonnet")));
        assert!(
            !claude
                .iter()
                .any(|option| option.value == Some("gpt-5.6-luna"))
        );
    }

    #[test]
    fn profile_guidance_is_explicitly_persistent_not_the_next_message() {
        assert!(agent_profile_guidance_help().contains("every new task"));
        assert!(agent_profile_guidance_help().contains("Message"));
    }

    #[test]
    fn older_snapshot_generations_and_event_heads_are_rejected() {
        assert!(!super::should_apply_snapshot(4, Some(10), 3, Some(11)));
        assert!(!super::should_apply_snapshot(4, Some(10), 5, Some(9)));
        assert!(super::should_apply_snapshot(4, Some(10), 5, Some(10)));
        assert!(super::snapshot_heads_are_stable(Some(10), Some(10)));
        assert!(!super::snapshot_heads_are_stable(Some(10), Some(11)));
        assert!(super::snapshot_heads_are_stable(None, None));
    }

    #[test]
    fn newer_task_detail_wins_when_a_snapshot_arrives_out_of_order() {
        let mut current = TaskDetail {
            snapshot: TaskSnapshot {
                id: factory_core::TaskId::try_from("task-1").unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_task_id: None,
                assigned_agent_id: None,
                title: "Current".into(),
                status: TaskStatus::Succeeded,
                priority: 0,
                created_at_ms: 1,
                updated_at_ms: 20,
            },
            body: "new body".into(),
            result: Some("new result".into()),
        };
        let older = TaskDetail {
            snapshot: TaskSnapshot {
                updated_at_ms: 10,
                status: TaskStatus::Queued,
                ..current.snapshot.clone()
            },
            body: "old body".into(),
            result: None,
        };

        super::merge_task_detail(&mut current, older);
        assert_eq!(current.snapshot.status, TaskStatus::Succeeded);
        assert_eq!(current.result.as_deref(), Some("new result"));

        let same_timestamp = TaskDetail {
            snapshot: TaskSnapshot {
                status: TaskStatus::Queued,
                ..current.snapshot.clone()
            },
            body: "same-time stale body".into(),
            result: None,
        };
        super::merge_task_detail(&mut current, same_timestamp);
        assert_eq!(current.snapshot.status, TaskStatus::Succeeded);
        assert_eq!(current.result.as_deref(), Some("new result"));
    }

    #[test]
    fn previously_unseen_task_events_request_bounded_detail_hydration() {
        let event = EventEnvelope {
            protocol_version: 1,
            sequence: 1,
            occurred_at_ms: 2,
            event: FactoryEvent::TaskChanged {
                task: TaskSnapshot {
                    id: factory_core::TaskId::try_from("task-1").unwrap(),
                    project_id: ProjectId::try_from("factory").unwrap(),
                    parent_task_id: None,
                    assigned_agent_id: None,
                    title: "New task".into(),
                    status: TaskStatus::Queued,
                    priority: 0,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                },
            },
        };

        assert!(super::task_event_needs_detail(false, &event));
        assert!(!super::task_event_needs_detail(true, &event));
    }

    #[test]
    fn board_columns_are_a_total_task_status_projection() {
        assert!(TaskColumn::Queued.matches(factory_core::TaskStatus::Queued));
        assert!(TaskColumn::Running.matches(factory_core::TaskStatus::Running));
        assert!(TaskColumn::Blocked.matches(factory_core::TaskStatus::Blocked));
        for status in [
            factory_core::TaskStatus::Succeeded,
            factory_core::TaskStatus::Failed,
            factory_core::TaskStatus::Cancelled,
        ] {
            assert!(TaskColumn::Done.matches(status));
        }
    }

    #[test]
    fn completed_tasks_are_hidden_from_the_default_active_board() {
        assert!(task_is_visible(TaskStatus::Queued, false));
        assert!(task_is_visible(TaskStatus::Blocked, false));
        assert!(!task_is_visible(TaskStatus::Succeeded, false));
        assert!(task_is_visible(TaskStatus::Succeeded, true));
    }

    #[test]
    fn agent_card_uses_only_public_bounded_state() {
        let agent = AgentSnapshot {
            id: AgentId::try_from("curie").unwrap(),
            project_id: ProjectId::try_from("factory").unwrap(),
            parent_agent_id: None,
            role: AgentRole::Worker,
            provider: Provider::ClaudeCode,
            current_run_id: Some(RunId::try_from("run-1").unwrap()),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let run = RunSnapshot {
            id: RunId::try_from("run-1").unwrap(),
            project_id: agent.project_id.clone(),
            agent_id: agent.id.clone(),
            parent_run_id: None,
            task_id: None,
            status: RunStatus::Running,
            activity: None,
            wait_reason: None,
            worktree: "/work/curie".into(),
            observer_health: ObserverHealth::Healthy,
            observer_health_since_ms: 1,
            started_at_ms: 1,
            status_since_ms: 1,
            updated_at_ms: 2,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
            failure_reason: None,
        };
        let card = agent_card_text(&agent, Some(&run));
        assert!(card.contains("curie"));
        assert!(card.contains("Running"));
        assert!(!card.contains("/work/curie"));
    }

    #[test]
    fn task_inspector_reads_the_persisted_result_without_event_payloads() {
        let task = TaskDetail {
            snapshot: TaskSnapshot {
                id: factory_core::TaskId::try_from("task-1").unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_task_id: None,
                assigned_agent_id: None,
                title: "Completed task".into(),
                status: TaskStatus::Succeeded,
                priority: 0,
                created_at_ms: 1,
                updated_at_ms: 2,
            },
            body: "private body".into(),
            result: Some("The bounded provider answer".into()),
        };

        assert_eq!(task_result_text(&task), Some("The bounded provider answer"));
    }

    #[test]
    fn terminal_task_events_request_a_detail_refresh() {
        let event = EventEnvelope {
            protocol_version: 1,
            sequence: 1,
            occurred_at_ms: 2,
            event: FactoryEvent::TaskChanged {
                task: TaskSnapshot {
                    id: factory_core::TaskId::try_from("task-1").unwrap(),
                    project_id: ProjectId::try_from("factory").unwrap(),
                    parent_task_id: None,
                    assigned_agent_id: None,
                    title: "Completed task".into(),
                    status: TaskStatus::Succeeded,
                    priority: 0,
                    created_at_ms: 1,
                    updated_at_ms: 2,
                },
            },
        };

        assert!(event_requires_detail_refresh(&event));
    }

    #[test]
    fn allocation_is_a_projection_of_existing_task_assignments() {
        let curie = AgentId::try_from("curie").unwrap();
        let other = AgentId::try_from("other").unwrap();
        let tasks = vec![
            TaskDetail {
                snapshot: TaskSnapshot {
                    id: factory_core::TaskId::try_from("queued-curie").unwrap(),
                    project_id: ProjectId::try_from("factory").unwrap(),
                    parent_task_id: None,
                    assigned_agent_id: Some(curie.clone()),
                    title: "Queued for Curie".into(),
                    status: TaskStatus::Queued,
                    priority: 0,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                },
                body: String::new(),
                result: None,
            },
            TaskDetail {
                snapshot: TaskSnapshot {
                    id: factory_core::TaskId::try_from("running-curie").unwrap(),
                    status: TaskStatus::Running,
                    assigned_agent_id: Some(curie.clone()),
                    ..tasks_snapshot("running-curie")
                },
                body: String::new(),
                result: None,
            },
            TaskDetail {
                snapshot: TaskSnapshot {
                    id: factory_core::TaskId::try_from("unassigned").unwrap(),
                    status: TaskStatus::Queued,
                    assigned_agent_id: None,
                    ..tasks_snapshot("unassigned")
                },
                body: String::new(),
                result: None,
            },
            TaskDetail {
                snapshot: TaskSnapshot {
                    id: factory_core::TaskId::try_from("done-other").unwrap(),
                    status: TaskStatus::Succeeded,
                    assigned_agent_id: Some(other),
                    ..tasks_snapshot("done-other")
                },
                body: String::new(),
                result: None,
            },
        ];
        assert_eq!(allocation_counts(&tasks, Some(&curie)), (1, 1, 0, 0));
        assert_eq!(allocation_counts(&tasks, None), (1, 0, 0, 0));
        assert_eq!(task_assignee_text(&tasks[0]), "assigned to curie");
        assert_eq!(task_assignee_text(&tasks[2]), "unassigned");
    }

    #[test]
    fn only_queued_tasks_can_change_assignment() {
        assert!(super::can_edit_task_assignment(TaskStatus::Queued));
        for status in [
            TaskStatus::Running,
            TaskStatus::Blocked,
            TaskStatus::Succeeded,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert!(!super::can_edit_task_assignment(status));
        }
    }

    #[test]
    fn all_queue_cards_include_project_context_without_ids() {
        let task = TaskDetail {
            snapshot: TaskSnapshot {
                id: factory_core::TaskId::try_from("task-1").unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_task_id: None,
                assigned_agent_id: Some(AgentId::try_from("curie").unwrap()),
                title: "Review the queue".into(),
                status: TaskStatus::Queued,
                priority: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            body: String::new(),
            result: None,
        };
        let card = super::queue_task_card_text(&task, "Factory");
        assert!(card.contains("Factory"));
        assert!(card.contains("Review the queue"));
        assert!(card.contains("curie"));
        assert!(!card.contains("task-1"));
    }

    #[test]
    fn queue_owner_editor_rejects_a_stale_live_reassignment() {
        let task_id = factory_core::TaskId::try_from("task-1").unwrap();
        let mut editor = super::QueueOwnerEditor::new(task_id, "curie".into());
        editor.value = "turing".into();

        assert!(editor.sync_from_server("other"));
        assert_eq!(editor.value, "other");
        assert_eq!(editor.base_value, "other");
        assert!(editor.conflict);
    }

    #[test]
    fn queue_owner_editor_accepts_its_pending_server_acknowledgement() {
        let task_id = factory_core::TaskId::try_from("task-1").unwrap();
        let mut editor = super::QueueOwnerEditor::new(task_id, "curie".into());
        editor.value = "turing".into();
        editor.mark_pending();

        assert!(!editor.sync_from_server("curie"));
        assert!(editor.pending_value.is_some());
        assert!(!editor.sync_from_server("turing"));
        assert_eq!(editor.base_value, "turing");
        assert!(!editor.conflict);
        assert!(editor.pending_value.is_none());

        editor.value = "other".into();
        editor.mark_pending();
        editor.clear_pending();
        assert!(editor.pending_value.is_none());
    }

    #[test]
    fn only_the_matching_task_can_clear_a_queue_owner_pending_request() {
        let first = factory_core::TaskId::try_from("task-1").unwrap();
        let second = factory_core::TaskId::try_from("task-2").unwrap();
        assert!(super::should_clear_queue_owner_pending(
            Some(&first),
            Some(&first)
        ));
        assert!(!super::should_clear_queue_owner_pending(
            Some(&first),
            Some(&second)
        ));
        assert!(!super::should_clear_queue_owner_pending(Some(&first), None));
    }

    #[test]
    fn task_terminal_selection_prefers_the_most_recent_attempt() {
        let project_id = ProjectId::try_from("factory").unwrap();
        let task_id = factory_core::TaskId::try_from("task-1").unwrap();
        let older = test_run("run-old", &project_id, &task_id, 10);
        let newer = test_run("run-new", &project_id, &task_id, 20);

        let selected =
            super::latest_run_for_task([&newer, &older].into_iter(), &project_id, &task_id)
                .unwrap();
        assert_eq!(selected.id, newer.id);
    }

    #[test]
    fn stale_terminal_refreshes_cannot_replace_the_latest_request() {
        let selected = RunId::try_from("run-1").unwrap();
        let stale = RunTerminal {
            run_id: selected.clone(),
            head_sequence: 7,
            output: "older".into(),
            truncated: false,
        };
        let other_run = RunTerminal {
            run_id: RunId::try_from("run-2").unwrap(),
            ..stale.clone()
        };

        assert!(!super::should_apply_terminal(1, 2, Some(&selected), &stale));
        assert!(!super::should_apply_terminal(2, 2, None, &stale));
        assert!(!super::should_apply_terminal(
            2,
            2,
            Some(&selected),
            &other_run
        ));
        assert!(super::should_apply_terminal(2, 2, Some(&selected), &stale));
    }

    #[test]
    fn task_cards_are_compact_and_leave_identifiers_to_the_inspector() {
        let task = TaskDetail {
            snapshot: TaskSnapshot {
                id: factory_core::TaskId::try_from("task-1").unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_task_id: None,
                assigned_agent_id: Some(AgentId::try_from("curie").unwrap()),
                title: "Ship the operator view".into(),
                status: TaskStatus::Queued,
                priority: 9,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            body: String::new(),
            result: None,
        };
        let card = super::task_card_text(&task);
        assert!(card.contains("Ship the operator view"));
        assert!(card.contains("curie"));
        assert!(!card.contains("task-1"));
    }

    fn test_run(
        id: &str,
        project_id: &ProjectId,
        task_id: &factory_core::TaskId,
        updated_at_ms: i64,
    ) -> RunSnapshot {
        RunSnapshot {
            id: RunId::try_from(id).unwrap(),
            project_id: project_id.clone(),
            agent_id: AgentId::try_from("curie").unwrap(),
            parent_run_id: None,
            task_id: Some(task_id.clone()),
            status: RunStatus::Succeeded,
            activity: None,
            wait_reason: None,
            worktree: "/work/curie".into(),
            observer_health: ObserverHealth::Healthy,
            observer_health_since_ms: updated_at_ms,
            started_at_ms: updated_at_ms,
            status_since_ms: updated_at_ms,
            updated_at_ms,
            ended_at_ms: Some(updated_at_ms),
            exit_code: Some(0),
            exit_signal: None,
            failure_reason: None,
        }
    }

    fn tasks_snapshot(id: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: factory_core::TaskId::try_from(id).unwrap(),
            project_id: ProjectId::try_from("factory").unwrap(),
            parent_task_id: None,
            assigned_agent_id: None,
            title: id.into(),
            status: TaskStatus::Queued,
            priority: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }
}
