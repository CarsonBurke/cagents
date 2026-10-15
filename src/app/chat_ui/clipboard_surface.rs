//! Native clipboard writes supersede older async copies synchronously.
//! The scope owns no body, focus or named widget ID. It also wraps overlays:
//! their updates are driven directly by UserInterface outside root.update.
use super::*;
use super::clipboard_intent::Intents;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree}};
use iced::{Event, Rectangle, Size, Vector};
use iced::clipboard::{dnd, mime};

pub(super) fn surface<'a>(content: Element<'a, Message>, intents: Intents) -> Element<'a, Message> {
    Element::new(Scope { content, intents })
}
struct Scope<'a> {content: Element<'a, Message>, intents: Intents}
struct ScopeState;
struct Proxy<'a> {inner: &'a mut dyn Clipboard, intents: &'a Intents}
impl Clipboard for Proxy<'_> {
    fn read(&self, kind: iced::advanced::clipboard::Kind) -> Option<String> {self.inner.read(kind)}
    fn write(&mut self, kind: iced::advanced::clipboard::Kind, contents: String) {
        // Async chat requests write Standard. Primary selection updates are a
        // separate clipboard and must not silently cancel that explicit copy.
        if kind == iced::advanced::clipboard::Kind::Standard {self.intents.next();}
        self.inner.write(kind, contents);
    }
    fn read_data(&self, kind: iced::advanced::clipboard::Kind, mimes: Vec<String>) -> Option<(Vec<u8>,String)> {
        self.inner.read_data(kind,mimes)
    }
    fn write_data(&mut self, kind: iced::advanced::clipboard::Kind,
        contents: mime::ClipboardStoreData<Box<dyn Send+Sync+'static+mime::AsMimeTypes>>) {
        if kind == iced::advanced::clipboard::Kind::Standard {self.intents.next();}
        self.inner.write_data(kind,contents);
    }
    fn register_dnd_destination(&self, surface: dnd::DndSurface, rectangles: Vec<dnd::DndDestinationRectangle>) {
        self.inner.register_dnd_destination(surface,rectangles);
    }
    fn set_action(&self, action:dnd::DndAction) {self.inner.set_action(action);}
    fn start_dnd(&mut self, internal:bool, source:Option<iced::advanced::clipboard::DndSource>,
        icon:Option<iced::advanced::clipboard::DynIconSurface>, content:Box<dyn mime::AsMimeTypes+Send+'static>, actions:dnd::DndAction) {
        self.inner.start_dnd(internal,source,icon,content,actions);
    }
    fn end_dnd(&self) {self.inner.end_dnd();}
    fn peek_dnd(&self, mime:String)->Option<(Vec<u8>,String)> {self.inner.peek_dnd(mime)}
    fn request_logical_window_size(&self,width:f32,height:f32) {self.inner.request_logical_window_size(width,height);}
}
impl Widget<Message,cosmic::Theme,cosmic::Renderer> for Scope<'_> {
    fn tag(&self)->tree::Tag {tree::Tag::of::<ScopeState>()}
    fn state(&self)->tree::State {tree::State::new(ScopeState)}
    fn children(&self)->Vec<Tree> {vec![Tree::new(&self.content)]}
    fn diff(&mut self, tree:&mut Tree) {tree.diff_children(std::slice::from_mut(&mut self.content));}
    fn size(&self)->Size<Length> {self.content.as_widget().size()}
    fn size_hint(&self)->Size<Length> {self.content.as_widget().size_hint()}
    fn layout(&mut self, tree:&mut Tree, renderer:&cosmic::Renderer, limits:&layout::Limits)->layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0],renderer,limits)
    }
    fn a11y_nodes(&self, layout:Layout<'_>, tree:&Tree, cursor:mouse::Cursor)->iced_accessibility::A11yTree {
        self.content.as_widget().a11y_nodes(layout,&tree.children[0],cursor)
    }
    fn drag_destinations(&self, tree:&Tree, layout:Layout<'_>, renderer:&cosmic::Renderer,
        rectangles:&mut iced::advanced::clipboard::DndDestinationRectangles) {
        self.content.as_widget().drag_destinations(&tree.children[0],layout,renderer,rectangles);
    }
    fn operate(&mut self, tree:&mut Tree, layout:Layout<'_>, renderer:&cosmic::Renderer, operation:&mut dyn Operation) {
        self.content.as_widget_mut().operate(&mut tree.children[0],layout,renderer,operation);
    }
    fn update(&mut self, tree:&mut Tree,event:&Event,layout:Layout<'_>,cursor:mouse::Cursor,renderer:&cosmic::Renderer,
        clipboard:&mut dyn Clipboard,shell:&mut Shell<'_,Message>,viewport:&Rectangle) {
        self.content.as_widget_mut().update(&mut tree.children[0],event,layout,cursor,renderer,
            &mut Proxy {inner:clipboard,intents:&self.intents},shell,viewport);
    }
    fn draw(&self,tree:&Tree,renderer:&mut cosmic::Renderer,theme:&cosmic::Theme,style:&renderer::Style,
        layout:Layout<'_>,cursor:mouse::Cursor,viewport:&Rectangle) {
        self.content.as_widget().draw(&tree.children[0],renderer,theme,style,layout,cursor,viewport);
    }
    fn mouse_interaction(&self,tree:&Tree,layout:Layout<'_>,cursor:mouse::Cursor,viewport:&Rectangle,renderer:&cosmic::Renderer)->mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0],layout,cursor,viewport,renderer)
    }
    fn overlay<'a>(&'a mut self,tree:&'a mut Tree,layout:Layout<'a>,renderer:&cosmic::Renderer,viewport:&Rectangle,translation:Vector)
        ->Option<overlay::Element<'a,Message,cosmic::Theme,cosmic::Renderer>> {
        let intents=self.intents.clone();
        self.content.as_widget_mut().overlay(&mut tree.children[0],layout,renderer,viewport,translation)
            .map(|content|overlay::Element::new(Box::new(OverlayScope {content,intents})))
    }
}
struct OverlayScope<'a> {content:overlay::Element<'a,Message,cosmic::Theme,cosmic::Renderer>,intents:Intents}
impl overlay::Overlay<Message,cosmic::Theme,cosmic::Renderer> for OverlayScope<'_> {
    fn layout(&mut self,renderer:&cosmic::Renderer,bounds:Size)->layout::Node {self.content.as_overlay_mut().layout(renderer,bounds)}
    fn draw(&self,renderer:&mut cosmic::Renderer,theme:&cosmic::Theme,style:&renderer::Style,layout:Layout<'_>,cursor:mouse::Cursor) {
        self.content.as_overlay().draw(renderer,theme,style,layout,cursor);
    }
    fn operate(&mut self,layout:Layout<'_>,renderer:&cosmic::Renderer,operation:&mut dyn Operation) {
        self.content.as_overlay_mut().operate(layout,renderer,operation);
    }
    fn update(&mut self,event:&Event,layout:Layout<'_>,cursor:mouse::Cursor,renderer:&cosmic::Renderer,clipboard:&mut dyn Clipboard,shell:&mut Shell<'_,Message>) {
        self.content.as_overlay_mut().update(event,layout,cursor,renderer,&mut Proxy {inner:clipboard,intents:&self.intents},shell);
    }
    fn mouse_interaction(&self,layout:Layout<'_>,cursor:mouse::Cursor,renderer:&cosmic::Renderer)->mouse::Interaction {
        self.content.as_overlay().mouse_interaction(layout,cursor,renderer)
    }
    fn overlay<'a>(&'a mut self,layout:Layout<'a>,renderer:&cosmic::Renderer)->Option<overlay::Element<'a,Message,cosmic::Theme,cosmic::Renderer>> {
        let intents=self.intents.clone();
        self.content.as_overlay_mut().overlay(layout,renderer)
            .map(|content|overlay::Element::new(Box::new(OverlayScope {content,intents})))
    }
    fn index(&self)->f32 {self.content.as_overlay().index()}
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::clipboard_intent::{Intent,Pending};
    use iced::advanced::clipboard::Kind;
    use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
    #[derive(Default)]
    struct NativeClipboard {text:String,stale_before_native_write:Option<Intent>}
    impl Clipboard for NativeClipboard {
        fn read(&self,_:Kind)->Option<String> {Some(self.text.clone())}
        fn write(&mut self,kind:Kind,text:String) {
            if kind==Kind::Standard {
                if let Some(old)=&self.stale_before_native_write {assert!(!old.current(),"invalidate before the actual native write, not a later Action");}
                self.text=text;
            }
        }
        fn read_data(&self,_:Kind,_:Vec<String>)->Option<(Vec<u8>,String)> {Some((b"read forwarded".to_vec(),"test/type".into()))}
        fn peek_dnd(&self,_:String)->Option<(Vec<u8>,String)> {Some((b"drag forwarded".to_vec(),"test/drag".into()))}
    }
    struct HeldInput(AtomicBool);
    impl Pending for HeldInput {fn withdraw(&self) {self.0.store(true,Ordering::Release);}}
    fn copy_key()->Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key:keyboard::Key::Character("c".into()),modified_key:keyboard::Key::Character("c".into()),
            physical_key:keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location:keyboard::Location::Standard,modifiers:keyboard::Modifiers::CTRL,text:None,repeat:false,
        })
    }
    #[test]
    fn actual_native_selection_copy_invalidates_a_held_task_before_late_ready() {
        const CHILD:&str="CAGENTS_TEST_NATIVE_COPY_INTENT";
        if std::env::var_os(CHILD).is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact",concat!("app::chat_ui::clipboard_surface::tests::",
                    "actual_native_selection_copy_invalidates_a_held_task_before_late_ready"),"--nocapture"])
                .env(CHILD,"1").output().unwrap();
            assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));return;
        }
        use iced::futures::{FutureExt,StreamExt};
        let renderer=crate::app::tests::headless_renderer();
        let intents=Intents::default();let older=intents.next();
        let input=Arc::new(HeldInput(AtomicBool::new(false)));older.register(&input);
        let (release,held)=tokio::sync::oneshot::channel::<()>();
        let late=older.clone();
        let task=iced::Task::perform(async move {held.await.unwrap();(late,"older asynchronous copy".to_owned())},|ready|ready);
        let mut outputs=iced::runtime::task::into_stream(task).unwrap();
        assert!(outputs.next().now_or_never().is_none(),"the real Task is held before native copy");
        let target=Target {card:CardId::new(),generation:1};
        let selected="new native selected text";
        let text=super::super::selectable_prose::plain(selected,16.0,iced::Font::default(),target);
        let mut element=surface(super::super::selection::transcript(text,target,None),intents.clone());
        let mut tree=Tree::new(&element);
        let viewport=Rectangle::with_size(Size::new(400.0,80.0));
        let limits=layout::Limits::new(Size::ZERO,viewport.size());
        let mut node=element.as_widget_mut().layout(&mut tree,&renderer,&limits);
        let start=iced::Point::new(0.0,3.0);let end=iced::Point::new(350.0,3.0);
        let mut clipboard=NativeClipboard {text:"old clipboard".into(),stale_before_native_write:Some(older.clone())};
        for (event,cursor) in [
            (Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),mouse::Cursor::Available(start)),
            (Event::Mouse(mouse::Event::CursorMoved {position:end}),mouse::Cursor::Available(end)),
            (Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),mouse::Cursor::Available(end)),
            (copy_key(),mouse::Cursor::Unavailable),
        ] {
            let mut messages=Vec::new();let mut shell=Shell::new(&mut messages);
            element.as_widget_mut().update(&mut tree,&event,Layout::new(&node),cursor,&renderer,&mut clipboard,&mut shell,&viewport);
            assert!(shell.is_event_captured());
            if shell.is_layout_invalid() {node=element.as_widget_mut().layout(&mut tree,&renderer,&limits);}
        }
        assert_eq!(clipboard.text,selected);assert!(input.0.load(Ordering::Acquire));assert!(!older.current());
        release.send(()).unwrap();
        let executor=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let Some(iced::runtime::Action::Output((intent,text)))=executor.block_on(outputs.next()) else {panic!("held Task must complete");};
        // This is the shared intent guard used by async consumers. Their full
        // Cagents Ready handlers are qualified separately in the caller union.
        assert!(!intent.current());if intent.current() {clipboard.write(Kind::Standard,text);}
        assert_eq!(clipboard.text,selected,"late async output cannot overwrite native selection");
    }
    #[test]
    fn primary_and_read_paths_preserve_standard_intent_and_forward_data() {
        let intents=Intents::default();let pending=intents.next();let mut clipboard=NativeClipboard::default();
        let mut proxy=Proxy {inner:&mut clipboard,intents:&intents};
        proxy.write(Kind::Primary,"automatic selection".into());assert!(pending.current());
        assert_eq!(proxy.read_data(Kind::Standard,vec!["test/type".into()]).unwrap().0,b"read forwarded");
        assert_eq!(proxy.peek_dnd("test/drag".into()).unwrap().0,b"drag forwarded");
        proxy.write(Kind::Standard,"new explicit native copy".into());assert!(!pending.current());
        let current=intents.next();drop(proxy);
        clipboard.write(Kind::Standard,"current programmatic result".into());
        assert!(current.current(),"programmatic writes outside native update do not invalidate themselves");
    }
    struct CopyOverlay {nested:bool}
    impl overlay::Overlay<Message,cosmic::Theme,cosmic::Renderer> for CopyOverlay {
        fn layout(&mut self,_:&cosmic::Renderer,_:Size)->layout::Node {layout::Node::new(Size::new(80.0,20.0))}
        fn draw(&self,_:&mut cosmic::Renderer,_:&cosmic::Theme,_:&renderer::Style,_:Layout<'_>,_:mouse::Cursor) {}
        fn update(&mut self,event:&Event,_:Layout<'_>,_:mouse::Cursor,_:&cosmic::Renderer,clipboard:&mut dyn Clipboard,_:&mut Shell<'_,Message>) {
            if matches!(event,Event::Keyboard(_)) {clipboard.write(Kind::Standard,"nested native overlay copy".into());}
        }
        fn overlay<'a>(&'a mut self,_:Layout<'a>,_:&cosmic::Renderer)->Option<overlay::Element<'a,Message,cosmic::Theme,cosmic::Renderer>> {
            self.nested.then(||overlay::Element::new(Box::new(Self {nested:false})))
        }
        fn index(&self)->f32 {7.0}
    }
    #[test]
    fn nested_overlays_keep_the_same_synchronous_clipboard_scope() {
        let renderer=crate::app::tests::headless_renderer();let intents=Intents::default();let old=intents.next();
        let mut overlay=OverlayScope {content:overlay::Element::new(Box::new(CopyOverlay {nested:true})),intents};
        assert_eq!(overlay::Overlay::index(&overlay),7.0);
        let node=overlay::Overlay::layout(&mut overlay,&renderer,Size::new(80.0,20.0));
        let mut nested=overlay::Overlay::overlay(&mut overlay,Layout::new(&node),&renderer).unwrap();
        let mut clipboard=NativeClipboard {stale_before_native_write:Some(old.clone()),..Default::default()};
        let mut messages=Vec::new();
        nested.as_overlay_mut().update(&copy_key(),Layout::new(&node),mouse::Cursor::Unavailable,&renderer,&mut clipboard,&mut Shell::new(&mut messages));
        assert!(!old.current());assert_eq!(clipboard.text,"nested native overlay copy");
    }
}
