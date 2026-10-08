use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use reqwest::blocking::Client;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use std::io::Cursor;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct PlaybackTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub server_alias: String,
    pub stream_url: String,
}

impl PlaybackTrack {
    pub fn label(&self) -> String {
        format!("{} — {} • {} [{}]", self.title, self.artist, self.album, self.server_alias)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
    Buffering,
    Error,
}

#[derive(Clone, Debug)]
pub struct PlaybackSnapshot {
    pub state: PlaybackState,
    pub current: Option<PlaybackTrack>,
    pub volume: u8,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    pub output_device: Option<String>,
    pub error: Option<String>,
    pub just_finished: bool,
    pub gapless_started: Option<PlaybackTrack>,
}

#[derive(Clone, Debug)]
pub struct PlaybackAudioStatus {
    pub active_output: Option<String>,
    pub default_output: Option<String>,
    pub output_devices: Vec<String>,
    pub output_error: Option<String>,
    pub last_reset_error: Option<String>,
}

struct EngineState {
    state: PlaybackState,
    current: Option<PlaybackTrack>,
    started_at: Option<Instant>,
    paused_accumulated: Duration,
    pause_started_at: Option<Instant>,
    duration: Option<Duration>,
    volume: u8,
    current_output_name: Option<String>,
    last_audio_reset_error: Option<String>,
    error: Option<String>,
    just_finished: bool,
    gapless_started: Option<PlaybackTrack>,
    finished_reported: bool,
    error_reported: bool,
}

impl EngineState {
    fn new() -> Self {
        Self {
            state: PlaybackState::Stopped,
            current: None,
            started_at: None,
            paused_accumulated: Duration::ZERO,
            pause_started_at: None,
            duration: None,
            volume: 80,
            current_output_name: None,
            last_audio_reset_error: None,
            error: None,
            just_finished: false,
            gapless_started: None,
            finished_reported: false,
            error_reported: false,
        }
    }
}

enum PlaybackCommand {
    PlayTrack(PlaybackTrack),
    PrepareNext(Option<PlaybackTrack>),
    InvalidatePreparedNext,
    Pause,
    Resume,
    Stop,
    SetVolume(u8),
    Seek(Duration),
    ResetAudioOutput,
    Tick,
    Shutdown,
}

enum PrefetchCommand {
    Prepare { request_id: u64, track: PlaybackTrack },
    Clear,
    Shutdown,
}

struct PrefetchResult {
    request_id: u64,
    track: PlaybackTrack,
    result: std::result::Result<Vec<u8>, String>,
}

struct PreparedTrack {
    track: PlaybackTrack,
    bytes: Vec<u8>,
    duration: Option<Duration>,
}

pub struct PlaybackEngine {
    commands: mpsc::Sender<PlaybackCommand>,
    state: Arc<Mutex<EngineState>>,
}

impl PlaybackEngine {
    pub fn new() -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(EngineState::new()));
        let worker_state = Arc::clone(&state);

        std::thread::Builder::new()
            .name("subsonic-playback".to_string())
            .spawn(move || {
                let mut worker = match PlaybackWorker::new(worker_state) {
                    Ok(worker) => {
                        let _ = init_tx.send(Ok(()));
                        worker
                    }
                    Err(error) => {
                        let _ = init_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                worker.run(command_rx);
            })
            .context("Could not start playback worker thread")?;

        match init_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self { commands: command_tx, state }),
            Ok(Err(message)) => Err(anyhow!(message)),
            Err(error) => Err(anyhow!("Playback worker did not initialise: {}", error)),
        }
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        let mut s = self.state.lock().expect("playback state lock");
        let error_to_emit = if s.error.is_some() && !s.error_reported {
            s.error_reported = true;
            s.error.clone()
        } else {
            None
        };
        let just_finished = s.just_finished;
        s.just_finished = false;
        let gapless_started = s.gapless_started.take();

        playback_snapshot_from_state(&s, error_to_emit, just_finished, gapless_started)
    }

    pub fn view_snapshot(&self) -> PlaybackSnapshot {
        let s = self.state.lock().expect("playback state lock");
        // This is intentionally non-consuming. Rendering the status panel must not
        // clear one-shot playback events such as a gapless handoff, otherwise the
        // UI can show the new track while the app state/queue cursor stays on the
        // previous row until a manual next/previous command is issued.
        playback_snapshot_from_state(&s, s.error.clone(), false, None)
    }

    pub fn tick(&self) {
        let _ = self.commands.send(PlaybackCommand::Tick);
    }

    pub fn play_track(&self, track: PlaybackTrack) -> Result<()> {
        self.commands
            .send(PlaybackCommand::PlayTrack(track))
            .context("Playback worker is not available")
    }

    pub fn prepare_next(&self, track: Option<PlaybackTrack>) {
        let _ = self.commands.send(PlaybackCommand::PrepareNext(track));
    }

    pub fn invalidate_prepared_next(&self) {
        let _ = self.commands.send(PlaybackCommand::InvalidatePreparedNext);
    }

    pub fn pause(&self) {
        let _ = self.commands.send(PlaybackCommand::Pause);
    }

    pub fn resume(&self) {
        let _ = self.commands.send(PlaybackCommand::Resume);
    }

    pub fn stop(&self) {
        let _ = self.commands.send(PlaybackCommand::Stop);
    }

    pub fn set_volume(&self, volume: u8) {
        let _ = self.commands.send(PlaybackCommand::SetVolume(volume.min(100)));
    }

    pub fn seek(&self, position: Duration) -> Result<()> {
        self.commands
            .send(PlaybackCommand::Seek(position))
            .context("Playback worker is not available")
    }

    pub fn reset_audio_output(&self) -> Result<()> {
        self.commands
            .send(PlaybackCommand::ResetAudioOutput)
            .context("Playback worker is not available")
    }

    pub fn audio_status(&self) -> PlaybackAudioStatus {
        let s = self.state.lock().expect("playback state lock");
        probe_audio_status(s.current_output_name.clone(), s.last_audio_reset_error.clone())
    }

    pub fn probe_audio_status() -> PlaybackAudioStatus {
        probe_audio_status(None, None)
    }
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        let _ = self.commands.send(PlaybackCommand::Shutdown);
    }
}

struct PlaybackWorker {
    _stream: OutputStream,
    handle: OutputStreamHandle,
    http: Client,
    state: Arc<Mutex<EngineState>>,
    sink: Option<Sink>,
    prefetch_tx: mpsc::Sender<PrefetchCommand>,
    prefetch_rx: mpsc::Receiver<PrefetchResult>,
    prepared_next: Option<PreparedTrack>,
    queued_gapless: Option<PreparedTrack>,
    requested_prefetch: Option<(u64, PlaybackTrack)>,
    next_prefetch_request_id: u64,
}

impl PlaybackWorker {
    fn new(state: Arc<Mutex<EngineState>>) -> Result<Self> {
        let output_name = default_output_device_name().ok();
        let (_stream, handle) = OutputStream::try_default()
            .context("Could not open a default audio output device. If Windows has just switched audio endpoints after RDP, Bluetooth, HDMI, USB audio, or sleep/wake, wait for the endpoint to settle and run audio reset or restart DISC locally.")?;
        {
            let mut s = state.lock().expect("playback state lock");
            s.current_output_name = output_name;
            s.last_audio_reset_error = None;
        }
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(300))
            .build()
            .context("Could not build playback HTTP client")?;
        let (prefetch_tx, prefetch_command_rx) = mpsc::channel();
        let (prefetch_result_tx, prefetch_rx) = mpsc::channel();
        let prefetch_http = http.clone();
        std::thread::Builder::new()
            .name("subsonic-gapless-prefetch".to_string())
            .spawn(move || prefetch_worker_loop(prefetch_http, prefetch_command_rx, prefetch_result_tx))
            .context("Could not start gapless prefetch worker thread")?;

        Ok(Self {
            _stream,
            handle,
            http,
            state,
            sink: None,
            prefetch_tx,
            prefetch_rx,
            prepared_next: None,
            queued_gapless: None,
            requested_prefetch: None,
            next_prefetch_request_id: 1,
        })
    }

    fn run(&mut self, commands: mpsc::Receiver<PlaybackCommand>) {
        loop {
            match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(PlaybackCommand::PlayTrack(track)) => self.play_track(track),
                Ok(PlaybackCommand::PrepareNext(track)) => self.prepare_next(track),
                Ok(PlaybackCommand::InvalidatePreparedNext) => self.invalidate_prepared_next(),
                Ok(PlaybackCommand::Pause) => self.pause(),
                Ok(PlaybackCommand::Resume) => self.resume(),
                Ok(PlaybackCommand::Stop) => self.stop(),
                Ok(PlaybackCommand::SetVolume(volume)) => self.set_volume(volume),
                Ok(PlaybackCommand::Seek(position)) => self.seek(position),
                Ok(PlaybackCommand::ResetAudioOutput) => self.reset_audio_output(),
                Ok(PlaybackCommand::Tick) => self.check_finished(),
                Ok(PlaybackCommand::Shutdown) => {
                    let _ = self.prefetch_tx.send(PrefetchCommand::Shutdown);
                    self.stop();
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => self.check_finished(),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.stop();
                    break;
                }
            }
        }
    }

    fn play_track(&mut self, track: PlaybackTrack) {
        self.poll_prefetch_results();
        if let Some(old_sink) = self.sink.take() {
            old_sink.stop();
        }
        let preloaded_bytes = self.take_prepared_bytes_for(&track);
        self.queued_gapless = None;
        {
            let mut s = self.state.lock().expect("playback state lock");
            s.state = PlaybackState::Buffering;
            s.current = Some(track.clone());
            s.started_at = None;
            s.paused_accumulated = Duration::ZERO;
            s.pause_started_at = None;
            s.duration = None;
            s.error = None;
            s.last_audio_reset_error = None;
            s.just_finished = false;
            s.gapless_started = None;
            s.finished_reported = false;
            s.error_reported = false;
        }

        if let Err(error) = self.load_and_start(track, preloaded_bytes) {
            self.mark_error(error.to_string());
        }
    }

    fn load_and_start(&mut self, track: PlaybackTrack, preloaded_bytes: Option<Vec<u8>>) -> Result<()> {
        let bytes = match preloaded_bytes {
            Some(bytes) => bytes,
            None => download_track_bytes(&self.http, &track)?,
        };

        let cursor = Cursor::new(bytes);
        let decoder = Decoder::new(cursor)
            .with_context(|| format!("Could not decode audio for {}", track.label()))?;
        let total_duration = decoder.total_duration();

        let sink = match Sink::try_new(&self.handle) {
            Ok(sink) => sink,
            Err(first_error) => {
                self.reopen_default_output_stream().with_context(|| {
                    format!(
                        "Could not create playback sink on the existing audio output, then could not reopen the current default output device. This can happen after Windows switches from RDP Remote Audio to local speakers/headphones, or after Bluetooth/HDMI/USB audio changes. Original sink error: {}",
                        first_error
                    )
                })?;
                Sink::try_new(&self.handle).with_context(|| {
                    format!(
                        "Could not create playback sink after reopening the current default audio output. Try audio devices, audio reset, Windows Sound > Volume mixer, or restart DISC after logging in locally. Original sink error: {}",
                        first_error
                    )
                })?
            }
        };

        let volume = {
            let s = self.state.lock().expect("playback state lock");
            s.volume
        };
        sink.set_volume((volume as f32) / 100.0);
        sink.append(decoder);
        sink.play();

        self.sink = Some(sink);
        let mut s = self.state.lock().expect("playback state lock");
        s.state = PlaybackState::Playing;
        s.started_at = Some(Instant::now());
        s.paused_accumulated = Duration::ZERO;
        s.pause_started_at = None;
        s.duration = total_duration;
        s.error = None;
        s.last_audio_reset_error = None;
        s.just_finished = false;
        s.gapless_started = None;
        s.finished_reported = false;
        s.error_reported = false;

        Ok(())
    }

    fn prepare_next(&mut self, track: Option<PlaybackTrack>) {
        self.poll_prefetch_results();
        match track {
            Some(track) => {
                if self
                    .prepared_next
                    .as_ref()
                    .map(|prepared| same_playback_track(&prepared.track, &track))
                    .unwrap_or(false)
                    || self
                        .queued_gapless
                        .as_ref()
                        .map(|queued| same_playback_track(&queued.track, &track))
                        .unwrap_or(false)
                {
                    return;
                }
                if self
                    .requested_prefetch
                    .as_ref()
                    .map(|(_, requested)| same_playback_track(requested, &track))
                    .unwrap_or(false)
                {
                    return;
                }
                self.prepared_next = None;
                let already_armed_label = self
                    .queued_gapless
                    .as_ref()
                    .filter(|queued| !same_playback_track(&queued.track, &track))
                    .map(|queued| queued.track.label());
                if let Some(label) = already_armed_label {
                    self.mark_transient_error(format!(
                        "Gapless handoff to {} is already armed and cannot be cancelled without interrupting playback.",
                        label
                    ));
                }
                let request_id = self.allocate_prefetch_request_id();
                self.requested_prefetch = Some((request_id, track.clone()));
                let _ = self.prefetch_tx.send(PrefetchCommand::Prepare { request_id, track });
            }
            None => {
                self.prepared_next = None;
                self.requested_prefetch = None;
                let already_armed_label = self
                    .queued_gapless
                    .as_ref()
                    .map(|queued| queued.track.label());
                if let Some(label) = already_armed_label {
                    self.mark_transient_error(format!(
                        "Gapless handoff to {} is already armed and will finish unless playback is changed or stopped.",
                        label
                    ));
                }
                let _ = self.prefetch_tx.send(PrefetchCommand::Clear);
            }
        }
    }

    fn invalidate_prepared_next(&mut self) {
        self.poll_prefetch_results();
        self.prepared_next = None;
        self.requested_prefetch = None;
        let _ = self.prefetch_tx.send(PrefetchCommand::Clear);

        if self.queued_gapless.take().is_some() {
            if let Err(error) = self.restart_current_without_queued_gapless() {
                self.mark_transient_error(format!(
                    "Could not reset stale gapless handoff after queue edit: {}",
                    error
                ));
            }
        }
    }

    fn restart_current_without_queued_gapless(&mut self) -> Result<()> {
        let (track, position, was_paused, volume) = {
            let s = self.state.lock().expect("playback state lock");
            let Some(track) = s.current.clone() else {
                return Ok(());
            };
            let position = Duration::from_millis(compute_position_ms(&s));
            let was_paused = s.state == PlaybackState::Paused;
            (track, position, was_paused, s.volume)
        };

        self.restart_track_on_current_output(track, position, was_paused, volume, "queue edit")
    }

    fn restart_track_on_current_output(
        &mut self,
        track: PlaybackTrack,
        position: Duration,
        was_paused: bool,
        volume: u8,
        reason: &str,
    ) -> Result<()> {
        let bytes = download_track_bytes(&self.http, &track)?;
        let decoder = Decoder::new(Cursor::new(bytes))
            .with_context(|| format!("Could not decode audio for {}", track.label()))?;
        let total_duration = decoder.total_duration();

        if let Some(old_sink) = self.sink.take() {
            old_sink.stop();
        }

        let sink = Sink::try_new(&self.handle).with_context(|| {
            format!(
                "Could not create playback sink while restoring playback after {}. If Windows audio changed after RDP, Bluetooth, HDMI, USB audio, or sleep/wake, try audio reset or restart DISC after selecting the correct output in Windows Sound > Volume mixer.",
                reason
            )
        })?;
        sink.set_volume((volume as f32) / 100.0);
        sink.append(decoder);
        if let Err(error) = sink.try_seek(position) {
            self.mark_transient_error(format!(
                "Could not restore playback position after {}: {}",
                reason,
                error
            ));
        }
        if was_paused {
            sink.pause();
        } else {
            sink.play();
        }

        self.sink = Some(sink);
        let mut s = self.state.lock().expect("playback state lock");
        let now = Instant::now();
        s.current = Some(track);
        s.started_at = Some(now.checked_sub(position).unwrap_or(now));
        s.pause_started_at = if was_paused { Some(now) } else { None };
        s.paused_accumulated = Duration::ZERO;
        s.duration = total_duration;
        s.state = if was_paused { PlaybackState::Paused } else { PlaybackState::Playing };
        s.error = None;
        s.last_audio_reset_error = None;
        s.just_finished = false;
        s.gapless_started = None;
        s.finished_reported = false;
        s.error_reported = false;
        Ok(())
    }

    fn reopen_default_output_stream(&mut self) -> Result<String> {
        let (stream, handle) = OutputStream::try_default()
            .context("Could not reopen the current default audio output device. Windows may still be holding or switching an endpoint such as RDP Remote Audio; check Sound settings/Volume mixer, then run audio reset again or restart DISC locally.")?;
        self._stream = stream;
        self.handle = handle;
        let output_name = default_output_device_name().unwrap_or_else(|_| "current default output device".to_string());
        let mut s = self.state.lock().expect("playback state lock");
        s.current_output_name = Some(output_name.clone());
        s.last_audio_reset_error = None;
        Ok(output_name)
    }

    fn reset_audio_output(&mut self) {
        self.poll_prefetch_results();
        self.prepared_next = None;
        self.queued_gapless = None;
        self.requested_prefetch = None;
        let _ = self.prefetch_tx.send(PrefetchCommand::Clear);

        let (track, position, was_paused, should_resume, volume) = {
            let s = self.state.lock().expect("playback state lock");
            let should_resume = matches!(
                s.state,
                PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Buffering | PlaybackState::Error
            ) && s.current.is_some();
            (
                s.current.clone(),
                Duration::from_millis(compute_position_ms(&s)),
                s.state == PlaybackState::Paused,
                should_resume,
                s.volume,
            )
        };

        if let Some(old_sink) = self.sink.take() {
            old_sink.stop();
        }

        if let Err(error) = self.reopen_default_output_stream() {
            let message = format!(
                "Audio reset failed: {}. If this followed an RDP/local-login switch, Windows may still be routing DISC to the old Remote Audio endpoint. Check Windows Sound > Volume mixer for disc.exe, select the laptop speakers/headphones, then run audio reset again or restart DISC locally.",
                error
            );
            self.mark_audio_reset_error(message);
            return;
        }

        if should_resume {
            if let Some(track) = track {
                if let Err(error) = self.restart_track_on_current_output(track, position, was_paused, volume, "audio reset") {
                    let message = format!(
                        "Audio reset reopened the current default output, but could not restore playback: {}. Try play again, audio reset, or restart DISC after confirming the Windows output device.",
                        error
                    );
                    self.mark_audio_reset_error(message);
                }
            }
        } else {
            let mut s = self.state.lock().expect("playback state lock");
            if s.state == PlaybackState::Error {
                s.state = PlaybackState::Stopped;
            }
            s.error = None;
            s.last_audio_reset_error = None;
            s.error_reported = false;
            s.just_finished = false;
            s.gapless_started = None;
            s.finished_reported = false;
        }
    }

    fn allocate_prefetch_request_id(&mut self) -> u64 {
        let request_id = self.next_prefetch_request_id;
        self.next_prefetch_request_id = self.next_prefetch_request_id.wrapping_add(1).max(1);
        request_id
    }

    fn poll_prefetch_results(&mut self) {
        while let Ok(result) = self.prefetch_rx.try_recv() {
            let is_current_request = self
                .requested_prefetch
                .as_ref()
                .map(|(request_id, requested)| *request_id == result.request_id && same_playback_track(requested, &result.track))
                .unwrap_or(false);
            if !is_current_request {
                continue;
            }
            self.requested_prefetch = None;
            match result.result {
                Ok(bytes) => {
                    let duration = match validate_preloaded_audio(&bytes) {
                        Ok(duration) => duration,
                        Err(message) => {
                            self.prepared_next = None;
                            self.mark_transient_error(format!(
                                "Gapless preload decode check failed for {}: {}",
                                result.track.label(),
                                message
                            ));
                            continue;
                        }
                    };
                    self.prepared_next = Some(PreparedTrack {
                        track: result.track,
                        bytes,
                        duration,
                    });
                }
                Err(message) => {
                    self.prepared_next = None;
                    self.mark_transient_error(format!(
                        "Gapless preload failed for {}: {}",
                        result.track.label(),
                        message
                    ));
                }
            }
        }
    }

    fn take_prepared_bytes_for(&mut self, track: &PlaybackTrack) -> Option<Vec<u8>> {
        if let Some(prepared) = self.prepared_next.take() {
            if same_playback_track(&prepared.track, track) {
                return Some(prepared.bytes);
            }
            self.prepared_next = Some(prepared);
        }

        if let Some(queued) = self.queued_gapless.take() {
            if same_playback_track(&queued.track, track) {
                return Some(queued.bytes);
            }
            self.queued_gapless = Some(queued);
        }

        None
    }

    fn pause(&mut self) {
        let mut s = self.state.lock().expect("playback state lock");
        if s.state == PlaybackState::Playing {
            if let Some(sink) = &self.sink {
                sink.pause();
            }
            s.state = PlaybackState::Paused;
            s.pause_started_at = Some(Instant::now());
        }
    }

    fn resume(&mut self) {
        let mut s = self.state.lock().expect("playback state lock");
        if s.state == PlaybackState::Paused {
            if let Some(sink) = &self.sink {
                sink.play();
            }
            if let Some(paused_at) = s.pause_started_at.take() {
                s.paused_accumulated += paused_at.elapsed();
            }
            s.state = PlaybackState::Playing;
        }
    }

    fn stop(&mut self) {
        self.prepared_next = None;
        self.queued_gapless = None;
        self.requested_prefetch = None;
        let _ = self.prefetch_tx.send(PrefetchCommand::Clear);
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        let mut s = self.state.lock().expect("playback state lock");
        s.state = PlaybackState::Stopped;
        s.current = None;
        s.started_at = None;
        s.pause_started_at = None;
        s.paused_accumulated = Duration::ZERO;
        s.duration = None;
        s.error = None;
        s.just_finished = false;
        s.gapless_started = None;
        s.finished_reported = false;
        s.error_reported = false;
    }

    fn set_volume(&mut self, volume: u8) {
        let volume = volume.min(100);
        let mut s = self.state.lock().expect("playback state lock");
        s.volume = volume;
        if let Some(sink) = &self.sink {
            sink.set_volume((volume as f32) / 100.0);
        }
    }

    fn seek(&mut self, position: Duration) {
        let Some(sink) = &self.sink else {
            self.mark_transient_error("Nothing is currently loaded to seek.".to_string());
            return;
        };

        if let Err(error) = sink.try_seek(position) {
            self.mark_transient_error(format!("Could not seek: {}", error));
            return;
        }

        let mut s = self.state.lock().expect("playback state lock");
        let now = Instant::now();
        s.started_at = Some(now.checked_sub(position).unwrap_or(now));
        s.paused_accumulated = Duration::ZERO;
        if s.state == PlaybackState::Paused {
            s.pause_started_at = Some(now);
        } else {
            s.pause_started_at = None;
        }
        s.just_finished = false;
        s.gapless_started = None;
        s.finished_reported = false;
        s.error = None;
        s.error_reported = false;
    }

    fn mark_transient_error(&mut self, message: String) {
        let mut s = self.state.lock().expect("playback state lock");
        s.error = Some(message);
        s.error_reported = false;
    }

    fn mark_audio_reset_error(&mut self, message: String) {
        let mut s = self.state.lock().expect("playback state lock");
        s.state = PlaybackState::Error;
        s.error = Some(message.clone());
        s.last_audio_reset_error = Some(message);
        s.started_at = None;
        s.pause_started_at = None;
        s.paused_accumulated = Duration::ZERO;
        s.duration = None;
        s.just_finished = false;
        s.gapless_started = None;
        s.finished_reported = false;
        s.error_reported = false;
    }

    fn mark_error(&mut self, message: String) {
        self.prepared_next = None;
        self.queued_gapless = None;
        self.requested_prefetch = None;
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        let mut s = self.state.lock().expect("playback state lock");
        s.state = PlaybackState::Error;
        s.error = Some(message);
        s.started_at = None;
        s.pause_started_at = None;
        s.paused_accumulated = Duration::ZERO;
        s.duration = None;
        s.just_finished = false;
        s.gapless_started = None;
        s.finished_reported = false;
        s.error_reported = false;
    }

    fn check_finished(&mut self) {
        self.poll_prefetch_results();
        if self.sink.is_none() {
            return;
        }

        let should_arm_gapless = {
            let s = self.state.lock().expect("playback state lock");
            if s.state != PlaybackState::Playing || s.current.is_none() || s.finished_reported {
                false
            } else if self.prepared_next.is_none() || self.queued_gapless.is_some() {
                false
            } else if let Some(duration) = s.duration {
                let position = Duration::from_millis(compute_position_ms(&s));
                position < duration
                    && duration.saturating_sub(position) <= Duration::from_millis(900)
            } else {
                false
            }
        };

        if should_arm_gapless {
            self.arm_gapless_next();
        }

        let gapless_transition = {
            let s = self.state.lock().expect("playback state lock");
            if s.state != PlaybackState::Playing || s.current.is_none() || s.finished_reported {
                None
            } else if self.queued_gapless.is_some() {
                s.duration.and_then(|duration| {
                    let position = Duration::from_millis(compute_position_ms(&s));
                    if position >= duration {
                        Some(position.saturating_sub(duration))
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        };

        if let Some(overrun) = gapless_transition {
            if let Some(queued) = self.queued_gapless.take() {
                let mut s = self.state.lock().expect("playback state lock");
                let now = Instant::now();
                s.current = Some(queued.track.clone());
                s.started_at = Some(now.checked_sub(overrun).unwrap_or(now));
                s.paused_accumulated = Duration::ZERO;
                s.pause_started_at = None;
                s.duration = queued.duration;
                s.state = PlaybackState::Playing;
                s.just_finished = false;
                s.gapless_started = Some(queued.track);
                s.finished_reported = false;
                s.error = None;
                s.error_reported = false;
                return;
            }
        }

        let sink_finished = self.sink.as_ref().map(|sink| sink.empty()).unwrap_or(false);
        let duration_elapsed = {
            let s = self.state.lock().expect("playback state lock");
            if s.state != PlaybackState::Playing || s.current.is_none() || s.finished_reported {
                false
            } else {
                match s.duration {
                    Some(duration) => {
                        let position = Duration::from_millis(compute_position_ms(&s));
                        position >= duration.checked_add(Duration::from_millis(250)).unwrap_or(duration)
                    }
                    None => false,
                }
            }
        };

        if sink_finished || duration_elapsed {
            let mut s = self.state.lock().expect("playback state lock");
            if s.state == PlaybackState::Playing && s.current.is_some() && !s.finished_reported {
                s.just_finished = true;
                s.finished_reported = true;
            }
        }
    }

    fn arm_gapless_next(&mut self) {
        if self.queued_gapless.is_some() {
            return;
        }
        let Some(prepared) = self.prepared_next.take() else {
            return;
        };
        if self.sink.is_none() {
            self.prepared_next = Some(prepared);
            return;
        }

        match Decoder::new(Cursor::new(prepared.bytes.clone())) {
            Ok(decoder) => {
                if let Some(sink) = &self.sink {
                    sink.append(decoder);
                    self.queued_gapless = Some(prepared);
                } else {
                    self.prepared_next = Some(prepared);
                }
            }
            Err(error) => {
                self.mark_transient_error(format!(
                    "Could not arm gapless handoff for {}: {}",
                    prepared.track.label(),
                    error
                ));
            }
        }
    }
}

fn probe_audio_status(active_output: Option<String>, last_reset_error: Option<String>) -> PlaybackAudioStatus {
    let mut output_error = None;
    let default_output = match default_output_device_name() {
        Ok(name) => Some(name),
        Err(error) => {
            output_error = Some(error.to_string());
            None
        }
    };

    let output_devices = match output_device_names() {
        Ok(devices) => devices,
        Err(error) => {
            let message = error.to_string();
            output_error = Some(match output_error {
                Some(existing) => format!("{}; {}", existing, message),
                None => message,
            });
            Vec::new()
        }
    };

    PlaybackAudioStatus {
        active_output,
        default_output,
        output_devices,
        output_error,
        last_reset_error,
    }
}

fn default_output_device_name() -> Result<String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("No default output audio device is currently reported by the OS"))?;
    device
        .name()
        .context("Could not read the default output device name")
}

fn output_device_names() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .context("Could not enumerate output audio devices")?;
    let mut names = Vec::new();
    for device in devices {
        names.push(device.name().unwrap_or_else(|_| "unknown output device".to_string()));
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn prefetch_worker_loop(
    http: Client,
    commands: mpsc::Receiver<PrefetchCommand>,
    results: mpsc::Sender<PrefetchResult>,
) {
    // Dispatch each prefetch in its own short-lived worker thread. That keeps
    // newer gapless targets from being blocked behind an older, slow download;
    // stale results are discarded by request id in the playback worker.
    while let Ok(command) = commands.recv() {
        match command {
            PrefetchCommand::Prepare { request_id, track } => {
                let http = http.clone();
                let results = results.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("subsonic-gapless-prefetch-{}", request_id))
                    .spawn(move || {
                        let result = download_track_bytes(&http, &track).map_err(|error| error.to_string());
                        let _ = results.send(PrefetchResult { request_id, track, result });
                    });
            }
            PrefetchCommand::Clear => {}
            PrefetchCommand::Shutdown => break,
        }
    }
}

fn validate_preloaded_audio(bytes: &[u8]) -> std::result::Result<Option<Duration>, String> {
    Decoder::new(Cursor::new(bytes.to_vec()))
        .map(|decoder| decoder.total_duration())
        .map_err(|error| error.to_string())
}

fn download_track_bytes(http: &Client, track: &PlaybackTrack) -> Result<Vec<u8>> {
    let bytes = http
        .get(&track.stream_url)
        .send()
        .with_context(|| format!("Could not reach stream for {}", track.label()))?
        .error_for_status()
        .with_context(|| format!("Stream request failed for {}", track.label()))?
        .bytes()
        .with_context(|| format!("Could not read audio stream for {}", track.label()))?;
    Ok(bytes.to_vec())
}


fn playback_snapshot_from_state(
    s: &EngineState,
    error: Option<String>,
    just_finished: bool,
    gapless_started: Option<PlaybackTrack>,
) -> PlaybackSnapshot {
    PlaybackSnapshot {
        state: s.state,
        current: s.current.clone(),
        volume: s.volume,
        position_ms: match s.duration {
            Some(duration) => compute_position_ms(s).min(duration.as_millis() as u64),
            None => compute_position_ms(s),
        },
        duration_ms: s.duration.map(|d| d.as_millis() as u64),
        output_device: s.current_output_name.clone(),
        error,
        just_finished,
        gapless_started,
    }
}

fn same_playback_track(left: &PlaybackTrack, right: &PlaybackTrack) -> bool {
    // Subsonic stream URLs include a fresh auth salt/token each time they are built,
    // so cache matching must use stable track identity rather than the exact URL.
    left.id == right.id && left.server_alias == right.server_alias
}

fn compute_position_ms(s: &EngineState) -> u64 {
    match s.state {
        PlaybackState::Stopped | PlaybackState::Error => 0,
        PlaybackState::Buffering => 0,
        PlaybackState::Paused => {
            if let Some(started_at) = s.started_at {
                let base = if let Some(paused_at) = s.pause_started_at {
                    paused_at.saturating_duration_since(started_at)
                } else {
                    Duration::ZERO
                };
                base.saturating_sub(s.paused_accumulated).as_millis() as u64
            } else {
                0
            }
        }
        PlaybackState::Playing => {
            if let Some(started_at) = s.started_at {
                started_at.elapsed().saturating_sub(s.paused_accumulated).as_millis() as u64
            } else {
                0
            }
        }
    }
}
