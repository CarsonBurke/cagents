//! Typed editor requests and workspace edits. Protocol ranges are converted
//! only against immutable snapshots; unopened targets retain UTF-16 tags.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspEditKind { Rename, Formatting, RangeFormatting, CodeAction }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LspServerEditId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LspWorkspaceEdit {
    pub documents: Vec<LspDocumentEdits>,
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDocumentEdits {
    pub path: PathBuf,
    /// The version supplied by the server, if any.
    pub version: Option<i32>,
    /// Open-buffer version used for conversion, including unversioned edits.
    pub observed_version: Option<i32>,
    pub coordinates: LspCoordinates,
    pub edits: Vec<LspTextEdit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspFormattingOptions { pub tab_size: u32, pub insert_spaces: bool }
impl Default for LspFormattingOptions {
    fn default()->Self {Self {tab_size:4,insert_spaces:true}}
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LspCapabilities {
    pub ready: bool,
    pub rename: bool,
    pub prepare_rename: bool,
    pub format_document: bool,
    pub format_selection: bool,
    pub code_actions: bool,
    pub code_action_resolve: bool,
    pub execute_commands: Vec<String>,
    pub signature_triggers: Vec<String>,
    pub signature_retriggers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LspCommand {
    pub title: String,
    pub command: String,
    pub arguments: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LspCodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub is_preferred: bool,
    pub disabled: Option<String>,
    pub edit: Option<LspWorkspaceEdit>,
    pub command: Option<LspCommand>,
    pub needs_resolve: bool,
    raw: lsp_types::CodeActionOrCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSignatureHelp {
    pub signatures: Vec<LspSignature>,
    pub active_signature: usize,
    pub active_parameter: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSignature {
    pub label: String,
    pub documentation: Option<String>,
    pub parameters: Vec<LspParameter>,
    pub active_parameter: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspParameter {
    pub label: String,
    /// UTF-8 bytes in the signature label, never UTF-16 code units.
    pub range: Option<Range<usize>>,
    pub documentation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspMessageLevel { Error, Warning, Info, Log }

#[derive(Debug, Clone, Default)]
pub(super) enum RequestArgs {
    #[default]
    None,
    Rename(String),
    Formatting(LspFormattingOptions),
    RangeFormatting {range:lsp_types::Range,options:LspFormattingOptions},
    CodeActions {range:lsp_types::Range,diagnostics:Vec<lsp_types::Diagnostic>},
    Resolve(lsp_types::CodeAction),
    Execute(LspCommand),
    Signature(Option<String>),
}

pub(super) enum RawFeatureResult {
    PrepareRename(Option<lsp_types::PrepareRenameResponse>),
    Rename(Option<lsp_types::WorkspaceEdit>),
    Formatting(Option<Vec<lsp_types::TextEdit>>),
    CodeActions(Option<lsp_types::CodeActionResponse>),
    CodeActionResolved(lsp_types::CodeAction),
    SignatureHelp(Option<lsp_types::SignatureHelp>),
    CommandExecuted(Option<serde_json::Value>),
}
