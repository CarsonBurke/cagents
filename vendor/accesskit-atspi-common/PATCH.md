Source: wash2/accesskit cosmic-0.14, commit f0599eed5f18111228266fe3f28991cc48b5964f.

Local patch adds AT-SPI EditableText to writable text controls explicitly supporting string SetValue. The supported method is SetTextContents, dispatched as Action::SetValue with string data. InsertText, DeleteText, clipboard and attribute editing explicitly return false; this patch does not claim those operations are implemented. The existing Text interface continues to provide caret and selection operations when the control advertises SetTextSelection.

The two crate manifests are standalone copies of the pinned workspace manifests; the common/consumer types stay at the exact same Git tag and lock revision. Upstream license files are retained. Remove this patch when the pinned Linux adapter provides equivalent string replacement. No ABI or wire protocol change is introduced.
