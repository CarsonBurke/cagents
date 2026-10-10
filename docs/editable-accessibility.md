# Editable field accessibility

The shell's shared single-line field and multiline form editor decorate the
existing COSMIC/Iced widgets. They preserve native layout, input, clipboard,
context menu, drag-and-drop, IME and undo handling. Accessibility actions are
handled as typed field edits or absolute cursor callbacks; they do not synthesize
keys, click a submit button or send a chat prompt.

Fields expose a name, bounds, enabled state and the appropriate text-input role.
Text-run children represent their current controlled value, with Unicode
boundaries and separate caret/selection positions. Retained field and line IDs
survive value changes and rebind when the field identity changes. Multiline
cursor columns remain the native editor's UTF-8 byte offsets; accessibility
positions are validated and converted before the app applies them.

Password fields publish password semantics and one mask per grapheme, retaining
length and selection without publishing the secret. Sensitive browser navigation
fields instead use `.private()`: no value, text-run children, length or selection
offsets are published. They retain a safe name, focus and whole-value replacement.
Sanitized browser-location descriptions belong to the separate browser surface.

The pinned Linux AccessKit adapter lacked `org.a11y.atspi.EditableText`. The two
small local adapter crates retain the exact pinned source and licenses; their
`PATCH.md` files explain the delta. `SetTextContents` dispatches a string
`SetValue` only to enabled, writable controls that advertise that action. Existing
AT-SPI Text caret/selection methods require the corresponding advertised action.
`InsertText`, `DeleteText`, clipboard and attribute-editing methods currently
return false. Ordinary keyboard and clipboard editing remain available through
the native widget. No numeric Value interface is used for string editing.

This covers Settings/search/dialog fields, permission answers, pull-request
descriptions, automation instructions/prechecks, chat composition and commit
forms. Arbitrary local/remote file-document editors remain a separate gap: they
need a document projection that avoids serializing large files on each tree
update and does not truncate offsets silently.

## Qualification

Source fixtures exercise mounted Unicode fields, secret/private non-disclosure,
the runtime focus-operation/event ordering, multiline selection and the adapter's
typed replacement/refusal. Compilation, these tests and native acceptance remain
pending until the reserved build lane is released.

Native acceptance must use the matched browserless development runner and an
owned private XDG/AT-SPI session. Check Settings search, a dialog, a multiline
form, secret permission input, ordinary typing/IME/undo and focus traversal. The
PID-pinned `scripts/probe-editable-accessibility` inspects one uniquely named
field and can exercise supported replacement without submitting. It prints no
text values. Use only disposable fixture values, including a known forbidden
secret for redaction checks. Successful GI imports or compilation alone do not
constitute native AT-SPI acceptance.
