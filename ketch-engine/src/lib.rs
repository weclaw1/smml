use ketch_editor::Editor;
use std::error::Error;
use ketch_core::input::input_event::InputEvent;
use ketch_core::resource::AssetManager;
use ketch_core::renderer::{Renderer};
use ketch_core::settings::Settings;
use ketch_core::input::InputSystem;
use ketch_core::input;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use fps_counter::FPSCounter;

use structopt::StructOpt;

use log::*;

#[derive(StructOpt, Debug)]
#[structopt(name = "basic")]
struct Opts {
    /// Activate GUI Editor
    #[structopt(short = "g", long = "gui-editor")]
    gui_editor: bool,
}

/// A struct representing the top level of this engine.
/// It provides access to all the subsystems that can be used.
pub struct Engine {
    renderer: Renderer,
    asset_manager: AssetManager,
    input_system: InputSystem,
    editor: Option<Editor>,
    settings: Rc<RefCell<Settings>>,
}

impl Engine {
    /// Creates and returns a new instance of this engine.
    pub fn new(mut settings: Settings) -> Self {
        let opts = Opts::from_args();
        settings.set_gui_editor(opts.gui_editor);
        let settings = Rc::new(RefCell::new(settings));

        let mut input_system = InputSystem::new(settings.clone());
        let renderer = match Renderer::new(settings.clone(), input_system.events_loop()) {
            Ok(renderer) => renderer,
            Err(e) => {
                error!("Error: {}", e);
                error!("Caused by: {}", e.cause().unwrap());
                panic!("Couldn't create renderer!");
            },
        };
        input_system.set_surface(renderer.surface());
        let asset_manager = AssetManager::new(settings.clone(), renderer.queues(), renderer.device());

        let editor = if opts.gui_editor {
            Some(Editor::new(settings.clone(), &renderer))
        } else {
            None
        };
        
        Engine {
            renderer,
            asset_manager,
            input_system,
            settings,
            editor,
        }
    }

    /// Returns settings used by this engine.
    pub fn settings(&self) -> Rc<RefCell<Settings>> {
        self.settings.clone()
    }

    /// Returns a reference to input system, which updates input mapping implemented by the user.
    pub fn input_system_mut(&mut self) -> &mut InputSystem {
        &mut self.input_system
    }

    /// Returns a mutable reference to the asset manager.
    pub fn asset_manager_mut(&mut self) -> &mut AssetManager {
        &mut self.asset_manager
    }

    pub fn run<S: EventHandler>(&mut self, mut state: S) {
        let mut fps_counter = FPSCounter::new();
        let log_fps_frequency = self.settings.borrow().log_fps_frequency();
        let time_per_update = self.settings.borrow().time_per_update();

        let mut last_fps_counter_log = Instant::now();
        let mut previous_time = Instant::now();
        let mut lag = Duration::new(0, 0);

        state.init(self.settings.clone(), &mut self.asset_manager);

        loop {
            let elapsed = previous_time.elapsed();
            previous_time = Instant::now();
            lag += elapsed;
            
            let pending_events = self.input_system.fetch_pending_events();

            if let Some(editor) = &mut self.editor {
                editor.handle_input(self.input_system.window().unwrap(), pending_events.clone());
            }
            state.process_input(input::convert_to_input_events(pending_events));

            while lag >= time_per_update {
                state.update(&mut self.settings.borrow_mut(), &mut self.asset_manager, time_per_update);

                lag -= time_per_update;
            }

            let (image_num, acquire_future, mut command_buffer) = match self.renderer.render(&mut self.asset_manager) {
                Ok(res) => res,
                Err(err) => {
                    error!("Couldn't render frame: {}", err);
                    continue;
                }
            };

            if let Some(editor) = &mut self.editor {
                command_buffer = editor.create_gui_command_buffer(self.renderer.queues().graphics_queue(), command_buffer, image_num);
            }

            match self.renderer.execute_command_buffer(image_num, acquire_future, command_buffer) {
                Ok(()) => {
                    let fps = fps_counter.tick();
                    if last_fps_counter_log.elapsed() >= log_fps_frequency {
                        info!("Current FPS: {}", fps);
                        last_fps_counter_log = Instant::now();
                    }
                },
                Err(err) => error!("Couldn't execute command buffer for frame: {}", err),
            }
        }
    }
}

pub trait EventHandler {
    fn process_input(&mut self, input_events: Vec<InputEvent>);
    fn update(&mut self, settings: &mut Settings, asset_manager: &mut AssetManager, elapsed_time: Duration);
    fn init(&mut self, settings: Rc<RefCell<Settings>>, asset_manager: &mut AssetManager);
}