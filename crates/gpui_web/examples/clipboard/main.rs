use gpui::prelude::*;
use gpui::{
    actions, App, Bounds, ClipboardItem, Context, ElementId, FocusHandle, Focusable, KeyBinding,
    SharedString, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};

const BG_BASE: u32 = 0x1e1e2e;
const BG_SURFACE: u32 = 0x313244;
const BG_OVERLAY: u32 = 0x45475a;
const TEXT_PRIMARY: u32 = 0xcdd6f4;
const TEXT_SECONDARY: u32 = 0xa6adc8;
const TEXT_DIM: u32 = 0x6c7086;
const ACCENT_GREEN: u32 = 0xa6e3a1;
const ACCENT_BLUE: u32 = 0x89b4fa;
const ACCENT_RED: u32 = 0xf38ba8;

const INITIAL_TEXTS: [&str; 3] = [
    "The quick brown fox jumps over the lazy dog.",
    "Hello from GPUI Web!",
    "Try pasting text from another application.",
];

actions!(clipboard_demo, [Copy, Paste, Cut]);

struct ClipboardDemo {
    focus_handle: FocusHandle,
    selected_slot: usize,
    slots: Vec<String>,
    log: Vec<SharedString>,
}

impl ClipboardDemo {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            selected_slot: 0,
            slots: INITIAL_TEXTS.iter().map(|text| text.to_string()).collect(),
            log: Vec::new(),
        }
    }

    fn copy_selected(&mut self, cx: &mut Context<Self>) {
        let index = self.selected_slot;
        let Some(text) = self.slots.get(index) else {
            return;
        };
        if text.is_empty() {
            self.append_log(format!("Slot {} is empty.", index + 1));
        } else {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            self.append_log(format!("Copied from slot {}.", index + 1));
        }
        cx.notify();
    }

    fn paste_selected(&mut self, cx: &mut Context<Self>) {
        let index = self.selected_slot;
        cx.spawn(async move |this: gpui::WeakEntity<ClipboardDemo>, cx| {
            this.update(cx, |this: &mut ClipboardDemo, cx: &mut Context<ClipboardDemo>| {
                match cx.read_from_clipboard().and_then(|item| item.text()) {
                    Some(text) => {
                        if let Some(slot) = this.slots.get_mut(index) {
                            *slot = text;
                            this.append_log(format!("Pasted into slot {}.", index + 1));
                        }
                    }
                    None => this.append_log("Clipboard is empty.".to_string()),
                }
                cx.notify();
            }).ok();
        }).detach();
    }

    fn cut_selected(&mut self, cx: &mut Context<Self>) {
        let index = self.selected_slot;
        let Some(text) = self.slots.get(index) else {
            return;
        };
        if text.is_empty() {
            self.append_log(format!("Slot {} is empty.", index + 1));
        } else {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            if let Some(slot) = self.slots.get_mut(index) {
                slot.clear();
            }
            self.append_log(format!("Cut from slot {}.", index + 1));
        }
        cx.notify();
    }

    fn clear_slot(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.clear();
            self.append_log(format!("Cleared slot {}.", index + 1));
        }
        cx.notify();
    }

    fn append_log(&mut self, message: String) {
        self.log.push(message.into());
        if self.log.len() > 15 {
            self.log.remove(0);
        }
    }
}

impl Focusable for ClipboardDemo {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn slot_button(id: ElementId, label: &'static str, color: u32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py(px(2.))
        .rounded_md()
        .bg(rgb(color))
        .text_color(rgb(BG_BASE))
        .text_xs()
        .cursor_pointer()
        .child(label)
}

impl Render for ClipboardDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let slots = self.slots.iter().enumerate().fold(
            div().flex().flex_col().gap_2().flex_1(),
            |column, (index, text)| {
                let is_empty = text.is_empty();
                let is_selected = index == self.selected_slot;
                let display_text: SharedString = if is_empty {
                    "(empty)".into()
                } else {
                    text.clone().into()
                };

                column.child(
                    div()
                        .id(ElementId::NamedInteger("slot".into(), index as u64))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .rounded_lg()
                        .bg(rgb(if is_selected { BG_OVERLAY } else { BG_SURFACE }))
                        .border_1()
                        .border_color(rgb(if is_selected { ACCENT_BLUE } else { BG_SURFACE }))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected_slot = index;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(TEXT_PRIMARY))
                                        .child(SharedString::from(format!("Slot {}", index + 1))),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_1()
                                        .child(
                                            slot_button(
                                                ElementId::NamedInteger("copy".into(), index as u64),
                                                "Copy",
                                                ACCENT_BLUE,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.selected_slot = index;
                                                this.copy_selected(cx);
                                            })),
                                        )
                                        .child(
                                            slot_button(
                                                ElementId::NamedInteger("paste".into(), index as u64),
                                                "Paste",
                                                ACCENT_GREEN,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.selected_slot = index;
                                                this.paste_selected(cx);
                                            })),
                                        )
                                        .child(
                                            slot_button(
                                                ElementId::NamedInteger("clear".into(), index as u64),
                                                "Clear",
                                                ACCENT_RED,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.clear_slot(index, cx);
                                            })),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(if is_empty { TEXT_DIM } else { TEXT_SECONDARY }))
                                .child(display_text),
                        ),
                )
            },
        );

        let log = self.log.iter().rev().fold(
            div().flex().flex_col().gap_1(),
            |column, entry| {
                column.child(div().text_xs().text_color(rgb(TEXT_SECONDARY)).child(entry.clone()))
            },
        );

        div()
            .key_context("ClipboardDemo")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _action: &Copy, _window, cx| {
                this.copy_selected(cx);
            }))
            .on_action(cx.listener(|this, _action: &Paste, _window, cx| {
                this.paste_selected(cx);
            }))
            .on_action(cx.listener(|this, _action: &Cut, _window, cx| {
                this.cut_selected(cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG_BASE))
            .items_center()
            .p_4()
            .gap_4()
            .child(div().text_xl().text_color(rgb(TEXT_PRIMARY)).child("Clipboard Demo :3"))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child("Click a slot to select it. Use Ctrl+C / Ctrl+V / Ctrl+X."),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .w(px(600.))
                    .child(slots)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .w(px(200.))
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(BG_SURFACE))
                            .child(div().text_sm().text_color(rgb(TEXT_DIM)).child("Log"))
                            .child(log),
                    ),
            )
    }
}

fn main() {
    gpui_platform::web_init();
    gpui_platform::application().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("ctrl-c", Copy, Some("ClipboardDemo")),
            KeyBinding::new("ctrl-v", Paste, Some("ClipboardDemo")),
            KeyBinding::new("ctrl-x", Cut, Some("ClipboardDemo")),
        ]);

        let bounds = Bounds::centered(None, size(px(660.), px(420.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(ClipboardDemo::new);
                let focus_handle = view.read(cx).focus_handle.clone();
                focus_handle.focus(window, cx);
                view
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
