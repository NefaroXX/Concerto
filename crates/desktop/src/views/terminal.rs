//! Native Concerto console. No PTY or system shell is started.
use crate::theme::AppTheme;
use async_trait::async_trait;
use concerto_config::AppConfig;
use concerto_core::types::PolicyAction;
use concerto_core::{ApprovalDecision, ApprovalSink, CancellationToken};
use concerto_shell::{CommandResult, ShellRuntime};
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Subscription, Task};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub enum Message {
    Input(String),
    Submit,
    Cancel,
    Restart,
    Tick,
    Answer(bool),
    Ready(u64, Result<Arc<ShellRuntime>, String>),
    Completed(u64, Box<CommandResult>),
}
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeConsoleMessage")
    }
}
struct Pending {
    summary: String,
    response: tokio::sync::oneshot::Sender<ApprovalDecision>,
}
type Prompt = Arc<Mutex<Option<Pending>>>;
struct ConsoleApproval {
    prompt: Prompt,
}
#[async_trait]
impl ApprovalSink for ConsoleApproval {
    async fn request_approval(
        &self,
        action: &PolicyAction<'_>,
        cancel: CancellationToken,
    ) -> ApprovalDecision {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let summary = if let Some(facts) = &action.command_facts {
            format!("Run {:?} in {:?}?", facts.argv, facts.working_directory)
        } else {
            format!("Allow {} operation {:?}?", action.tool_name, action.path_facts)
        };
        {
            let Ok(mut prompt) = self.prompt.lock() else {
                return ApprovalDecision::Deny;
            };
            *prompt = Some(Pending { summary, response: sender });
        }
        tokio::select! { _ = cancel.cancelled() => ApprovalDecision::Deny, result = receiver => result.unwrap_or(ApprovalDecision::Deny) }
    }
    async fn approve_all_for_session(
        &self,
        _session: concerto_core::ids::Ulid,
        _cancel: CancellationToken,
    ) {
    }
    async fn request_ack(
        &self,
        _session: concerto_core::ids::Ulid,
        _message: &str,
        _cancel: CancellationToken,
    ) -> bool {
        false
    }
}

pub struct State {
    project_dir: PathBuf,
    config: AppConfig,
    runtime: Option<Arc<ShellRuntime>>,
    input: String,
    transcript: String,
    busy: bool,
    cancel: CancellationToken,
    epoch: u64,
    prompt: Prompt,
    prompt_text: Option<String>,
}
impl State {
    pub fn new(project_dir: PathBuf, config: AppConfig) -> Self {
        Self { project_dir, config, runtime: None, input: String::new(), transcript: "Concerto native shell. Use help for commands. File changes pass through policy and are saved after approval.\n".into(), busy: false, cancel: CancellationToken::new(), epoch: 0, prompt: Arc::new(Mutex::new(None)), prompt_text: None }
    }
    pub fn set_config(&mut self, config: AppConfig, _theme: &AppTheme) -> Task<Message> {
        if self.config.shell_security != config.shell_security
            || self.config.policy != config.policy
        {
            self.cancel.cancel();
            self.runtime = None;
            self.busy = false;
            self.epoch = self.epoch.wrapping_add(1);
            self.prompt = Arc::new(Mutex::new(None));
            self.prompt_text = None;
            self.transcript.push_str("Settings changed. Restart to use the new policy.\n");
        }
        self.config = config;
        Task::none()
    }
    pub fn ensure_started(&mut self, _theme: &AppTheme) -> Task<Message> {
        if self.runtime.is_some() || self.busy {
            return Task::none();
        }
        self.start()
    }
    fn start(&mut self) -> Task<Message> {
        self.cancel.cancel();
        self.cancel = CancellationToken::new();
        self.prompt = Arc::new(Mutex::new(None));
        self.prompt_text = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.busy = true;
        let epoch = self.epoch;
        let project = self.project_dir.clone();
        let config = self.config.clone();
        let approval = Arc::new(ConsoleApproval { prompt: self.prompt.clone() });
        let cancel = self.cancel.clone();
        Task::perform(
            async move {
                tokio::select! { _ = cancel.cancelled() => Err("Console initialization cancelled".into()), result = concerto_shell::client::open(project, config, approval) => result }
            },
            move |result| Message::Ready(epoch, result),
        )
    }
    pub fn update(&mut self, message: Message, _theme: &AppTheme) -> Task<Message> {
        match message {
            Message::Input(input) => self.input = input,
            Message::Restart => {
                self.runtime = None;
                return self.start();
            }
            Message::Cancel => self.cancel.cancel(),
            Message::Tick => {
                self.prompt_text =
                    self.prompt.lock().ok().and_then(|p| p.as_ref().map(|p| p.summary.clone()))
            }
            Message::Answer(allow) => {
                if let Ok(mut prompt) = self.prompt.lock() {
                    if let Some(pending) = prompt.take() {
                        let _ = pending.response.send(if allow {
                            ApprovalDecision::Approve
                        } else {
                            ApprovalDecision::Deny
                        });
                    }
                }
                self.prompt_text = None;
            }
            Message::Ready(epoch, result) if epoch == self.epoch => {
                self.busy = false;
                match result {
                    Ok(runtime) => self.runtime = Some(runtime),
                    Err(error) => self.transcript.push_str(&format!("{error}\n")),
                }
            }
            Message::Completed(epoch, result) if epoch == self.epoch => {
                self.busy = false;
                self.prompt_text = None;
                if let Ok(mut prompt) = self.prompt.lock() {
                    *prompt = None;
                }
                self.transcript.push_str(&format!("{}\n", result.summary));
                if !result.data.is_null() {
                    self.transcript.push_str(&format!(
                        "{}\n",
                        serde_json::to_string_pretty(&result.data).unwrap_or_default()
                    ));
                }
                // Bound the retained display independently of process pipe capture.
                if self.transcript.len() > 2 * 1024 * 1024 {
                    let mut offset = self.transcript.len() - 1024 * 1024;
                    while !self.transcript.is_char_boundary(offset) {
                        offset += 1;
                    }
                    self.transcript.drain(..offset);
                }
            }
            Message::Submit if !self.busy && !self.input.trim().is_empty() => {
                if let Some(runtime) = self.runtime.clone() {
                    let line = std::mem::take(&mut self.input);
                    self.transcript.push_str("> command\n");
                    self.cancel = CancellationToken::new();
                    let cancel = self.cancel.clone();
                    let epoch = self.epoch;
                    self.busy = true;
                    return Task::perform(
                        async move { runtime.execute_line(&line, cancel).await },
                        move |result| Message::Completed(epoch, Box::new(result)),
                    );
                }
            }
            _ => {}
        }
        Task::none()
    }
    pub fn set_project_dir(&mut self, project_dir: PathBuf, _theme: &AppTheme) -> Task<Message> {
        if self.project_dir == project_dir {
            return Task::none();
        }
        self.project_dir = project_dir;
        self.runtime = None;
        self.transcript.clear();
        self.start()
    }
    pub fn set_theme(&mut self, _theme: &AppTheme) {}
    pub fn subscription(&self) -> Subscription<Message> {
        if self.busy {
            iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        }
    }
    pub fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let mut content = column![
            row![
                text("Concerto native shell").size(16),
                button("Restart").on_press(Message::Restart)
            ]
            .spacing(12),
            text(self.project_dir.to_string_lossy().into_owned())
                .size(12)
                .color(palette.text_muted),
            scrollable(text(&self.transcript).size(13)).height(Length::Fill),
        ]
        .spacing(10);
        if let Some(prompt) = &self.prompt_text {
            content = content.push(text(prompt).color(palette.warning)).push(
                row![
                    button("Approve this command").on_press(Message::Answer(true)),
                    button("Deny").on_press(Message::Answer(false)),
                ]
                .spacing(10),
            );
        }
        content = content.push(
            row![
                text_input("help · ls . · cat FILE · run PROGRAM ARG", &self.input)
                    .on_input(Message::Input)
                    .on_submit(Message::Submit)
                    .width(Length::Fill),
                button("Run").on_press_maybe(
                    (!self.busy && self.runtime.is_some()).then_some(Message::Submit)
                ),
                button("Cancel").on_press_maybe(self.busy.then_some(Message::Cancel)),
            ]
            .spacing(10),
        );
        container(content).padding(12).width(Length::Fill).height(Length::Fill).into()
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
