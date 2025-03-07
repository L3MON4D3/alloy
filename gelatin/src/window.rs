use std::{
    cell::{Cell, RefCell, RefMut},
    cmp::Eq,
    hash::{Hash, Hasher},
    ops::{Deref, DerefMut},
    rc::Rc,
};

use cgmath::{ortho, Matrix4, Vector3};
use glium::{
    glutin::{
        self,
        dpi::{PhysicalPosition, PhysicalSize},
        event::WindowEvent,
        window::{CursorIcon, Icon, WindowId},
        platform::unix::WindowBuilderExtUnix,
    },
    program, uniform, Blend, BlendingFunction, Display, Frame, IndexBuffer,
    Program, Rect, Surface, VertexBuffer,
};
use typed_builder::TypedBuilder;

use crate::{
    application::Application,
    misc::{FromPhysical, LogicalRect, LogicalVector},
    shaders, DrawContext, Event, EventKind, NextUpdate, Vertex, Widget,
};

const EVENT_UPDATE_DELTA: std::time::Duration =
    std::time::Duration::from_millis(2);

/// Stores whether the window contets need to be re-rendered.
///
/// Widgets must call `invalidate` whenever they go through a
/// a change that requires the widget to be re-drawn.
///
/// This object holds a reference counted bool.
#[derive(Debug, Clone, Default)]
pub struct RenderValidity {
    validity: Rc<Cell<bool>>,
}
impl RenderValidity {
    pub fn invalidate(&self) {
        self.validity.set(false);
    }

    pub fn get(&self) -> bool {
        self.validity.get()
    }

    /// Private accessability because this is only allowed for the window.
    fn make_valid(&self) {
        self.validity.set(true);
    }
}

pub struct WindowDisplayRefMut<'a> {
    window_ref: RefMut<'a, WindowData>,
}
impl<'a> Deref for WindowDisplayRefMut<'a> {
    type Target = Display;

    fn deref(&self) -> &Self::Target {
        &self.window_ref.display
    }
}
impl<'a> DerefMut for WindowDisplayRefMut<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.window_ref.display
    }
}

#[derive(TypedBuilder, Clone)]
pub struct WindowDescriptor {
    #[builder(setter(into), default)]
    icon: Option<Icon>,

    #[builder(setter(into), default = PhysicalSize::<u32>::new(800, 600))]
    size: PhysicalSize<u32>,

    #[builder(setter(into), default)]
    position: Option<PhysicalPosition<i32>>,
}

type WindowGlobalEventHandler = Box<dyn FnMut(&WindowEvent)>;

struct WindowData {
    display: glium::Display,
    size_before_fullscreen: PhysicalSize<u32>,
    fullscreen: bool,
    last_mouse_move_update_time: std::time::Instant,
    unprocessed_move_event: Option<Event>,
    last_event_invalidated: bool,
    should_sleep: bool,

    new_title: Option<String>,

    render_validity: RenderValidity,
    cursor_pos: LogicalVector,
    modifiers: glutin::event::ModifiersState,
    root_widget: Rc<dyn Widget>,
    bg_color: [f32; 4],

    global_event_handlers: Vec<WindowGlobalEventHandler>,

    // Draw data
    unit_quad_vertices: VertexBuffer<Vertex>,
    unit_quad_indices: IndexBuffer<u16>,
    textured_program: Program,
    colored_shadowed_program: Program,
    colored_program: Program,
}

pub struct Window {
    data: RefCell<WindowData>,
}
impl Hash for Window {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.data.as_ptr().hash(state);
    }
}
impl PartialEq for Window {
    fn eq(&self, other: &Window) -> bool {
        self.data.as_ptr() == other.data.as_ptr()
    }
}
impl Eq for Window {}

impl Window {
    pub fn new(
        application: &mut Application,
        desc: WindowDescriptor,
    ) -> Rc<Self> {
        //use glium::glutin::window::Icon;
        //let exe_parent = std::env::current_exe().unwrap().parent().unwrap().to_owned();

        let window = glutin::window::WindowBuilder::new()
            .with_title("Loading")
            .with_fullscreen(None)
            .with_name("alloy", "alloy")
            .with_inner_size(desc.size)
            .with_window_icon(desc.icon)
            .with_visible(desc.position.is_none());

        let context = glutin::ContextBuilder::new()
            .with_gl_profile(glutin::GlProfile::Core)
            .with_vsync(true);
        let display =
            glium::Display::new(window, context, &application.event_loop)
                .unwrap();

        if let Some(pos) = desc.position {
            display.gl_window().window().set_outer_position(pos);
            display.gl_window().window().set_visible(true);
        }

        display
            .gl_window()
            .window()
            .set_cursor_icon(CursorIcon::Default);

        // All the draw stuff
        use glium::index::PrimitiveType;
        let vertex_buffer = {
            VertexBuffer::new(
                &display,
                &[
                    Vertex {
                        position: [0.0, 0.0],
                        tex_coords: [0.0, 0.0],
                    },
                    Vertex {
                        position: [0.0, 1.0],
                        tex_coords: [0.0, 1.0],
                    },
                    Vertex {
                        position: [1.0, 1.0],
                        tex_coords: [1.0, 1.0],
                    },
                    Vertex {
                        position: [1.0, 0.0],
                        tex_coords: [1.0, 0.0],
                    },
                ],
            )
            .unwrap()
        };

        // building the index buffer
        let index_buffer = IndexBuffer::new(
            &display,
            PrimitiveType::TriangleStrip,
            &[1_u16, 2, 0, 3],
        )
        .unwrap();

        // compiling shaders and linking them together
        let textured_program = program!(&display,
            140 => {
                vertex: shaders::VERTEX_140,
                fragment: shaders::TEXTURE_SHADOW_F_140
            },
            110 => {
                vertex: shaders::VERTEX_110,
                fragment: shaders::TEXTURE_SHADOW_F_110
            },
        )
        .unwrap();
        let colored_shadowed_program = program!(&display,
            140 => {
                vertex: shaders::VERTEX_140,
                fragment: shaders::COLOR_SHADOW_F_140
            },
            110 => {
                vertex: shaders::VERTEX_110,
                fragment: shaders::COLOR_SHADOW_F_110
            },
        )
        .unwrap();
        let colored_program = program!(&display,
            140 => {
                vertex: shaders::VERTEX_140,
                fragment: shaders::COLOR_F_140
            },
            110 => {
                vertex: shaders::VERTEX_110,
                fragment: shaders::COLOR_F_110
            },
        )
        .unwrap();

        let resulting_window = Rc::new(Window {
            data: RefCell::new(WindowData {
                display,
                size_before_fullscreen: desc.size,
                fullscreen: false,
                last_mouse_move_update_time: std::time::Instant::now(),
                unprocessed_move_event: None,
                last_event_invalidated: true,
                should_sleep: false,
                new_title: None,
                cursor_pos: Default::default(),
                modifiers: glutin::event::ModifiersState::empty(),
                render_validity: RenderValidity {
                    validity: Rc::new(Cell::new(false)),
                },
                root_widget: Rc::new(
                    crate::line_layout_container::VerticalLayoutContainer::new(
                    ),
                ),
                bg_color: [0.85, 0.85, 0.85, 1.0],

                global_event_handlers: Vec::new(),

                unit_quad_vertices: vertex_buffer,
                unit_quad_indices: index_buffer,
                textured_program,
                colored_shadowed_program,
                colored_program,
            }),
        });

        application.register_window(resulting_window.clone());
        resulting_window
    }

    pub fn add_global_event_handler<F: FnMut(&WindowEvent) + 'static>(
        &self,
        fun: F,
    ) {
        let mut borrowed = self.data.borrow_mut();
        borrowed.global_event_handlers.push(Box::new(fun));
    }

    pub fn set_root<T: Widget>(&self, widget: Rc<T>) {
        let mut borrowed = self.data.borrow_mut();
        widget.set_valid_ref(borrowed.render_validity.clone());
        borrowed.root_widget = widget;
        borrowed.render_validity.invalidate();
    }

    pub fn set_bg_color(&self, color: [f32; 4]) {
        let mut borrowed = self.data.borrow_mut();
        borrowed.bg_color = color;
    }

    pub fn process_event(&self, native_event: WindowEvent) {
        use glutin::event::MouseScrollDelta;

        let event;
        {
            let mut borrowed = self.data.borrow_mut();
            for handler in borrowed.global_event_handlers.iter_mut() {
                handler(&native_event);
            }
            match native_event {
                WindowEvent::CloseRequested => {
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::CloseRequested,
                    });
                }
                WindowEvent::KeyboardInput { input, .. } => {
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::KeyInput { input },
                    });
                }
                WindowEvent::ReceivedCharacter(ch) => {
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::ReceivedCharacter(ch),
                    });
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let logical_pos;
                    {
                        let gl_window = borrowed.display.gl_window();
                        let window = gl_window.window();

                        let scaling = window.scale_factor() as f32;

                        logical_pos =
                            LogicalVector::from_physical(position, scaling);
                        //logical_pos.vec.y = logical_dimensions.vec.y - logical_pos.vec.y;
                    }
                    borrowed.cursor_pos = logical_pos;
                    let move_event = Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::MouseMove,
                    };
                    let last_update_elapsed =
                        borrowed.last_mouse_move_update_time.elapsed();
                    if borrowed.last_event_invalidated
                        || last_update_elapsed > EVENT_UPDATE_DELTA
                    {
                        borrowed.last_mouse_move_update_time =
                            std::time::Instant::now();
                        event = Some(move_event);
                    } else {
                        event = None;
                        borrowed.unprocessed_move_event = Some(move_event);
                    }
                }
                WindowEvent::MouseWheel {
                    delta: native_delta,
                    ..
                } => {
                    let delta = match native_delta {
                        MouseScrollDelta::LineDelta(x, y) => {
                            LogicalVector::new(x, y)
                        }
                        MouseScrollDelta::PixelDelta(native_pos) => {
                            LogicalVector::new(
                                native_pos.x as f32 / 13.0,
                                native_pos.y as f32 / 8.0,
                            )
                        }
                    };
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::MouseScroll { delta },
                    });
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::MouseButton { state, button },
                    });
                }
                WindowEvent::DroppedFile(path) => {
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::DroppedFile(path),
                    });
                }
                WindowEvent::HoveredFile(path) => {
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::HoveredFile(path),
                    });
                }
                WindowEvent::HoveredFileCancelled => {
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::HoveredFileCancelled,
                    });
                }
                WindowEvent::Focused(focused) => {
                    event = Some(Event {
                        cursor_pos: borrowed.cursor_pos,
                        modifiers: borrowed.modifiers,
                        kind: EventKind::Focused(focused),
                    });
                }
                WindowEvent::ModifiersChanged(modifiers) => {
                    borrowed.modifiers = modifiers;
                    event = None;
                }
                _ => event = None,
            }
        }

        if let Some(event) = event {
            let cloned = self.data.borrow().root_widget.clone();
            cloned.handle_event(&event);
            let mut borrowed = self.data.borrow_mut();
            borrowed.should_sleep = false;
            if borrowed.render_validity.get() {
                if let EventKind::MouseMove = event.kind {
                    borrowed.should_sleep = true;
                }
            } else {
                borrowed.last_event_invalidated = true;
            }
        }
    }

    pub fn should_sleep(&self) -> bool {
        self.data.borrow().should_sleep
    }

    pub fn set_title(&self, title: String) {
        // Deferring to set the title later, because
        // the program sometimes crashes on wayland if this is
        // done in the `MainEventsCleared` event.
        let mut borrowed = self.data.borrow_mut();
        borrowed.new_title = Some(title);
        borrowed.render_validity.invalidate();
    }

    pub fn display_mut(&self) -> WindowDisplayRefMut<'_> {
        WindowDisplayRefMut {
            window_ref: self.data.borrow_mut(),
        }
    }

    pub fn get_id(&self) -> WindowId {
        self.data.borrow().display.gl_window().window().id()
    }

    pub fn request_redraw(&self) {
        self.data
            .borrow_mut()
            .display
            .gl_window()
            .window()
            .request_redraw();
    }

    pub fn main_events_cleared(&self) -> NextUpdate {
        // this way self.data is not borrowed while `before_draw` is running.
        let root_widget = self.data.borrow().root_widget.clone();
        if let Some(event) =
            self.data.borrow_mut().unprocessed_move_event.take()
        {
            root_widget.handle_event(&event);
        }
        root_widget.before_draw(self)
    }

    pub fn redraw_needed(&self) -> bool {
        !self.data.borrow().render_validity.get()
    }

    /// WARNING The window may not be changed during the drawing phase.
    /// This means that trying to borrow the window *mutably* in a widget's
    /// draw function will fail.
    pub fn redraw(&self) -> crate::NextUpdate {
        // Using a scope to only borrow the data mutable for the very beggining.
        {
            let mut borrowed = self.data.borrow_mut();
            if let Some(new_title) = borrowed.new_title.take() {
                borrowed.display.gl_window().window().set_title(&new_title);
            }
            borrowed.last_event_invalidated = false;
        }
        // this way self.data is not borrowed while before draw is running.
        let dpi_scaling = self
            .data
            .borrow()
            .display
            .gl_window()
            .window()
            .scale_factor();
        let mut target = self.data.borrow().display.draw();

        // Can't change the window during drawing phase. Deal with it.
        let borrowed = self.data.borrow();
        let dimensions = target.get_dimensions();
        let phys_dimensions = glutin::dpi::PhysicalSize::new(
            dimensions.0 as f32,
            dimensions.1 as f32,
        );
        let phys_width = phys_dimensions.width;
        let phys_height = phys_dimensions.height;
        let logical_dimensions =
            LogicalVector::from_physical(phys_dimensions, dpi_scaling as f32);

        // Invoke the layout functions
        let available_widget_space = LogicalRect {
            pos: LogicalVector::new(0.0, 0.0),
            size: logical_dimensions,
        };
        borrowed.root_widget.layout(available_widget_space);

        let left = 0f32;
        let right = logical_dimensions.vec.x;
        let bottom = logical_dimensions.vec.y;
        let top = 0f32;
        let projection_transform = ortho(left, right, bottom, top, -1f32, 1f32);

        let viewport = Rect {
            left: 0_u32,
            width: phys_width as u32,
            bottom: 0_u32,
            height: phys_height as u32,
        };

        let draw_context = DrawContext {
            display: &borrowed.display,
            dpi_scale_factor: dpi_scaling as f32,
            unit_quad_vertices: &borrowed.unit_quad_vertices,
            unit_quad_indices: &borrowed.unit_quad_indices,
            textured_program: &borrowed.textured_program,
            colored_shadowed_program: &borrowed.colored_shadowed_program,
            colored_program: &borrowed.colored_program,
            viewport: &viewport,
            projection_transform: &projection_transform,
        };

        // Clearing the framebuffer with fully black
        // then drawing a full-screen quad to emulate colored clearing.
        // This is a workaround for https://github.com/glium/glium/issues/1842
        target.clear_color(0.0, 0.0, 0.0, 1.0);
        draw_context.clear_color(&mut target, borrowed.bg_color, None);

        // Using the cloned root instead of self.root_widget doesn't make much difference
        // because self is being borrowed by through the draw_context anyways but it's fine.
        let retval = borrowed
            .root_widget
            .draw(&mut target, &draw_context)
            .unwrap();

        // After all widgets are drawn, let's set the alpha values of all the pixels to 1.
        // This is required on Wayland because the Wayland compositor very kindly takes
        // the alpha values into account and blends the framebuffer set by applications
        // with the rest of the desktop.
        self.set_alpha_to_1(&mut target, &draw_context);

        target.finish().unwrap();
        borrowed.render_validity.make_valid();
        retval
    }

    pub fn fullscreen(&self) -> bool {
        self.data.borrow().fullscreen
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        let mut borrowed = self.data.borrow_mut();
        borrowed.fullscreen = fullscreen;
        let monitor = if fullscreen {
            let curr_mon;
            borrowed.size_before_fullscreen = {
                let gl_win = borrowed.display.gl_window();
                curr_mon = gl_win.window().current_monitor();
                gl_win.window().inner_size()
            };
            Some(glutin::window::Fullscreen::Borderless(curr_mon))
        } else {
            None
        };
        let gl_win = borrowed.display.gl_window();
        gl_win.window().set_fullscreen(monitor);
    }

    pub fn set_maximized(&self, maximized: bool) {
        self.display_mut()
            .gl_window()
            .window()
            .set_maximized(maximized);
    }

    /// Sets the alpha values by drawing a quad covering the entire framebuffer
    /// with a blending mode set to max and a shader that draws (0,0,0,1) values
    fn set_alpha_to_1(&self, target: &mut Frame, context: &DrawContext) {
        let transform = Matrix4::from_scale(2.0);
        let transform =
            Matrix4::from_translation(Vector3::new(-1.0, -1.0, 0.0))
                * transform;
        let image_draw_params = glium::DrawParameters {
            blend: Blend {
                color: BlendingFunction::Max,
                alpha: BlendingFunction::Max,
                ..Default::default()
            },
            ..Default::default()
        };
        let uniforms = uniform! {
            matrix: Into::<[[f32; 4]; 4]>::into(transform),
            color: [0.0f32, 0.0, 0.0, 1.0],
        };
        target
            .draw(
                context.unit_quad_vertices,
                context.unit_quad_indices,
                context.colored_program,
                &uniforms,
                &image_draw_params,
            )
            .unwrap();
    }
}
