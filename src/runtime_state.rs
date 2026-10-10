//! Thread-local storage shared by the ordinary library and the native hot root.
//!
//! Keep these accessors concrete and out of line: an executable's TLS references
//! cannot be linked into cargo-hot's shared patch. Generic or inlined accessors
//! would put those references back into the patchable root. Application state
//! types stay in their own root; only dependency-owned Markdown items cross here.

use std::cell::Cell;

thread_local! {
    static RECOVERED_PANIC_DEPTH: Cell<u32> = const { Cell::new(0) };
    static REPORTING_PANIC: Cell<bool> = const { Cell::new(false) };
}

#[must_use = "the scope lasts until this guard is dropped"]
pub struct RecoveredPanicScope(std::marker::PhantomData<std::rc::Rc<()>>);

impl Drop for RecoveredPanicScope {
    #[inline(never)]
    fn drop(&mut self) {
        let _ = RECOVERED_PANIC_DEPTH.try_with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

#[inline(never)]
pub fn recovered_panic_scope() -> RecoveredPanicScope {
    let _ = RECOVERED_PANIC_DEPTH.try_with(|depth| depth.set(depth.get().saturating_add(1)));
    RecoveredPanicScope(std::marker::PhantomData)
}

/// Starts one report, refusing a recursive panic on this thread.
#[inline(never)]
pub fn begin_panic_report() -> Option<bool> {
    if REPORTING_PANIC
        .try_with(|reporting| reporting.replace(true))
        .unwrap_or(false)
    {
        return None;
    }
    Some(RECOVERED_PANIC_DEPTH.try_with(Cell::get).unwrap_or(0) > 0)
}

#[inline(never)]
pub fn finish_panic_report() {
    let _ = REPORTING_PANIC.try_with(|reporting| reporting.set(false));
}

#[cfg(feature = "integrated-browser")]
thread_local! {
    static IS_BROWSER_ENGINE_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// The browser engine cannot synchronously wait for its own shutdown.
#[cfg(feature = "integrated-browser")]
#[inline(never)]
pub fn on_browser_engine_thread() -> bool {
    IS_BROWSER_ENGINE_THREAD.with(Cell::get)
}

#[cfg(feature = "integrated-browser")]
#[inline(never)]
pub fn enter_browser_engine_thread() {
    IS_BROWSER_ENGINE_THREAD.with(|flag| flag.set(true));
}

#[cfg(feature = "ui")]
thread_local! {
    // One inspector hover is visible at a time. Retain only its latest parse,
    // and apply theme-dependent rendering in the caller on every view.
    static INSPECTOR_MARKDOWN: std::cell::RefCell<Option<(String, Vec<cosmic::widget::markdown::Item>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "ui")]
#[inline(never)]
pub fn with_inspector_markdown(
    source: &str,
    render: &mut dyn FnMut(&[cosmic::widget::markdown::Item]),
) {
    INSPECTOR_MARKDOWN.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache
            .as_ref()
            .is_none_or(|(previous, _)| previous != source)
        {
            *cache = Some((
                source.to_owned(),
                cosmic::widget::markdown::parse(source).collect(),
            ));
        }
        render(&cache.as_ref().unwrap().1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovered_scopes_and_recursive_reporting_are_thread_local() {
        let outer = recovered_panic_scope();
        let inner = recovered_panic_scope();
        assert_eq!(begin_panic_report(), Some(true));
        assert_eq!(begin_panic_report(), None);
        finish_panic_report();
        drop(inner);
        assert_eq!(begin_panic_report(), Some(true));
        finish_panic_report();
        std::thread::spawn(|| {
            assert_eq!(begin_panic_report(), Some(false));
            finish_panic_report();
        })
        .join()
        .unwrap();
        drop(outer);
        assert_eq!(begin_panic_report(), Some(false));
        finish_panic_report();
    }

    #[cfg(feature = "integrated-browser")]
    #[test]
    fn browser_self_shutdown_guard_is_owned_by_its_thread() {
        assert!(!on_browser_engine_thread());
        enter_browser_engine_thread();
        assert!(on_browser_engine_thread());
        std::thread::spawn(|| {
            assert!(!on_browser_engine_thread());
            enter_browser_engine_thread();
            assert!(on_browser_engine_thread());
        })
        .join()
        .unwrap();
        assert!(on_browser_engine_thread());
    }

    #[cfg(feature = "ui")]
    #[test]
    fn inspector_hover_reuses_equal_sources_without_cross_thread_contamination() {
        let source = "First hover paragraph";
        let mut first = std::ptr::null();
        with_inspector_markdown(source, &mut |items| {
            assert_eq!(items.len(), 1);
            first = items.as_ptr();
        });
        std::thread::spawn(|| {
            with_inspector_markdown("Another hover\n\nSecond paragraph", &mut |items| {
                assert_eq!(items.len(), 2);
            });
        })
        .join()
        .unwrap();
        with_inspector_markdown(&source.to_owned(), &mut |items| {
            assert_eq!(
                items.as_ptr(),
                first,
                "unchanged hover must retain its parse"
            );
        });
        with_inspector_markdown("Changed hover\n\nSecond paragraph", &mut |items| {
            assert_eq!(
                items.len(),
                2,
                "a changed hover must replace cached content"
            );
        });
    }
}
