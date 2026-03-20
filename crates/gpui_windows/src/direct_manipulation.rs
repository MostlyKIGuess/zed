use std::cell::Cell;
use std::rc::Rc;

use ::util::ResultExt;
use gpui::*;
use windows::Win32::{
    Foundation::*,
    Graphics::{DirectManipulation::*, Gdi::*},
    System::Com::*,
    UI::{Input::Pointer::*, WindowsAndMessaging::*},
};

use crate::*;

/// Default viewport size in pixels. The actual content size doesn't matter
/// because we're using the viewport only for gesture recognition, not for
/// visual output. (Same value Chromium uses.)
const DEFAULT_VIEWPORT_SIZE: i32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureState {
    None,
    Scroll,
    Pinch,
    Fling,
}

pub(crate) struct DirectManipulationHandler {
    _manager: IDirectManipulationManager,
    update_manager: IDirectManipulationUpdateManager,
    viewport: IDirectManipulationViewport,
    _handler_cookie: u32,
    window: HWND,
    pending_events: Rc<PendingEvents>,
}

pub(crate) struct PendingEvents {
    events: std::cell::RefCell<Vec<PlatformInput>>,
}

impl PendingEvents {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            events: std::cell::RefCell::new(Vec::new()),
        })
    }

    fn push(&self, event: PlatformInput) {
        self.events.borrow_mut().push(event);
    }

    pub(crate) fn drain(&self) -> Vec<PlatformInput> {
        std::mem::take(&mut *self.events.borrow_mut())
    }
}

impl DirectManipulationHandler {
    pub fn new(window: HWND, scale_factor: f32) -> Option<Self> {
        unsafe {
            let manager: IDirectManipulationManager =
                CoCreateInstance(&DirectManipulationManager, None, CLSCTX_INPROC_SERVER)
                    .log_err()?;

            let update_manager: IDirectManipulationUpdateManager =
                manager.GetUpdateManager().log_err()?;

            let viewport: IDirectManipulationViewport =
                manager.CreateViewport(None, window).log_err()?;

            let configuration = DIRECTMANIPULATION_CONFIGURATION_INTERACTION
                | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_X
                | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_Y
                | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_INERTIA
                | DIRECTMANIPULATION_CONFIGURATION_RAILS_X
                | DIRECTMANIPULATION_CONFIGURATION_RAILS_Y
                | DIRECTMANIPULATION_CONFIGURATION_SCALING;
            viewport.ActivateConfiguration(configuration).log_err()?;

            // Use MANUALUPDATE since we poll ourselves each frame
            viewport
                .SetViewportOptions(DIRECTMANIPULATION_VIEWPORT_OPTIONS_MANUALUPDATE)
                .log_err()?;

            let mut rect = RECT {
                left: 0,
                top: 0,
                right: DEFAULT_VIEWPORT_SIZE,
                bottom: DEFAULT_VIEWPORT_SIZE,
            };
            viewport.SetViewportRect(&mut rect).log_err()?;

            manager.Activate(window).log_err()?;

            viewport.Enable().log_err()?;

            let pending_events = PendingEvents::new();

            let event_handler: IDirectManipulationViewportEventHandler =
                DirectManipulationEventHandler::new(
                    window,
                    scale_factor,
                    Rc::clone(&pending_events),
                )
                .into();

            let handler_cookie = viewport
                .AddEventHandler(Some(window), &event_handler)
                .log_err()?;

            // initial update for making the system ready
            update_manager.Update(None).log_err()?;

            Some(Self {
                _manager: manager,
                update_manager,
                viewport,
                _handler_cookie: handler_cookie,
                window,
                pending_events,
            })
        }
    }

    pub fn on_pointer_hit_test(&self, wparam: WPARAM) {
        unsafe {
            let pointer_id = (wparam.0 & 0xFFFF) as u32;
            let mut pointer_type = POINTER_INPUT_TYPE::default();
            if GetPointerType(pointer_id, &mut pointer_type).is_ok() && pointer_type == PT_TOUCHPAD
            {
                self.viewport.SetContact(pointer_id).log_err();
            }
        }
    }

    pub fn update(&self) {
        unsafe {
            self.update_manager.Update(None).log_err();
        }
    }

    pub fn drain_events(&self) -> Vec<PlatformInput> {
        self.pending_events.drain()
    }
}

impl Drop for DirectManipulationHandler {
    fn drop(&mut self) {
        unsafe {
            self.viewport.Stop().log_err();
            self.viewport.Abandon().log_err();
            self._manager.Deactivate(self.window).log_err();
        }
    }
}

#[windows_core::implement(IDirectManipulationViewportEventHandler)]
struct DirectManipulationEventHandler {
    window: HWND,
    scale_factor: Cell<f32>,
    gesture_state: Cell<GestureState>,
    last_scale: Cell<f32>,
    last_x_offset: Cell<f32>,
    last_y_offset: Cell<f32>,
    should_send_scroll_begin: Cell<bool>,
    pending_events: Rc<PendingEvents>,
}

impl DirectManipulationEventHandler {
    fn new(window: HWND, scale_factor: f32, pending_events: Rc<PendingEvents>) -> Self {
        Self {
            window,
            scale_factor: Cell::new(scale_factor),
            gesture_state: Cell::new(GestureState::None),
            last_scale: Cell::new(1.0),
            last_x_offset: Cell::new(0.0),
            last_y_offset: Cell::new(0.0),
            should_send_scroll_begin: Cell::new(false),
            pending_events,
        }
    }

    fn float_equals(f1: f32, f2: f32) -> bool {
        const EPSILON_SCALE: f32 = 0.00001;
        (f1 - f2).abs() < EPSILON_SCALE * f1.abs().max(f2.abs()).max(EPSILON_SCALE)
    }

    fn transition_to_state(&self, new_state: GestureState) {
        let previous = self.gesture_state.get();
        if previous == new_state {
            return;
        }

        self.gesture_state.set(new_state);

        if new_state == GestureState::Scroll {
            self.should_send_scroll_begin.set(true);
        }

        if previous == GestureState::Pinch {
            let position = self.mouse_position();
            self.pending_events.push(PlatformInput::Pinch(PinchEvent {
                position,
                delta: 0.0,
                modifiers: current_modifiers(),
                phase: TouchPhase::Ended,
            }));
        }

        if new_state == GestureState::Pinch {
            let position = self.mouse_position();
            self.pending_events.push(PlatformInput::Pinch(PinchEvent {
                position,
                delta: 0.0,
                modifiers: current_modifiers(),
                phase: TouchPhase::Started,
            }));
        }
    }

    fn mouse_position(&self) -> Point<Pixels> {
        let scale_factor = self.scale_factor.get();
        unsafe {
            let mut point: POINT = std::mem::zeroed();
            let _ = GetCursorPos(&mut point);
            let _ = ScreenToClient(self.window, &mut point);
            logical_point(point.x as f32, point.y as f32, scale_factor)
        }
    }
}

impl IDirectManipulationViewportEventHandler_Impl for DirectManipulationEventHandler_Impl {
    fn OnViewportStatusChanged(
        &self,
        viewport: windows_core::Ref<'_, IDirectManipulationViewport>,
        current: DIRECTMANIPULATION_STATUS,
        previous: DIRECTMANIPULATION_STATUS,
    ) -> windows_core::Result<()> {
        if current == previous {
            return Ok(());
        }

        if current == DIRECTMANIPULATION_INERTIA {
            if previous != DIRECTMANIPULATION_RUNNING
                || self.gesture_state.get() != GestureState::Scroll
            {
                return Ok(());
            }
            self.transition_to_state(GestureState::Fling);
        }

        if current == DIRECTMANIPULATION_RUNNING && previous == DIRECTMANIPULATION_INERTIA {
            self.transition_to_state(GestureState::None);
        }

        if current == DIRECTMANIPULATION_READY {
            let last_scale = self.last_scale.get();
            let last_x = self.last_x_offset.get();
            let last_y = self.last_y_offset.get();

            if last_scale != 1.0 || last_x != 0.0 || last_y != 0.0 {
                if let Some(viewport) = viewport.as_ref() {
                    unsafe {
                        viewport
                            .ZoomToRect(
                                0.0,
                                0.0,
                                DEFAULT_VIEWPORT_SIZE as f32,
                                DEFAULT_VIEWPORT_SIZE as f32,
                                false,
                            )
                            .log_err();
                    }
                }
            }

            self.last_scale.set(1.0);
            self.last_x_offset.set(0.0);
            self.last_y_offset.set(0.0);

            self.transition_to_state(GestureState::None);
        }

        Ok(())
    }

    fn OnViewportUpdated(
        &self,
        _viewport: windows_core::Ref<'_, IDirectManipulationViewport>,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnContentUpdated(
        &self,
        _viewport: windows_core::Ref<'_, IDirectManipulationViewport>,
        content: windows_core::Ref<'_, IDirectManipulationContent>,
    ) -> windows_core::Result<()> {
        let content = content.as_ref().ok_or(E_POINTER)?;

        // Get the 6-element content transform: [scale, 0, 0, scale, tx, ty]
        let mut xform = [0.0f32; 6];
        unsafe {
            content.GetContentTransform(&mut xform)?;
        }

        let scale = xform[0];
        let scale_factor = self.scale_factor.get();
        let x_offset = xform[4] / scale_factor;
        let y_offset = xform[5] / scale_factor;

        if scale == 0.0 {
            log::error!("Direct Manipulation returned scale = 0");
            return Ok(());
        }

        let last_scale = self.last_scale.get();
        let last_x = self.last_x_offset.get();
        let last_y = self.last_y_offset.get();

        if DirectManipulationEventHandler::float_equals(scale, last_scale)
            && (x_offset as i32) == (last_x as i32)
            && (y_offset as i32) == (last_y as i32)
        {
            return Ok(());
        }

        if DirectManipulationEventHandler::float_equals(scale, 1.0) {
            if self.gesture_state.get() == GestureState::None {
                self.transition_to_state(GestureState::Scroll);
            }
        } else {
            self.transition_to_state(GestureState::Pinch);
        }

        if self.gesture_state.get() == GestureState::Pinch {
            let position = self.mouse_position();
            let scale_delta = if last_scale != 0.0 {
                scale / last_scale
            } else {
                1.0
            };
            let zoom_delta = scale_delta - 1.0;

            self.pending_events.push(PlatformInput::Pinch(PinchEvent {
                position,
                delta: zoom_delta,
                modifiers: current_modifiers(),
                phase: TouchPhase::Moved,
            }));
        }

        self.last_scale.set(scale);
        self.last_x_offset.set(x_offset);
        self.last_y_offset.set(y_offset);

        Ok(())
    }
}
