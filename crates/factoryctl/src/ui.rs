use std::{
    collections::BTreeMap,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use eframe::egui::{self, Color32, RichText};
use factory_core::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, ProjectId, ProjectSnapshot,
    Provider, RunId, RunSnapshot, TaskDetail, TaskId, TaskStatus,
    local::{
        LocalRequest, LocalResponse, MAX_AGENT_PAGE_ITEMS, MAX_PROJECT_PAGE_ITEMS,
        MAX_RUN_PAGE_ITEMS, MAX_TASK_PAGE_ITEMS, ServerFrame, SubscriptionSeverity,
        SubscriptionUsageStatus,
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
    Snapshot(Snapshot),
    Event(EventEnvelope),
    Operation(Result<String, String>),
    StreamFailed(String),
}

#[derive(Default)]
struct Snapshot {
    projects: Vec<ProjectSnapshot>,
    tasks: Vec<TaskDetail>,
    agents: Vec<AgentSnapshot>,
    runs: Vec<RunSnapshot>,
    usage: Option<SubscriptionUsageStatus>,
}

struct FactoryApp {
    client: Client,
    sender: Sender<UiMessage>,
    receiver: Receiver<UiMessage>,
    projects: BTreeMap<ProjectId, ProjectSnapshot>,
    tasks: BTreeMap<TaskId, TaskDetail>,
    agents: BTreeMap<AgentId, AgentSnapshot>,
    runs: BTreeMap<RunId, RunSnapshot>,
    usage: Option<SubscriptionUsageStatus>,
    recent: Vec<EventEnvelope>,
    selected_project: Option<ProjectId>,
    selected_task: Option<TaskId>,
    selected_agent: Option<AgentId>,
    connection: ConnectionState,
    notice: Option<String>,
    create_project: Option<ProjectForm>,
    create_agent: Option<AgentForm>,
    create_task: Option<TaskForm>,
    start: StartForm,
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
    id: String,
    parent: String,
    role: AgentRole,
    provider: Provider,
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

impl FactoryApp {
    fn new(context: &eframe::CreationContext<'_>, client: Client) -> Self {
        context.egui_ctx.set_visuals(egui::Visuals::dark());
        let (sender, receiver) = mpsc::channel();
        spawn_refresh(client.clone(), sender.clone(), context.egui_ctx.clone());
        spawn_subscription(client.clone(), sender.clone(), context.egui_ctx.clone());
        Self {
            client,
            sender,
            receiver,
            projects: BTreeMap::new(),
            tasks: BTreeMap::new(),
            agents: BTreeMap::new(),
            runs: BTreeMap::new(),
            usage: None,
            recent: Vec::new(),
            selected_project: None,
            selected_task: None,
            selected_agent: None,
            connection: ConnectionState::Loading,
            notice: None,
            create_project: None,
            create_agent: None,
            create_task: None,
            start: StartForm::default(),
        }
    }

    fn receive(&mut self, context: &egui::Context) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                UiMessage::Snapshot(snapshot) => {
                    self.projects = snapshot
                        .projects
                        .into_iter()
                        .map(|project| (project.id.clone(), project))
                        .collect();
                    self.tasks = snapshot
                        .tasks
                        .into_iter()
                        .map(|task| (task.snapshot.id.clone(), task))
                        .collect();
                    self.agents = snapshot
                        .agents
                        .into_iter()
                        .map(|agent| (agent.id.clone(), agent))
                        .collect();
                    self.runs = snapshot
                        .runs
                        .into_iter()
                        .map(|run| (run.id.clone(), run))
                        .collect();
                    self.usage = snapshot.usage;
                    if self.selected_project.is_none() {
                        self.selected_project = self.projects.keys().next().cloned();
                    }
                    self.connection = ConnectionState::Live;
                }
                UiMessage::Event(event) => self.apply_event(event),
                UiMessage::Operation(result) => match result {
                    Ok(message) => {
                        self.notice = Some(message);
                        self.refresh(context);
                    }
                    Err(message) => self.notice = Some(message),
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
        }
        self.recent.push(envelope);
        if self.recent.len() > RECENT_EVENT_LIMIT {
            self.recent.remove(0);
        }
        self.connection = ConnectionState::Live;
    }

    fn refresh(&self, context: &egui::Context) {
        spawn_refresh(self.client.clone(), self.sender.clone(), context.clone());
    }

    fn submit(&self, request: LocalRequest, context: &egui::Context) {
        let client = self.client.clone();
        let sender = self.sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result = request_response(&client, request).and_then(operation_message);
            let _ = sender.send(UiMessage::Operation(result));
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

    fn run_for_agent(&self, agent: &AgentSnapshot) -> Option<&RunSnapshot> {
        agent
            .current_run_id
            .as_ref()
            .and_then(|run| self.runs.get(run))
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
                if let Some(usage) = &self.usage {
                    ui.separator();
                    let (label, color) = match usage.overall_severity {
                        SubscriptionSeverity::Ok => ("capacity ok", Color32::LIGHT_GREEN),
                        SubscriptionSeverity::Warning => ("capacity warning", Color32::YELLOW),
                        SubscriptionSeverity::Critical => ("capacity critical", Color32::LIGHT_RED),
                    };
                    ui.label(RichText::new(label).color(color));
                    for provider in &usage.providers {
                        let percent = provider
                            .used_percent
                            .map_or_else(|| "unknown".to_owned(), |value| format!("{value}%"));
                        ui.label(format!("{:?} {percent}", provider.provider));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.refresh(context);
                    }
                    if ui.button("New task").clicked() && self.selected_project.is_some() {
                        self.create_task = Some(TaskForm {
                            id: short_id("task"),
                            title: String::new(),
                            body: String::new(),
                            priority: 0,
                        });
                    }
                    if ui.button("New agent").clicked() && self.selected_project.is_some() {
                        self.create_agent = Some(AgentForm {
                            id: short_id("worker"),
                            parent: String::new(),
                            role: AgentRole::Worker,
                            provider: Provider::Codex,
                        });
                    }
                    if ui.button("New project").clicked() {
                        self.create_project = Some(ProjectForm {
                            id: short_id("project"),
                            name: String::new(),
                            root: String::new(),
                        });
                    }
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
                for project in self.projects.values() {
                    let selected = self.selected_project.as_ref() == Some(&project.id);
                    if ui
                        .selectable_label(selected, format!("{}\n{}", project.name, project.id))
                        .clicked()
                    {
                        self.selected_project = Some(project.id.clone());
                        self.selected_task = None;
                        self.selected_agent = None;
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
                        self.task_inspector(ui, context, &task);
                        return;
                    }
                }
                if let Some(agent_id) = self.selected_agent.clone() {
                    if let Some(agent) = self.agents.get(&agent_id) {
                        agent_inspector(ui, agent, self.run_for_agent(agent));
                        return;
                    }
                }
                ui.label("Select a task or agent.");
            });
    }

    fn task_inspector(&mut self, ui: &mut egui::Ui, context: &egui::Context, task: &TaskDetail) {
        ui.heading(&task.snapshot.title);
        ui.label(format!("{} · {:?}", task.snapshot.id, task.snapshot.status));
        ui.separator();
        ui.label(RichText::new("Instructions").strong());
        if task.body.is_empty() {
            ui.label(RichText::new("Refresh to load private instructions.").italics());
        } else {
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .show(ui, |ui| {
                    ui.label(&task.body);
                });
        }
        if let Some(result) = task_result_text(task) {
            ui.separator();
            ui.label(RichText::new("Result").strong());
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .show(ui, |ui| {
                    ui.label(result);
                });
        }
        if !task.snapshot.depends_on.is_empty() {
            ui.separator();
            ui.label(format!(
                "Depends on: {}",
                task.snapshot
                    .depends_on
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if task.snapshot.status == TaskStatus::Queued {
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

    fn factory_view(&mut self, context: &egui::Context) {
        egui::CentralPanel::default().show(context, |ui| {
            let Some(project) = self.project().cloned() else {
                ui.centered_and_justified(|ui| {
                    ui.label("Create a project to start the factory.");
                });
                return;
            };
            ui.heading(project.name);
            ui.label(RichText::new(&project.root).small().weak());
            ui.separator();
            ui.label(RichText::new("Agents").strong());
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
                                self.selected_agent = Some(agent.id);
                                self.selected_task = None;
                            }
                        }
                    });
                });
            ui.separator();
            ui.label(RichText::new("Tasks").strong());
            let tasks = self
                .project_tasks()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            ui.columns(4, |columns| {
                for (index, (title, status)) in [
                    ("QUEUED", TaskColumn::Queued),
                    ("RUNNING", TaskColumn::Running),
                    ("BLOCKED", TaskColumn::Blocked),
                    ("DONE", TaskColumn::Done),
                ]
                .into_iter()
                .enumerate()
                {
                    columns[index].label(RichText::new(title).strong());
                    columns[index].separator();
                    for task in tasks
                        .iter()
                        .filter(|task| status.matches(task.snapshot.status))
                    {
                        let selected = self.selected_task.as_ref() == Some(&task.snapshot.id);
                        if columns[index]
                            .selectable_label(
                                selected,
                                format!(
                                    "{}\n{} · p{}",
                                    task.snapshot.title, task.snapshot.id, task.snapshot.priority
                                ),
                            )
                            .clicked()
                        {
                            self.selected_task = Some(task.snapshot.id.clone());
                            self.selected_agent = None;
                            self.start.agent_id = task
                                .snapshot
                                .assigned_agent_id
                                .as_ref()
                                .map_or_else(String::new, ToString::to_string);
                            self.start.worktree = project.root.clone();
                        }
                    }
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
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for event in self.recent.iter().rev().take(20).rev() {
                            ui.label(RichText::new(event_summary(event)).monospace().small());
                        }
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
        let mut open = true;
        let mut submit = false;
        egui::Window::new("New agent")
            .open(&mut open)
            .show(context, |ui| {
                field(ui, "ID", &mut form.id);
                field(ui, "Parent (optional)", &mut form.parent);
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
                submit = ui.button("Create agent").clicked();
            });
        if submit {
            let parsed = AgentId::try_from(form.id.clone()).and_then(|id| {
                let parent_agent_id = if form.parent.trim().is_empty() {
                    None
                } else {
                    Some(AgentId::try_from(form.parent.clone())?)
                };
                Ok((id, parent_agent_id))
            });
            match parsed {
                Ok((id, parent_agent_id)) => self.submit(
                    LocalRequest::CreateAgent {
                        id,
                        project_id,
                        parent_agent_id,
                        role: form.role,
                        provider: form.provider,
                    },
                    context,
                ),
                Err(error) => self.notice = Some(error.to_string()),
            }
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

fn agent_inspector(ui: &mut egui::Ui, agent: &AgentSnapshot, run: Option<&RunSnapshot>) {
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
    };
    format!("#{:06} {detail}", event.sequence)
}

fn spawn_refresh(client: Client, sender: Sender<UiMessage>, context: egui::Context) {
    thread::spawn(move || {
        let result = load_snapshot(&client);
        let message = result.map_or_else(UiMessage::StreamFailed, UiMessage::Snapshot);
        let _ = sender.send(message);
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
        usage: Some(load_usage(client)?),
        ..Snapshot::default()
    };
    for project in &snapshot.projects {
        snapshot.tasks.extend(load_tasks(client, &project.id)?);
        snapshot.agents.extend(load_agents(client, &project.id)?);
        snapshot.runs.extend(load_runs(client, &project.id)?);
    }
    Ok(snapshot)
}

fn load_usage(client: &Client) -> Result<SubscriptionUsageStatus, String> {
    match request_response(client, LocalRequest::SubscriptionUsage)? {
        LocalResponse::SubscriptionUsage { usage } => Ok(usage),
        _ => Err("daemon returned an unexpected subscription usage response".into()),
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
    match client.request(request).map_err(|error| error.to_string())? {
        ServerFrame::Response { response, .. } => match response {
            LocalResponse::Error { message, .. } => Err(message),
            response => Ok(response),
        },
        ServerFrame::Event { .. } => Err("daemon returned an event instead of a response".into()),
    }
}

fn operation_message(response: LocalResponse) -> Result<String, String> {
    let message = match response {
        LocalResponse::Health => "Daemon is healthy".to_owned(),
        LocalResponse::ProjectCreated { project } => format!("Created project {}", project.id),
        LocalResponse::TaskCreated { task } => format!("Created task {}", task.snapshot.id),
        LocalResponse::AgentCreated { agent } => format!("Created agent {}", agent.id),
        LocalResponse::RunAccepted { run_id } => format!("Accepted run {run_id}"),
        _ => return Err("daemon returned an unexpected operation response".into()),
    };
    Ok(message)
}

#[cfg(test)]
mod tests {
    use factory_core::{
        AgentId, AgentRole, AgentSnapshot, ObserverHealth, ProjectId, Provider, RunId, RunSnapshot,
        RunStatus, TaskDetail, TaskSnapshot, TaskStatus,
    };

    use super::{TaskColumn, agent_card_text, task_result_text};

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
                depends_on: Vec::new(),
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
}
