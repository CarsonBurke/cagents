//! Ordinary rendered Copy belongs to its displayed part, scope, and actor.
use super::*;
use chat::client::rendered_copy::Part;
use std::sync::Weak;

fn authority_part(part: &CopyPart) -> Part {
    match part {
        CopyPart::Input => Part::Input,
        CopyPart::Output => Part::Output,
        CopyPart::Text | CopyPart::Change(_) => Part::Text,
    }
}

/// Weak references never retain transcript bodies or actors. The part token
/// survives only publication-validated continuations, independently of the
/// bounded Markdown append history.
#[derive(Clone)]
pub struct Source {
    context: Option<String>,
    lease: Weak<()>,
    token: Weak<()>,
    part: CopyPart,
    handle: Weak<ClientHandle>,
    session: TerminalId,
    conversation: Option<String>,
}
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.context == other.context
            && self.lease.ptr_eq(&other.lease)
            && self.token.ptr_eq(&other.token)
            && self.part == other.part
            && self.handle.ptr_eq(&other.handle)
            && self.session == other.session
            && self.conversation == other.conversation
    }
}
impl Eq for Source {}
impl Source {
    pub(super) fn matches(&self, runtime: &Runtime, id: &str, part: &CopyPart) -> bool {
        &self.part == part
            && runtime.visible.load(Ordering::Relaxed)
            && std::ptr::eq(
                self.lease.as_ptr(),
                Arc::as_ptr(&runtime.rendered_copy_lease),
            )
            && runtime.selected_subagent == self.context
            && runtime.session.id == self.session
            && runtime
                .handle()
                .is_some_and(|handle| std::ptr::eq(self.handle.as_ptr(), Arc::as_ptr(handle)))
            && runtime.active_item(id).is_some()
            && runtime.presentation_cache().get(id).is_some_and(|cached| {
                !cached.placeholder
                    && cached.item.as_ref().is_some_and(|item| {
                        cached.copy_receipt.as_ref().is_some_and(|receipt| {
                            receipt.matches(item)
                                && receipt.token(authority_part(part)).ptr_eq(&self.token)
                        })
                    })
            })
            && self.is_current(id)
    }
    pub(super) fn is_current(&self, id: &str) -> bool {
        if self.lease.upgrade().is_none() {
            return false;
        }
        let Some(handle) = self.handle.upgrade() else {
            return false;
        };
        // One receiver publication owns both current scope and part tokens.
        // Rewrites revoke authority before UI wake; typed output appends do not.
        handle.rendered_copy_is_current(
            self.conversation.as_deref(),
            self.context.as_deref(),
            id,
            authority_part(&self.part),
            &self.token,
        )
    }
}
impl Runtime {
    pub(super) fn revoke_rendered_copy(&mut self) {
        self.rendered_copy_lease = Arc::new(());
    }
    pub(super) fn rendered_copy_source(&self, item: &chat::Item, part: CopyPart) -> Option<Source> {
        let cached = self.presentation_cache().get(&item.id)?;
        let shown = cached.item.as_ref()?;
        let receipt = cached.copy_receipt.as_ref()?;
        if cached.placeholder || !receipt.matches(shown) {
            return None;
        }
        let token = receipt.token(authority_part(&part));
        // The active item and shown part must share this exact publication
        // authority. An old displayed rewrite cannot acquire a current alias.
        let current = self
            .copy_owners
            .receipt(self.selected_subagent.as_deref(), item)?;
        if !current.token(authority_part(&part)).ptr_eq(&token) {
            return None;
        }
        let source = Source {
            context: self.selected_subagent.clone(),
            lease: Arc::downgrade(&self.rendered_copy_lease),
            token,
            part,
            handle: Arc::downgrade(self.handle()?),
            session: self.session.id,
            conversation: self.snapshot.session_id.clone(),
        };
        source.is_current(&item.id).then_some(source)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Code,
    Message,
    Report,
}

#[derive(Clone)]
struct SharedSource(Weak<str>);
impl PartialEq for SharedSource {
    fn eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
}
impl Eq for SharedSource {}

#[derive(Clone, PartialEq, Eq)]
enum Control {
    Rendered {
        target: Target,
        item: String,
        part: CopyPart,
        source: Option<Source>,
        role: Role,
    },
    Message {
        target: Target,
        item: String,
        // A verified growing prefix keeps its native press. Byte length is
        // deliberately excluded; the existing request validator owns it.
        source: Option<(uuid::Uuid, chat::TextCopyScope, u64, u64)>,
        role: Role,
    },
    SharedCode {
        target: Target,
        source: SharedSource,
        role: Role,
    },
}

pub(super) fn enabled(copy: &Action) -> bool {
    !matches!(copy, Action::Copy(_, _, _, None))
}

pub(super) fn owned<'a>(
    copy: &Action,
    role: Role,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let key = match copy {
        Action::Copy(target, item, part, source) => Control::Rendered {
            target: *target,
            item: item.clone(),
            part: part.clone(),
            source: source.clone(),
            role,
        },
        Action::CopyMessage(target, item, source) => Control::Message {
            target: *target,
            item: item.clone(),
            source: source.as_ref().map(|source| {
                (
                    source.cache,
                    source.scope.clone(),
                    source.token,
                    source.revision,
                )
            }),
            role,
        },
        Action::CopySharedCode(target, source) => Control::SharedCode {
            target: *target,
            source: SharedSource(Arc::downgrade(source)),
            role,
        },
        _ => return control.into(),
    };
    crate::ui_controls::owned(key, control)
}

pub(super) fn icon<'a>(
    icon: Icon,
    label: impl Into<Cow<'a, str>>,
    copy: Action,
    role: Role,
) -> Element<'a, Message> {
    owned(
        &copy,
        role,
        crate::ui_controls::icon_button(icon, label)
            .compact()
            .on_press_maybe(enabled(&copy).then(|| action(copy.clone())))
            // Preserve the ordinary message control's muted-at-rest tone.
            .tone(design::Tone::Themed(design::muted_icon_color)),
    )
}
