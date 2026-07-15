//! A thin wrapper widget that enables the OS input method editor (IME) so
//! CJK and other composed-text input works inside the terminal. Without this,
//! iced disables IME by default — only `text_input` widgets request it.
//!
//! `TermIme` wraps the terminal content (`mouse_area` + `canvas`) and, in its
//! `update` handler, calls `shell.request_input_method` to keep IME active.
//! When the IME commits composed text (e.g. a Chinese character), it publishes
//! an `ImeCommit` message; the app forwards the text to the session.

use iced::advanced::widget::{Tree, Operation};
use iced::advanced::{
    Clipboard, Layout, Shell, input_method, mouse, overlay, renderer,
};
use iced::{Element, Event, Length, Rectangle, Size, Vector, Point};

type OnIme<'a, Message> = Box<dyn Fn(String) -> Message + 'a>;

/// A widget that enables IME for the wrapped terminal content.
/// The IME composition window follows the terminal cursor position.
pub struct TermIme<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    on_ime: Option<OnIme<'a, Message>>,
    /// Terminal cursor position in widget-local pixel coordinates.
    cursor_pos: Point,
    /// Height of one terminal row in pixels (for the IME cursor area height).
    cell_h: f32,
}

impl<'a, Message, Theme, Renderer> TermIme<'a, Message, Theme, Renderer> {
    pub fn new(content: Element<'a, Message, Theme, Renderer>) -> Self {
        Self {
            content,
            on_ime: None,
            cursor_pos: Point::new(0.0, 0.0),
            cell_h: 19.0,
        }
    }

    /// Set the callback that fires when IME commits text.
    pub fn on_ime(mut self, f: impl Fn(String) -> Message + 'a) -> Self {
        self.on_ime = Some(Box::new(f));
        self
    }

    /// Set the terminal cursor position (widget-local pixel coords) and row
    /// height so the IME composition window sits at the text cursor.
    pub fn cursor(mut self, pos: Point, cell_h: f32) -> Self {
        self.cursor_pos = pos;
        self.cell_h = cell_h;
        self
    }
}

impl<'a, Message: 'a, Theme, Renderer> iced::advanced::widget::Widget<Message, Theme, Renderer>
    for TermIme<'a, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &iced::advanced::layout::Limits,
    ) -> iced::advanced::layout::Node {
        self.content.as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            limits,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        // Preserve the child tree to avoid resetting the inner widget's
        // state on every view rebuild. Clearing and recreating triggers
        // repeated layout invalidation (iced warns after 3 consecutive).
        if tree.children.len() == 1 {
            self.content.as_widget().diff(&mut tree.children[0]);
        } else {
            tree.children.clear();
            tree.children.push(Tree::new(&self.content));
        }
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            renderer,
            operation,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Compute the IME cursor area in window coordinates: widget-local
        // cursor position offset by the widget's position within the window.
        let bounds = layout.bounds();
        let ime: input_method::InputMethod = input_method::InputMethod::Enabled {
            cursor: Rectangle::new(
                Point::new(bounds.x + self.cursor_pos.x, bounds.y + self.cursor_pos.y),
                Size::new(1.0, self.cell_h),
            ),
            purpose: input_method::Purpose::Terminal,
            preedit: None,
        };
        shell.request_input_method(&ime);

        // Intercept IME commit events.
        if let Event::InputMethod(input_method::Event::Commit(text)) = event {
            if let Some(on_ime) = &self.on_ime {
                shell.publish(on_ime(text.clone()));
            }
            shell.capture_event();
            return;
        }

        // Delegate all other events to the inner widget.
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            _clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<TermIme<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Renderer: iced::advanced::Renderer + 'a,
    Theme: 'a,
{
    fn from(widget: TermIme<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}
