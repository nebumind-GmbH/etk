use egui::{Event, Key, PointerButton, Pos2, RawInput};
use egui_backend::*;
pub use glfw;
use glfw::Action;
use glfw::ClientApiHint;
use glfw::Context;
use glfw::Glfw;
use glfw::StandardCursor;
use glfw::WindowEvent;
use glfw::WindowHint;
use raw_window_handle::*;
use std::sync::mpsc::Receiver;

pub struct GlfwBackend {
    pub glfw: glfw::Glfw,
    pub events_receiver: Receiver<(f64, WindowEvent)>,
    pub window: glfw::Window,
    pub size_physical_pixels: [u32; 2],
    pub scale: [f32; 2],
    pub cursor_pos_physical_pixels: [f32; 2],
    pub raw_input: RawInput,
    pub cursor_icon: glfw::StandardCursor,
    pub frame_events: Vec<WindowEvent>,
    pub resized_event_pending: bool,
    pub backend_config: BackendConfig,
}

unsafe impl HasRawWindowHandle for GlfwBackend {
    fn raw_window_handle(&self) -> raw_window_handle::RawWindowHandle {
        self.window.raw_window_handle()
    }
}
unsafe impl HasRawDisplayHandle for GlfwBackend {
    fn raw_display_handle(&self) -> raw_window_handle::RawDisplayHandle {
        self.window.raw_display_handle()
    }
}

/// The configuration struct for Glfw Backend
///
#[derive(Default)]
pub struct GlfwConfig {
    /// This callback is called with `&mut Glfw` just before creating a window
    pub glfw_callback: Option<Box<dyn FnOnce(&mut Glfw)>>,
    /// This will be called right after window creation. you can use this to do things at startup like
    /// resizing, changing title, changing to fullscreen etc..
    pub window_callback: Option<Box<dyn FnOnce(&mut glfw::Window)>>,
}
impl WindowBackend for GlfwBackend {
    type Configuration = GlfwConfig;

    type WindowType = glfw::Window;
    fn new(config: Self::Configuration, backend_config: BackendConfig) -> Self {
        let mut glfw_context =
            glfw::init(glfw::FAIL_ON_ERRORS).expect("failed to create glfw context");

        // set hints based on gfx api config
        match &backend_config.gfx_api_type {
            GfxApiType::GL => {
                glfw_context.window_hint(WindowHint::ClientApi(ClientApiHint::OpenGl));
            }
            GfxApiType::NoApi => {
                glfw_context.window_hint(WindowHint::ClientApi(ClientApiHint::NoApi));
            }
        }
        if let Some(glfw_callback) = config.glfw_callback {
            glfw_callback(&mut glfw_context);
        }
        // create a window
        let (mut window, events_receiver) = glfw_context
            .create_window(800, 600, "Overlay Window", glfw::WindowMode::Windowed)
            .expect("failed to create glfw window");
        if let GfxApiType::GL = backend_config.gfx_api_type {
            window.make_current();
        }
        // set which events you care about
        window.set_all_polling(true);
        window.set_store_lock_key_mods(true);
        if let Some(window_callback) = config.window_callback {
            window_callback(&mut window);
        }
        // collect details and keep them updated
        let (width, height) = window.get_framebuffer_size();
        let scale = window.get_content_scale();
        let cursor_position = window.get_cursor_pos();
        let size_physical_pixels = [width as u32, height as u32];
        let mut raw_input = RawInput::default();
        // set raw input screen rect details so that first frame
        // will have correct size even without any resize event
        raw_input.screen_rect = Some(egui::Rect::from_points(&[
            Default::default(),
            [width as f32 / scale.0, height as f32 / scale.0].into(),
        ]));
        raw_input.pixels_per_point = Some(scale.0);
        Self {
            glfw: glfw_context,
            events_receiver,
            window,
            size_physical_pixels,
            scale: [scale.0, scale.1],
            cursor_pos_physical_pixels: [cursor_position.0 as f32, cursor_position.1 as f32],
            raw_input,
            frame_events: vec![],
            resized_event_pending: true, // provide so that on first prepare frame, renderers can set their viewport sizes
            backend_config,
            cursor_icon: StandardCursor::Arrow,
        }
    }

    fn take_raw_input(&mut self) -> RawInput {
        self.raw_input.take()
    }
    fn get_window(&mut self) -> Option<&mut Self::WindowType> {
        Some(&mut self.window)
    }

    fn get_live_physical_size_framebuffer(&mut self) -> Option<[u32; 2]> {
        let physical_fb_size = self.window.get_framebuffer_size();
        self.size_physical_pixels = [physical_fb_size.0 as u32, physical_fb_size.1 as u32];
        Some(self.size_physical_pixels)
    }

    fn run_event_loop<G: GfxBackend<Self>, U: UserAppData<Self, G>>(
        mut self,
        mut gfx_backend: G,
        mut user_app: U,
    ) {
        let egui_context = egui::Context::default();
        while !self.window.should_close() {
            // gather events
            self.tick();
            // take egui input
            let raw_input = self.take_raw_input();
            // take any frambuffer resize events

            // prepare surface for drawing
            gfx_backend.prepare_frame(self.resized_event_pending, &mut self);
            self.resized_event_pending = false;
            // run userapp gui function. let user do anything he wants with window or gfx backends
            let output = user_app.run(&egui_context, raw_input, &mut self, &mut gfx_backend);
            if !output.platform_output.copied_text.is_empty() {
                self.window
                    .set_clipboard_string(&output.platform_output.copied_text);
            }
            self.set_cursor(output.platform_output.cursor_icon);
            // prepare egui render data for gfx backend
            let egui_gfx_data = EguiGfxData {
                meshes: egui_context.tessellate(output.shapes),
                textures_delta: output.textures_delta,
                screen_size_logical: [
                    self.size_physical_pixels[0] as f32 / self.scale[0],
                    self.size_physical_pixels[1] as f32 / self.scale[0],
                ],
            };
            // render egui with gfx backend
            gfx_backend.render(egui_gfx_data);
            // present the frame and loop back
            gfx_backend.present(&mut self);
        }
    }

    fn get_config(&self) -> &BackendConfig {
        &self.backend_config
    }

    fn swap_buffers(&mut self) {
        self.window.swap_buffers()
    }

    fn get_proc_address(&mut self, symbol: &str) -> *const core::ffi::c_void {
        self.window.get_proc_address(symbol)
    }
}

impl GlfwBackend {
    pub fn tick(&mut self) {
        self.glfw.poll_events();
        self.frame_events.clear();
        // whether we got a cursor event in this frame.
        // if false, and the window is passthrough, we will manually get cursor pos and push it
        // otherwise, we do nothing.
        let mut cursor_event = false;
        for (_, event) in glfw::flush_messages(&self.events_receiver) {
            self.frame_events.push(event.clone());
            // if let &glfw::WindowEvent::CursorPos(..) = &event {
            //     continue;
            // }

            if let Some(ev) = match event {
                glfw::WindowEvent::FramebufferSize(w, h) => {
                    self.size_physical_pixels = [w as u32, h as u32];
                    self.resized_event_pending = true;
                    self.raw_input.screen_rect = Some(egui::Rect::from_two_pos(
                        Default::default(),
                        [w as f32 / self.scale[0], h as f32 / self.scale[1]].into(),
                    ));

                    None
                }
                glfw::WindowEvent::MouseButton(mb, a, m) => {
                    let emb = Event::PointerButton {
                        pos: Pos2 {
                            x: self.cursor_pos_physical_pixels[0] / self.scale[0],
                            y: self.cursor_pos_physical_pixels[1] / self.scale[1],
                        },
                        button: glfw_to_egui_pointer_button(mb),
                        pressed: glfw_to_egui_action(a),
                        modifiers: glfw_to_egui_modifers(m),
                    };
                    Some(emb)
                }
                // we scroll 25 pixels at a time
                glfw::WindowEvent::Scroll(x, y) => {
                    Some(Event::Scroll([x as f32 * 25.0, y as f32 * 25.0].into()))
                }
                glfw::WindowEvent::Key(k, _, a, m) => match k {
                    glfw::Key::C => {
                        if glfw_to_egui_action(a) && m.contains(glfw::Modifiers::Control) {
                            Some(Event::Copy)
                        } else {
                            None
                        }
                    }
                    glfw::Key::X => {
                        if glfw_to_egui_action(a) && m.contains(glfw::Modifiers::Control) {
                            Some(Event::Cut)
                        } else {
                            None
                        }
                    }
                    glfw::Key::V => {
                        if glfw_to_egui_action(a) && m.contains(glfw::Modifiers::Control) {
                            Some(Event::Text(
                                self.window.get_clipboard_string().unwrap_or_default(),
                            ))
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
                .or_else(|| {
                    glfw_to_egui_key(k).map(|key| Event::Key {
                        key,
                        pressed: glfw_to_egui_action(a),
                        modifiers: glfw_to_egui_modifers(m),
                    })
                }),
                glfw::WindowEvent::Char(c) => Some(Event::Text(c.to_string())),
                glfw::WindowEvent::ContentScale(x, y) => {
                    self.raw_input.pixels_per_point = Some(x);
                    self.scale = [x, y];
                    None
                }
                glfw::WindowEvent::Close => {
                    self.window.set_should_close(true);
                    None
                }

                glfw::WindowEvent::FileDrop(f) => {
                    self.raw_input
                        .dropped_files
                        .extend(f.into_iter().map(|p| egui::DroppedFile {
                            path: Some(p),
                            name: "".to_string(),
                            last_modified: None,
                            bytes: None,
                        }));
                    None
                }
                glfw::WindowEvent::CursorPos(x, y) => {
                    cursor_event = true;
                    self.cursor_pos_physical_pixels =
                        [x as f32 * self.scale[0], y as f32 * self.scale[1]];
                    Some(egui::Event::PointerMoved([x as f32, y as f32].into()))
                }
                _rest => None,
            } {
                self.raw_input.events.push(ev);
            }
        }

        let cursor_position = self.window.get_cursor_pos();
        let cursor_position = [cursor_position.0 as f32, cursor_position.1 as f32];
        // when there's no cursor event and cursor position has changed and window is passthrough
        if !cursor_event
            && cursor_position != self.cursor_pos_physical_pixels
            && self.window.is_mouse_passthrough()
        {
            // we will manually push the cursor moved event.
            self.raw_input.events.push(Event::PointerMoved(
                [
                    cursor_position[0] / self.scale[0],
                    cursor_position[1] / self.scale[1],
                ]
                .into(),
            ))
        }
        self.cursor_pos_physical_pixels = cursor_position;
    }
    fn set_cursor(&mut self, cursor: egui::CursorIcon) {
        let cursor = egui_to_glfw_cursor(cursor);
        if cursor != self.cursor_icon {
            self.cursor_icon = cursor;
            self.window.set_cursor(Some(glfw::Cursor::standard(cursor)));
        }
    }
}

/// a function to get the matching egui key event for a given glfw key. egui does not support all the keys provided here.
fn glfw_to_egui_key(key: glfw::Key) -> Option<Key> {
    match key {
        glfw::Key::Space => Some(Key::Space),
        glfw::Key::Num0 => Some(Key::Num0),
        glfw::Key::Num1 => Some(Key::Num1),
        glfw::Key::Num2 => Some(Key::Num2),
        glfw::Key::Num3 => Some(Key::Num3),
        glfw::Key::Num4 => Some(Key::Num4),
        glfw::Key::Num5 => Some(Key::Num5),
        glfw::Key::Num6 => Some(Key::Num6),
        glfw::Key::Num7 => Some(Key::Num7),
        glfw::Key::Num8 => Some(Key::Num8),
        glfw::Key::Num9 => Some(Key::Num9),
        glfw::Key::A => Some(Key::A),
        glfw::Key::B => Some(Key::B),
        glfw::Key::C => Some(Key::C),
        glfw::Key::D => Some(Key::D),
        glfw::Key::E => Some(Key::E),
        glfw::Key::F => Some(Key::F),
        glfw::Key::G => Some(Key::G),
        glfw::Key::H => Some(Key::H),
        glfw::Key::I => Some(Key::I),
        glfw::Key::J => Some(Key::J),
        glfw::Key::K => Some(Key::K),
        glfw::Key::L => Some(Key::L),
        glfw::Key::M => Some(Key::M),
        glfw::Key::N => Some(Key::N),
        glfw::Key::O => Some(Key::O),
        glfw::Key::P => Some(Key::P),
        glfw::Key::Q => Some(Key::Q),
        glfw::Key::R => Some(Key::R),
        glfw::Key::S => Some(Key::S),
        glfw::Key::T => Some(Key::T),
        glfw::Key::U => Some(Key::U),
        glfw::Key::V => Some(Key::V),
        glfw::Key::W => Some(Key::W),
        glfw::Key::X => Some(Key::X),
        glfw::Key::Y => Some(Key::Y),
        glfw::Key::Z => Some(Key::Z),
        glfw::Key::Escape => Some(Key::Escape),
        glfw::Key::Enter => Some(Key::Enter),
        glfw::Key::Tab => Some(Key::Tab),
        glfw::Key::Backspace => Some(Key::Backspace),
        glfw::Key::Insert => Some(Key::Insert),
        glfw::Key::Delete => Some(Key::Delete),
        glfw::Key::Right => Some(Key::ArrowRight),
        glfw::Key::Left => Some(Key::ArrowLeft),
        glfw::Key::Down => Some(Key::ArrowDown),
        glfw::Key::Up => Some(Key::ArrowUp),
        glfw::Key::PageUp => Some(Key::PageUp),
        glfw::Key::PageDown => Some(Key::PageDown),
        glfw::Key::Home => Some(Key::Home),
        glfw::Key::End => Some(Key::End),
        _ => None,
    }
}

pub fn glfw_to_egui_modifers(modifiers: glfw::Modifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: modifiers.contains(glfw::Modifiers::Alt),
        ctrl: modifiers.contains(glfw::Modifiers::Control),
        shift: modifiers.contains(glfw::Modifiers::Shift),
        mac_cmd: false,
        command: modifiers.contains(glfw::Modifiers::Control),
    }
}

pub fn glfw_to_egui_pointer_button(mb: glfw::MouseButton) -> PointerButton {
    match mb {
        glfw::MouseButton::Button1 => PointerButton::Primary,
        glfw::MouseButton::Button2 => PointerButton::Secondary,
        glfw::MouseButton::Button3 => PointerButton::Middle,
        glfw::MouseButton::Button4 => PointerButton::Extra1,
        glfw::MouseButton::Button5 => PointerButton::Extra2,
        _ => PointerButton::Secondary,
    }
}

pub fn glfw_to_egui_action(a: glfw::Action) -> bool {
    match a {
        Action::Release => false,
        Action::Press => true,
        Action::Repeat => true,
    }
}
/// This converts egui's cursor  icon into glfw's cursor which can be set by glfw.
/// we can get some sample cursor images and use them in place of missing icons (like diagonal resizing cursor)
pub fn egui_to_glfw_cursor(cursor: egui::CursorIcon) -> glfw::StandardCursor {
    match cursor {
        egui::CursorIcon::Default => StandardCursor::Arrow,
        egui::CursorIcon::Crosshair => StandardCursor::Crosshair,
        egui::CursorIcon::VerticalText | egui::CursorIcon::Text => StandardCursor::IBeam,
        egui::CursorIcon::Grab | egui::CursorIcon::Grabbing => StandardCursor::Hand,
        egui::CursorIcon::ResizeColumn
        | egui::CursorIcon::ResizeWest
        | egui::CursorIcon::ResizeEast
        | egui::CursorIcon::ResizeHorizontal => StandardCursor::HResize,
        egui::CursorIcon::ResizeRow
        | egui::CursorIcon::ResizeNorth
        | egui::CursorIcon::ResizeSouth
        | egui::CursorIcon::ResizeVertical => StandardCursor::VResize,
        _ => StandardCursor::Arrow,
    }
}
