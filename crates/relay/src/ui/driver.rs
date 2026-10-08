//! What a UI command targets, how it fails, and the driver the UI thread owns.

use serde::Serialize;

use super::ax::AxError;

/// The chat tab a command must select: its 1-based position among the workspace's open chats
/// (oldest first), how many there are, and its title when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub index: usize,
    pub count: usize,
    pub title: Option<String>,
}

/// Where a command acts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub workspace_id: String,
    /// The chat; `None` for a new chat.
    pub session_id: Option<String>,
    /// `repos.name`.
    pub repo: Option<String>,
    /// The workspace's branch; empty means it has none.
    pub branch: String,
    /// `workspaces.workspace_name`, the label of its sidebar link.
    pub workspace_name: Option<String>,
    /// `None`: no tab is selected (a new chat, or a chat that is the workspace's only one).
    pub tab: Option<Tab>,
}

impl Target {
    /// The part of the branch after its last `/`.
    pub fn branch_tail(&self) -> &str {
        self.branch.rsplit('/').next().unwrap_or("")
    }

    /// `conductor://workspace?id=<id>[&session=<id>]`, both values percent-encoded (every byte
    /// outside `A-Z a-z 0-9 - . _ ~`).
    pub fn deep_link(&self) -> String {
        let mut link = format!("conductor://workspace?id={}", encode(&self.workspace_id));
        if let Some(session_id) = &self.session_id {
            link.push_str("&session=");
            link.push_str(&encode(session_id));
        }
        link
    }
}

/// `conductor://` followed by `prompt=<enc>` and `path=<enc>` joined with `&`, each only when it
/// is given and not empty (no `?`). `<enc>` is JavaScript's `encodeURIComponent`: every UTF-8 byte
/// outside `A-Z a-z 0-9 - _ . ! ~ * ' ( )` as upper-case `%XX`.
pub fn create_link(prompt: Option<&str>, path: Option<&str>) -> String {
    let parts: Vec<String> = [("prompt", prompt), ("path", path)]
        .into_iter()
        .filter_map(|(name, value)| {
            value
                .filter(|value| !value.is_empty())
                .map(|value| format!("{name}={}", encode_component(value)))
        })
        .collect();
    format!("conductor://{}", parts.join("&"))
}

/// JavaScript's `encodeURIComponent`: every byte outside `A-Z a-z 0-9 - _ . ! ~ * ' ( )` as
/// `%XX`.
fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Percent-encodes every byte outside the unreserved set `A-Z a-z 0-9 - . _ ~`, as `%XX`.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// What the phone is told when a UI command fails. The texts are the contract.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UiError {
    #[error("the relay is not trusted for Accessibility - grant Conductor Remote access in System Settings > Privacy & Security > Accessibility, then try again")]
    NotTrusted,
    #[error("The Mac is locked - the lock screen hides Conductor from the relay, so nothing can be sent or pressed. Unlock the Mac and try again.")]
    Locked,
    #[error("Conductor is not running - open it on your Mac and try again.")]
    NotRunning,
    #[error(
        "Conductor is running but shows no window - open its window on your Mac and try again."
    )]
    NoWindow,
    #[error("Conductor stopped answering Accessibility reads - it looks wedged. Quit and reopen Conductor on your Mac, then try again.")]
    NotResponding,
    #[error("couldn't bring Conductor to the front - bring it forward on your Mac and try again.")]
    NotFrontmost,
    #[error("workspace has no branch to focus")]
    NoBranch,
    #[error("couldn't open {0} in Conductor - open the workspace on your Mac and try again.")]
    WorkspaceNotFocused(String),
    #[error("couldn't find the chat tab strip")]
    NoChatStrip,
    #[error("several chat tabs match {0}")]
    SeveralTabs(String),
    #[error("chat tab {0} not found")]
    TabNotFound(usize),
    #[error("couldn't switch to the target chat tab")]
    TabNotSelected,
    #[error("couldn't find the composer")]
    NoComposer,
    #[error("This question is no longer active in the target chat. Refresh and check Conductor.")]
    QuestionStale,
    #[error("Conductor did not accept these question answers. Check the choices and try again.")]
    QuestionInvalid,
    #[error("The answer was submitted, but confirmation is unavailable. Check Conductor before trying again.")]
    QuestionSubmissionUnknown,
    #[error("Conductor did not accept the prompt in its composer")]
    ComposerRejected,
    #[error("Conductor ignored Enter - the prompt is still sitting in its composer")]
    StillInComposer,
    #[error("couldn't find the model picker in Conductor's composer")]
    NoModelPicker,
    #[error("Conductor did not open its {0} menu")]
    MenuNotOpened(String),
    #[error("couldn't close Conductor's menu - close it on your Mac and try again.")]
    MenuStuck,
    #[error("Conductor's model list has no model named {0}")]
    NoModel(String),
    #[error("several models match {0} - use the full name")]
    SeveralModels(String),
    #[error("Conductor did not switch to {0}")]
    ModelNotApplied(String),
    #[error("this model has no {0} effort")]
    NoEffort(String),
    #[error("Conductor did not set the effort to {0}")]
    EffortNotApplied(String),
    #[error("this model has no Fast mode")]
    NoFast,
    #[error("Conductor did not change Fast mode")]
    FastNotApplied,
    #[error("this chat has no Plan mode")]
    NoPlan,
    #[error("Conductor did not change Plan mode")]
    PlanNotApplied,
    #[error("Conductor asks to confirm - an agent is still working")]
    NeedsConfirmation,
    #[error("couldn't dismiss Conductor's dialog - dismiss it on your Mac and try again.")]
    DialogStuck,
    #[error("couldn't find {0} in Conductor's sidebar")]
    NoSidebarRow(String),
    #[error("couldn't open the workspace's Set status menu")]
    NoStatusMenu,
    #[error("Conductor has no status named {0}")]
    NoStatus(String),
    #[error("Conductor shows no Continue button for this workspace - is its pull request merged?")]
    NoContinue,
    #[error("Conductor did not show its New workspace dialog")]
    NoCreateDialog,
    #[error("couldn't find the Run controls of this workspace")]
    NoRunStrip,
    #[error("Conductor has no Run task named {0}")]
    NoRunTask(String),
    #[error("Conductor did not start or stop the Run task")]
    RunNotChanged,
    #[error("couldn't press a key: {0}")]
    Key(String),
    #[error("Accessibility error: {0}")]
    Ax(AxError),
}

impl From<AxError> for UiError {
    /// `CannotComplete` → `NotResponding`, `ApiDisabled` → `NotTrusted`, anything else → `Ax`.
    fn from(error: AxError) -> UiError {
        match error {
            AxError::CannotComplete => UiError::NotResponding,
            AxError::ApiDisabled => UiError::NotTrusted,
            other => UiError::Ax(other),
        }
    }
}

impl UiError {
    /// The command certainly typed and pressed nothing: every variant except `Key`, `Ax`,
    /// `NotResponding` and `StillInComposer` (Return was pressed). A send that failed this way
    /// needs no confirm window.
    pub fn sent_nothing(&self) -> bool {
        !matches!(
            self,
            UiError::Key(_) | UiError::Ax(_) | UiError::NotResponding | UiError::StillInComposer
        )
    }

    /// Trying again within the same request cannot help: `NotTrusted`, `NotRunning`, `NoBranch`.
    pub fn retry_wont_help(&self) -> bool {
        matches!(
            self,
            UiError::NotTrusted | UiError::NotRunning | UiError::NoBranch
        )
    }

    /// The Mac is locked (`Locked`); its text starts with "The Mac is locked".
    pub fn is_lock(&self) -> bool {
        matches!(self, UiError::Locked)
    }
}

/// What `UiDriver::locate` sees in Conductor's window. Read-only; for `doctor`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewReport {
    /// The label of the main pane's header pop-up ("<repo> <branch> …").
    pub pane_header: Option<String>,
    /// How many chat tabs the pane shows.
    pub chat_tabs: usize,
    /// The 1-based position of the selected chat tab.
    pub selected_tab: Option<usize>,
    /// Whether the composer's text area was found.
    pub composer: bool,
}

/// What `UiDriver::set_agent` did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentOutcome {
    /// Conductor opened a new chat for the model (another provider's model, in a chat with messages).
    pub new_chat: bool,
}

/// Why `UiDriver::set_agent` failed, and whether a new chat had been opened by then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFailure {
    pub error: UiError,
    /// The model item that opens a new chat was pressed before the failure.
    pub new_chat: bool,
}

impl From<UiError> for AgentFailure {
    /// A failure before any new chat: `new_chat` is false.
    fn from(error: UiError) -> AgentFailure {
        AgentFailure {
            error,
            new_chat: false,
        }
    }
}

/// What `UiDriver::run_task` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOutcome {
    /// A button was pressed (the task was not already as asked).
    pub changed: bool,
    /// The task's name as the strip's button shows it, when one is known.
    pub task: Option<String>,
}

/// The commands the UI thread runs. Object-safe; `actions::Driver` implements it over any
/// `Desktop`. Not `Send`: it is made on the UI thread and stays there.
pub trait UiDriver {
    fn trusted(&self) -> bool;
    fn answer_questions(
        &mut self,
        _target: &Target,
        _request: &crate::transcript::questions::QuestionRequest,
        _answers: &[crate::transcript::questions::QuestionAnswer],
        _guard: &dyn Fn() -> bool,
    ) -> Result<(), UiError> {
        Err(UiError::QuestionStale)
    }
    /// Types `text` into the target chat's composer and submits it (Cmd+Return when `queue`).
    /// Returns which press emptied the composer (1 or 2).
    fn send_prompt(&mut self, target: &Target, text: &str, queue: bool) -> Result<u32, UiError>;
    fn stop_turn(&mut self, target: &Target) -> Result<(), UiError>;
    fn new_chat(&mut self, target: &Target) -> Result<(), UiError>;
    fn locate(&mut self) -> Result<ViewReport, UiError>;
    /// Opens a `conductor://` link (no window is read and nothing is typed). Fails with `Locked`,
    /// `NotTrusted` or `NotRunning` like the other commands; `Ax`-free.
    fn open_link(&mut self, url: &str) -> Result<(), UiError>;
    /// Opens the model menu of the target chat, reads the model names in menu order and closes it.
    fn list_models(&mut self, _target: &Target) -> Result<Vec<String>, UiError> {
        Err(UiError::NoModelPicker)
    }
    /// Applies `patch` to the target chat through Conductor's composer controls.
    fn set_agent(
        &mut self,
        _target: &Target,
        _patch: &crate::agent::AgentPatch,
    ) -> Result<AgentOutcome, AgentFailure> {
        Err(UiError::NoModelPicker.into())
    }
    /// Closes the target chat (Cmd+L, Cmd+W). When Conductor asks to confirm because the agent
    /// is running: `confirm` presses "Close anyway", otherwise the dialog is cancelled and the
    /// answer is `NeedsConfirmation`.
    fn close_chat(&mut self, _target: &Target, _confirm: bool) -> Result<(), UiError> {
        Err(UiError::NoWindow)
    }
    /// Sets the status of the workspace whose sidebar row is titled `row` to the menu item
    /// `label` (for example "In review").
    fn set_status(&mut self, _target: &Target, _row: &str, _label: &str) -> Result<(), UiError> {
        Err(UiError::NoWindow)
    }
    /// Archives the target workspace (Cmd+L, Cmd+Shift+A). When Conductor asks to confirm
    /// because agents are running: `confirm` presses "Stop agents and archive", otherwise the
    /// dialog is cancelled and the answer is `NeedsConfirmation`.
    fn archive(&mut self, _target: &Target, _confirm: bool) -> Result<(), UiError> {
        Err(UiError::NoWindow)
    }
    /// Presses the main pane's Continue button of the target workspace.
    fn press_continue(&mut self, _target: &Target) -> Result<(), UiError> {
        Err(UiError::NoWindow)
    }
    /// Presses Create in the New workspace dialog a `conductor://` create link opened.
    fn confirm_create(&mut self) -> Result<(), UiError> {
        Err(UiError::NoWindow)
    }
    /// Starts (`start`) or stops the target workspace's Run task with the Run strip's own
    /// buttons. `task` names the task to start (its display name); `None` is the strip's current one.
    fn run_task(
        &mut self,
        _target: &Target,
        _task: Option<&str>,
        _start: bool,
    ) -> Result<RunOutcome, UiError> {
        Err(UiError::NoWindow)
    }
}
