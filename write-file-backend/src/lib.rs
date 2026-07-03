//! Minimal Slint platform backend that renders a single frame into an
//! off-screen buffer and writes it to disk as a BMP file, then quits.
//!
//! Unlike the KMS backend, this touches no display device: everything is
//! rendered by the Slint software renderer into an in-memory buffer. It is
//! meant for screenshots and headless smoke tests.
//!
//! Flow: create an off-screen window of the configured size/scale, let the
//! event loop run for a short settle delay (so startup callbacks, layouting and
//! fonts are ready), render one frame, save it as a BMP, and return from the
//! event loop (which makes `Window::run` / `ui.run()` return so the app exits).
//!
//! The output size and UI scale factor are configurable via [`Options`], which
//! can be built from environment variables with [`Options::from_env`]:
//!
//! * `SLINT_WRITE_FILE_PATH`     — output path (default `screenshot.bmp`)
//! * `SLINT_WRITE_FILE_WIDTH`    — output width in physical pixels (default 800)
//! * `SLINT_WRITE_FILE_HEIGHT`   — output height in physical pixels (default 600)
//! * `SLINT_WRITE_FILE_SCALE`    — UI scale factor (default 1.0)
//! * `SLINT_WRITE_FILE_DELAY_MS` — settle delay before the screenshot (default 100)

mod bmp;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use i_slint_core::api::{PhysicalSize, Window};
use i_slint_core::graphics::Rgb8Pixel;
use i_slint_core::platform::{
    EventLoopProxy, Platform, PlatformError, Renderer, WindowAdapter, WindowEvent,
    duration_until_next_timer_update, update_timers_and_animations,
};
use i_slint_renderer_software::{RepaintBufferType, SoftwareRenderer};

/// Configuration for the off-screen render and the resulting file.
#[derive(Clone, Debug)]
pub struct Options {
    /// Output width in physical pixels.
    pub width: u32,
    /// Output height in physical pixels.
    pub height: u32,
    /// UI scale factor. The logical size handed to the app is
    /// `(width, height) / scale_factor`.
    pub scale_factor: f32,
    /// Where the BMP is written.
    pub path: PathBuf,
    /// How long to run the event loop before taking the screenshot.
    pub delay: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            width: 800,
            height: 600,
            scale_factor: 1.0,
            path: PathBuf::from("screenshot.bmp"),
            delay: Duration::from_millis(100),
        }
    }
}

impl Options {
    /// Build [`Options`], overriding the defaults from `SLINT_WRITE_FILE_*`
    /// environment variables. Malformed values fall back to the default.
    pub fn from_env() -> Self {
        let mut opts = Self::default();
        if let Some(v) = env_parse("SLINT_WRITE_FILE_WIDTH") {
            opts.width = v;
        }
        if let Some(v) = env_parse("SLINT_WRITE_FILE_HEIGHT") {
            opts.height = v;
        }
        if let Some(v) = env_parse("SLINT_WRITE_FILE_SCALE") {
            opts.scale_factor = v;
        }
        if let Some(v) = env_parse::<u64>("SLINT_WRITE_FILE_DELAY_MS") {
            opts.delay = Duration::from_millis(v);
        }
        if let Ok(path) = std::env::var("SLINT_WRITE_FILE_PATH") {
            opts.path = PathBuf::from(path);
        }
        opts
    }
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// Event loop proxy: forwards `invoke_from_event_loop` callbacks over a channel
/// that the run loop drains, and lets the app quit the loop early.
#[derive(Clone)]
struct Proxy {
    sender: Arc<Mutex<Sender<Box<dyn FnOnce() + Send>>>>,
    quit: Arc<AtomicBool>,
}

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), i_slint_core::api::EventLoopError> {
        self.quit.store(true, Ordering::Release);
        Ok(())
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), i_slint_core::api::EventLoopError> {
        self.sender
            .lock()
            .unwrap()
            .send(event)
            .map_err(|_| i_slint_core::api::EventLoopError::EventLoopTerminated)
    }
}

pub struct Backend {
    options: Options,
    window: RefCell<Option<Rc<WriteFileWindowAdapter>>>,
    receiver: RefCell<Option<Receiver<Box<dyn FnOnce() + Send>>>>,
    proxy: Proxy,
}

impl Backend {
    /// Create a backend configured from `SLINT_WRITE_FILE_*` environment
    /// variables (see the crate docs).
    pub fn new() -> Self {
        Self::with_options(Options::from_env())
    }

    /// Create a backend with explicit [`Options`].
    pub fn with_options(options: Options) -> Self {
        let (sender, receiver) = channel();
        Self {
            options,
            window: RefCell::new(None),
            receiver: RefCell::new(Some(receiver)),
            proxy: Proxy {
                sender: Arc::new(Mutex::new(sender)),
                quit: Arc::new(AtomicBool::new(false)),
            },
        }
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for Backend {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let adapter = WriteFileWindowAdapter::new(PhysicalSize::new(
            self.options.width,
            self.options.height,
        ));
        *self.window.borrow_mut() = Some(adapter.clone());
        Ok(adapter)
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(self.proxy.clone()))
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let adapter = self
            .window
            .borrow()
            .clone()
            .ok_or_else(|| PlatformError::from("No window was created".to_string()))?;

        let receiver = self
            .receiver
            .borrow_mut()
            .take()
            .ok_or_else(|| PlatformError::from("Event loop already ran".to_string()))?;

        // Apply the configured scale factor first, then the size, so the logical
        // size the app lays out at is derived from the requested physical size.
        adapter.window().dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: self.options.scale_factor,
        });
        adapter.apply_configured_size();

        // Let the app settle: run timers/animations and any queued callbacks for
        // the configured delay before capturing the frame.
        let start = Instant::now();
        while start.elapsed() < self.options.delay && !self.proxy.quit.load(Ordering::Acquire) {
            update_timers_and_animations();
            while let Ok(callback) = receiver.try_recv() {
                callback();
            }

            let remaining = self.options.delay.saturating_sub(start.elapsed());
            let sleep = duration_until_next_timer_update()
                .unwrap_or(remaining)
                .min(remaining)
                .min(Duration::from_millis(10));
            std::thread::sleep(sleep);
        }
        update_timers_and_animations();

        // Capture one frame into an off-screen buffer and write it to disk.
        adapter
            .capture_to_file(&self.options.path)
            .map_err(|e| PlatformError::from(format!("Error writing screenshot: {e}")))?;

        Ok(())
    }
}

/// An off-screen window backed by the software renderer. It never presents to a
/// display; instead [`Self::capture_to_file`] renders one frame into a buffer.
struct WriteFileWindowAdapter {
    window: Window,
    renderer: SoftwareRenderer,
    size: Cell<PhysicalSize>,
}

impl WriteFileWindowAdapter {
    fn new(size: PhysicalSize) -> Rc<Self> {
        Rc::<Self>::new_cyclic(|weak| Self {
            window: Window::new(weak.clone()),
            renderer: SoftwareRenderer::new_with_repaint_buffer_type(RepaintBufferType::NewBuffer),
            size: Cell::new(size),
        })
    }

    /// (Re-)dispatch the configured physical size as a logical resize using the
    /// window's current scale factor.
    fn apply_configured_size(&self) {
        let physical = self.size.get();
        let logical = physical.to_logical(self.window.scale_factor());
        self.window.dispatch_event(WindowEvent::Resized { size: logical });
    }

    fn capture_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let size = self.size.get();
        let (width, height) = (size.width, size.height);
        let mut buffer = vec![Rgb8Pixel { r: 0, g: 0, b: 0 }; (width * height) as usize];
        self.renderer.render(buffer.as_mut_slice(), width as usize);
        bmp::write_bmp(path, width, height, &buffer)
    }
}

impl WindowAdapter for WriteFileWindowAdapter {
    fn window(&self) -> &Window {
        &self.window
    }

    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, size: i_slint_core::api::WindowSize) {
        let sf = self.window.scale_factor();
        self.size.set(size.to_physical(sf));
        self.window
            .dispatch_event(WindowEvent::Resized { size: size.to_logical(sf) });
    }
}
