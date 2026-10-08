use anyhow::{anyhow, Context, Result};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum MediaKeyCommand {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    SeekBy(i64),
    SeekTo(Duration),
    SetVolume(f64),
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaPlaybackStatus {
    Stopped,
    Paused,
    Playing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub position: Option<Duration>,
    pub status: MediaPlaybackStatus,
    pub volume: u8,
}

pub struct MediaIntegration {
    controls: MediaControls,
    events: mpsc::Receiver<MediaKeyCommand>,
    last_now_playing: Option<NowPlaying>,
}

impl MediaIntegration {
    pub fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let config = PlatformConfig {
            display_name: "DISC",
            dbus_name: "disc",
            hwnd: platform_hwnd().context("Could not prepare OS media-control window handle")?,
        };
        let mut controls = MediaControls::new(config).context("Could not create OS media controls")?;
        controls
            .attach(move |event| {
                if let Some(command) = media_event_to_command(event) {
                    let _ = tx.send(command);
                }
            })
            .context("Could not attach OS media key handler")?;
        Ok(Self {
            controls,
            events: rx,
            last_now_playing: None,
        })
    }

    pub fn drain_commands(&mut self) -> Vec<MediaKeyCommand> {
        let mut commands = Vec::new();
        while let Ok(command) = self.events.try_recv() {
            commands.push(command);
        }
        commands
    }

    pub fn update(&mut self, now_playing: NowPlaying) -> Result<()> {
        if self.last_now_playing.as_ref() == Some(&now_playing) {
            return Ok(());
        }

        self.controls.set_metadata(MediaMetadata {
            title: now_playing.title.as_deref(),
            artist: now_playing.artist.as_deref(),
            album: now_playing.album.as_deref(),
            duration: now_playing.duration,
            ..Default::default()
        })?;

        let progress = now_playing.position.map(MediaPosition);
        let playback = match now_playing.status {
            MediaPlaybackStatus::Stopped => MediaPlayback::Stopped,
            MediaPlaybackStatus::Paused => MediaPlayback::Paused { progress },
            MediaPlaybackStatus::Playing => MediaPlayback::Playing { progress },
        };
        self.controls.set_playback(playback)?;
        // souvlaki 0.8.x does not expose a MediaControls::set_volume method.
        // DISC still accepts OS SetVolume events where the platform sends them,
        // but outbound now-playing metadata is limited to track, position,
        // duration, and playback state for this backend version.
        self.last_now_playing = Some(now_playing);
        Ok(())
    }
}

fn media_event_to_command(event: MediaControlEvent) -> Option<MediaKeyCommand> {
    match event {
        MediaControlEvent::Play => Some(MediaKeyCommand::Play),
        MediaControlEvent::Pause => Some(MediaKeyCommand::Pause),
        MediaControlEvent::Toggle => Some(MediaKeyCommand::Toggle),
        MediaControlEvent::Next => Some(MediaKeyCommand::Next),
        MediaControlEvent::Previous => Some(MediaKeyCommand::Previous),
        MediaControlEvent::Stop => Some(MediaKeyCommand::Stop),
        MediaControlEvent::Seek(direction) => Some(match direction {
            SeekDirection::Forward => MediaKeyCommand::SeekBy(10_000),
            SeekDirection::Backward => MediaKeyCommand::SeekBy(-10_000),
        }),
        MediaControlEvent::SeekBy(direction, duration) => {
            let millis = duration.as_millis().min(i64::MAX as u128) as i64;
            Some(match direction {
                SeekDirection::Forward => MediaKeyCommand::SeekBy(millis),
                SeekDirection::Backward => MediaKeyCommand::SeekBy(-millis),
            })
        }
        MediaControlEvent::SetPosition(position) => Some(MediaKeyCommand::SeekTo(position.0)),
        MediaControlEvent::SetVolume(volume) => Some(MediaKeyCommand::SetVolume(volume)),
        MediaControlEvent::Quit => Some(MediaKeyCommand::Quit),
        MediaControlEvent::OpenUri(_) | MediaControlEvent::Raise => None,
    }
}

#[cfg(target_os = "windows")]
fn platform_hwnd() -> Result<Option<*mut std::ffi::c_void>> {
    create_hidden_windows_media_hwnd().map(Some)
}

#[cfg(target_os = "windows")]
fn create_hidden_windows_media_hwnd() -> Result<*mut std::ffi::c_void> {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::sync::OnceLock;

    const DISC_MEDIA_CLASS: &str = "DISC_Hidden_Media_Window";

    #[repr(C)]
    struct WndClassW {
        style: u32,
        lpfn_wnd_proc: Option<unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize>,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: *mut c_void,
        h_icon: *mut c_void,
        h_cursor: *mut c_void,
        hbr_background: *mut c_void,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(lp_module_name: *const u16) -> *mut c_void;
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassW(lp_wnd_class: *const WndClassW) -> u16;
        fn CreateWindowExW(
            dw_ex_style: u32,
            lp_class_name: *const u16,
            lp_window_name: *const u16,
            dw_style: u32,
            x: i32,
            y: i32,
            n_width: i32,
            n_height: i32,
            hwnd_parent: *mut c_void,
            h_menu: *mut c_void,
            h_instance: *mut c_void,
            lp_param: *mut c_void,
        ) -> *mut c_void;
        fn DefWindowProcW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    unsafe extern "system" fn disc_media_window_proc(
        hwnd: *mut c_void,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(std::iter::once(0)).collect()
    }

    static HWND_CACHE: OnceLock<Result<usize, String>> = OnceLock::new();
    let hwnd_result = HWND_CACHE.get_or_init(|| {
        let class_name = wide(DISC_MEDIA_CLASS);
        let window_name = wide("DISC media controls");
        let h_instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        if h_instance.is_null() {
            return Err("GetModuleHandleW returned a null module handle".to_string());
        }

        let wnd_class = WndClassW {
            style: 0,
            lpfn_wnd_proc: Some(disc_media_window_proc),
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance,
            h_icon: std::ptr::null_mut(),
            h_cursor: std::ptr::null_mut(),
            hbr_background: std::ptr::null_mut(),
            lpsz_menu_name: std::ptr::null(),
            lpsz_class_name: class_name.as_ptr(),
        };

        // RegisterClassW returns zero if the class is already registered or if
        // registration failed. Creating the window is the authoritative check,
        // so continue either way and report a clearer error if creation fails.
        let _ = unsafe { RegisterClassW(&wnd_class) };

        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_name.as_ptr(),
                0,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                h_instance,
                std::ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            Err("CreateWindowExW returned a null HWND for the hidden media-control window".to_string())
        } else {
            Ok(hwnd as usize)
        }
    });

    match hwnd_result {
        Ok(hwnd) => Ok(*hwnd as *mut std::ffi::c_void),
        Err(message) => Err(anyhow!(message.clone())),
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_hwnd() -> Result<Option<*mut std::ffi::c_void>> {
    Ok(None)
}
