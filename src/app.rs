//! The graphical interface.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};

use crate::backup::{self, Event, Plan, RemoteTarget, RepoJob, RepoStatus, ServerAccess};
use crate::config::{
    Account, Config, Destination, DestinationKind, ProviderKind, delete_token, is_safe_segment,
    load_token, new_id, normalize_base_url, store_token,
};
use crate::i18n::{set_language, t, tf};
use crate::provider::{Client, Listing, Repo};

const MAX_LOG_LINES: usize = 5000;
const GREEN: Color32 = Color32::from_rgb(70, 170, 90);
const AMBER: Color32 = Color32::from_rgb(215, 150, 40);
const RED: Color32 = Color32::from_rgb(220, 80, 70);

struct RepoRow {
    key: String,
    account_id: String,
    repo: Repo,
    status: RepoStatus,
}

#[derive(Default)]
struct AccountState {
    loading: bool,
    login: Option<String>,
    error: Option<String>,
}

struct ListResult {
    account_id: String,
    result: Result<Listing, String>,
}

struct LogLine {
    error: bool,
    text: String,
}

struct AccountForm {
    /// Set when editing an existing account.
    id: Option<String>,
    kind: ProviderKind,
    name: String,
    base_url: String,
    token: String,
    use_as_source: bool,
    error: Option<String>,
}

struct DestinationForm {
    /// Set when editing an existing destination.
    id: Option<String>,
    is_remote: bool,
    path: String,
    account_id: String,
    namespace: String,
    error: Option<String>,
}

enum Dialog {
    None,
    Account(AccountForm),
    Destination(DestinationForm),
    ConfirmDeleteAccount(String),
    ConfirmDeleteDestination(String),
}

/// A change requested while the UI borrows the state, applied afterwards.
enum Action {
    Refresh,
    NewAccount,
    EditAccount(String),
    AskDeleteAccount(String),
    NewDestination,
    EditDestination(String),
    AskDeleteDestination(String),
    SaveConfig,
    Start,
    Cancel,
}

pub struct App {
    config: Config,
    /// False if the settings file exists but could not be read. Saving would
    /// then replace the user's settings with empty ones.
    can_save: bool,
    banner: Option<String>,
    egui_ctx: egui::Context,
    list_sender: Sender<ListResult>,
    list_receiver: Receiver<ListResult>,
    backup_sender: Sender<Event>,
    backup_receiver: Receiver<Event>,
    account_states: HashMap<String, AccountState>,
    repos: Vec<RepoRow>,
    filter: String,
    log: Vec<LogLine>,
    run_cancel: Option<Arc<AtomicBool>>,
    run_thread: Option<JoinHandle<()>>,
    run_total: usize,
    run_finished: usize,
    dialog: Dialog,
}

impl App {
    pub fn new(ctx: &egui::Context) -> Self {
        let (config, can_save, banner) = match Config::load() {
            Ok(config) => (config, true, None),
            Err(error) => (
                Config::default(),
                false,
                Some(format!("{}: {error:#}", t("Settings could not be read and will not be saved"))),
            ),
        };
        set_language(&config.language);
        let (list_sender, list_receiver) = channel();
        let (backup_sender, backup_receiver) = channel();
        let mut app = Self {
            config,
            can_save,
            banner,
            egui_ctx: ctx.clone(),
            list_sender,
            list_receiver,
            backup_sender,
            backup_receiver,
            account_states: HashMap::new(),
            repos: Vec::new(),
            filter: String::new(),
            log: Vec::new(),
            run_cancel: None,
            run_thread: None,
            run_total: 0,
            run_finished: 0,
            dialog: Dialog::None,
        };
        app.refresh_all();
        app
    }

    fn is_running(&self) -> bool {
        self.run_cancel.is_some()
    }

    fn is_loading(&self) -> bool {
        self.account_states.values().any(|state| state.loading)
    }

    fn save_config(&mut self) {
        if !self.can_save {
            return;
        }
        if let Err(error) = self.config.save() {
            self.push_log(true, format!("{}: {error:#}", t("Cannot save settings")));
        }
    }

    fn push_log(&mut self, error: bool, text: String) {
        self.log.push(LogLine { error, text });
        if self.log.len() > MAX_LOG_LINES {
            self.log.drain(..self.log.len() - MAX_LOG_LINES);
        }
    }

    fn refresh_all(&mut self) {
        let ids: Vec<String> = self.config.accounts.iter().map(|account| account.id.clone()).collect();
        for id in ids {
            self.refresh_account(&id);
        }
    }

    fn refresh_account(&mut self, account_id: &str) {
        let Some(account) = self.config.account(account_id).cloned() else {
            return;
        };
        if !account.use_as_source {
            self.repos.retain(|row| row.account_id != account.id);
            self.account_states.remove(&account.id);
            return;
        }
        let state = self.account_states.entry(account.id.clone()).or_default();
        state.loading = true;
        state.error = None;
        let token = load_token(&account.id);
        let sender = self.list_sender.clone();
        let ctx = self.egui_ctx.clone();
        std::thread::spawn(move || {
            let result = token.map_err(|error| error.to_string()).and_then(|token| {
                Client::new(account.kind, &account.base_url, &token)
                    .list_repos()
                    .map_err(|error| error.to_string())
            });
            let _ = sender.send(ListResult { account_id: account.id, result });
            ctx.request_repaint();
        });
    }

    fn drain_events(&mut self) {
        while let Ok(message) = self.list_receiver.try_recv() {
            self.apply_listing(message);
        }
        while let Ok(event) = self.backup_receiver.try_recv() {
            self.apply_backup_event(event);
        }
    }

    fn apply_listing(&mut self, message: ListResult) {
        let ListResult { account_id, result } = message;
        // The account may have been deleted while its list was loading.
        if self.config.account(&account_id).is_none() {
            return;
        }
        let state = self.account_states.entry(account_id.clone()).or_default();
        state.loading = false;
        match result {
            Ok(listing) => {
                state.login = Some(listing.login);
                self.repos.retain(|row| row.account_id != account_id);
                for repo in listing.repos {
                    self.repos.push(RepoRow {
                        key: format!("{account_id}/{}", repo.full_name()),
                        account_id: account_id.clone(),
                        repo,
                        status: RepoStatus::Idle,
                    });
                }
                self.repos.sort_by_key(|row| row.repo.full_name().to_lowercase());
                for warning in listing.warnings {
                    self.push_log(true, warning);
                }
            }
            Err(error) => {
                state.error = Some(error.clone());
                self.push_log(true, error);
            }
        }
    }

    fn apply_backup_event(&mut self, event: Event) {
        match event {
            Event::Status { key, status } => {
                if matches!(
                    status,
                    RepoStatus::Done | RepoStatus::Partial(_) | RepoStatus::Failed(_) | RepoStatus::Cancelled
                ) {
                    self.run_finished += 1;
                }
                if let Some(row) = self.repos.iter_mut().find(|row| row.key == key) {
                    row.status = status;
                }
            }
            Event::Log { error, text } => self.push_log(error, text),
            Event::Finished { done, failed, cancelled } => {
                self.run_cancel = None;
                self.run_thread = None;
                for row in &mut self.repos {
                    if matches!(row.status, RepoStatus::Queued | RepoStatus::Running) {
                        row.status = RepoStatus::Cancelled;
                    }
                }
                let key = if cancelled {
                    "Cancelled: {} succeeded, {} failed"
                } else {
                    "Finished: {} succeeded, {} failed"
                };
                self.push_log(failed > 0, tf(key, &[&done.to_string(), &failed.to_string()]));
            }
        }
    }

    fn access_for(&self, account: &Account) -> Result<ServerAccess, String> {
        let token = load_token(&account.id)
            .map_err(|error| format!("{}: {error}", account.name))?;
        let login = self
            .account_states
            .get(&account.id)
            .and_then(|state| state.login.clone())
            .unwrap_or_default();
        Ok(ServerAccess {
            kind: account.kind,
            base_url: account.base_url.clone(),
            token,
            login,
        })
    }

    fn build_plan(&self) -> Result<Plan, String> {
        let mut local = Vec::new();
        let mut remote = Vec::new();
        for destination in self.config.destinations.iter().filter(|d| d.enabled) {
            match &destination.kind {
                DestinationKind::Local { path } => local.push(path.clone()),
                DestinationKind::Remote { account_id, namespace } => {
                    let account = self
                        .config
                        .account(account_id)
                        .ok_or_else(|| t("A destination refers to a deleted account").to_owned())?;
                    remote.push(RemoteTarget {
                        label: destination_label(&self.config, destination),
                        access: self.access_for(account)?,
                        namespace: namespace.clone(),
                    });
                }
            }
        }

        let mut accesses: HashMap<&str, ServerAccess> = HashMap::new();
        let mut jobs = Vec::new();
        for row in self.repos.iter().filter(|row| self.config.selected.contains(&row.key)) {
            if !accesses.contains_key(row.account_id.as_str()) {
                let account = self
                    .config
                    .account(&row.account_id)
                    .ok_or_else(|| t("A repository refers to a deleted account").to_owned())?;
                accesses.insert(&row.account_id, self.access_for(account)?);
            }
            jobs.push(RepoJob {
                key: row.key.clone(),
                repo: row.repo.clone(),
                source: accesses[row.account_id.as_str()].clone(),
            });
        }
        Ok(Plan {
            jobs: backup::dedupe_jobs(jobs),
            local,
            remote,
            prune: self.config.prune,
            workers: self.config.workers,
        })
    }

    fn start_backup(&mut self) {
        let plan = match self.build_plan() {
            Ok(plan) => plan,
            Err(error) => {
                self.push_log(true, error);
                return;
            }
        };
        let queued: Vec<&str> = plan.jobs.iter().map(|job| job.key.as_str()).collect();
        for row in &mut self.repos {
            row.status = if queued.contains(&row.key.as_str()) {
                RepoStatus::Queued
            } else {
                RepoStatus::Idle
            };
        }
        self.run_total = plan.jobs.len();
        self.run_finished = 0;
        let count = plan.jobs.len().to_string();
        let targets = (plan.local.len() + plan.remote.len()).to_string();
        self.push_log(false, tf("Starting: {} repositories to {} destinations", &[&count, &targets]));
        let cancel = Arc::new(AtomicBool::new(false));
        self.run_cancel = Some(cancel.clone());
        let ctx = self.egui_ctx.clone();
        self.run_thread = Some(backup::start(plan, cancel, self.backup_sender.clone(), move || {
            ctx.request_repaint()
        }));
    }

    fn open_account_form(&mut self, id: Option<String>) {
        let existing = id.as_deref().and_then(|id| self.config.account(id));
        let form = match existing {
            Some(account) => AccountForm {
                id: Some(account.id.clone()),
                kind: account.kind,
                name: account.name.clone(),
                base_url: account.base_url.clone(),
                token: String::new(),
                use_as_source: account.use_as_source,
                error: None,
            },
            None => AccountForm {
                id: None,
                kind: ProviderKind::GitHub,
                name: String::new(),
                base_url: "https://github.com".to_owned(),
                token: String::new(),
                use_as_source: true,
                error: None,
            },
        };
        self.dialog = Dialog::Account(form);
    }

    fn open_destination_form(&mut self, id: Option<String>) {
        let existing = id
            .as_deref()
            .and_then(|id| self.config.destinations.iter().find(|d| d.id == id));
        let mut form = DestinationForm {
            id: None,
            is_remote: false,
            path: String::new(),
            account_id: self.config.accounts.first().map(|a| a.id.clone()).unwrap_or_default(),
            namespace: String::new(),
            error: None,
        };
        if let Some(destination) = existing {
            form.id = Some(destination.id.clone());
            match &destination.kind {
                DestinationKind::Local { path } => form.path = path.display().to_string(),
                DestinationKind::Remote { account_id, namespace } => {
                    form.is_remote = true;
                    form.account_id = account_id.clone();
                    form.namespace = namespace.clone();
                }
            }
        }
        self.dialog = Dialog::Destination(form);
    }

    /// Validates and stores the account form. Returns the id on success.
    fn save_account_form(&mut self, form: &AccountForm) -> Result<String, String> {
        let base_url = normalize_base_url(&form.base_url).map_err(|error| error.to_string())?;
        let token = form.token.trim();
        if form.id.is_none() && token.is_empty() {
            return Err(t("Please enter an access token").to_owned());
        }
        let id = form.id.clone().unwrap_or_else(new_id);
        // The token goes first: an account without one would be useless.
        if !token.is_empty() {
            store_token(&id, token).map_err(|error| error.to_string())?;
        }
        let name = match form.name.trim() {
            "" => base_url.split_once("://").map_or(base_url.as_str(), |(_, rest)| rest).to_owned(),
            name => name.to_owned(),
        };
        let account = Account {
            id: id.clone(),
            kind: form.kind,
            name,
            base_url,
            use_as_source: form.use_as_source,
        };
        match self.config.accounts.iter_mut().find(|a| a.id == id) {
            Some(slot) => *slot = account,
            None => self.config.accounts.push(account),
        }
        self.save_config();
        Ok(id)
    }

    fn save_destination_form(&mut self, form: &DestinationForm) -> Result<(), String> {
        let kind = if form.is_remote {
            let namespace = form.namespace.trim().to_owned();
            if !namespace.is_empty() && !is_safe_segment(&namespace) {
                return Err(t("The organization or user name is not valid").to_owned());
            }
            if self.config.account(&form.account_id).is_none() {
                return Err(t("Please choose an account").to_owned());
            }
            DestinationKind::Remote { account_id: form.account_id.clone(), namespace }
        } else {
            let path = form.path.trim();
            if path.is_empty() {
                return Err(t("Please choose a folder").to_owned());
            }
            DestinationKind::Local { path: PathBuf::from(path) }
        };
        let label = |kind: &DestinationKind| serde_json::to_string(kind).unwrap_or_default();
        let duplicate = self
            .config
            .destinations
            .iter()
            .any(|d| Some(&d.id) != form.id.as_ref() && label(&d.kind) == label(&kind));
        if duplicate {
            return Err(t("This destination already exists").to_owned());
        }
        match form.id.as_ref().and_then(|id| self.config.destinations.iter_mut().find(|d| &d.id == id)) {
            Some(slot) => slot.kind = kind,
            None => self.config.destinations.push(Destination { id: new_id(), enabled: true, kind }),
        }
        self.save_config();
        Ok(())
    }

    fn delete_account(&mut self, id: &str) {
        if let Err(error) = delete_token(id) {
            self.push_log(true, error.to_string());
        }
        self.config.accounts.retain(|account| account.id != id);
        self.config.destinations.retain(|destination| {
            !matches!(&destination.kind, DestinationKind::Remote { account_id, .. } if account_id == id)
        });
        let prefix = format!("{id}/");
        self.config.selected.retain(|key| !key.starts_with(&prefix));
        self.account_states.remove(id);
        self.repos.retain(|row| row.account_id != id);
        self.save_config();
    }

    fn apply_actions(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Refresh => self.refresh_all(),
                Action::NewAccount => self.open_account_form(None),
                Action::EditAccount(id) => self.open_account_form(Some(id)),
                Action::AskDeleteAccount(id) => self.dialog = Dialog::ConfirmDeleteAccount(id),
                Action::NewDestination => self.open_destination_form(None),
                Action::EditDestination(id) => self.open_destination_form(Some(id)),
                Action::AskDeleteDestination(id) => self.dialog = Dialog::ConfirmDeleteDestination(id),
                Action::SaveConfig => self.save_config(),
                Action::Start => self.start_backup(),
                Action::Cancel => {
                    if let Some(cancel) = &self.run_cancel {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.heading("Git Backup");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let before = self.config.language.clone();
                egui::ComboBox::from_id_salt("language")
                    .selected_text(&before)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.config.language, "zh-TW".to_owned(), "zh-TW");
                        ui.selectable_value(&mut self.config.language, "en".to_owned(), "en");
                    });
                if self.config.language != before {
                    set_language(&self.config.language);
                    actions.push(Action::SaveConfig);
                }
            });
        });
        if let Some(banner) = &self.banner {
            ui.colored_label(RED, banner);
        }
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(t("Accounts"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(t("Add")).clicked() {
                        actions.push(Action::NewAccount);
                    }
                });
            });
            ui.separator();
            if self.config.accounts.is_empty() {
                ui.weak(t("Add a GitHub or Gitea account to list its repositories."));
            }
            for account in &mut self.config.accounts {
                ui.push_id(&account.id, |ui| {
                    ui.horizontal(|ui| {
                        let response = ui
                            .checkbox(&mut account.use_as_source, "")
                            .on_hover_text(t("List this account's repositories for backup"));
                        if response.changed() {
                            actions.push(Action::SaveConfig);
                            actions.push(Action::Refresh);
                        }
                        ui.label(RichText::new(account.kind.label()).small().strong());
                        ui.label(&account.name);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(t("Delete")).clicked() {
                                actions.push(Action::AskDeleteAccount(account.id.clone()));
                            }
                            if ui.small_button(t("Edit")).clicked() {
                                actions.push(Action::EditAccount(account.id.clone()));
                            }
                        });
                    });
                    let state = self.account_states.get(&account.id);
                    match state {
                        Some(state) if state.loading => {
                            ui.weak(t("Loading..."));
                        }
                        Some(AccountState { error: Some(error), .. }) => {
                            ui.colored_label(RED, error);
                        }
                        Some(AccountState { login: Some(login), .. }) => {
                            let count = self.repos.iter().filter(|r| r.account_id == account.id).count();
                            ui.weak(tf("{}: {} repositories", &[login, &count.to_string()]));
                        }
                        _ => {
                            ui.weak(t("Not listed (destination only)"));
                        }
                    }
                });
                ui.add_space(4.0);
            }

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.strong(t("Destinations"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(t("Add")).clicked() {
                        actions.push(Action::NewDestination);
                    }
                    if ui.small_button(t("None")).clicked() {
                        self.config.destinations.iter_mut().for_each(|d| d.enabled = false);
                        actions.push(Action::SaveConfig);
                    }
                    if ui.small_button(t("All")).clicked() {
                        self.config.destinations.iter_mut().for_each(|d| d.enabled = true);
                        actions.push(Action::SaveConfig);
                    }
                });
            });
            ui.separator();
            if self.config.destinations.is_empty() {
                ui.weak(t("Add a local folder, or a GitHub or Gitea account, to back up to."));
            }
            let labels: Vec<String> = self
                .config
                .destinations
                .iter()
                .map(|destination| destination_label(&self.config, destination))
                .collect();
            for (destination, label) in self.config.destinations.iter_mut().zip(labels) {
                ui.push_id(&destination.id, |ui| {
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut destination.enabled, "").changed() {
                            actions.push(Action::SaveConfig);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(t("Delete")).clicked() {
                                actions.push(Action::AskDeleteDestination(destination.id.clone()));
                            }
                            if ui.small_button(t("Edit")).clicked() {
                                actions.push(Action::EditDestination(destination.id.clone()));
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(egui::Label::new(&label).truncate()).on_hover_text(&label);
                            });
                        });
                    });
                });
            }
        });
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let selected = self.selected_count();
        let enabled_destinations = self.config.destinations.iter().filter(|d| d.enabled).count();
        let running = self.is_running();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if running {
                if ui.button(t("Cancel")).clicked() {
                    actions.push(Action::Cancel);
                }
            } else {
                let can_start = selected > 0 && enabled_destinations > 0;
                let label = RichText::new(tf("Back up {} repositories", &[&selected.to_string()])).strong();
                let mut response = ui.add_enabled(can_start, egui::Button::new(label));
                if selected == 0 {
                    response = response.on_disabled_hover_text(t("Select at least one repository"));
                } else if enabled_destinations == 0 {
                    response = response.on_disabled_hover_text(t("Select at least one destination"));
                }
                if response.clicked() {
                    actions.push(Action::Start);
                }
            }

            ui.add_enabled_ui(!running, |ui| {
                ui.label(t("Parallel:"));
                let response = ui.add(egui::DragValue::new(&mut self.config.workers).range(1..=16));
                if response.changed() {
                    actions.push(Action::SaveConfig);
                }
                let response = ui
                    .checkbox(&mut self.config.prune, t("Delete branches removed at the source"))
                    .on_hover_text(t(
                        "When off, branches and tags deleted at the source stay in the backup, so they can still be recovered.",
                    ));
                if response.changed() {
                    actions.push(Action::SaveConfig);
                }
            });

            if self.run_total > 0 {
                let fraction = self.run_finished as f32 / self.run_total as f32;
                let text = format!("{} / {}", self.run_finished, self.run_total);
                ui.add(egui::ProgressBar::new(fraction).text(text).animate(running));
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong(t("Log"));
            if ui.small_button(t("Clear")).clicked() {
                self.log.clear();
            }
        });
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.log {
                    let text = RichText::new(&line.text).small();
                    let text = if line.error { text.color(RED) } else { text };
                    ui.add(egui::Label::new(text).wrap());
                }
            });
    }

    fn selected_count(&self) -> usize {
        self.repos.iter().filter(|row| self.config.selected.contains(&row.key)).count()
    }

    fn repo_table(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let filter = self.filter.trim().to_lowercase();
        let visible: Vec<usize> = self
            .repos
            .iter()
            .enumerate()
            .filter(|(_, row)| filter.is_empty() || row.repo.full_name().to_lowercase().contains(&filter))
            .map(|(index, _)| index)
            .collect();
        let visible_selected = visible
            .iter()
            .filter(|&&index| self.config.selected.contains(&self.repos[index].key))
            .count();
        let loading = self.is_loading();

        ui.horizontal(|ui| {
            ui.strong(t("Repositories"));
            ui.add_enabled_ui(!loading, |ui| {
                if ui.button(t("Refresh")).clicked() {
                    actions.push(Action::Refresh);
                }
            });
            if loading {
                ui.spinner();
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(t("Filter by name"))
                    .desired_width(180.0),
            );
            let hint = t("Applies to the repositories currently shown");
            if ui.button(t("Select all")).on_hover_text(hint).clicked() {
                for &index in &visible {
                    self.config.selected.insert(self.repos[index].key.clone());
                }
                actions.push(Action::SaveConfig);
            }
            if ui.button(t("Select none")).on_hover_text(hint).clicked() {
                for &index in &visible {
                    self.config.selected.remove(&self.repos[index].key);
                }
                actions.push(Action::SaveConfig);
            }
            ui.weak(tf(
                "{} of {} selected",
                &[&self.selected_count().to_string(), &self.repos.len().to_string()],
            ));
        });
        ui.separator();

        if self.repos.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.weak(if loading {
                    t("Loading...")
                } else if self.config.accounts.is_empty() {
                    t("No accounts yet. Add one on the left.")
                } else {
                    t("No repositories found.")
                });
            });
            return;
        }

        let selected = &mut self.config.selected;
        let repos = &self.repos;
        let accounts = &self.config.accounts;
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        TableBuilder::new(ui)
            .striped(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(26.0))
            .column(Column::remainder().at_least(180.0).clip(true))
            .column(Column::auto().at_least(120.0))
            .column(Column::auto().at_least(120.0).clip(true))
            .column(Column::auto().at_least(120.0))
            .header(row_height, |mut header| {
                header.col(|ui| {
                    let total = visible.len();
                    let mut all = total > 0 && visible_selected == total;
                    let partly = visible_selected > 0 && visible_selected < total;
                    let checkbox = egui::Checkbox::without_text(&mut all).indeterminate(partly);
                    if ui.add(checkbox).on_hover_text(t("Select or clear all shown")).changed() {
                        for &index in &visible {
                            if all {
                                selected.insert(repos[index].key.clone());
                            } else {
                                selected.remove(&repos[index].key);
                            }
                        }
                        actions.push(Action::SaveConfig);
                    }
                });
                header.col(|ui| {
                    ui.strong(t("Repository"));
                });
                header.col(|ui| {
                    ui.strong(t("Type"));
                });
                header.col(|ui| {
                    ui.strong(t("Account"));
                });
                header.col(|ui| {
                    ui.strong(t("Status"));
                });
            })
            .body(|body| {
                body.rows(row_height, visible.len(), |mut table_row| {
                    let row = &repos[visible[table_row.index()]];
                    table_row.col(|ui| {
                        let mut checked = selected.contains(&row.key);
                        if ui.checkbox(&mut checked, "").changed() {
                            if checked {
                                selected.insert(row.key.clone());
                            } else {
                                selected.remove(&row.key);
                            }
                            actions.push(Action::SaveConfig);
                        }
                    });
                    table_row.col(|ui| {
                        let response = ui.add(egui::Label::new(row.repo.full_name()).truncate());
                        if !row.repo.description.is_empty() {
                            response.on_hover_text(&row.repo.description);
                        }
                    });
                    table_row.col(|ui| {
                        ui.label(repo_type_text(&row.repo));
                    });
                    table_row.col(|ui| {
                        let name = accounts
                            .iter()
                            .find(|account| account.id == row.account_id)
                            .map_or("", |account| account.name.as_str());
                        ui.add(egui::Label::new(name).truncate());
                    });
                    table_row.col(|ui| status_cell(ui, &row.status));
                });
            });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let dialog = std::mem::replace(&mut self.dialog, Dialog::None);
        self.dialog = match dialog {
            Dialog::None => Dialog::None,
            Dialog::Account(form) => self.account_dialog(ctx, form),
            Dialog::Destination(form) => self.destination_dialog(ctx, form),
            Dialog::ConfirmDeleteAccount(id) => {
                let name = self.config.account(&id).map(|a| a.name.clone()).unwrap_or_default();
                let message = tf(
                    "Delete the account {}? Its saved token and any destination using it are also removed. Nothing is deleted from the server.",
                    &[&name],
                );
                match confirm_dialog(ctx, &message) {
                    Some(true) => {
                        self.delete_account(&id);
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::ConfirmDeleteAccount(id),
                }
            }
            Dialog::ConfirmDeleteDestination(id) => {
                let label = self
                    .config
                    .destinations
                    .iter()
                    .find(|d| d.id == id)
                    .map(|d| destination_label(&self.config, d))
                    .unwrap_or_default();
                let message = tf(
                    "Remove the destination {}? Backups already made there are not deleted.",
                    &[&label],
                );
                match confirm_dialog(ctx, &message) {
                    Some(true) => {
                        self.config.destinations.retain(|d| d.id != id);
                        self.save_config();
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::ConfirmDeleteDestination(id),
                }
            }
        };
    }

    fn account_dialog(&mut self, ctx: &egui::Context, mut form: AccountForm) -> Dialog {
        let mut outcome = None;
        egui::Modal::new(egui::Id::unique("account_dialog")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(if form.id.is_some() { t("Edit account") } else { t("Add account") });
            ui.add_space(6.0);
            egui::Grid::new("account_form").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label(t("Service"));
                ui.horizontal(|ui| {
                    let before = form.kind;
                    ui.radio_value(&mut form.kind, ProviderKind::GitHub, "GitHub");
                    ui.radio_value(&mut form.kind, ProviderKind::Gitea, "Gitea");
                    if before != form.kind {
                        form.base_url = match form.kind {
                            ProviderKind::GitHub => "https://github.com".to_owned(),
                            ProviderKind::Gitea => String::new(),
                        };
                    }
                });
                ui.end_row();
                ui.label(t("Server address"));
                let hint = match form.kind {
                    ProviderKind::GitHub => "https://github.com",
                    ProviderKind::Gitea => "https://gitea.example.com",
                };
                ui.add(
                    egui::TextEdit::singleline(&mut form.base_url)
                        .hint_text(hint)
                        .desired_width(320.0),
                );
                ui.end_row();
                ui.label(t("Display name"));
                ui.add(
                    egui::TextEdit::singleline(&mut form.name)
                        .hint_text(t("Optional"))
                        .desired_width(320.0),
                );
                ui.end_row();
                ui.label(t("Access token"));
                let hint = if form.id.is_some() { t("Leave blank to keep the current token") } else { "" };
                ui.add(
                    egui::TextEdit::singleline(&mut form.token)
                        .password(true)
                        .hint_text(hint)
                        .desired_width(320.0),
                );
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.checkbox(&mut form.use_as_source, t("List this account's repositories for backup"));
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    RichText::new(match form.kind {
                        ProviderKind::GitHub => t(
                            "Token permissions: repo. To use as a destination, also workflow, or pushing workflow files is refused.",
                        ),
                        ProviderKind::Gitea => t(
                            "Token permissions: repository (read and write), user (read), organization (read).",
                        ),
                    })
                    .small()
                    .weak(),
                )
                .wrap(),
            );
            ui.small(t("The token is kept in the system's credential store, not in the settings file."));
            if form.base_url.trim().starts_with("http://") {
                ui.colored_label(AMBER, t("Plain http sends the token unencrypted over the network."));
            }
            if let Some(error) = &form.error {
                ui.colored_label(RED, error);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(t("Save")).clicked() {
                    outcome = Some(true);
                }
                if ui.button(t("Cancel")).clicked() {
                    outcome = Some(false);
                }
            });
        });
        match outcome {
            Some(true) => match self.save_account_form(&form) {
                Ok(id) => {
                    self.refresh_account(&id);
                    Dialog::None
                }
                Err(error) => {
                    form.error = Some(error);
                    Dialog::Account(form)
                }
            },
            Some(false) => Dialog::None,
            None => Dialog::Account(form),
        }
    }

    fn destination_dialog(&mut self, ctx: &egui::Context, mut form: DestinationForm) -> Dialog {
        let mut outcome = None;
        let accounts = self.config.accounts.clone();
        egui::Modal::new(egui::Id::unique("destination_dialog")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading(if form.id.is_some() { t("Edit destination") } else { t("Add destination") });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.radio_value(&mut form.is_remote, false, t("Local folder"));
                ui.radio_value(&mut form.is_remote, true, t("GitHub or Gitea account"));
            });
            ui.add_space(6.0);
            if form.is_remote {
                if accounts.is_empty() {
                    ui.colored_label(AMBER, t("Please add the account first."));
                }
                egui::Grid::new("destination_form").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label(t("Account"));
                    let current = accounts
                        .iter()
                        .find(|account| account.id == form.account_id)
                        .map_or(String::new(), account_label);
                    egui::ComboBox::from_id_salt("destination_account")
                        .selected_text(current)
                        .width(320.0)
                        .show_ui(ui, |ui| {
                            for account in &accounts {
                                ui.selectable_value(
                                    &mut form.account_id,
                                    account.id.clone(),
                                    account_label(account),
                                );
                            }
                        });
                    ui.end_row();
                    ui.label(t("Organization or user"));
                    ui.add(
                        egui::TextEdit::singleline(&mut form.namespace)
                            .hint_text(t("Blank means the account itself"))
                            .desired_width(320.0),
                    );
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(t(
                            "Each repository becomes a repository named owner-name, private or public like its source. Existing repositories are never overwritten unless they are the backup of the same source.",
                        ))
                        .small()
                        .weak(),
                    )
                    .wrap(),
                );
            } else {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut form.path).desired_width(370.0));
                    if ui.button(t("Browse...")).clicked()
                        && let Some(folder) = rfd::FileDialog::new().pick_folder()
                    {
                        form.path = folder.display().to_string();
                    }
                });
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(t(
                            "Each repository is stored as a bare mirror in server/owner/name.git, and updated on later runs.",
                        ))
                        .small()
                        .weak(),
                    )
                    .wrap(),
                );
            }
            if let Some(error) = &form.error {
                ui.colored_label(RED, error);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(t("Save")).clicked() {
                    outcome = Some(true);
                }
                if ui.button(t("Cancel")).clicked() {
                    outcome = Some(false);
                }
            });
        });
        match outcome {
            Some(true) => match self.save_destination_form(&form) {
                Ok(()) => Dialog::None,
                Err(error) => {
                    form.error = Some(error);
                    Dialog::Destination(form)
                }
            },
            Some(false) => Dialog::None,
            None => Dialog::Destination(form),
        }
    }
}

fn account_label(account: &Account) -> String {
    format!("{} - {}", account.kind.label(), account.name)
}

fn destination_label(config: &Config, destination: &Destination) -> String {
    match &destination.kind {
        DestinationKind::Local { path } => path.display().to_string(),
        DestinationKind::Remote { account_id, namespace } => {
            let account = config.account(account_id).map_or(String::new(), account_label);
            if namespace.is_empty() {
                account
            } else {
                format!("{account} / {namespace}")
            }
        }
    }
}

fn repo_type_text(repo: &Repo) -> String {
    let mut parts = vec![if repo.private { t("Private") } else { t("Public") }];
    if repo.fork {
        parts.push(t("Fork"));
    }
    if repo.archived {
        parts.push(t("Archived"));
    }
    parts.join(", ")
}

fn status_cell(ui: &mut egui::Ui, status: &RepoStatus) {
    match status {
        RepoStatus::Idle => {}
        RepoStatus::Queued => {
            ui.weak(t("Queued"));
        }
        RepoStatus::Running => {
            ui.spinner();
            ui.label(t("Running"));
        }
        RepoStatus::Done => {
            ui.colored_label(GREEN, t("Done"));
        }
        RepoStatus::Partial(detail) => {
            ui.colored_label(AMBER, t("Partly failed")).on_hover_text(detail);
        }
        RepoStatus::Failed(detail) => {
            ui.colored_label(RED, t("Failed")).on_hover_text(detail);
        }
        RepoStatus::Cancelled => {
            ui.weak(t("Cancelled"));
        }
    }
}

/// Shows a yes/no dialog. `None` means the user has not answered yet.
fn confirm_dialog(ctx: &egui::Context, message: &str) -> Option<bool> {
    let mut answer = None;
    egui::Modal::new(egui::Id::unique("confirm_dialog")).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.add(egui::Label::new(message).wrap());
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button(t("Delete")).clicked() {
                answer = Some(true);
            }
            if ui.button(t("Cancel")).clicked() {
                answer = Some(false);
            }
        });
    });
    answer
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        let ctx = ui.ctx().clone();
        let mut actions = Vec::new();
        let running = self.is_running();

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui, &mut actions));
        egui::Panel::bottom("bottom")
            .resizable(true)
            .default_size(210.0)
            .min_size(120.0)
            .show(ui, |ui| self.bottom_panel(ui, &mut actions));
        egui::Panel::left("side")
            .resizable(true)
            .default_size(330.0)
            .min_size(260.0)
            .show(ui, |ui| {
                // Settings stay frozen during a run, which has its own copy.
                ui.add_enabled_ui(!running, |ui| self.side_panel(ui, &mut actions));
            });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_enabled_ui(!running, |ui| self.repo_table(ui, &mut actions));
        });

        self.dialogs(&ctx);
        self.apply_actions(actions);
    }

    fn on_exit(&mut self) {
        // Stop git child processes instead of leaving them running unattended.
        if let Some(cancel) = &self.run_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(thread) = self.run_thread.take() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
