//! Preserve annotated edits before lsp-types' plain-first untagged union can
//! discard `annotationId`. Only wire decoding differs; conversion, version
//! checks, preview authority and atomic publication still use the same types.
use lsp_types::{self as lsp, request::Request};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::collections::HashMap;
use url::Url;

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(super) struct WorkspaceEdit(pub lsp::WorkspaceEdit);

#[derive(Default)]
struct AnnotationId(Option<String>);

impl<'de> Deserialize<'de> for AnnotationId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Missing is plain, but an explicitly null or malformed ID must not
        // fall back to TextEdit and silently lose the annotation.
        String::deserialize(deserializer).map(|id| Self(Some(id)))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextEdit {
    range: lsp::Range,
    new_text: String,
    #[serde(default)]
    annotation_id: AnnotationId,
}

impl TextEdit {
    fn into_lsp(self) -> lsp::OneOf<lsp::TextEdit, lsp::AnnotatedTextEdit> {
        let text_edit = lsp::TextEdit { range: self.range, new_text: self.new_text };
        match self.annotation_id.0 {
            Some(annotation_id) => lsp::OneOf::Right(lsp::AnnotatedTextEdit { text_edit, annotation_id }),
            None => lsp::OneOf::Left(text_edit),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentEdit {
    text_document: lsp::OptionalVersionedTextDocumentIdentifier,
    edits: Vec<TextEdit>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DocumentChange {
    Resource(lsp::ResourceOp),
    Text(DocumentEdit),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceFields {
    changes: Option<HashMap<Url, Vec<TextEdit>>>,
    document_changes: Option<Vec<DocumentChange>>,
    change_annotations: Option<HashMap<String, lsp::ChangeAnnotation>>,
}

impl<'de> Deserialize<'de> for WorkspaceEdit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = WorkspaceFields::deserialize(deserializer)?;
        let changes = fields.changes.map(|changes| {
            changes.into_iter().map(|(uri, edits)| {
                let edits = edits.into_iter().map(|edit| {
                    if edit.annotation_id.0.is_some() {
                        return Err(de::Error::custom("annotated edits require documentChanges"));
                    }
                    Ok(lsp::TextEdit { range: edit.range, new_text: edit.new_text })
                }).collect::<Result<Vec<_>, D::Error>>()?;
                Ok((uri, edits))
            }).collect::<Result<HashMap<_, _>, D::Error>>()
        }).transpose()?;
        let document_changes = fields.document_changes.map(|changes| {
            lsp::DocumentChanges::Operations(changes.into_iter().map(|change| match change {
                DocumentChange::Resource(operation) => lsp::DocumentChangeOperation::Op(operation),
                DocumentChange::Text(edit) => lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
                    text_document: edit.text_document,
                    edits: edit.edits.into_iter().map(TextEdit::into_lsp).collect(),
                }),
            }).collect())
        });
        Ok(Self(lsp::WorkspaceEdit { changes, document_changes, change_annotations: fields.change_annotations }))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionFields {
    title: String,
    kind: Option<lsp::CodeActionKind>,
    diagnostics: Option<Vec<lsp::Diagnostic>>,
    edit: Option<WorkspaceEdit>,
    command: Option<lsp::Command>,
    is_preferred: Option<bool>,
    disabled: Option<lsp::CodeActionDisabled>,
    data: Option<serde_json::Value>,
}

impl From<ActionFields> for lsp::CodeAction {
    fn from(fields: ActionFields) -> Self {
        Self { title: fields.title, kind: fields.kind, diagnostics: fields.diagnostics,
            edit: fields.edit.map(|edit| edit.0), command: fields.command,
            is_preferred: fields.is_preferred, disabled: fields.disabled, data: fields.data }
    }
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(super) struct CodeAction(pub lsp::CodeAction);

impl<'de> Deserialize<'de> for CodeAction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ActionFields::deserialize(deserializer).map(|fields| Self(fields.into()))
    }
}

#[derive(Default)]
struct NoWorkspaceEdit;

impl<'de> Deserialize<'de> for NoWorkspaceEdit {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(de::Error::custom("a command cannot discard a workspace edit"))
    }
}

#[derive(Deserialize)]
struct CommandFields {
    title: String,
    command: String,
    arguments: Option<Vec<serde_json::Value>>,
    // A malformed annotated action must not fall through to the command
    // variant, losing its edit. Unrelated extension fields remain tolerated.
    #[serde(default, rename = "edit")]
    _no_workspace_edit: NoWorkspaceEdit,
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(super) struct CodeActionOrCommand(pub lsp::CodeActionOrCommand);

impl<'de> Deserialize<'de> for CodeActionOrCommand {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Choice { Action(ActionFields), Command(CommandFields) }
        Ok(Self(match Choice::deserialize(deserializer)? {
            Choice::Action(fields) => lsp::CodeActionOrCommand::CodeAction(fields.into()),
            Choice::Command(fields) => lsp::CodeActionOrCommand::Command(lsp::Command {
                title: fields.title, command: fields.command, arguments: fields.arguments,
            }),
        }))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ApplyWorkspaceEditParams {
    pub label: Option<String>,
    pub edit: WorkspaceEdit,
}

impl From<ApplyWorkspaceEditParams> for lsp::ApplyWorkspaceEditParams {
    fn from(params: ApplyWorkspaceEditParams) -> Self {
        Self { label: params.label, edit: params.edit.0 }
    }
}

macro_rules! response_request {
    ($name:ident, $original:ty, $result:ty) => {
        pub(super) enum $name {}
        impl Request for $name {
            type Params = <$original as Request>::Params;
            type Result = $result;
            const METHOD: &'static str = <$original as Request>::METHOD;
        }
    };
}
response_request!(Rename, lsp::request::Rename, Option<WorkspaceEdit>);
response_request!(CodeActions, lsp::request::CodeActionRequest, Option<Vec<CodeActionOrCommand>>);
response_request!(ResolveCodeAction, lsp::request::CodeActionResolveRequest, CodeAction);

pub(super) enum ApplyWorkspaceEdit {}
impl Request for ApplyWorkspaceEdit {
    type Params = ApplyWorkspaceEditParams;
    type Result = lsp::ApplyWorkspaceEditResponse;
    const METHOD: &'static str = <lsp::request::ApplyWorkspaceEdit as Request>::METHOD;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn annotated() -> Value {
        json!({"changeAnnotations":{"confirm":{"label":"Rename export","needsConfirmation":true}},
            "documentChanges":[{"textDocument":{"uri":"file:///repo/a.rs","version":1},
                "edits":[{"annotationId":"confirm","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"bar"}]}]})
    }

    fn annotation(edit: &lsp::WorkspaceEdit) -> &lsp::AnnotatedTextEdit {
        let Some(lsp::DocumentChanges::Operations(items)) = &edit.document_changes else { panic!("missing document changes"); };
        let lsp::DocumentChangeOperation::Edit(document) = &items[0] else { panic!("expected text edit"); };
        let lsp::OneOf::Right(edit) = &document.edits[0] else { panic!("annotation was discarded"); };
        edit
    }

    #[test]
    fn annotated_text_edits_survive_wire_decoding_and_serialized_round_trip() {
        let edit: WorkspaceEdit = serde_json::from_value(annotated()).unwrap();
        assert_eq!(annotation(&edit.0).annotation_id, "confirm");
        assert_eq!(edit.0.change_annotations.as_ref().unwrap()["confirm"].needs_confirmation, Some(true));
        let again: WorkspaceEdit = serde_json::from_value(serde_json::to_value(&edit).unwrap()).unwrap();
        assert_eq!(edit.0, again.0);
        let plain: WorkspaceEdit = serde_json::from_value(json!({"changes":{"file:///repo/a.rs":[
            {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"bar"}]}})).unwrap();
        assert_eq!(plain.0.changes.unwrap().len(), 1);
        let resource: WorkspaceEdit = serde_json::from_value(json!({"documentChanges":[{"kind":"create","uri":"file:///repo/new.rs"}]})).unwrap();
        assert!(matches!(&resource.0.document_changes.unwrap(), lsp::DocumentChanges::Operations(items) if matches!(items[0],lsp::DocumentChangeOperation::Op(_))));
    }

    #[test]
    fn invalid_annotation_ids_cannot_fall_back_to_plain_text_edits() {
        for invalid in [Value::Null, json!(7), json!(true), json!({"id":"confirm"})] {
            let mut value = annotated();
            value["documentChanges"][0]["edits"][0]["annotationId"] = invalid;
            assert!(serde_json::from_value::<WorkspaceEdit>(value).is_err());
        }
        let mut invalid = annotated();
        invalid["changes"] = json!({"file:///repo/a.rs":[invalid["documentChanges"][0]["edits"][0].clone()]});
        assert!(serde_json::from_value::<WorkspaceEdit>(invalid).is_err(), "changes cannot encode annotated edits");
    }

    #[test]
    fn action_and_resolve_decoders_preserve_annotated_edits_and_opaque_data() {
        let value = json!({"title":"Fix","edit":annotated(),"data":{"opaque":[7,"kept"]}});
        let choice: CodeActionOrCommand = serde_json::from_value(value.clone()).unwrap();
        let lsp::CodeActionOrCommand::CodeAction(action) = choice.0 else { panic!("expected action"); };
        assert_eq!(annotation(action.edit.as_ref().unwrap()).annotation_id, "confirm");
        assert_eq!(action.data, Some(json!({"opaque":[7,"kept"]})));
        let resolved: CodeAction = serde_json::from_value(value).unwrap();
        assert_eq!(annotation(resolved.0.edit.as_ref().unwrap()).annotation_id, "confirm");
        let command: CodeActionOrCommand = serde_json::from_value(json!({"title":"Run","command":"fixture.run","arguments":[7],"extension":"allowed"})).unwrap();
        assert!(matches!(command.0,lsp::CodeActionOrCommand::Command(command) if command.arguments==Some(vec![json!(7)])));
        let mut invalid = json!({"title":"Do not discard","command":"fixture.run","edit":annotated()});
        invalid["edit"]["documentChanges"][0]["edits"][0]["annotationId"] = Value::Null;
        assert!(serde_json::from_value::<CodeActionOrCommand>(invalid).is_err());
    }

    #[test]
    fn apply_edit_request_decodes_the_same_annotation_preserving_workspace_type() {
        let params: ApplyWorkspaceEditParams = serde_json::from_value(json!({"label":"Server fix","edit":annotated()})).unwrap();
        let params: lsp::ApplyWorkspaceEditParams = params.into();
        assert_eq!(params.label.as_deref(),Some("Server fix"));
        assert_eq!(annotation(&params.edit).annotation_id,"confirm");
    }
}
