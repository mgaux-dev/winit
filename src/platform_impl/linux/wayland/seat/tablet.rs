//! Tablet-v2 reception on the window event loop's connection.
//!
//! Adapts the event/frame routing in upstream winit's wp_tablet_input_v2 (Apache-2.0)
//! to the 0.30 pointer API: the pen is a mouse whose tip is the left button. Per-window samples
//! retain the axes that 0.30 drops.
use crate::dpi::LogicalPosition;
use crate::event::{ElementState, MouseButton, WindowEvent};
use crate::platform::wayland::{TabletInput, TabletPhase};
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId};
use sctk::compositor::SurfaceData;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::{wl_seat::WlSeat, wl_surface::WlSurface};
use sctk::reexports::client::{
    event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use sctk::reexports::protocols::wp::tablet::zv2::client::{
    zwp_tablet_manager_v2::ZwpTabletManagerV2,
    zwp_tablet_pad_group_v2::{self, ZwpTabletPadGroupV2},
    zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
    zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
    zwp_tablet_pad_v2::{self, ZwpTabletPadV2},
    zwp_tablet_seat_v2::{self, ZwpTabletSeatV2},
    zwp_tablet_tool_v2::{self, ZwpTabletToolV2},
    zwp_tablet_v2::{self, ZwpTabletV2},
};
use std::sync::{Mutex, PoisonError};

#[derive(Debug, Clone)]
pub struct TabletManager(ZwpTabletManagerV2);
impl TabletManager {
    pub fn new(globals: &GlobalList, qh: &QueueHandle<WinitState>) -> Result<Self, BindError> {
        globals.bind(qh, 1..=1, ()).map(Self)
    }
    pub fn seat(&self, seat: &WlSeat, qh: &QueueHandle<WinitState>) -> ZwpTabletSeatV2 {
        self.0.get_tablet_seat(seat, qh, SeatData { seat: seat.clone(), objects: Mutex::default() })
    }
}
#[derive(Debug)]
pub struct SeatData {
    seat: WlSeat,
    objects: Mutex<Vec<SeatObject>>,
}
#[derive(Debug)]
enum SeatObject {
    Tablet(ZwpTabletV2),
    Pad(ZwpTabletPadV2),
}
impl SeatData {
    pub fn destroy_objects(&self) {
        for object in self.objects.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
            match object {
                SeatObject::Tablet(tablet) if tablet.is_alive() => tablet.destroy(),
                SeatObject::Pad(pad) if pad.is_alive() => {
                    destroy_pad(&pad);
                    pad.destroy();
                },
                _ => {},
            }
        }
    }
}
#[derive(Debug, Default)]
pub struct PadData(Mutex<Vec<ZwpTabletPadGroupV2>>);
#[derive(Debug, Default)]
pub struct GroupData(Mutex<Vec<PadChild>>);
#[derive(Debug)]
enum PadChild {
    Ring(ZwpTabletPadRingV2),
    Strip(ZwpTabletPadStripV2),
}
fn destroy_pad(pad: &ZwpTabletPadV2) {
    if let Some(data) = pad.data::<PadData>() {
        for group in data.0.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
            if let Some(data) = group.data::<GroupData>() {
                for child in data.0.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
                    match child {
                        PadChild::Ring(ring) if ring.is_alive() => ring.destroy(),
                        PadChild::Strip(strip) if strip.is_alive() => strip.destroy(),
                        _ => {},
                    }
                }
            }
            if group.is_alive() {
                group.destroy();
            }
        }
    }
}
#[derive(Debug, Default)]
pub struct ToolData(Mutex<ToolState>);
#[derive(Debug, Default)]
struct ToolState {
    surface: Option<WlSurface>,
    position: LogicalPosition<f64>,
    pressure: f64,
    tilt: [f64; 2],
    rotation: f64,
    eraser: bool,
    has_pressure: bool,
    down: bool,
    pending: Vec<Pending>,
    overflowed: bool,
    seat: Option<WlSeat>,
    down_serial: u32,
    cursor: Option<std::sync::Arc<Mutex<TabletCursor>>>,
    pressed_buttons: Vec<MouseButton>,
}
#[derive(Debug)]
enum Pending {
    Enter(WlSurface),
    Move,
    Down(u32),
    Up,
    Leave,
    Barrel(MouseButton, ElementState),
}
impl ToolState {
    fn push(&mut self, event: Pending) {
        // Motion/axis updates within one protocol frame describe one coherent sample.
        if matches!(event, Pending::Move) && self.pending.iter().any(|e| matches!(e, Pending::Move))
        {
            return;
        }
        if self.pending.len() >= 32 {
            self.overflowed = true;
            return;
        }
        self.pending.push(event);
    }
    fn reset_axes(&mut self) {
        self.pressure = 0.0;
        self.tilt = [0.0; 2];
        self.rotation = 0.0;
    }
}
impl WinitState {
    pub fn remove_tablet_seat(&mut self, seat: &WlSeat) {
        let ids: Vec<_> = self
            .tablet_tools
            .iter()
            .filter_map(|(id, tool)| {
                let data = tool.data::<ToolData>()?;
                data.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .seat
                    .as_ref()
                    .is_some_and(|tool_seat| tool_seat.id() == seat.id())
                    .then(|| id.clone())
            })
            .collect();
        for id in ids {
            if let Some(proxy) = self.tablet_tools.remove(&id) {
                if let Some(data) = proxy.data::<ToolData>() {
                    let mut tool = data.0.lock().unwrap_or_else(PoisonError::into_inner);
                    tool.pending.clear();
                    tool.push(Pending::Leave);
                    self.tablet_frame(
                        &mut tool,
                        crate::platform::wayland::TABLET_TOUCH_TAG
                            | u64::from(proxy.id().protocol_id()),
                    );
                    tool.cursor = None;
                }
                if proxy.is_alive() {
                    proxy.destroy();
                }
            }
        }
    }
    fn tablet_emit(&mut self, tool: &mut ToolState, tool_id: u64, phase: TabletPhase) {
        let Some(surface) = tool.surface.as_ref() else {
            return;
        };
        let parent =
            surface.data::<SurfaceData>().and_then(|data| data.parent_surface()).unwrap_or(surface);
        let window_id = wayland::make_wid(parent);
        let Some(window) = self.windows.get_mut().get(&window_id) else {
            return;
        };
        let mut window = window.lock().unwrap_or_else(PoisonError::into_inner);
        let position = tool.position.to_physical(window.scale_factor());
        let sample = TabletInput {
            tool_id,
            phase,
            position,
            pressure: tool.pressure,
            tilt: tool.tilt,
            rotation: tool.rotation,
            eraser: tool.eraser,
        };
        match phase {
            TabletPhase::Pressed => {
                window.tablet_grab = tool.seat.clone().map(|seat| (tool_id, seat, tool.down_serial))
            },
            TabletPhase::Released | TabletPhase::Left
                if window.tablet_grab.as_ref().is_some_and(|(id, _, _)| *id == tool_id) =>
            {
                window.tablet_grab = None;
            },
            _ => {},
        }
        window.push_tablet_input(sample);
        drop(window);
        // The pen is a mouse to the app, as on X11: hovering moves the cursor and the tip is the
        // left button. Reported as touch, toolkits would take it for a finger: egui, having seen
        // one touch, treats the machine as a touch screen and scrolls on every drag instead of
        // selecting or drawing. The pressure is in the samples above.
        let device_id = crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId));
        let left = |state| WindowEvent::MouseInput { device_id, state, button: MouseButton::Left };
        let moved = WindowEvent::CursorMoved { device_id, position };
        let sink = &mut self.events_sink;
        match phase {
            TabletPhase::Entered => {
                sink.push_window_event(WindowEvent::CursorEntered { device_id }, window_id)
            },
            TabletPhase::Moved => sink.push_window_event(moved, window_id),
            // The position goes first, so the tip presses and lifts where it touched.
            TabletPhase::Pressed => {
                sink.push_window_event(moved, window_id);
                sink.push_window_event(left(ElementState::Pressed), window_id);
            },
            TabletPhase::Released => {
                sink.push_window_event(moved, window_id);
                sink.push_window_event(left(ElementState::Released), window_id);
            },
            TabletPhase::Left => {
                if tool.down {
                    sink.push_window_event(left(ElementState::Released), window_id);
                }
                sink.push_window_event(WindowEvent::CursorLeft { device_id }, window_id);
            },
        }
        self.dispatched_events = true;
    }
    fn tablet_button(&mut self, tool: &ToolState, button: MouseButton, state: ElementState) {
        if let Some(surface) = tool.surface.as_ref() {
            let parent = surface
                .data::<SurfaceData>()
                .and_then(|data| data.parent_surface())
                .unwrap_or(surface);
            let id = wayland::make_wid(parent);
            if self.windows.get_mut().contains_key(&id) {
                let device_id =
                    crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId));
                self.events_sink
                    .push_window_event(WindowEvent::MouseInput { device_id, state, button }, id);
                self.dispatched_events = true;
            }
        }
    }
    fn tablet_frame(&mut self, tool: &mut ToolState, tool_id: u64) {
        if tool.overflowed {
            for button in std::mem::take(&mut tool.pressed_buttons) {
                self.tablet_button(tool, button, ElementState::Released);
            }
            if let Some(cursor) = &tool.cursor {
                cursor.lock().unwrap_or_else(PoisonError::into_inner).active = false;
            }
            self.tablet_emit(tool, tool_id, TabletPhase::Left);
            tool.down = false;
            tool.surface = None;
            tool.pending.clear();
            tool.overflowed = false;
            tool.reset_axes();
            return;
        }
        for event in std::mem::take(&mut tool.pending) {
            match event {
                Pending::Enter(surface) => {
                    if tool.surface.is_some() {
                        self.tablet_emit(tool, tool_id, TabletPhase::Left);
                        tool.down = false;
                    }
                    if let Some(cursor) = &tool.cursor {
                        let parent = surface
                            .data::<SurfaceData>()
                            .and_then(|data| data.parent_surface())
                            .unwrap_or(&surface);
                        if let Some(window) = self.windows.get_mut().get(&wayland::make_wid(parent))
                        {
                            window
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .enter_tablet_cursor(cursor);
                        }
                    }
                    tool.surface = Some(surface);
                    self.tablet_emit(tool, tool_id, TabletPhase::Entered);
                    self.tablet_emit(tool, tool_id, TabletPhase::Moved);
                },
                Pending::Move => self.tablet_emit(tool, tool_id, TabletPhase::Moved),
                Pending::Down(serial) => {
                    tool.down_serial = serial;
                    if let (Some(seat), Some(surface)) = (&tool.seat, &tool.surface) {
                        let parent = surface
                            .data::<SurfaceData>()
                            .and_then(|data| data.parent_surface())
                            .unwrap_or(surface);
                        let window_id = crate::window::WindowId(wayland::make_wid(parent));
                        self.clipboard.pointer_input(&seat.id(), serial, window_id, true);
                    }
                    if !tool.down {
                        if !tool.has_pressure {
                            tool.pressure = 1.0;
                        }
                        tool.down = true;
                        self.tablet_emit(tool, tool_id, TabletPhase::Pressed);
                    }
                },
                Pending::Up => {
                    if tool.down {
                        self.tablet_emit(tool, tool_id, TabletPhase::Released);
                        tool.down = false;
                    }
                },
                Pending::Leave => {
                    for button in std::mem::take(&mut tool.pressed_buttons) {
                        self.tablet_button(tool, button, ElementState::Released);
                    }
                    if let Some(cursor) = &tool.cursor {
                        cursor.lock().unwrap_or_else(PoisonError::into_inner).active = false;
                    }
                    self.tablet_emit(tool, tool_id, TabletPhase::Left);
                    tool.down = false;
                    tool.surface = None;
                    tool.reset_axes();
                },
                Pending::Barrel(button, state) => {
                    if state == ElementState::Pressed {
                        if !tool.pressed_buttons.contains(&button)
                            && tool.pressed_buttons.len() < 32
                        {
                            tool.pressed_buttons.push(button);
                        }
                    } else {
                        tool.pressed_buttons.retain(|pressed| *pressed != button);
                    }
                    self.tablet_button(tool, button, state);
                },
            }
        }
    }
}
impl Dispatch<ZwpTabletToolV2, ToolData> for WinitState {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletToolV2,
        event: zwp_tablet_tool_v2::Event,
        data: &ToolData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_tablet_tool_v2::Event;
        let mut tool = data.0.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            Event::Type { tool_type: WEnum::Value(ty) } => {
                tool.eraser = ty == zwp_tablet_tool_v2::Type::Eraser
            },
            Event::Capability {
                capability: WEnum::Value(zwp_tablet_tool_v2::Capability::Pressure),
            } => tool.has_pressure = true,
            Event::ProximityIn { surface, serial, .. } => {
                if let Some(cursor) = &tool.cursor {
                    let mut cursor = cursor.lock().unwrap_or_else(PoisonError::into_inner);
                    cursor.serial = serial;
                    cursor.active = true;
                }
                tool.reset_axes();
                tool.push(Pending::Enter(surface));
            },
            Event::ProximityOut => tool.push(Pending::Leave),
            Event::Motion { x, y } if x.is_finite() && y.is_finite() => {
                tool.position = LogicalPosition::new(x, y);
                tool.push(Pending::Move);
            },
            Event::Pressure { pressure } => {
                tool.pressure = f64::from(pressure.min(65535)) / 65535.0;
                tool.push(Pending::Move);
            },
            Event::Tilt { tilt_x, tilt_y } if tilt_x.is_finite() && tilt_y.is_finite() => {
                tool.tilt = [tilt_x.clamp(-90.0, 90.0), tilt_y.clamp(-90.0, 90.0)];
                tool.push(Pending::Move);
            },
            Event::Rotation { degrees } if degrees.is_finite() => {
                tool.rotation = degrees.rem_euclid(360.0);
                tool.push(Pending::Move);
            },
            Event::Down { serial } => tool.push(Pending::Down(serial)),
            Event::Up => tool.push(Pending::Up),
            Event::Button { button, state: WEnum::Value(button_state), .. } => {
                let s = match button_state {
                    zwp_tablet_tool_v2::ButtonState::Pressed => ElementState::Pressed,
                    zwp_tablet_tool_v2::ButtonState::Released => ElementState::Released,
                    _ => return,
                };
                let button = match button {
                    0x14b => MouseButton::Right,
                    0x14c => MouseButton::Middle,
                    other => match u16::try_from(other) {
                        Ok(n) => MouseButton::Other(n),
                        Err(_) => return,
                    },
                };
                tool.push(Pending::Barrel(button, s));
            },
            Event::Frame { .. } => state.tablet_frame(
                &mut tool,
                crate::platform::wayland::TABLET_TOUCH_TAG | u64::from(proxy.id().protocol_id()),
            ),
            Event::Removed => {
                tool.pending.clear();
                tool.push(Pending::Leave);
                state.tablet_frame(
                    &mut tool,
                    crate::platform::wayland::TABLET_TOUCH_TAG
                        | u64::from(proxy.id().protocol_id()),
                );
                if proxy.is_alive() {
                    proxy.destroy();
                }
                tool.cursor = None;
                state.tablet_tools.remove(&proxy.id());
            },
            _ => {},
        }
    }
}
macro_rules! ignore_protocol {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for WinitState {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
ignore_protocol!(ZwpTabletManagerV2);
impl Dispatch<ZwpTabletV2, ()> for WinitState {
    fn event(
        _: &mut Self,
        proxy: &ZwpTabletV2,
        event: zwp_tablet_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, zwp_tablet_v2::Event::Removed) && proxy.is_alive() {
            proxy.destroy();
        }
    }
}
ignore_protocol!(ZwpTabletPadRingV2);
ignore_protocol!(ZwpTabletPadStripV2);
impl Dispatch<ZwpTabletSeatV2, SeatData> for WinitState {
    event_created_child!(WinitState,ZwpTabletSeatV2,[zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE=>(ZwpTabletV2,()),zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE=>(ZwpTabletToolV2,ToolData::default()),zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE=>(ZwpTabletPadV2,PadData::default())]);
    fn event(
        state: &mut Self,
        _: &ZwpTabletSeatV2,
        event: <ZwpTabletSeatV2 as Proxy>::Event,
        data: &SeatData,
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let zwp_tablet_seat_v2::Event::TabletAdded { id } = &event {
            let mut objects = data.objects.lock().unwrap_or_else(PoisonError::into_inner);
            objects.retain(|object| match object {
                SeatObject::Tablet(p) => p.is_alive(),
                SeatObject::Pad(p) => p.is_alive(),
            });
            if objects.len() < 64 {
                objects.push(SeatObject::Tablet(id.clone()));
            } else {
                id.destroy();
            }
        }
        if let zwp_tablet_seat_v2::Event::PadAdded { id } = &event {
            let mut objects = data.objects.lock().unwrap_or_else(PoisonError::into_inner);
            objects.retain(|object| match object {
                SeatObject::Tablet(p) => p.is_alive(),
                SeatObject::Pad(p) => p.is_alive(),
            });
            if objects.len() < 64 {
                objects.push(SeatObject::Pad(id.clone()));
            } else {
                id.destroy();
            }
        }
        if let zwp_tablet_seat_v2::Event::ToolAdded { id } = event {
            if state.tablet_tools.len() >= 64 {
                id.destroy();
                return;
            }
            if let Some(tool) = id.data::<ToolData>() {
                let mut tool = tool.0.lock().unwrap_or_else(PoisonError::into_inner);
                tool.seat = Some(data.seat.clone());
                tool.cursor = Some(std::sync::Arc::new(Mutex::new(TabletCursor {
                    tool: id.downgrade(),
                    surface: state.compositor_state.create_surface(qh),
                    connection: connection.backend().downgrade(),
                    shm: state.shm.wl_shm().clone(),
                    theme: None,
                    theme_scale: 0,
                    window_id: None,
                    serial: 0,
                    active: false,
                })));
            }
            state.tablet_tools.insert(id.id(), id);
        }
    }
}
impl Dispatch<ZwpTabletPadV2, PadData> for WinitState {
    event_created_child!(WinitState,ZwpTabletPadV2,[zwp_tablet_pad_v2::EVT_GROUP_OPCODE=>(ZwpTabletPadGroupV2,GroupData::default())]);
    fn event(
        _: &mut Self,
        proxy: &ZwpTabletPadV2,
        event: zwp_tablet_pad_v2::Event,
        data: &PadData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_tablet_pad_v2::Event::Group { pad_group } => {
                let mut groups = data.0.lock().unwrap_or_else(PoisonError::into_inner);
                if groups.len() < 64 {
                    groups.push(pad_group);
                } else {
                    pad_group.destroy();
                }
            },
            zwp_tablet_pad_v2::Event::Removed => {
                destroy_pad(proxy);
                if proxy.is_alive() {
                    proxy.destroy();
                }
            },
            _ => {},
        }
    }
}
impl Dispatch<ZwpTabletPadGroupV2, GroupData> for WinitState {
    event_created_child!(WinitState,ZwpTabletPadGroupV2,[zwp_tablet_pad_group_v2::EVT_RING_OPCODE=>(ZwpTabletPadRingV2,()),zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE=>(ZwpTabletPadStripV2,())]);
    fn event(
        _: &mut Self,
        _: &ZwpTabletPadGroupV2,
        event: zwp_tablet_pad_group_v2::Event,
        data: &GroupData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut children = data.0.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            zwp_tablet_pad_group_v2::Event::Ring { ring } => {
                if children.len() < 64 {
                    children.push(PadChild::Ring(ring));
                } else {
                    ring.destroy();
                }
            },
            zwp_tablet_pad_group_v2::Event::Strip { strip } => {
                if children.len() < 64 {
                    children.push(PadChild::Strip(strip));
                } else {
                    strip.destroy();
                }
            },
            _ => {},
        }
    }
}

/// A pen has its own cursor protocol; wl_pointer cursor requests do not affect it.
#[derive(Debug)]
pub(crate) struct TabletCursor {
    tool: sctk::reexports::client::Weak<ZwpTabletToolV2>,
    surface: WlSurface,
    connection: sctk::reexports::client::backend::WeakBackend,
    shm: sctk::reexports::client::protocol::wl_shm::WlShm,
    theme: Option<wayland_cursor::CursorTheme>,
    theme_scale: i32,
    pub window_id: Option<crate::platform_impl::wayland::WindowId>,
    serial: u32,
    pub active: bool,
}
impl TabletCursor {
    pub fn named(&mut self, icon: crate::window::CursorIcon, scale: f64) {
        if !self.active {
            return;
        }
        // wayland-cursor's shm allocator can panic on descriptor exhaustion. Input must not
        // terminate an app on that error; keep the compositor's current cursor instead.
        let Ok(tool) = self.tool.upgrade() else {
            return;
        };
        let Some(backend) = self.connection.upgrade() else {
            return;
        };
        let connection = Connection::from_backend(backend);
        let max_scale = cursor_scale(scale);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if self.theme.is_none() || self.theme_scale != max_scale {
                self.theme_scale = max_scale;
                let name = std::env::var("XCURSOR_THEME")
                    .ok()
                    .filter(|name| name.len() <= 1024)
                    .unwrap_or_else(|| "default".into());
                self.theme = wayland_cursor::CursorTheme::load_from_name(
                    &connection,
                    self.shm.clone(),
                    &name,
                    24 * max_scale as u32,
                )
                .ok();
            }
            let Some(theme) = self.theme.as_mut() else {
                return;
            };
            let name =
                if theme.get_cursor(icon.name()).is_some() { icon.name() } else { "left_ptr" };
            let cursor = theme.get_cursor(name);
            let Some(cursor) = cursor else {
                return;
            };
            if cursor.image_count() == 0 {
                return;
            }
            let image = &cursor[0];
            let (w, h) = image.dimensions();
            let (x, y) = image.hotspot();
            let scale = fit_buffer_scale(w, h, max_scale);
            // Named cursor images are logical-size assets, independent of output scaling.
            self.surface.set_buffer_scale(scale);
            self.surface.attach(Some(image), 0, 0);
            self.surface.damage(
                0,
                0,
                i32::try_from(w).unwrap_or(i32::MAX),
                i32::try_from(h).unwrap_or(i32::MAX),
            );
            self.surface.commit();
            tool.set_cursor(
                self.serial,
                Some(&self.surface),
                i32::try_from(x).unwrap_or(0) / scale,
                i32::try_from(y).unwrap_or(0) / scale,
            );
        }));
        if result.is_err() {
            tracing::warn!("tablet cursor allocation failed");
            self.theme = None;
        }
    }
    pub fn hide(&mut self) {
        if self.active {
            if let Ok(tool) = self.tool.upgrade() {
                tool.set_cursor(self.serial, None, 0, 0);
            }
        }
    }
    pub fn custom(
        &mut self,
        buffer: &sctk::reexports::client::protocol::wl_buffer::WlBuffer,
        w: i32,
        h: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) {
        if !self.active {
            return;
        }
        let Ok(tool) = self.tool.upgrade() else {
            return;
        };
        let scale = fit_buffer_scale(
            u32::try_from(w).unwrap_or(0),
            u32::try_from(h).unwrap_or(0),
            cursor_scale(scale),
        );
        self.surface.set_buffer_scale(scale);
        self.surface.attach(Some(buffer), 0, 0);
        self.surface.damage(0, 0, w / scale + 1, h / scale + 1);
        self.surface.commit();
        tool.set_cursor(self.serial, Some(&self.surface), x / scale, y / scale);
    }
}
/// The window's scale factor rounded up to a whole buffer scale, 1 to 8.
fn cursor_scale(scale_factor: f64) -> i32 {
    if scale_factor.is_finite() {
        scale_factor.ceil().clamp(1.0, 8.0) as i32
    } else {
        1
    }
}

/// The largest buffer scale up to `max_scale` that divides the image. A buffer whose size is
/// not a multiple of its scale is a protocol error, which would end the connection.
fn fit_buffer_scale(width: u32, height: u32, max_scale: i32) -> i32 {
    (1..=max_scale.max(1))
        .rev()
        .find(|&scale| {
            let scale = scale.unsigned_abs();
            width % scale == 0 && height % scale == 0
        })
        .unwrap_or(1)
}

impl Drop for TabletCursor {
    fn drop(&mut self) {
        if self.surface.is_alive() {
            self.surface.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{cursor_scale, fit_buffer_scale};

    #[test]
    fn cursor_scale_is_a_whole_scale_from_1_to_8() {
        assert_eq!(cursor_scale(1.0), 1);
        assert_eq!(cursor_scale(1.25), 2);
        assert_eq!(cursor_scale(100.0), 8);
        assert_eq!(cursor_scale(0.0), 1);
        assert_eq!(cursor_scale(f64::NAN), 1);
        assert_eq!(cursor_scale(f64::INFINITY), 1);
    }

    #[test]
    fn buffer_scale_divides_the_image() {
        assert_eq!(fit_buffer_scale(48, 48, 2), 2);
        assert_eq!(fit_buffer_scale(64, 64, 3), 2);
        assert_eq!(fit_buffer_scale(33, 32, 2), 1);
        assert_eq!(fit_buffer_scale(0, 0, 2), 2);
        assert_eq!(fit_buffer_scale(24, 24, 0), 1);
    }
}
